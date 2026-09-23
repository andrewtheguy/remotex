//! Apple's Screen Sharing pasteboard messages.
//!
//! macOS does not bridge its pasteboard through RFB Client/ServerCutText, even
//! after accepting the Extended Clipboard pseudo-encoding. Apple's viewers instead
//! enable `AutoPasteboard`, receive change status messages, fetch a compressed
//! pasteboard archive, and send the same archive shape in reverse. Standard mode
//! carries those messages on the plain RFB stream. High Performance enables
//! monitoring before encryption starts, then carries fetches and archive data
//! inside encrypted records. This module owns the byte formats; session state and
//! browser messages remain in [`crate::vnc`].

use anyhow::Context as _;
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::protocol::{MAX_CLIPBOARD_BYTES, clipboard_fits};

const UTF8_TEXT: &[u8] = b"public.utf8-plain-text";
/// The largest archive either of Apple's ends accepts, compressed or inflated. An
/// archive carries every saved flavor of every item — rich text, HTML, images, PDF
/// — so a short text selection can arrive inside megabytes of the rest, which is
/// streamed past rather than held.
pub const MAX_ARCHIVE_BYTES: u32 = 100 * 1024 * 1024;
/// Bytes inflated per step.
const INFLATE_CHUNK: usize = 64 * 1024;

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

/// Inflate a server pasteboard archive as it arrives and keep only its text.
///
/// The archive is a run of items, each a `u32` flavor count and then per flavor a
/// counted name, a reserved `u32`, a `u32` count of `{counted key, counted value}`
/// tags, and the counted data — `ScreensharingAgent`'s `CopyPackedScrapData`. Only
/// the first `public.utf8-plain-text` flavor is kept; once it is read, the rest of
/// the compressed bytes are not inflated at all.
pub struct Receiver {
    decoder: Decompress,
    declared: u32,
    archive: Archive,
}

impl Receiver {
    pub fn new(header: Header) -> anyhow::Result<Self> {
        anyhow::ensure!(
            header.compressed <= MAX_ARCHIVE_BYTES && header.uncompressed <= MAX_ARCHIVE_BYTES,
            "Apple clipboard declares {} bytes compressed from {}, over the {MAX_ARCHIVE_BYTES} byte limit",
            header.compressed,
            header.uncompressed
        );
        Ok(Self { decoder: Decompress::new(true), declared: header.uncompressed, archive: Archive::default() })
    }

    /// Inflate the next compressed bytes of the archive.
    ///
    /// A buffer filled to the brim may leave inflated bytes behind in the decoder
    /// after the last compressed byte is taken — flate2 does not promise otherwise,
    /// and C zlib can stop mid-match — so the loop runs until a pass leaves room
    /// to spare, not until the input is gone.
    pub fn feed(&mut self, mut compressed: &[u8]) -> anyhow::Result<()> {
        let mut out = vec![0u8; INFLATE_CHUNK];
        let mut full = false;
        while (full || !compressed.is_empty()) && self.archive.text.is_none() {
            let (before_in, before_out) = (self.decoder.total_in(), self.decoder.total_out());
            self.decoder
                .decompress(compressed, &mut out, FlushDecompress::Sync)
                .context("inflating the Apple pasteboard")?;
            let read = (self.decoder.total_in() - before_in) as usize;
            let written = (self.decoder.total_out() - before_out) as usize;
            anyhow::ensure!(
                self.decoder.total_out() <= u64::from(self.declared),
                "Apple pasteboard inflates past its declared {} bytes",
                self.declared
            );
            full = written == out.len();
            anyhow::ensure!(
                full || read != 0 || written != 0 || compressed.is_empty(),
                "Apple pasteboard inflater made no progress"
            );
            compressed = &compressed[read..];
            self.archive.feed(&out[..written])?;
        }
        Ok(())
    }

    /// What the archive held, once every compressed byte has been fed.
    pub fn finish(self) -> anyhow::Result<Incoming> {
        let Some(text) = self.archive.text else {
            anyhow::ensure!(
                self.decoder.total_out() == u64::from(self.declared),
                "Apple pasteboard inflated {} of its declared {} bytes",
                self.decoder.total_out(),
                self.declared
            );
            anyhow::ensure!(
                self.archive.at_item_boundary(),
                "Apple pasteboard archive ends inside an item"
            );
            return Ok(if self.declared == 0 { Incoming::Text(String::new()) } else { Incoming::NoText });
        };
        let text = match text {
            Found::Text(bytes) => String::from_utf8(bytes).context("Apple pasteboard text is not UTF-8")?,
            Found::Oversized(size) => return Ok(Incoming::Oversized(size)),
        };
        Ok(if clipboard_fits(&text) { Incoming::Text(text) } else { Incoming::Oversized(text.len() as u64) })
    }
}

