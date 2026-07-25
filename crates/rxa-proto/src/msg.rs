//! The message set, hand-rolled little-endian in the style of `src/vnc.rs`.
//!
//! There are sixteen messages between the two enums, so a serialization
//! dependency would buy nothing that these ~200 lines and their roundtrip tests
//! don't. The payload of a [`AgentMsg::Tile`] is **already** a PNG or JPEG
//! stream — the exact bytes the browser decodes — so the gateway relays it
//! without ever looking inside.
//!
//! Every encoded message is `u8 type + body`, handed to [`crate::frame`] which
//! adds the length prefix. Conventions inside a body:
//!
//! - integers little-endian
//! - `String` as `u16` byte length + UTF-8, for the short strings ([`put_str`])
//! - long text as `u32` byte length + UTF-8, for clipboard ([`put_text`])
//! - `Vec<u8>` as `u32` byte length + bytes
//! - `Option<T>` as `u8` 0/1 followed by `T`'s body when present

/// Why a message could not be decoded. All of these mean the peer is buggy or
/// not speaking this protocol — the transport underneath is authenticated, so
/// they are never attacker-controlled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MsgError {
    #[error("empty message")]
    Empty,
    #[error("unknown message type 0x{0:02x}")]
    UnknownType(u8),
    #[error("message body ended early")]
    Truncated,
    #[error("message body has {0} trailing bytes")]
    Trailing(usize),
    #[error("string field is not valid UTF-8")]
    BadUtf8,
    #[error("invalid boolean byte 0x{0:02x}")]
    BadBool(u8),
}

/// Ceiling on one clipboard transfer, in bytes, in either direction.
///
/// Lives here rather than in the gateway because all three hops — browser link,
/// gateway, agent — have to agree on it, and the agent cannot see the gateway's
/// `src/protocol.rs`. Clipboard text rides the same link as live frames, so an
/// accidental 200 MB copy must not stall a session; text over this is truncated
/// (see [`clamp_clipboard`]) rather than refused, because a truncated paste is
/// recoverable and a silently dropped one just looks broken.
pub const MAX_CLIPBOARD_BYTES: usize = 65_536;

/// Truncate `text` to at most [`MAX_CLIPBOARD_BYTES`], on a char boundary.
///
/// Returns the input untouched when it already fits, so the common case does
/// not allocate.
pub fn clamp_clipboard(text: &str) -> &str {
    if text.len() <= MAX_CLIPBOARD_BYTES {
        return text;
    }
    let mut end = MAX_CLIPBOARD_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The tile payload codec, mirroring the gateway's `Tile::FORMAT_*` constants
/// (`src/protocol.rs`) so the byte passes straight through to the browser.
pub mod format {
    pub const PNG: u8 = 1;
    pub const JPEG: u8 = 2;
}

/// A pointer shape: an RGBA PNG plus its hotspot, ready for the gateway's
/// `CursorShape` and from there the browser's `paintCursor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub w: u16,
    pub h: u16,
    /// Hotspot within the image, in cursor pixels.
    pub hx: u16,
    pub hy: u16,
    /// PNG-encoded RGBA (the alpha channel carries the mask).
    pub png: Vec<u8>,
}

/// Agent → gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentMsg {
    /// First message after the handshake.
    Hello {
        version: u16,
        agent_version: String,
        w: u16,
        h: u16,
    },
    /// A dirty rectangle, already encoded as PNG or JPEG (see [`format`]).
    Tile {
        format: u8,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        data: Vec<u8>,
    },
    /// The pointer shape changed. `None` means the pointer is hidden.
    Cursor(Option<CursorImage>),
    /// The display was reconfigured on the Mac.
    DisplaySize { w: u16, h: u16 },
    Pong { nonce: u64 },
    /// Something the user has to act on — most often a missing TCC grant.
    /// Surfaced in the browser rather than dying quietly in a log.
    Error { message: String },
    /// The Mac's pasteboard text. Sent either in reply to a
    /// [`GatewayMsg::ClipboardRequest`], or unprompted after
    /// [`GatewayMsg::ClipboardWatch`] turned the watcher on and the pasteboard
    /// changed. Never sent otherwise: with the watch off the agent does not
    /// look at the pasteboard at all.
    Clipboard { text: String },
}

