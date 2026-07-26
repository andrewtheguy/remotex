//! RFB Extended Clipboard (pseudo-encoding `0xc0a1e5ce`), which is how a VNC
//! clipboard carries anything outside latin-1.
//!
//! ## Why this exists
//!
//! Baseline RFB cut text is latin-1, one byte per character. Every codepoint
//! above U+00FF has to become `?` on the way out and cannot be represented at
//! all on the way in, so `画面` or an emoji copied either direction arrives as
//! `??`. The extension replaces that with UTF-8.
//!
//! ## The exchange
//!
//! An extended message rides inside an ordinary `ServerCutText`/`ClientCutText`
//! whose length field is **negative**; `-length` is then the size of a body
//! that starts with a `u32` of flags. The low 16 bits are formats (only
//! [`FORMAT_TEXT`] here), the top 8 are the action.
//!
//! Transfers are lazy, which is the point of the design — a clipboard is often
//! large and rarely pasted:
//!
//! ```text
//! caps    both peers, once, saying which actions and formats they handle
//! notify  "my clipboard changed, here is what I now hold"
//! request "send me these formats"
//! provide the data itself, deflated
//! peek    "tell me what you hold" (answered with notify)
//! ```
//!
//! So a copy on the remote is `notify` → our `request` → its `provide`, and a
//! copy in the browser is our `notify` → its `request` → our `provide`.
//!
//! ## Deliberate limits
//!
//! Text only. RTF, HTML, bitmaps and file lists all have formats defined here,
//! but the browser protocol carries a `text` string and nothing else, so there
//! would be nowhere to put them.
//!
//! The caps we send advertise a maximum unsolicited size of zero, meaning "do
//! not push data at me, tell me and I will ask". That is what noVNC does, and
//! it keeps a large remote clipboard off the wire until something wants it.

use std::io::Read as _;

use anyhow::Context as _;

use crate::protocol::{MAX_CLIPBOARD_BYTES, clamp_clipboard};

/// The pseudo-encoding advertised in SetEncodings to turn this on.
pub const ENCODING: i32 = 0xc0a1_e5ce_u32 as i32;

/// Plain text, UTF-8, NUL-terminated, CRLF line endings.
pub const FORMAT_TEXT: u32 = 1 << 0;

pub const ACTION_CAPS: u32 = 1 << 24;
pub const ACTION_REQUEST: u32 = 1 << 25;
pub const ACTION_PEEK: u32 = 1 << 26;
pub const ACTION_NOTIFY: u32 = 1 << 27;
pub const ACTION_PROVIDE: u32 = 1 << 28;

/// Mask selecting the action bits of the flags word.
const ACTION_MASK: u32 = 0xff00_0000;
/// Mask selecting the format bits.
const FORMAT_MASK: u32 = 0x0000_ffff;

/// Ceiling on the inflated Provide payload.
///
/// The wire carries deflated bytes, so a small message can expand without
/// bound — a "zip bomb" is trivial to build. Inflation is capped rather than
/// trusted; the clipboard is clamped to [`MAX_CLIPBOARD_BYTES`] anyway, and the
/// slack here covers the length prefixes and the NUL.
const MAX_INFLATED: u64 = MAX_CLIPBOARD_BYTES as u64 + 1024;

/// What a peer said it can do, from its caps message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Caps {
    pub actions: u32,
    pub formats: u32,
}

impl Caps {
    pub fn handles(self, action: u32) -> bool {
        self.actions & action != 0
    }

    pub fn has_text(self) -> bool {
        self.formats & FORMAT_TEXT != 0
    }
}

/// One decoded extended-clipboard message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// The peer's capabilities. Also the signal that the extension is live.
    Caps(Caps),
    /// The peer wants these formats sent as a Provide.
    Request(u32),
    /// The peer wants to know what we hold; answer with a Notify.
    Peek,
    /// The peer's clipboard changed and now holds these formats.
    Notify(u32),
    /// The data itself. `None` when the message carried no text format, which
    /// is an image or file copy we have nowhere to put.
    Provide(Option<String>),
    /// An action this build does not implement. Named rather than an error:
    /// the extension is open-ended and an unknown action is not a protocol
    /// violation, just something to ignore.
    Unknown(u32),
}