/// Inflate and read the text flavor from a whole server pasteboard archive.
#[cfg(test)]
pub fn parse(header: Header, compressed: &[u8]) -> anyhow::Result<Incoming> {
    anyhow::ensure!(
        compressed.len() == header.compressed as usize,
        "Apple clipboard declared {} compressed bytes but supplied {}",
        header.compressed,
        compressed.len()
    );
    let mut receiver = Receiver::new(header)?;
    receiver.feed(compressed)?;
    receiver.finish()
}

fn archive(text: &str) -> Vec<u8> {
    // An item with no flavors clears the Mac's pasteboard. An empty text flavor
    // would not: the agent takes a flavor with no data for a promise, and a paste
    // on the Mac would then wait on the viewer to keep it.
    if text.is_empty() {
        return 0u32.to_be_bytes().to_vec();
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(24 + UTF8_TEXT.len() + bytes.len());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&(UTF8_TEXT.len() as u32).to_be_bytes());
    out.extend_from_slice(UTF8_TEXT);
    out.extend_from_slice(&0u32.to_be_bytes()); // reserved
    out.extend_from_slice(&0u32.to_be_bytes()); // tag count
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

/// The text flavor, once the archive has reached it.
enum Found {
    Text(Vec<u8>),
    Oversized(u64),
}

/// Where the archive reader is: the field it is reading, and whether its bytes are
/// kept (a count, a name, the text) or only counted past (tags, other data).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Field {
    #[default]
    FlavorCount,
    NameLength,
    Name,
    Reserved,
    TagCount,
    TagKeyLength,
    TagKey,
    TagValueLength,
    TagValue,
    DataLength,
    Data,
}

#[derive(Default)]
struct Archive {
    field: Field,
    /// Bytes the current field still needs.
    want: usize,
    /// Whether the current field's bytes are kept in `kept`.
    keep: bool,
    kept: Vec<u8>,
    flavors_left: u32,
    tags_left: u32,
    is_text: bool,
    text: Option<Found>,
}

impl Archive {
    fn feed(&mut self, mut input: &[u8]) -> anyhow::Result<()> {
        if self.want == 0 && self.field == Field::FlavorCount && self.kept.is_empty() {
            self.expect(Field::FlavorCount, 4, true);
        }
        while !input.is_empty() && self.text.is_none() {
            let take = self.want.min(input.len());
            if self.keep {
                self.kept.extend_from_slice(&input[..take]);
            }
            input = &input[take..];
            self.want -= take;
            while self.want == 0 && self.text.is_none() {
                self.next()?;
            }
        }
        Ok(())
    }

    fn at_item_boundary(&self) -> bool {
        self.field == Field::FlavorCount && self.kept.is_empty()
    }

    fn expect(&mut self, field: Field, want: usize, keep: bool) {
        self.field = field;
        self.want = want;
        self.keep = keep;
        self.kept.clear();
    }

    fn counted(&mut self, field: Field, max: usize, keep: bool) -> anyhow::Result<()> {
        let len = self.number() as usize;
        anyhow::ensure!(len <= max, "Apple pasteboard field is {len} bytes, over its {max}-byte limit");
        self.expect(field, len, keep);
        Ok(())
    }

    fn number(&self) -> u32 {
        u32::from_be_bytes(self.kept[..4].try_into().expect("a four-byte field"))
    }