impl AgentMsg {
    const T_HELLO: u8 = 0x01;
    const T_TILE: u8 = 0x02;
    const T_CURSOR: u8 = 0x03;
    const T_DISPLAY_SIZE: u8 = 0x04;
    const T_PONG: u8 = 0x05;
    const T_ERROR: u8 = 0x06;
    const T_CLIPBOARD: u8 = 0x07;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            AgentMsg::Hello {
                version,
                agent_version,
                w,
                h,
            } => {
                out.push(Self::T_HELLO);
                put_u16(&mut out, *version);
                put_str(&mut out, agent_version);
                put_u16(&mut out, *w);
                put_u16(&mut out, *h);
            }
            AgentMsg::Tile {
                format,
                x,
                y,
                w,
                h,
                data,
            } => {
                out.reserve(12 + data.len());
                out.push(Self::T_TILE);
                out.push(*format);
                put_u16(&mut out, *x);
                put_u16(&mut out, *y);
                put_u16(&mut out, *w);
                put_u16(&mut out, *h);
                put_bytes(&mut out, data);
            }
            AgentMsg::Cursor(shape) => {
                out.push(Self::T_CURSOR);
                match shape {
                    Some(c) => {
                        out.push(1);
                        put_u16(&mut out, c.w);
                        put_u16(&mut out, c.h);
                        put_u16(&mut out, c.hx);
                        put_u16(&mut out, c.hy);
                        put_bytes(&mut out, &c.png);
                    }
                    None => out.push(0),
                }
            }
            AgentMsg::DisplaySize { w, h } => {
                out.push(Self::T_DISPLAY_SIZE);
                put_u16(&mut out, *w);
                put_u16(&mut out, *h);
            }
            AgentMsg::Pong { nonce } => {
                out.push(Self::T_PONG);
                out.extend_from_slice(&nonce.to_le_bytes());
            }
            AgentMsg::Error { message } => {
                out.push(Self::T_ERROR);
                put_str(&mut out, message);
            }
            AgentMsg::Clipboard { text } => {
                out.push(Self::T_CLIPBOARD);
                put_text(&mut out, text);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MsgError> {
        let (&kind, body) = bytes.split_first().ok_or(MsgError::Empty)?;
        let mut r = Reader::new(body);
        let msg = match kind {
            Self::T_HELLO => AgentMsg::Hello {
                version: r.u16()?,
                agent_version: r.string()?,
                w: r.u16()?,
                h: r.u16()?,
            },
            Self::T_TILE => AgentMsg::Tile {
                format: r.u8()?,
                x: r.u16()?,
                y: r.u16()?,
                w: r.u16()?,
                h: r.u16()?,
                data: r.bytes()?.to_vec(),
            },
            Self::T_CURSOR => AgentMsg::Cursor(if r.bool()? {
                Some(CursorImage {
                    w: r.u16()?,
                    h: r.u16()?,
                    hx: r.u16()?,
                    hy: r.u16()?,
                    png: r.bytes()?.to_vec(),
                })
            } else {
                None
            }),
            Self::T_DISPLAY_SIZE => AgentMsg::DisplaySize {
                w: r.u16()?,
                h: r.u16()?,
            },
            Self::T_PONG => AgentMsg::Pong { nonce: r.u64()? },
            Self::T_ERROR => AgentMsg::Error {
                message: r.string()?,
            },
            Self::T_CLIPBOARD => AgentMsg::Clipboard { text: r.text()? },
            other => return Err(MsgError::UnknownType(other)),
        };
        r.finish()?;
        Ok(msg)
    }
}

/// Gateway → agent.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayMsg {
    /// Start the capture stream; the agent replies with a full keyframe.
    Attach,
    /// Repaint everything (from `ClientMsg::Refresh`).
    Refresh,
    /// Framebuffer pixel coordinates — the agent converts to display points.
    PointerMove { x: u16, y: u16 },
    /// `button` uses the DOM `MouseEvent.button` numbering (0/1/2).
    PointerButton { button: u8, pressed: bool },
    /// Raw DOM wheel deltas, exactly as remotex already carries them.
    Wheel { dx: f32, dy: f32 },
    /// DOM `KeyboardEvent.code`, plus the browser's authoritative CapsLock
    /// state so the agent never has to infer lock state.
    Key {
        code: String,
        pressed: bool,
        caps: bool,
    },
    Ping { nonce: u64 },
    /// Read the Mac's pasteboard and reply with [`AgentMsg::Clipboard`].
    /// Sent when the browser presses Fetch.
    ClipboardRequest,
    /// Put `text` on the Mac's pasteboard.
    Clipboard { text: String },
    /// Start or stop watching the pasteboard for changes. While on, the agent
    /// pushes [`AgentMsg::Clipboard`] whenever the Mac's pasteboard changes,
    /// so a copy on the Mac reaches the browser without a Fetch.
    ///
    /// Gated by the gateway's per-target `clipboard` flag, and load-bearing:
    /// watching costs one pasteboard *content* read per change, which recent
    /// macOS may report to the user as a paste. A target that did not opt in
    /// never sends this, and the agent then never reads the pasteboard
    /// unprompted.
    ClipboardWatch { enabled: bool },
}

