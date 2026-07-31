//! Apple's Screen Sharing pasteboard messages.
//!
//! macOS does not bridge its pasteboard through RFB Client/ServerCutText, even
//! after accepting the Extended Clipboard pseudo-encoding. Its Standard viewer
//! instead enables `AutoPasteboard`, receives change status messages, fetches a
//! compressed pasteboard archive, and sends the same archive shape in reverse.
//! This module owns those byte formats; session state and browser messages remain
//! in [`crate::vnc`].

use anyhow::Context as _;
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};

use crate::protocol::{MAX_CLIPBOARD_BYTES, clipboard_fits};

const UTF8_TEXT: &[u8] = b"public.utf8-plain-text";
const MAX_ARCHIVE_BYTES: usize = MAX_CLIPBOARD_BYTES * 4 + 64 * 1024;
pub const MAX_COMPRESSED_BYTES: u64 = MAX_ARCHIVE_BYTES as u64 + 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Incoming {
    Text(String),
    Oversized(u64),
    NoText,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub session_id: u32,
    pub uncompressed: u32,
    pub compressed: u32,
}

/// Start or stop monitoring the Mac's pasteboard.
pub fn auto_pasteboard(enabled: bool) -> [u8; 8] {
    [0x15, 0, 0, if enabled { 1 } else { 2 }, 0, 0, 0, 0]
}

/// Ask for the full pasteboard after a `MiscStatus` change notification.
pub fn fetch(session_id: u32) -> [u8; 8] {
    let mut msg = [0u8; 8];
    msg[0] = 0x0b;
    msg[4..8].copy_from_slice(&session_id.to_be_bytes());
    msg
}

/// The 15 bytes after a server `ClipboardSend` message type.
pub fn header(raw: &[u8; 15]) -> Header {
    Header {
        session_id: u32::from_be_bytes(raw[3..7].try_into().expect("four-byte session id")),
        uncompressed: u32::from_be_bytes(
            raw[7..11].try_into().expect("four-byte uncompressed size"),
        ),
        compressed: u32::from_be_bytes(
            raw[11..15].try_into().expect("four-byte compressed size"),
        ),
    }
}

/// Put UTF-8 text on the Mac's pasteboard immediately.
pub fn send(session_id: u32, text: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        clipboard_fits(text),
        "clipboard is {} bytes, over the {MAX_CLIPBOARD_BYTES} byte limit",
        text.len()
    );
    let archive = archive(text);
    let compressed = deflate(&archive)?;
    let mut msg = Vec::with_capacity(16 + compressed.len());
    msg.extend_from_slice(&[0x1f, 0, 0, 0]);
    msg.extend_from_slice(&session_id.to_be_bytes());
    msg.extend_from_slice(&(archive.len() as u32).to_be_bytes());
    msg.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    msg.extend_from_slice(&compressed);
    Ok(msg)
}

/// Inflate and read the text flavor from a server pasteboard archive.
pub fn parse(header: Header, compressed: &[u8]) -> anyhow::Result<Incoming> {
    anyhow::ensure!(
        u64::from(header.compressed) <= MAX_COMPRESSED_BYTES,
        "Apple clipboard declares {} compressed bytes, over the {MAX_COMPRESSED_BYTES} byte limit",
        header.compressed
    );
    anyhow::ensure!(
        compressed.len() == header.compressed as usize,
        "Apple clipboard declared {} compressed bytes but supplied {}",
        header.compressed,
        compressed.len()
    );
    if header.uncompressed == 0 {
        return Ok(Incoming::Text(String::new()));
    }
    anyhow::ensure!(
        header.uncompressed as usize <= MAX_ARCHIVE_BYTES,
        "Apple clipboard declares {} inflated bytes, over the {MAX_ARCHIVE_BYTES} byte limit",
        header.uncompressed
    );
    let inflated = inflate(compressed, header.uncompressed as usize)?;
    parse_archive(&inflated)
}

fn archive(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(24 + UTF8_TEXT.len() + bytes.len());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&(UTF8_TEXT.len() as u32).to_be_bytes());
    out.extend_from_slice(UTF8_TEXT);
    out.extend_from_slice(&0u32.to_be_bytes()); // reserved
    out.extend_from_slice(&0u32.to_be_bytes()); // alias count
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