/// Decode the body of an extended cut-text message (everything after the
/// negative length).
pub fn parse(body: &[u8]) -> anyhow::Result<Incoming> {
    let raw = body
        .get(..4)
        .context("extended clipboard message shorter than its flags word")?;
    let flags = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let formats = flags & FORMAT_MASK;
    let actions = flags & ACTION_MASK;
    let rest = &body[4..];

    // Tested in order of the spec's own precedence. Caps first: a peer may set
    // caps alongside format bits, and that is still a caps message.
    if actions & ACTION_CAPS != 0 {
        return Ok(Incoming::Caps(Caps { actions, formats }));
    }
    match actions {
        ACTION_REQUEST => Ok(Incoming::Request(formats)),
        ACTION_PEEK => Ok(Incoming::Peek),
        ACTION_NOTIFY => Ok(Incoming::Notify(formats)),
        ACTION_PROVIDE => Ok(Incoming::Provide(parse_provide(formats, rest)?)),
        other => Ok(Incoming::Unknown(other)),
    }
}

/// Inflate a Provide payload and pull the text format out of it.
///
/// The inflated stream is, for each format bit set in ascending order, a `u32`
/// byte count followed by that many bytes. Formats before the text one have to
/// be walked past rather than skipped to, since their sizes are only known from
/// the stream itself.
fn parse_provide(formats: u32, deflated: &[u8]) -> anyhow::Result<Option<String>> {
    let mut stream = flate2::read::ZlibDecoder::new(deflated).take(MAX_INFLATED);
    let mut inflated = Vec::new();
    stream
        .read_to_end(&mut inflated)
        .context("inflating the extended clipboard payload")?;

    let mut rest = inflated.as_slice();
    for bit in 0..16 {
        let format = 1u32 << bit;
        if formats & format == 0 {
            continue;
        }
        let (raw, tail) = rest
            .split_at_checked(4)
            .context("extended clipboard payload ended inside a length")?;
        let size = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        let (data, tail) = tail
            .split_at_checked(size)
            .context("extended clipboard payload ended inside its data")?;
        if format == FORMAT_TEXT {
            return Ok(Some(from_wire(data)));
        }
        rest = tail;
    }
    Ok(None)
}

/// The 4-byte flags word, big-endian: actions in the top byte, the full
/// 16-bit format set in the bottom two.
///
/// Both format bytes are written even though this build only ever sets
/// [`FORMAT_TEXT`], so that encoding stays the exact inverse of [`parse`],
/// which reads all 16 bits. noVNC truncates to one byte here and gets away
/// with it for the same reason we would; an asymmetry that only shows up on a
/// format nobody has needed yet is not worth leaving in.
fn flags(actions: u32, formats: u32) -> [u8; 4] {
    [
        (actions >> 24) as u8,
        0, // reserved
        (formats >> 8) as u8,
        formats as u8,
    ]
}

/// Our capabilities: text, every action, and no appetite for unsolicited data.
///
/// The trailing `u32` is the maximum size we will accept without asking, one
/// per format bit set. Zero means "notify me instead of pushing".
pub fn caps() -> Vec<u8> {
    let actions = ACTION_CAPS | ACTION_REQUEST | ACTION_PEEK | ACTION_NOTIFY | ACTION_PROVIDE;
    let mut body = flags(actions, FORMAT_TEXT).to_vec();
    body.extend_from_slice(&0u32.to_be_bytes());
    body
}

/// "My clipboard changed." `formats` of 0 says it is now empty.
pub fn notify(formats: u32) -> Vec<u8> {
    flags(ACTION_NOTIFY, formats).to_vec()
}

/// "Send me these formats."
pub fn request(formats: u32) -> Vec<u8> {
    flags(ACTION_REQUEST, formats).to_vec()
}

/// The text itself, deflated.
///
/// Clamped here, not trusted to arrive clamped: this is the deferred half of a
/// browser copy, so the text comes from a Provide request that can land long
/// after the copy did. The latin-1 path has the same ceiling of its own
/// (`latin1_from_str` in src/vnc.rs), and [`MAX_INFLATED`] on the way back in
/// assumes it. The clamp runs before [`to_wire`], so the wire form can overshoot
/// by one byte per line break plus the NUL — harmless, where clamping afterwards
/// could slice a CRLF pair or a multi-byte character in half.
pub fn provide(text: &str) -> anyhow::Result<Vec<u8>> {
    use std::io::Write as _;

    let wire = to_wire(clamp_clipboard(text));
    let mut payload = (wire.len() as u32).to_be_bytes().to_vec();
    payload.extend_from_slice(&wire);

    let mut encoder =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(&payload)
        .context("deflating the clipboard payload")?;
    let deflated = encoder.finish().context("finishing the clipboard deflate")?;

    let mut body = flags(ACTION_PROVIDE, FORMAT_TEXT).to_vec();
    body.extend_from_slice(&deflated);
    Ok(body)
}