impl GatewayMsg {
    const T_ATTACH: u8 = 0x01;
    const T_REFRESH: u8 = 0x02;
    const T_POINTER_MOVE: u8 = 0x03;
    const T_POINTER_BUTTON: u8 = 0x04;
    const T_WHEEL: u8 = 0x05;
    const T_KEY: u8 = 0x06;
    const T_PING: u8 = 0x07;
    const T_CLIPBOARD_REQUEST: u8 = 0x08;
    const T_CLIPBOARD: u8 = 0x09;
    const T_CLIPBOARD_WATCH: u8 = 0x0a;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            GatewayMsg::Attach => out.push(Self::T_ATTACH),
            GatewayMsg::Refresh => out.push(Self::T_REFRESH),
            GatewayMsg::PointerMove { x, y } => {
                out.push(Self::T_POINTER_MOVE);
                put_u16(&mut out, *x);
                put_u16(&mut out, *y);
            }
            GatewayMsg::PointerButton { button, pressed } => {
                out.push(Self::T_POINTER_BUTTON);
                out.push(*button);
                out.push(u8::from(*pressed));
            }
            GatewayMsg::Wheel { dx, dy } => {
                out.push(Self::T_WHEEL);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
            }
            GatewayMsg::Key {
                code,
                pressed,
                caps,
            } => {
                out.push(Self::T_KEY);
                put_str(&mut out, code);
                out.push(u8::from(*pressed));
                out.push(u8::from(*caps));
            }
            GatewayMsg::Ping { nonce } => {
                out.push(Self::T_PING);
                out.extend_from_slice(&nonce.to_le_bytes());
            }
            GatewayMsg::ClipboardRequest => out.push(Self::T_CLIPBOARD_REQUEST),
            GatewayMsg::Clipboard { text } => {
                out.push(Self::T_CLIPBOARD);
                put_text(&mut out, text);
            }
            GatewayMsg::ClipboardWatch { enabled } => {
                out.push(Self::T_CLIPBOARD_WATCH);
                out.push(u8::from(*enabled));
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MsgError> {
        let (&kind, body) = bytes.split_first().ok_or(MsgError::Empty)?;
        let mut r = Reader::new(body);
        let msg = match kind {
            Self::T_ATTACH => GatewayMsg::Attach,
            Self::T_REFRESH => GatewayMsg::Refresh,
            Self::T_POINTER_MOVE => GatewayMsg::PointerMove {
                x: r.u16()?,
                y: r.u16()?,
            },
            Self::T_POINTER_BUTTON => GatewayMsg::PointerButton {
                button: r.u8()?,
                pressed: r.bool()?,
            },
            Self::T_WHEEL => GatewayMsg::Wheel {
                dx: f32::from_le_bytes(r.array::<4>()?),
                dy: f32::from_le_bytes(r.array::<4>()?),
            },
            Self::T_KEY => GatewayMsg::Key {
                code: r.string()?,
                pressed: r.bool()?,
                caps: r.bool()?,
            },
            Self::T_PING => GatewayMsg::Ping { nonce: r.u64()? },
            Self::T_CLIPBOARD_REQUEST => GatewayMsg::ClipboardRequest,
            Self::T_CLIPBOARD => GatewayMsg::Clipboard { text: r.text()? },
            Self::T_CLIPBOARD_WATCH => GatewayMsg::ClipboardWatch {
                enabled: r.bool()?,
            },
            other => return Err(MsgError::UnknownType(other)),
        };
        r.finish()?;
        Ok(msg)
    }
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    // Every string here is a DOM key code, a version, or an error message —
    // none can approach u16::MAX. Truncating a pathological one keeps the
    // encoder infallible.
    let bytes = s.as_bytes();
    let len = bytes.len().min(usize::from(u16::MAX));
    put_u16(out, len as u16);
    out.extend_from_slice(&bytes[..len]);
}

fn put_bytes(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

/// Long text: `u32` byte length + UTF-8.
///
/// Separate from [`put_str`] because clipboard text can exceed that encoder's
/// `u16::MAX` ceiling — a 64 KiB copy is ordinary, and silently losing its tail
/// to a length field would be a puzzling bug. Both ends cap clipboard text
/// before it gets here; the framing itself is bounded by
/// [`crate::frame::MAX_FRAME_LEN`].
fn put_text(out: &mut Vec<u8>, text: &str) {
    put_bytes(out, text.as_bytes());
}

/// A cursor over a message body.
struct Reader<'a> {
    body: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self { body, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], MsgError> {
        let end = self.pos.checked_add(n).ok_or(MsgError::Truncated)?;
        let slice = self.body.get(self.pos..end).ok_or(MsgError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], MsgError> {
        let slice = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, MsgError> {
        Ok(self.array::<1>()?[0])
    }

    fn bool(&mut self) -> Result<bool, MsgError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(MsgError::BadBool(other)),
        }
    }

    fn u16(&mut self) -> Result<u16, MsgError> {
        Ok(u16::from_le_bytes(self.array::<2>()?))
    }

    fn u32(&mut self) -> Result<u32, MsgError> {
        Ok(u32::from_le_bytes(self.array::<4>()?))
    }

    fn u64(&mut self) -> Result<u64, MsgError> {
        Ok(u64::from_le_bytes(self.array::<8>()?))
    }

    fn string(&mut self) -> Result<String, MsgError> {
        let len = usize::from(self.u16()?);
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| MsgError::BadUtf8)
    }

