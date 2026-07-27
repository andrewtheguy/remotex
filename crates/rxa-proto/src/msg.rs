//! The message set, hand-rolled little-endian in the style of `src/vnc.rs`.
//!
//! There are eighteen messages between the two enums, so a serialization
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
/// accidental 200 MB copy must not stall a session.
///
/// Text over this is refused, not truncated. Truncation is the worse failure of
/// the two: it arrives looking exactly like a complete paste, and neither end
/// can tell that the rest is missing until something downstream is quietly
/// wrong. A refusal is reported as one — the panels name the size and the limit
/// (see the `oversized_bytes` on the clipboard messages) — so the surprise
/// happens at the clipboard, where it can be understood, instead of in whatever
/// the text was pasted into.
pub const MAX_CLIPBOARD_BYTES: usize = 65_536;

/// Whether `text` fits one clipboard transfer.
///
/// The byte length is what counts: [`MAX_CLIPBOARD_BYTES`] bounds the wire, and
/// `str::len` is already UTF-8 bytes.
pub fn clipboard_fits(text: &str) -> bool {
    text.len() <= MAX_CLIPBOARD_BYTES
}

/// A display's backing scale as it travels on this wire: hundredths of a
/// captured pixel per point of the desktop being captured — 100 for a 1× panel,
/// 200 for a Retina one. Hundredths rather than a float because every other
/// number in these messages is an integer, and the scale is a ratio of two
/// integer display-mode sizes to begin with.
pub const SCALE_ONE: u16 = 100;

/// The largest scale worth believing. macOS has only ever shipped 1× and 2×
/// panels; this leaves room for one more doubling and rejects the rest.
const SCALE_MAX: u16 = 4 * SCALE_ONE;

/// A wire `scale` as the ratio clients divide the framebuffer by.
///
/// Anything outside [`SCALE_ONE`]`..=`[`SCALE_MAX`] — a zero from an agent that
/// could not read the display's mode, a number no panel has — reads as 1×,
/// which is the answer that leaves the framebuffer alone. A scale below 1 is as
/// wrong as one above 4: it would blow the desktop up rather than shrink it.
pub fn scale_ratio(scale: u16) -> f32 {
    if (SCALE_ONE..=SCALE_MAX).contains(&scale) {
        f32::from(scale) / f32::from(SCALE_ONE)
    } else {
        1.0
    }
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

/// One of the Mac's displays, as the clients list it.
///
/// `id` is the `CGDirectDisplayID`, and it is the only field the agent reads
/// back out of a [`GatewayMsg::SelectDisplay`] — position in the list is not an
/// identity, because attaching or unplugging a screen renumbers everything after
/// it. The two strings are built on the Mac so a menu item and a panel row read
/// the same in both clients, and so that neither has to know how macOS names
/// displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayEntry {
    /// `CGDirectDisplayID`. Stable while the display stays attached.
    pub id: u32,
    /// Short enough for a menu item: `"Display 2"`, or `"Virtual display"`.
    pub label: String,
    /// The line under it: `"1600×1000 at 2x"`.
    pub detail: String,
    /// Captured pixels, as [`AgentMsg::DisplaySize`] would report them.
    pub w: u16,
    pub h: u16,
    /// Backing scale in [`SCALE_ONE`] hundredths.
    pub scale: u16,
    /// [`DisplayEntry::MAIN`] and [`DisplayEntry::OWNED`].
    pub flags: u8,
}

impl DisplayEntry {
    /// The Mac's main display — where a fresh session starts.
    pub const MAIN: u8 = 1 << 0;
    /// The display the agent created for itself, rather than one of the Mac's
    /// own screens.
    pub const OWNED: u8 = 1 << 1;

    pub fn is_main(&self) -> bool {
        self.flags & Self::MAIN != 0
    }

