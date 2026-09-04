//! The MS-RDPECLIP half of the clipboard bridge: the format this gateway
//! carries, and the conversions either direction needs.
//!
//! ## What the engine crate does and does not do
//!
//! [`freerdp`] carries the clipboard *negotiation* — format lists, requests,
//! responses — and nothing else: what crosses that boundary is a format id and a
//! `Vec<u8>`. That is the right seam, and it is why the UTF-16 lives here.
//! Choosing a text format, deciding what a line ending is, and deciding what
//! happens to a format nobody understands are three decisions that belong to
//! whoever is bridging a real clipboard, and this gateway's answer to all three
//! is shaped by the browser protocol carrying a single `text` string.
//!
//! ## Delayed rendering
//!
//! Unlike VNC's `ServerCutText` and Apple pasteboard messages, which carry the
//! text, RDP only announces *which formats* the remote clipboard now
//! holds; the bytes cost a second round trip. Both directions are lazy on the
//! wire:
//!
//! - remote copies → `FormatList` → we ask with `FormatDataRequest`;
//! - browser copies → we advertise `FormatList` → the remote asks when the user
//!   actually pastes, and only then do we hand over the text.
//!
//! The gateway hides that from the browser by asking immediately, so a copy on
//! the remote reaches the browser unprompted exactly as it does for the other
//! two engines.
//!
//! ## Scope
//!
//! `CF_UNICODETEXT` only. The browser protocol carries a `text` string, so
//! HTML, bitmaps and file lists have nowhere to go, and nothing is planned for
//! them.

use freerdp::ClipboardFormat;

use crate::protocol::clipboard_fits;

/// `CF_UNICODETEXT`, the one Windows clipboard format id this gateway speaks.
///
/// A bare constant rather than an import: the engine crate deliberately carries
/// format ids as plain `u32` (they are Windows' numbers, not FreeRDP's), and 13
/// is fixed by the platform rather than by any library here.
pub const CF_UNICODETEXT: u32 = 13;

/// The one format worth asking for out of what the remote advertised.
///
/// `CF_UNICODETEXT` or nothing. There is deliberately no `CF_TEXT` fallback:
/// that format is ANSI in the remote's code page, which we would have to guess
/// at, and a server offering text at all offers the Unicode flavour beside it.
/// An image or file-list copy simply produces no browser-visible clipboard.
pub fn pick_text_format(formats: &[ClipboardFormat]) -> Option<u32> {
    formats.iter().map(|format| format.id).find(|&id| id == CF_UNICODETEXT)
}

/// `CF_UNICODETEXT` bytes → a Rust string.
///
/// The wire format is UTF-16 **little-endian** with a NUL terminator, and every
/// part of that sentence is load-bearing. An odd byte count is not a
/// half-character to be salvaged — it means the payload is not what it claimed
/// to be — and the terminator is included in the length by some peers and not
/// by others, so it is stripped here rather than trusted either way.
///
/// `None` for anything that is not decodable UTF-16, which the caller reports as
/// a malformed clipboard rather than retrying: the same bytes will not become
/// valid on a second request.
pub fn decode_unicode(bytes: &[u8]) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> =
        bytes.as_chunks::<2>().0.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
    // Trailing NULs only. One is the terminator; a peer that pads with several
    // is padding, and a NUL in the middle is the remote's own data.
    let end = units.iter().rposition(|unit| *unit != 0).map_or(0, |last| last + 1);
    String::from_utf16(&units[..end]).ok()
}