    /// The current field is complete: act on it and move to the next.
    fn next(&mut self) -> anyhow::Result<()> {
        match self.field {
            Field::FlavorCount => {
                self.flavors_left = self.number();
                if self.flavors_left == 0 {
                    self.expect(Field::FlavorCount, 4, true);
                    // A whole empty item, not a count half-read.
                    self.kept.clear();
                } else {
                    self.expect(Field::NameLength, 4, true);
                }
            }
            Field::NameLength => self.counted(Field::Name, 1024, true)?,
            Field::Name => {
                self.is_text = self.kept == UTF8_TEXT;
                self.expect(Field::Reserved, 4, false);
            }
            Field::Reserved => self.expect(Field::TagCount, 4, true),
            Field::TagCount => {
                self.tags_left = self.number();
                anyhow::ensure!(self.tags_left <= 64, "Apple pasteboard flavor declares {} tags", self.tags_left);
                if self.tags_left == 0 {
                    self.expect(Field::DataLength, 4, true);
                } else {
                    self.expect(Field::TagKeyLength, 4, true);
                }
            }
            Field::TagKeyLength => self.counted(Field::TagKey, 4096, false)?,
            Field::TagKey => self.expect(Field::TagValueLength, 4, true),
            Field::TagValueLength => self.counted(Field::TagValue, 4096, false)?,
            Field::TagValue => {
                self.tags_left -= 1;
                if self.tags_left == 0 {
                    self.expect(Field::DataLength, 4, true);
                } else {
                    self.expect(Field::TagKeyLength, 4, true);
                }
            }
            Field::DataLength => {
                let size = self.number();
                if self.is_text && size as usize > MAX_CLIPBOARD_BYTES {
                    self.text = Some(Found::Oversized(u64::from(size)));
                } else {
                    self.expect(Field::Data, size as usize, self.is_text);
                }
            }
            Field::Data => {
                if self.is_text {
                    self.text = Some(Found::Text(std::mem::take(&mut self.kept)));
                    return Ok(());
                }
                self.flavors_left -= 1;
                if self.flavors_left == 0 {
                    self.expect(Field::FlavorCount, 4, true);
                    self.kept.clear();
                } else {
                    self.expect(Field::NameLength, 4, true);
                }
            }
        }
        Ok(())
    }
}