    pub fn is_owned(&self) -> bool {
        self.flags & Self::OWNED != 0
    }
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
        /// The shared display's backing scale, in [`SCALE_ONE`] hundredths. This
        /// is what makes `w`/`h` above readable: they are captured *pixels*, and
        /// only this says how many of them the Mac draws per point of its own
        /// desktop, which is what a client needs to present a Retina Mac at its
        /// own size rather than at twice it.
        scale: u16,
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
    /// The display was reconfigured on the Mac — by whoever is using it, in
    /// System Settings or wherever else macOS changes a display's mode. `scale`
    /// is its backing scale in [`SCALE_ONE`] hundredths, re-sent with every size
    /// because a mode switch can change it: the same panel has HiDPI and 1×
    /// modes, and which one it is in decides how the pixels below should be
    /// presented.
    ///
    /// This is the only direction resolution travels on this wire. The Mac owns
    /// its own resolution; nothing here asks it to change.
    DisplaySize { w: u16, h: u16, scale: u16 },
    /// Every display the agent could share, and which one it is sharing now.
    ///
    /// Sent unprompted: after `Hello`, on `Attach`, whenever the set of attached
    /// displays changes, and after acting on a [`GatewayMsg::SelectDisplay`].
    /// The clients hold no display state of their own — the checkmark follows
    /// `active`, so a selection that failed leaves the menu honest.
    ///
    /// Which display, unlike what resolution, *is* a client's to choose: a Mac
    /// with three screens has three things worth looking at, and the person
    /// looking is not the one sitting in front of them.
    Displays {
        /// The `id` of the entry being captured. Not an index.
        active: u32,
        displays: Vec<DisplayEntry>,
    },
    Pong { nonce: u64 },
    /// Something the user has to act on — most often a missing TCC grant.
    /// Surfaced in the browser rather than dying quietly in a log.
    Error { message: String },
    /// The Mac's pasteboard text. Sent either in reply to a
    /// [`GatewayMsg::ClipboardRequest`], or unprompted after
    /// [`GatewayMsg::ClipboardWatch`] turned the watcher on and the pasteboard
    /// changed. `requested` is true only for the former, so the gateway can
    /// preserve automatic browser clipboard sync for watcher pushes without
    /// treating a panel Fetch as a copy action. `changed_at_ms` is when the
    /// agent observed that change, and is retained on later Fetch replies.
    /// `None` means the pasteboard content predates this watched session, so its
    /// real change time is unknown.
    /// Never sent otherwise: with the watch off the agent does not look at the
    /// pasteboard at all.
    /// `oversized_bytes` is `Some(len)` when the pasteboard held more than
    /// [`MAX_CLIPBOARD_BYTES`] of text: `text` is then empty and `len` is what
    /// the Mac actually holds, so the browser can say so rather than show an
    /// empty clipboard or a truncated one.
    Clipboard {
        text: String,
        changed_at_ms: Option<u64>,
        requested: bool,
        oversized_bytes: Option<u64>,
    },
}