    fn bytes(&mut self) -> Result<&'a [u8], MsgError> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    /// The [`put_text`] counterpart: `u32` byte length + UTF-8.
    fn text(&mut self) -> Result<String, MsgError> {
        std::str::from_utf8(self.bytes()?)
            .map(str::to_owned)
            .map_err(|_| MsgError::BadUtf8)
    }

    /// Reject a body with bytes left over: a length field that disagrees with
    /// the payload means the two sides have drifted, and silently ignoring the
    /// tail is how that goes unnoticed.
    fn finish(&self) -> Result<(), MsgError> {
        match self.body.len() - self.pos {
            0 => Ok(()),
            n => Err(MsgError::Trailing(n)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_variants() -> Vec<AgentMsg> {
        vec![
            AgentMsg::Hello {
                version: crate::VERSION,
                agent_version: "0.0.19".to_owned(),
                w: 3456,
                h: 2234,
            },
            // An empty version string and a zero-size display still roundtrip.
            AgentMsg::Hello {
                version: 0,
                agent_version: String::new(),
                w: 0,
                h: 0,
            },
            AgentMsg::Tile {
                format: format::JPEG,
                x: 64,
                y: 128,
                w: 320,
                h: 64,
                data: vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3],
            },
            AgentMsg::Tile {
                format: format::PNG,
                x: 0,
                y: 0,
                w: 1,
                h: 1,
                data: Vec::new(),
            },
            AgentMsg::Cursor(Some(CursorImage {
                w: 24,
                h: 24,
                hx: 4,
                hy: 3,
                png: vec![0x89, b'P', b'N', b'G'],
            })),
            AgentMsg::Cursor(None),
            AgentMsg::DisplaySize { w: 1920, h: 1080 },
            AgentMsg::Pong { nonce: u64::MAX },
            AgentMsg::Error {
                message: "Screen Recording permission not granted".to_owned(),
            },
            // Non-ASCII survives the u16-length UTF-8 encoding.
            AgentMsg::Error {
                message: "écran — 画面".to_owned(),
            },
            AgentMsg::Clipboard {
                text: "pasteboard contents — 画面".to_owned(),
            },
            // An empty pasteboard is text too, and distinct from "no reply".
            AgentMsg::Clipboard {
                text: String::new(),
            },
        ]
    }

    fn gateway_variants() -> Vec<GatewayMsg> {
        vec![
            GatewayMsg::Attach,
            GatewayMsg::Refresh,
            GatewayMsg::PointerMove { x: 0, y: 0 },
            GatewayMsg::PointerMove {
                x: u16::MAX,
                y: 1234,
            },
            GatewayMsg::PointerButton {
                button: 2,
                pressed: true,
            },
            GatewayMsg::PointerButton {
                button: 0,
                pressed: false,
            },
            GatewayMsg::Wheel { dx: 0.0, dy: -2.5 },
            GatewayMsg::Wheel {
                dx: 120.0,
                dy: f32::MIN,
            },
            GatewayMsg::Key {
                code: "KeyA".to_owned(),
                pressed: true,
                caps: false,
            },
            GatewayMsg::Key {
                code: "MetaLeft".to_owned(),
                pressed: false,
                caps: true,
            },
            GatewayMsg::Ping { nonce: 7 },
            GatewayMsg::ClipboardRequest,
            GatewayMsg::Clipboard {
                text: "copied in the browser — 画面".to_owned(),
            },
            GatewayMsg::Clipboard {
                text: String::new(),
            },
            GatewayMsg::ClipboardWatch { enabled: true },
            GatewayMsg::ClipboardWatch { enabled: false },
        ]
    }

    #[test]
    fn every_agent_variant_roundtrips() {
        for msg in agent_variants() {
            let bytes = msg.encode();
            assert_eq!(AgentMsg::decode(&bytes).unwrap(), msg, "{msg:?}");
        }
    }

    #[test]
    fn every_gateway_variant_roundtrips() {
        for msg in gateway_variants() {
            let bytes = msg.encode();
            assert_eq!(GatewayMsg::decode(&bytes).unwrap(), msg, "{msg:?}");
        }
    }

    // The two enums are carried in opposite directions, so their type bytes
    // may overlap — but a message must never silently decode as the wrong
    // variant of its *own* enum.
    #[test]
    fn type_bytes_are_distinct_within_each_direction() {
        let mut agent: Vec<u8> = agent_variants().iter().map(|m| m.encode()[0]).collect();
        agent.sort_unstable();
        agent.dedup();
        assert_eq!(agent.len(), 7, "seven agent message types");

        let mut gateway: Vec<u8> = gateway_variants().iter().map(|m| m.encode()[0]).collect();
        gateway.sort_unstable();
        gateway.dedup();
        assert_eq!(gateway.len(), 10, "ten gateway message types");
    }

    // Clipboard text uses u32 framing, not the u16 `put_str` every other string
    // field uses — a copy larger than u16::MAX must arrive whole rather than
    // silently losing its tail.
    #[test]
    fn clipboard_text_roundtrips_past_the_u16_string_ceiling() {
        let text = "é".repeat(usize::from(u16::MAX)); // 128 KiB of UTF-8
        assert!(text.len() > usize::from(u16::MAX));

        let msg = AgentMsg::Clipboard { text: text.clone() };
        assert_eq!(AgentMsg::decode(&msg.encode()).unwrap(), msg);

        let msg = GatewayMsg::Clipboard { text };
        assert_eq!(GatewayMsg::decode(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn clipboard_text_that_is_not_utf8_is_rejected() {
        // A well-formed frame whose payload is not UTF-8: length 2, then a lone
        // continuation byte pair.
        let mut bytes = vec![AgentMsg::T_CLIPBOARD];
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xC3, 0x28]);
        assert_eq!(AgentMsg::decode(&bytes), Err(MsgError::BadUtf8));

        let mut bytes = vec![GatewayMsg::T_CLIPBOARD];
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xC3, 0x28]);
        assert_eq!(GatewayMsg::decode(&bytes), Err(MsgError::BadUtf8));

        // A length that overruns the body is truncation, not bad UTF-8.
        let mut bytes = vec![AgentMsg::T_CLIPBOARD];
        bytes.extend_from_slice(&9u32.to_le_bytes());
        bytes.extend_from_slice(b"short");
        assert_eq!(AgentMsg::decode(&bytes), Err(MsgError::Truncated));
    }

    // A tile's on-the-wire layout, byte for byte: the gateway copies `format`
    // and the payload straight into a browser frame, so this is a contract.
    #[test]
    fn tile_encodes_to_the_documented_layout() {
        let bytes = AgentMsg::Tile {
            format: format::JPEG,
            x: 0x0102,
            y: 0x0304,
            w: 2,
            h: 1,
            data: vec![9, 8, 7],
        }
        .encode();
        assert_eq!(bytes[0], 0x02, "tile type byte");
        assert_eq!(bytes[1], format::JPEG);
        assert_eq!(&bytes[2..4], &[0x02, 0x01]); // x, little-endian
        assert_eq!(&bytes[4..6], &[0x04, 0x03]); // y
        assert_eq!(&bytes[6..8], &[2, 0]); // w
        assert_eq!(&bytes[8..10], &[1, 0]); // h
        assert_eq!(&bytes[10..14], &[3, 0, 0, 0]); // payload length, u32 LE
        assert_eq!(&bytes[14..], &[9, 8, 7]);
    }

    #[test]
    fn empty_and_unknown_messages_are_rejected() {
        assert_eq!(AgentMsg::decode(&[]), Err(MsgError::Empty));
        assert_eq!(GatewayMsg::decode(&[]), Err(MsgError::Empty));
        assert_eq!(AgentMsg::decode(&[0x00]), Err(MsgError::UnknownType(0)));
        assert_eq!(AgentMsg::decode(&[0xFE]), Err(MsgError::UnknownType(0xFE)));
        assert_eq!(GatewayMsg::decode(&[0x42]), Err(MsgError::UnknownType(0x42)));
    }

    #[test]
    fn a_truncated_body_is_rejected_at_every_cut() {
        for msg in agent_variants() {
            let bytes = msg.encode();
            for cut in 1..bytes.len() {
                assert!(
                    AgentMsg::decode(&bytes[..cut]).is_err(),
                    "{msg:?} decoded from a {cut}-byte prefix"
                );
            }
        }
        for msg in gateway_variants() {
            let bytes = msg.encode();
            for cut in 1..bytes.len() {
                assert!(
                    GatewayMsg::decode(&bytes[..cut]).is_err(),
                    "{msg:?} decoded from a {cut}-byte prefix"
                );
            }
        }
    }

    #[test]
    fn trailing_bytes_are_rejected_rather_than_ignored() {
        let mut bytes = GatewayMsg::Attach.encode();
        bytes.push(0);
        assert_eq!(GatewayMsg::decode(&bytes), Err(MsgError::Trailing(1)));

        let mut bytes = AgentMsg::DisplaySize { w: 1, h: 2 }.encode();
        bytes.extend_from_slice(&[0, 0, 0]);
        assert_eq!(AgentMsg::decode(&bytes), Err(MsgError::Trailing(3)));
    }

    #[test]
    fn a_length_field_larger_than_the_body_is_rejected() {
        // Claim a 4 GB tile payload in a 14-byte message.
        let mut bytes = AgentMsg::Tile {
            format: format::PNG,
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            data: vec![1, 2, 3],
        }
        .encode();
        bytes[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(AgentMsg::decode(&bytes), Err(MsgError::Truncated));
    }

    #[test]
    fn invalid_utf8_and_bad_booleans_are_rejected() {
        // Hand-build an Error message whose string is invalid UTF-8.
        let bytes = [0x06, 0x01, 0x00, 0xFF];
        assert_eq!(AgentMsg::decode(&bytes), Err(MsgError::BadUtf8));

        // PointerButton with a `pressed` byte that is neither 0 nor 1.
        let bytes = [0x04, 0x00, 0x02];
        assert_eq!(GatewayMsg::decode(&bytes), Err(MsgError::BadBool(2)));
    }

    // Wheel deltas are f32 and pass through untouched: no rounding, and the
    // sign survives (the agent flips it for macOS, not the wire).
    #[test]
    fn wheel_deltas_survive_exactly() {
        let msg = GatewayMsg::Wheel {
            dx: -0.5,
            dy: 33.333_332,
        };
        match GatewayMsg::decode(&msg.encode()).unwrap() {
            GatewayMsg::Wheel { dx, dy } => {
                assert_eq!(dx, -0.5);
                assert_eq!(dy, 33.333_332);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