fn parse_archive(mut input: &[u8]) -> anyhow::Result<Incoming> {
    let items = take_u32(&mut input).context("Apple pasteboard has no item count")?;
    anyhow::ensure!(items <= 32, "Apple pasteboard declares {items} flavors");
    for _ in 0..items {
        let name = take_counted(&mut input, 1024).context("reading Apple pasteboard flavor")?;
        take_u32(&mut input).context("Apple pasteboard flavor has no reserved word")?;
        let aliases = take_u32(&mut input).context("Apple pasteboard flavor has no alias count")?;
        anyhow::ensure!(aliases <= 64, "Apple pasteboard flavor declares {aliases} aliases");
        for _ in 0..aliases {
            take_counted(&mut input, 4096).context("reading Apple pasteboard alias name")?;
            take_counted(&mut input, 4096).context("reading Apple pasteboard alias value")?;
        }
        let size = take_u32(&mut input).context("Apple pasteboard flavor has no data size")?;
        if name == UTF8_TEXT && u64::from(size) > MAX_CLIPBOARD_BYTES as u64 {
            return Ok(Incoming::Oversized(u64::from(size)));
        }
        let data = take(&mut input, size as usize).context("reading Apple pasteboard flavor data")?;
        if name == UTF8_TEXT {
            let text = std::str::from_utf8(data)
                .context("Apple pasteboard text is not UTF-8")?
                .to_owned();
            return Ok(if clipboard_fits(&text) {
                Incoming::Text(text)
            } else {
                Incoming::Oversized(text.len() as u64)
            });
        }
    }
    Ok(Incoming::NoText)
}

fn deflate(input: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut encoder = Compress::new(Compression::default(), true);
    let mut out = Vec::with_capacity(input.len() + 64);
    encoder
        .compress_vec(input, &mut out, FlushCompress::Sync)
        .context("deflating the Apple pasteboard")?;
    anyhow::ensure!(
        encoder.total_in() == input.len() as u64,
        "Apple pasteboard compressor consumed {} of {} bytes",
        encoder.total_in(),
        input.len()
    );
    Ok(out)
}

fn inflate(input: &[u8], expected: usize) -> anyhow::Result<Vec<u8>> {
    let mut decoder = Decompress::new(true);
    let mut out = Vec::with_capacity(expected);
    decoder
        .decompress_vec(input, &mut out, FlushDecompress::Sync)
        .context("inflating the Apple pasteboard")?;
    anyhow::ensure!(
        decoder.total_in() == input.len() as u64 && out.len() == expected,
        "Apple pasteboard inflated {} bytes from {} of {} compressed bytes; expected {expected}",
        out.len(),
        decoder.total_in(),
        input.len()
    );
    Ok(out)
}

fn take_u32(input: &mut &[u8]) -> Option<u32> {
    let raw = take(input, 4)?;
    Some(u32::from_be_bytes(raw.try_into().expect("four bytes")))
}

fn take_counted<'a>(input: &mut &'a [u8], max: usize) -> anyhow::Result<&'a [u8]> {
    let len = take_u32(input).context("missing byte count")? as usize;
    anyhow::ensure!(len <= max, "counted field is {len} bytes, over its {max}-byte limit");
    take(input, len).context("counted field is truncated")
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    let (head, tail) = input.split_at_checked(len)?;
    *input = tail;
    Some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_messages_match_the_apple_wire() {
        assert_eq!(auto_pasteboard(true), [0x15, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(auto_pasteboard(false), [0x15, 0, 0, 2, 0, 0, 0, 0]);
        assert_eq!(fetch(0x1234_5678), [0x0b, 0, 0, 0, 0x12, 0x34, 0x56, 0x78]);
    }

    #[test]
    fn clipboard_send_round_trips_unicode() {
        let msg = send(7, "copied ☕").unwrap();
        assert_eq!(msg[0], 0x1f);
        let raw: [u8; 15] = msg[1..16].try_into().unwrap();
        let header = header(&raw);
        assert_eq!(header.session_id, 7);
        assert_eq!(
            parse(header, &msg[16..]).unwrap(),
            Incoming::Text("copied ☕".to_owned())
        );
    }

    #[test]
    fn an_empty_remote_pasteboard_is_empty_text() {
        assert_eq!(
            parse(
                Header { session_id: 0, uncompressed: 0, compressed: 0 },
                &[]
            )
            .unwrap(),
            Incoming::Text(String::new())
        );
    }

    #[test]
    fn oversized_text_is_reported_from_its_flavor_length() {
        let mut bytes = archive("");
        let size_at = 4 + 4 + UTF8_TEXT.len() + 4 + 4;
        bytes[size_at..size_at + 4]
            .copy_from_slice(&((MAX_CLIPBOARD_BYTES + 1) as u32).to_be_bytes());
        assert_eq!(
            parse_archive(&bytes).unwrap(),
            Incoming::Oversized((MAX_CLIPBOARD_BYTES + 1) as u64)
        );
    }
}