/// A Rust string → `CF_UNICODETEXT` bytes.
///
/// Terminated, because MS-RDPECLIP says the payload for this format is a
/// null-terminated string and a Windows peer pasting an unterminated one gets
/// whatever followed it in its own buffer.
pub fn encode_unicode(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() * 2 + 2);
    for unit in text.encode_utf16().chain(std::iter::once(0)) {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Remote → browser: CRLF to LF.
///
/// Windows clipboard text uses CRLF. Passing it through would put a stray `\r`
/// at the end of every line in the browser's textarea and in the local OS
/// clipboard.
///
/// `Err(len)` when the text exceeds [`MAX_CLIPBOARD_BYTES`](crate::protocol::MAX_CLIPBOARD_BYTES), carrying the size
/// it was: the caller reports that instead, because a truncated paste is
/// indistinguishable from a whole one.
pub fn from_remote(text: &str) -> Result<String, u64> {
    // Some servers pad the response past the terminator [`decode_unicode`]
    // already stripped; a trailing NUL renders as a replacement glyph in the panel.
    let text = text.trim_end_matches('\0');
    if !clipboard_fits(text) {
        return Err(text.len() as u64);
    }
    Ok(text.replace("\r\n", "\n"))
}

/// Browser → remote: LF to CRLF.
///
/// Normalizing to LF before expanding is what keeps text that already contains
/// CRLF from coming out with doubled line breaks.
///
/// `None` when the text exceeds [`MAX_CLIPBOARD_BYTES`](crate::protocol::MAX_CLIPBOARD_BYTES) — the remote keeps what
/// it had rather than taking ownership of a partial copy. The expansion can
/// still overshoot the ceiling by the number of line breaks, which is the
/// remote's own clipboard and no longer this link's problem.
pub fn to_remote(text: &str) -> Option<String> {
    clipboard_fits(text).then(|| text.replace("\r\n", "\n").replace('\n', "\r\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MAX_CLIPBOARD_BYTES;

    #[test]
    fn unix_line_endings_become_windows_ones() {
        assert_eq!(to_remote("one\ntwo").unwrap(), "one\r\ntwo");
        assert_eq!(to_remote("trailing\n").unwrap(), "trailing\r\n");
    }

    // The bug this guards: normalizing first is what stops "\r\n" turning into
    // "\r\r\n" and doubling every line break on the remote.
    #[test]
    fn windows_line_endings_are_not_doubled() {
        assert_eq!(to_remote("one\r\ntwo").unwrap(), "one\r\ntwo");
        assert_eq!(
            to_remote("mixed\r\nand\nmatched").unwrap(),
            "mixed\r\nand\r\nmatched"
        );
    }

    // A lone CR is not a line ending in either convention, so neither
    // direction may invent one around it.
    #[test]
    fn a_bare_carriage_return_survives_both_directions() {
        assert_eq!(to_remote("a\rb").unwrap(), "a\rb");
        assert_eq!(from_remote("a\rb").unwrap(), "a\rb");
    }

    #[test]
    fn windows_line_endings_come_back_as_unix_ones() {
        assert_eq!(from_remote("one\r\ntwo").unwrap(), "one\ntwo");
        assert_eq!(from_remote("no line breaks").unwrap(), "no line breaks");
    }

    #[test]
    fn a_round_trip_through_the_remote_preserves_the_text() {
        for original in ["", "plain", "one\ntwo\nthree", "画面 ☕\nemoji 🚀"] {
            assert_eq!(
                from_remote(&to_remote(original).unwrap()).unwrap(),
                original,
                "{original:?}"
            );
        }
    }

    // Padding past the string terminator would otherwise reach the browser as
    // a replacement glyph on the end of every paste.
    #[test]
    fn trailing_nul_padding_is_stripped() {
        assert_eq!(from_remote("text\0").unwrap(), "text");
        assert_eq!(from_remote("text\0\0\0").unwrap(), "text");
        // Only trailing: a NUL in the middle is the remote's own data.
        assert_eq!(from_remote("a\0b").unwrap(), "a\0b");
    }

    // Refused in both directions rather than truncated: a partial paste is
    // indistinguishable from a whole one, on either side of the link.
    #[test]
    fn oversized_text_is_refused_in_both_directions() {
        let text = "é".repeat(MAX_CLIPBOARD_BYTES); // two bytes each, so 2x over
        assert_eq!(to_remote(&text), None);
        assert_eq!(from_remote(&text), Err(text.len() as u64));

        // At the ceiling both still pass, so the boundary is inclusive.
        let fits = "a".repeat(MAX_CLIPBOARD_BYTES);
        assert_eq!(to_remote(&fits).unwrap().len(), MAX_CLIPBOARD_BYTES);
        assert_eq!(from_remote(&fits).unwrap().len(), MAX_CLIPBOARD_BYTES);

        // The size reported is the text's own, measured after the NUL padding
        // this direction strips.
        let padded = format!("{}\0\0", "a".repeat(MAX_CLIPBOARD_BYTES + 5));
        assert_eq!(from_remote(&padded), Err(MAX_CLIPBOARD_BYTES as u64 + 5));
    }

    #[test]
    fn unicode_text_is_the_only_format_taken() {
        let unicode = ClipboardFormat::new(CF_UNICODETEXT);
        let ansi = ClipboardFormat::new(1); // CF_TEXT
        let bitmap = ClipboardFormat::new(2); // CF_BITMAP

        assert_eq!(
            pick_text_format(&[ansi.clone(), unicode.clone(), bitmap.clone()]),
            Some(CF_UNICODETEXT)
        );
        assert_eq!(pick_text_format(&[unicode]), Some(CF_UNICODETEXT));

        // ANSI alone is refused rather than guessed at: CF_TEXT is in the
        // remote's code page, which nothing here knows.
        assert_eq!(pick_text_format(&[ansi]), None);
        assert_eq!(pick_text_format(&[bitmap]), None);
        assert_eq!(pick_text_format(&[]), None);
    }

    /// The encoding is this module's own now — it used to be IronRDP's — so the
    /// round trip is a real test rather than a pin on somebody else's library.
    #[test]
    fn utf16_survives_a_round_trip_including_the_hard_cases() {
        for original in ["plain", "画面 ☕", "emoji 🚀 non-BMP", "", "line\r\nbreak", "a\0b"] {
            let encoded = encode_unicode(original);
            assert_eq!(encoded.len() % 2, 0, "UTF-16 is whole code units");
            assert_eq!(decode_unicode(&encoded).unwrap(), original, "{original:?}");
        }
        // The terminator is really there, and it is what a Windows peer reads to
        // know where the string stops.
        assert_eq!(encode_unicode("hi"), vec![b'h', 0, b'i', 0, 0, 0]);
    }

    /// Every one of these is a payload a peer really can send, and none of them
    /// may panic — this decode runs on bytes from the far end.
    #[test]
    fn a_payload_that_is_not_utf16_is_refused_rather_than_salvaged() {
        // An odd length cannot be UTF-16 at all.
        assert_eq!(decode_unicode(&[0x41]), None);
        assert_eq!(decode_unicode(&[0x41, 0x00, 0x42]), None);
        // A lone surrogate is well-formed UTF-16 code units and not a string.
        assert_eq!(decode_unicode(&[0x00, 0xD8]), None);
        // Empty, and terminator-only, are both the empty string rather than an error.
        assert_eq!(decode_unicode(&[]).unwrap(), "");
        assert_eq!(decode_unicode(&[0, 0]).unwrap(), "");
        assert_eq!(decode_unicode(&[0, 0, 0, 0]).unwrap(), "");
    }
}