impl AgentMsg {
    const T_HELLO: u8 = 0x01;
    const T_TILE: u8 = 0x02;
    const T_CURSOR: u8 = 0x03;
    const T_DISPLAY_SIZE: u8 = 0x04;
    const T_PONG: u8 = 0x05;
    const T_ERROR: u8 = 0x06;
    const T_CLIPBOARD: u8 = 0x07;
    const T_DISPLAYS: u8 = 0x08;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            AgentMsg::Hello {
                version,
                agent_version,
                w,
                h,
                scale,
            } => {
                out.push(Self::T_HELLO);
                put_u16(&mut out, *version);
                put_str(&mut out, agent_version);
                put_u16(&mut out, *w);
                put_u16(&mut out, *h);
                put_u16(&mut out, *scale);
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
            AgentMsg::DisplaySize { w, h, scale } => {
                out.push(Self::T_DISPLAY_SIZE);
                put_u16(&mut out, *w);
                put_u16(&mut out, *h);
                put_u16(&mut out, *scale);
            }
            AgentMsg::Displays { active, displays } => {
                out.push(Self::T_DISPLAYS);
                put_u32(&mut out, *active);
                // A Mac has a handful of displays; the count is u16 for the
                // same reason every other length here is: one width for all of
                // them beats a byte saved.
                put_u16(&mut out, displays.len().min(usize::from(u16::MAX)) as u16);
                for display in displays.iter().take(usize::from(u16::MAX)) {
                    put_u32(&mut out, display.id);
                    put_str(&mut out, &display.label);
                    put_str(&mut out, &display.detail);
                    put_u16(&mut out, display.w);
                    put_u16(&mut out, display.h);
                    put_u16(&mut out, display.scale);
                    out.push(display.flags);
                }
            }
            AgentMsg::Pong { nonce } => {
                out.push(Self::T_PONG);
                out.extend_from_slice(&nonce.to_le_bytes());
            }
            AgentMsg::Error { message } => {
                out.push(Self::T_ERROR);
                put_str(&mut out, message);
            }
            AgentMsg::Clipboard {
                text,
                changed_at_ms,
                requested,
                oversized_bytes,
            } => {
                out.push(Self::T_CLIPBOARD);
                out.push(u8::from(*requested));
                match changed_at_ms {
                    Some(value) => {
                        out.push(1);
                        out.extend_from_slice(&value.to_le_bytes());
                    }
                    None => out.push(0),
                }
                match oversized_bytes {
                    Some(value) => {
                        out.push(1);
                        out.extend_from_slice(&value.to_le_bytes());
                    }
                    None => out.push(0),
                }
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
                scale: r.u16()?,
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
                scale: r.u16()?,
            },
            Self::T_DISPLAYS => {
                let active = r.u32()?;
                let count = usize::from(r.u16()?);
                // Not `with_capacity(count)`: the count is the peer's claim, and
                // a truncated body would otherwise reserve for a list that isn't
                // there. Every read below is bounds-checked, so a lie costs a
                // `Truncated` and nothing else.
                let mut displays = Vec::new();
                for _ in 0..count {
                    displays.push(DisplayEntry {
                        id: r.u32()?,
                        label: r.string()?,
                        detail: r.string()?,
                        w: r.u16()?,
                        h: r.u16()?,
                        scale: r.u16()?,
                        flags: r.u8()?,
                    });
                }
                AgentMsg::Displays { active, displays }
            }
            Self::T_PONG => AgentMsg::Pong { nonce: r.u64()? },
            Self::T_ERROR => AgentMsg::Error {
                message: r.string()?,
            },
            // Field order is wire order: a struct literal evaluates in the
            // order written, and every one of these reads moves the cursor.
            Self::T_CLIPBOARD => AgentMsg::Clipboard {
                requested: r.bool()?,
                changed_at_ms: if r.bool()? { Some(r.u64()?) } else { None },
                oversized_bytes: if r.bool()? { Some(r.u64()?) } else { None },
                text: r.text()?,
            },
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
    /// Share a different display: the `id` of one of the entries the agent last
    /// reported in [`AgentMsg::Displays`].
    ///
    /// The agent answers with a `DisplaySize` for the new display and a fresh
    /// `Displays` naming it active, or — for an `id` it cannot resolve — an
    /// [`AgentMsg::Error`] while it keeps capturing what it already had. Nothing
    /// here changes a display's *mode*; this only picks which one is shared.
    SelectDisplay { id: u32 },
    /// The density of the screen the *client's* window is on, in [`SCALE_ONE`]
    /// hundredths — 100 for a 1x screen, 200 for a Retina one. Sent when a
    /// session starts and again whenever the client's window changes screen.
    ///
    /// This is the one message that asks the Mac to change a display rather than
    /// describing what the client is doing, and it is deliberately narrow: only a
    /// display the *agent made* can act on it, and only its density. The Mac's own
    /// screens ignore it — nobody's physical panel should change because someone
    /// connected — and no message changes a display's resolution (see
    /// [`GatewayMsg::SelectDisplay`]).
    ///
    /// Acting on it costs nothing when the two already agree, and saves three
    /// quarters of the pixels when they do not: a 2x guest viewed from a 1x screen
    /// is four times the framebuffer for a picture the client immediately halves.
    HostScale { scale: u16 },
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
    const T_SELECT_DISPLAY: u8 = 0x0b;
    const T_HOST_SCALE: u8 = 0x0c;

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
            GatewayMsg::SelectDisplay { id } => {
                out.push(Self::T_SELECT_DISPLAY);
                put_u32(&mut out, *id);
            }
            GatewayMsg::HostScale { scale } => {
                out.push(Self::T_HOST_SCALE);
                put_u16(&mut out, *scale);
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
            Self::T_SELECT_DISPLAY => GatewayMsg::SelectDisplay { id: r.u32()? },
            Self::T_HOST_SCALE => GatewayMsg::HostScale { scale: r.u16()? },
            other => return Err(MsgError::UnknownType(other)),
        };
        r.finish()?;
        Ok(msg)
    }
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
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
                scale: 2 * SCALE_ONE,
            },
            // An empty version string, a zero-size display and a scale no display
            // has still roundtrip: the codec carries what it is given, and
            // `scale_ratio` is where an impossible one is refused.
            AgentMsg::Hello {
                version: 0,
                agent_version: String::new(),
                w: 0,
                h: 0,
                scale: 0,
            },
            // A display of the agent's own, which is created at twice its point
            // size: nothing about the wire distinguishes it from the Mac's own
            // screen, because nothing needs to.
            AgentMsg::Hello {
                version: crate::VERSION,
                agent_version: "0.0.33".to_owned(),
                w: 3200,
                h: 2000,
                scale: 2 * SCALE_ONE,
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
            AgentMsg::DisplaySize {
                w: 1920,
                h: 1080,
                scale: SCALE_ONE,
            },
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
                changed_at_ms: Some(1_721_234_567_890),
                requested: true,
                oversized_bytes: None,
            },
            // An empty pasteboard is text too, and distinct from "no reply".
            AgentMsg::Clipboard {
                text: String::new(),
                changed_at_ms: None,
                requested: false,
                oversized_bytes: None,
            },
            // Refused for its size: no text, and the size it would have been.
            AgentMsg::Clipboard {
                text: String::new(),
                changed_at_ms: Some(1_721_234_567_890),
                requested: true,
                oversized_bytes: Some(200 * 1024 * 1024),
            },
            AgentMsg::Displays {
                active: 2,
                displays: vec![
                    DisplayEntry {
                        id: 1,
                        label: "Display 1".to_owned(),
                        detail: "1920×1080 at 1x".to_owned(),
                        w: 1920,
                        h: 1080,
                        scale: SCALE_ONE,
                        flags: DisplayEntry::MAIN,
                    },
                    DisplayEntry {
                        id: 2,
                        label: "Virtual display".to_owned(),
                        detail: "1600×1000 at 2x — écran 画面".to_owned(),
                        w: 3200,
                        h: 2000,
                        scale: 2 * SCALE_ONE,
                        flags: DisplayEntry::OWNED,
                    },
                ],
            },
            // No displays at all is a real state — every screen unplugged from a
            // headless Mac — and distinct from never having reported.
            AgentMsg::Displays {
                active: 0,
                displays: Vec::new(),
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
            GatewayMsg::SelectDisplay { id: 0 },
            GatewayMsg::SelectDisplay { id: u32::MAX },
            GatewayMsg::HostScale { scale: SCALE_ONE },
            GatewayMsg::HostScale {
                scale: 2 * SCALE_ONE,
            },
            // A client on a screen with a fractional ratio, and one whose report
            // is nonsense. Both travel; `scale_ratio` is where the second is
            // refused, exactly as it is for a display's own scale.
            GatewayMsg::HostScale { scale: 150 },
            GatewayMsg::HostScale { scale: 0 },
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
        assert_eq!(agent.len(), 8, "eight agent message types");

        let mut gateway: Vec<u8> = gateway_variants().iter().map(|m| m.encode()[0]).collect();
        gateway.sort_unstable();
        gateway.dedup();
        assert_eq!(gateway.len(), 12, "twelve gateway message types");
    }

    // The count on the wire is the peer's claim about what follows. A body that
    // does not back it must be refused rather than reserving for it or handing
    // back a short list.
    #[test]
    fn a_display_list_shorter_than_its_count_is_truncated_not_trusted() {
        let mut bytes = (AgentMsg::Displays {
            active: 7,
            displays: vec![DisplayEntry {
                id: 7,
                label: "Display 1".to_owned(),
                detail: "800×600 at 1x".to_owned(),
                w: 800,
                h: 600,
                scale: SCALE_ONE,
                flags: DisplayEntry::MAIN,
            }],
        })
        .encode();
        // Claim four more entries than the body carries.
        bytes[5..7].copy_from_slice(&5u16.to_le_bytes());
        assert_eq!(AgentMsg::decode(&bytes), Err(MsgError::Truncated));
    }

    #[test]
    fn display_flags_name_the_main_and_owned_displays() {
        let plain = DisplayEntry {
            id: 3,
            label: "Display 2".to_owned(),
            detail: "1440×900 at 1x".to_owned(),
            w: 1440,
            h: 900,
            scale: SCALE_ONE,
            flags: 0,
        };
        assert!(!plain.is_main() && !plain.is_owned());

        // macOS can arrange the agent's own display as the main one, and the flags
        // are independent bits rather than an enum, so both together must read.
        let both = DisplayEntry {
            flags: DisplayEntry::MAIN | DisplayEntry::OWNED,
            ..plain.clone()
        };
        assert!(both.is_main() && both.is_owned());
    }

    // Clipboard text uses u32 framing, not the u16 `put_str` every other string
    // field uses — a copy larger than u16::MAX must arrive whole rather than
    // silently losing its tail.
    #[test]
    fn clipboard_text_roundtrips_past_the_u16_string_ceiling() {
        let text = "é".repeat(usize::from(u16::MAX)); // 128 KiB of UTF-8
        assert!(text.len() > usize::from(u16::MAX));

        let msg = AgentMsg::Clipboard {
            text: text.clone(),
            changed_at_ms: Some(u64::MAX),
            requested: true,
            oversized_bytes: None,
        };
        assert_eq!(AgentMsg::decode(&msg.encode()).unwrap(), msg);

        let msg = GatewayMsg::Clipboard { text };
        assert_eq!(GatewayMsg::decode(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn clipboard_text_that_is_not_utf8_is_rejected() {
        // A well-formed frame whose payload is not UTF-8: the three option/flag
        // bytes (requested, no timestamp, not oversized), length 2, then a lone
        // continuation byte pair.
        let mut bytes = vec![AgentMsg::T_CLIPBOARD, 0, 0, 0];
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xC3, 0x28]);
        assert_eq!(AgentMsg::decode(&bytes), Err(MsgError::BadUtf8));

        let mut bytes = vec![GatewayMsg::T_CLIPBOARD];
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xC3, 0x28]);
        assert_eq!(GatewayMsg::decode(&bytes), Err(MsgError::BadUtf8));

        // A length that overruns the body is truncation, not bad UTF-8.
        let mut bytes = vec![AgentMsg::T_CLIPBOARD, 0, 0, 0];
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

        let mut bytes = AgentMsg::DisplaySize {
            w: 1,
            h: 2,
            scale: SCALE_ONE,
        }
        .encode();
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

    // Clients *divide* the framebuffer by this, so every answer has to be a
    // number that leaves a desktop on screen — 1x for anything unbelievable
    // rather than a zero to divide by or a factor that would shrink the desktop
    // to nothing.
    #[test]
    fn an_unbelievable_scale_reads_as_one() {
        assert_eq!(scale_ratio(SCALE_ONE), 1.0);
        assert_eq!(scale_ratio(2 * SCALE_ONE), 2.0);
        assert_eq!(scale_ratio(150), 1.5);
        assert_eq!(scale_ratio(SCALE_MAX), 4.0);

        // An agent that could not read the display's mode, a scale that would
        // magnify rather than reduce, and one no panel has.
        assert_eq!(scale_ratio(0), 1.0);
        assert_eq!(scale_ratio(50), 1.0);
        assert_eq!(scale_ratio(SCALE_MAX + 1), 1.0);
        assert_eq!(scale_ratio(u16::MAX), 1.0);
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