/// Browser text to the wire form: CRLF line endings, UTF-8, NUL-terminated.
///
/// Every line ending is normalized to LF first so that input already using
/// CRLF does not come out doubled, and a lone CR — which RFB also calls a line
/// ending, unlike the rest of this codebase — is folded in too.
fn to_wire(text: &str) -> Vec<u8> {
    let unix = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = unix.replace('\n', "\r\n").into_bytes();
    out.push(0);
    out
}

/// The wire form back to browser text: drop the terminator, CRLF to LF.
///
/// Lossy on purpose. This is display text, and a peer that sends a stray byte
/// should cost one replacement character rather than the whole paste.
fn from_wire(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(data);
    let text = text.trim_end_matches('\0');
    text.replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap a body the way the wire does, then read it back.
    fn roundtrip(body: &[u8]) -> Incoming {
        parse(body).expect("parse")
    }

    #[test]
    fn the_flags_word_puts_actions_on_top_and_formats_at_the_bottom() {
        assert_eq!(flags(ACTION_NOTIFY, FORMAT_TEXT), [0x08, 0, 0, 0x01]);
        assert_eq!(flags(ACTION_PROVIDE, FORMAT_TEXT), [0x10, 0, 0, 0x01]);
        assert_eq!(flags(ACTION_NOTIFY, 0), [0x08, 0, 0, 0x00]);
    }

    // Formats are 16 bits wide. Writing only the low byte would round-trip
    // every format this build uses and silently drop the rest, so encoding is
    // checked against the full range rather than against what we happen to
    // send.
    #[test]
    fn the_whole_16_bit_format_set_survives_encoding() {
        assert_eq!(flags(ACTION_NOTIFY, 0xffff), [0x08, 0, 0xff, 0xff]);
        assert_eq!(flags(ACTION_NOTIFY, 1 << 8), [0x08, 0, 0x01, 0x00]);

        // And parse reads back exactly what flags wrote, for every bit.
        for bit in 0..16 {
            let formats = 1u32 << bit;
            assert_eq!(
                parse(&notify(formats)).expect("parse"),
                Incoming::Notify(formats),
                "format bit {bit}"
            );
        }
    }

    // No path may hand the remote an unbounded string. The latin-1 cut carries
    // its own ceiling, and before this the extended path had none: an oversized
    // copy went out whole, and would have come back past MAX_INFLATED.
    #[test]
    fn an_oversized_provide_is_clamped_rather_than_sent_whole() {
        let text = "a".repeat(MAX_CLIPBOARD_BYTES + 5_000);
        match roundtrip(&provide(&text).expect("provide")) {
            Incoming::Provide(Some(out)) => assert_eq!(out.len(), MAX_CLIPBOARD_BYTES),
            other => panic!("expected a text provide, got {other:?}"),
        }

        // The boundary is a char boundary, so a multi-byte tail is dropped
        // whole rather than cut in half (which would arrive as U+FFFD).
        let two_byte = "é".repeat(MAX_CLIPBOARD_BYTES); // 2 bytes each
        match roundtrip(&provide(&two_byte).expect("provide")) {
            Incoming::Provide(Some(out)) => {
                assert_eq!(out.len(), MAX_CLIPBOARD_BYTES);
                assert!(!out.contains('\u{fffd}'), "a char was sliced in half");
            }
            other => panic!("expected a text provide, got {other:?}"),
        }
    }

    #[test]
    fn caps_advertises_text_every_action_and_no_unsolicited_data() {
        let body = caps();
        match roundtrip(&body) {
            Incoming::Caps(caps) => {
                assert!(caps.has_text());
                for action in [
                    ACTION_CAPS,
                    ACTION_REQUEST,
                    ACTION_PEEK,
                    ACTION_NOTIFY,
                    ACTION_PROVIDE,
                ] {
                    assert!(caps.handles(action), "missing action {action:#x}");
                }
            }
            other => panic!("expected caps, got {other:?}"),
        }
        // The size that follows the flags is zero: notify us, don't push.
        assert_eq!(&body[4..], &0u32.to_be_bytes());
    }

    #[test]
    fn the_simple_actions_roundtrip() {
        assert_eq!(
            roundtrip(&notify(FORMAT_TEXT)),
            Incoming::Notify(FORMAT_TEXT)
        );
        // An empty notify is how a peer says its clipboard was cleared.
        assert_eq!(roundtrip(&notify(0)), Incoming::Notify(0));
        assert_eq!(
            roundtrip(&request(FORMAT_TEXT)),
            Incoming::Request(FORMAT_TEXT)
        );
        assert_eq!(roundtrip(&flags(ACTION_PEEK, 0)), Incoming::Peek);
    }

    // The whole reason this module exists: text the latin-1 path turns into
    // '?' has to survive intact.
    #[test]
    fn provide_carries_unicode_the_latin1_path_would_destroy() {
        for original in [
            "画面 ☕",
            "emoji 🚀 beyond the BMP",
            "café — naïve",
            "Ελληνικά Кириллица",
            "",
            "plain ascii",
        ] {
            let body = provide(original).expect("provide");
            match roundtrip(&body) {
                Incoming::Provide(Some(text)) => assert_eq!(text, original, "{original:?}"),
                other => panic!("expected provide {original:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn line_endings_are_crlf_on_the_wire_and_lf_in_the_browser() {
        assert_eq!(to_wire("one\ntwo"), b"one\r\ntwo\0");
        // Already CRLF must not double.
        assert_eq!(to_wire("one\r\ntwo"), b"one\r\ntwo\0");
        // RFB counts a lone CR as a line ending too.
        assert_eq!(to_wire("one\rtwo"), b"one\r\ntwo\0");

        assert_eq!(from_wire(b"one\r\ntwo\0"), "one\ntwo");
        // The terminator is optional in practice; absence must not eat a byte.
        assert_eq!(from_wire(b"one\r\ntwo"), "one\ntwo");
    }

    #[test]
    fn a_multi_line_round_trip_survives() {
        let original = "first — 画面\nsecond ☕\nthird";
        let body = provide(original).expect("provide");
        match roundtrip(&body) {
            Incoming::Provide(Some(text)) => assert_eq!(text, original),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Build a Provide carrying arbitrary formats, so the tests can express
    /// what a peer sends rather than only what this module sends.
    fn provide_formats(entries: &[(u32, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;
        let mut formats = 0;
        let mut payload = Vec::new();
        for (format, data) in entries {
            formats |= format;
            payload.extend_from_slice(&(data.len() as u32).to_be_bytes());
            payload.extend_from_slice(data);
        }
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(&payload).unwrap();
        let mut body = flags(ACTION_PROVIDE, formats).to_vec();
        body.extend_from_slice(&e.finish().unwrap());
        body
    }

    // An image copy sets no text bit; there is nothing to show, and that must
    // read as "no text" rather than as an error or as empty text.
    #[test]
    fn a_provide_without_the_text_format_yields_nothing() {
        let body = provide_formats(&[(1 << 3, b"fake dib bytes")]); // dib only
        assert_eq!(roundtrip(&body), Incoming::Provide(None));
    }

    // Formats arrive in ascending bit order and are length-prefixed, so a
    // format ahead of text has to be stepped over using its own length. Get
    // that wrong and the text comes out shifted into garbage.
    #[test]
    fn a_format_ordered_before_text_is_stepped_over() {
        // Text is bit 0, so nothing sorts before it; the real ordering risk is
        // a format *after* text being mistaken for it.
        let body = provide_formats(&[
            (FORMAT_TEXT, b"the text\0"),
            (1 << 2, b"<html>ignored</html>"),
        ]);
        assert_eq!(
            roundtrip(&body),
            Incoming::Provide(Some("the text".to_owned()))
        );
    }

    #[test]
    fn an_unknown_action_is_ignored_rather_than_fatal() {
        let body = flags(1 << 29, 0).to_vec();
        assert!(matches!(roundtrip(&body), Incoming::Unknown(_)));
    }

    #[test]
    fn a_truncated_message_is_rejected() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[0x10, 0, 0]).is_err());
        // Claims text but the deflate stream is nonsense.
        let mut body = flags(ACTION_PROVIDE, FORMAT_TEXT).to_vec();
        body.extend_from_slice(b"not zlib at all");
        assert!(parse(&body).is_err());
    }

    // A peer must not be able to make us allocate without bound from a tiny
    // message.
    #[test]
    fn an_inflation_bomb_is_capped() {
        use std::io::Write as _;
        let huge = vec![b'a'; (MAX_INFLATED as usize) * 4];
        let mut payload = (huge.len() as u32).to_be_bytes().to_vec();
        payload.extend_from_slice(&huge);
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        e.write_all(&payload).unwrap();
        let deflated = e.finish().unwrap();
        assert!(deflated.len() < 10_000, "the bomb should be small on the wire");

        let mut body = flags(ACTION_PROVIDE, FORMAT_TEXT).to_vec();
        body.extend_from_slice(&deflated);
        // Truncated by the cap, so the length prefix no longer matches and the
        // parse fails — refused, not swallowed whole.
        assert!(parse(&body).is_err());
    }
}