fn deflate(input: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut encoder = Compress::new(Compression::default(), true);
    // A Vec's spare capacity is the entire output buffer `compress_vec` gets: it
    // never reallocates. This is a generous single-pass bound, while the loop is
    // still required because Sync output may exceed ordinary deflate bounds.
    let bound = input.len().saturating_add(input.len() / 16).saturating_add(256);
    let mut out = Vec::with_capacity(bound);
    let flush_status = loop {
        if out.spare_capacity_mut().len() < 32 * 1024 {
            out.reserve(32 * 1024);
        }
        let available = out.spare_capacity_mut().len();
        let before_in = encoder.total_in();
        let before_out = encoder.total_out();
        let consumed = encoder.total_in() as usize;
        let status = encoder
            .compress_vec(&input[consumed..], &mut out, FlushCompress::Sync)
            .context("deflating the Apple pasteboard")?;
        let read = encoder.total_in() - before_in;
        let written = encoder.total_out() - before_out;
        anyhow::ensure!(
            status != Status::StreamEnd,
            "Apple pasteboard compressor ended a Sync-flushed stream"
        );
        if encoder.total_in() == input.len() as u64 && written < available as u64 {
            break status;
        }
        anyhow::ensure!(
            read != 0 || written != 0,
            "Apple pasteboard compressor made no progress before completing its Sync flush"
        );
    };
    anyhow::ensure!(
        encoder.total_in() == input.len() as u64,
        "Apple pasteboard compressor consumed {} of {} bytes",
        encoder.total_in(),
        input.len()
    );
    anyhow::ensure!(
        matches!(flush_status, Status::Ok | Status::BufError),
        "Apple pasteboard compressor did not complete its Sync flush"
    );
    Ok(out)
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
    fn a_large_incompressible_stream_is_fully_sync_flushed() {
        let mut state = 0x1234_5678u32;
        let input: Vec<u8> = (0..256 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let compressed = deflate(&input).unwrap();
        assert!(compressed.ends_with(&[0, 0, 0xff, 0xff]));
        let mut inflated = Vec::with_capacity(input.len());
        Decompress::new(true)
            .decompress_vec(&compressed, &mut inflated, FlushDecompress::Sync)
            .unwrap();
        assert_eq!(inflated, input);
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

    /// A header and the compressed archive, as the Mac would send them.
    fn from_mac(archive: &[u8]) -> (Header, Vec<u8>) {
        let compressed = deflate(archive).unwrap();
        let header = Header {
            session_id: 0,
            uncompressed: archive.len() as u32,
            compressed: compressed.len() as u32,
        };
        (header, compressed)
    }

    /// A flavor as the agent packs it: name, reserved word, tags, data.
    fn flavor(out: &mut Vec<u8>, name: &[u8], tags: &[(&[u8], &[u8])], data: &[u8]) {
        out.extend_from_slice(&(name.len() as u32).to_be_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(tags.len() as u32).to_be_bytes());
        for (key, value) in tags {
            out.extend_from_slice(&(key.len() as u32).to_be_bytes());
            out.extend_from_slice(key);
            out.extend_from_slice(&(value.len() as u32).to_be_bytes());
            out.extend_from_slice(value);
        }
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
    }

    #[test]
    fn oversized_text_is_reported_from_its_flavor_length() {
        let mut bytes = archive("x");
        let size_at = 4 + 4 + UTF8_TEXT.len() + 4 + 4;
        bytes[size_at..size_at + 4]
            .copy_from_slice(&((MAX_CLIPBOARD_BYTES + 1) as u32).to_be_bytes());
        let (header, compressed) = from_mac(&bytes);
        assert_eq!(
            parse(header, &compressed).unwrap(),
            Incoming::Oversized((MAX_CLIPBOARD_BYTES + 1) as u64)
        );
    }

    /// A copy from a rich-text app: the text sits among flavors many times its
    /// size, and more of them than any fixed cap — the whole archive well past
    /// what used to be refused outright.
    #[test]
    fn text_is_found_among_large_and_many_flavors() {
        let image = vec![0x5au8; 3 * 1024 * 1024];
        let mut bytes = 40u32.to_be_bytes().to_vec();
        flavor(&mut bytes, b"public.tiff", &[(b"NSPboardType", b"NSTIFFPboardType")], &image);
        for i in 0..38 {
            flavor(&mut bytes, format!("com.example.flavor{i}").as_bytes(), &[], b"x");
        }
        flavor(&mut bytes, UTF8_TEXT, &[(b"MIME", b"text/plain")], "copied ✓".as_bytes());
        let (header, compressed) = from_mac(&bytes);

        // Fed in the pieces a socket would deliver.
        let mut receiver = Receiver::new(header).unwrap();
        for piece in compressed.chunks(1000) {
            receiver.feed(piece).unwrap();
        }
        assert_eq!(receiver.finish().unwrap(), Incoming::Text("copied ✓".to_owned()));
    }

    /// The last compressed bytes can inflate to more than one buffer's worth, all
    /// of which has to be read out before the input counts as consumed.
    #[test]
    fn inflated_bytes_past_one_buffer_are_all_read() {
        let filler = vec![0u8; 8 * INFLATE_CHUNK];
        let mut bytes = 1u32.to_be_bytes().to_vec();
        flavor(&mut bytes, b"public.tiff", &[], &filler);
        let (header, compressed) = from_mac(&bytes);
        assert!(compressed.len() < INFLATE_CHUNK, "inflates well past one buffer");
        assert_eq!(parse(header, &compressed).unwrap(), Incoming::NoText);
        for size in [1, 7, 100, 1000] {
            let mut receiver = Receiver::new(header).unwrap();
            for piece in compressed.chunks(size) {
                receiver.feed(piece).unwrap();
            }
            assert_eq!(receiver.finish().unwrap(), Incoming::NoText, "pieces of {size}");
        }

        let mut bytes = 2u32.to_be_bytes().to_vec();
        flavor(&mut bytes, b"public.tiff", &[], &filler);
        flavor(&mut bytes, UTF8_TEXT, &[], b"after");
        let (header, compressed) = from_mac(&bytes);
        assert_eq!(parse(header, &compressed).unwrap(), Incoming::Text("after".to_owned()));
    }

    /// The text may be in a later item than the first.
    #[test]
    fn text_is_found_in_a_later_item() {
        let mut bytes = 1u32.to_be_bytes().to_vec();
        flavor(&mut bytes, b"public.file-url", &[], b"file:///tmp/a");
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        flavor(&mut bytes, UTF8_TEXT, &[], b"second");
        let (header, compressed) = from_mac(&bytes);
        assert_eq!(parse(header, &compressed).unwrap(), Incoming::Text("second".to_owned()));
    }

    #[test]
    fn an_archive_without_text_or_cut_short_is_told_apart() {
        let mut bytes = 1u32.to_be_bytes().to_vec();
        flavor(&mut bytes, b"public.png", &[], &[1, 2, 3]);
        let (header, compressed) = from_mac(&bytes);
        assert_eq!(parse(header, &compressed).unwrap(), Incoming::NoText);

        let (header, compressed) = from_mac(&bytes[..bytes.len() - 1]);
        assert!(parse(header, &compressed).is_err(), "ends inside an item");
    }

    /// Empty text clears the Mac's pasteboard: an item with no flavors, never a
    /// flavor with no data, which the agent would take for a promise.
    #[test]
    fn empty_text_is_an_item_without_flavors() {
        assert_eq!(archive(""), [0, 0, 0, 0]);
        let msg = send(0, "").unwrap();
        let raw: [u8; 15] = msg[1..16].try_into().unwrap();
        assert_eq!(parse(header(&raw), &msg[16..]).unwrap(), Incoming::NoText);
    }
}
