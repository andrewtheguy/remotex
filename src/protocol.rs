//! Wire protocol shared (in shape) with the frontend `src/protocol.ts`.
//!
//! `ClientMsg` flows browser -> server (input events) as JSON text frames.
//! Server -> browser, the transport is split by weight (see
//! docs/architecture.md):
//!
//! - **Screen tiles** are binary WebSocket frames: a fixed 10-byte header
//!   followed by an encoded image payload (PNG, or JPEG from the macOS agent).
//!   This replaced base64 RGBA inside JSON text, which inflated the bottleneck
//!   backend->browser link by ~4.3x (4 bytes/px, +33% base64).
//! - **Control messages** (`resize`, `error`, `cursor`, …) are rare and small;
//!   they stay JSON text frames with a `type` tag. `cursor` carries a base64
//!   PNG — a pointer shape is a couple of hundred bytes and changes a handful
//!   of times a session, so it is not worth a second binary frame kind.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Transport policy shared by all engines: a dirty rectangle taller than this
/// is split into strips before being sent, so a full-screen repaint doesn't
/// produce one huge WebSocket message.
pub const STRIP_ROWS: u16 = 64;

/// The revision of everything in this file: [`ClientMsg`], [`ControlMsg`], and
/// the [`Tile`] frame layout. Served from `GET /api/config` so a client that
/// isn't shipped with the gateway can refuse a version it cannot speak.
///
/// The SPA doesn't check it — it is served by this same binary, so it cannot
/// disagree. The macOS viewer is a separate artifact and does. Bump this only
/// for a change that would break a client compiled against the old shape; a
/// purely additive control message is not one, because clients are required to
/// ignore tags they don't know.
pub const PROTOCOL_VERSION: u32 = 2;

/// The clipboard transfer cap and its test, defined in `rxa-proto` so the
/// browser link, the gateway and the Mac agent cannot drift apart on it (the
/// agent crate can't see this file). Re-exported here because every other
/// boundary in this crate reaches for `protocol::` first.
pub use rxa_proto::msg::{MAX_CLIPBOARD_BYTES, clipboard_fits};

/// A remote clipboard value held by an engine, plus when remotex last observed
/// that clipboard change. `None` is honest for content that predates the
/// session: VNC and RDP do not expose an OS clipboard timestamp, so there is no
/// reliable time to invent until a change arrives on their clipboard channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardSnapshot {
    pub text: String,
    pub changed_at_ms: Option<u64>,
    /// `Some(len)` when the remote's clipboard was refused for exceeding
    /// [`MAX_CLIPBOARD_BYTES`], carrying the size it actually was. `text` is
    /// empty then — build these through [`Self::oversized`], because empty text
    /// on its own already means "the remote has copied nothing".
    pub oversized_bytes: Option<u64>,
}

impl ClipboardSnapshot {
    /// Record a clipboard change observed right now.
    pub fn changed(text: String, previous: Option<&Self>) -> Self {
        Self {
            text,
            changed_at_ms: Some(Self::now(previous)),
            oversized_bytes: None,
        }
    }

    /// Record that the remote holds `bytes` of text, too much to transfer.
    ///
    /// Still a clipboard change with a timestamp: something was copied over
    /// there, and the panel says so — just not what.
    pub fn oversized(bytes: u64, previous: Option<&Self>) -> Self {
        Self {
            text: String::new(),
            changed_at_ms: Some(Self::now(previous)),
            oversized_bytes: Some(bytes),
        }
    }

    /// The answer before this session has observed any remote clipboard
    /// activity. Empty text is still a successful Fetch response.
    pub fn unobserved() -> Self {
        Self {
            text: String::new(),
            changed_at_ms: None,
            oversized_bytes: None,
        }
    }

    fn now(previous: Option<&Self>) -> u64 {
        rxa_proto::next_clipboard_time(previous.and_then(|snapshot| snapshot.changed_at_ms))
    }
}

/// A mouse button, matching the DOM `MouseEvent.button` numbering.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Browser -> server: input events captured over the remote canvas.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMsg {
    /// Pointer moved to framebuffer coordinates (x, y).
    MouseMove { x: i32, y: i32 },
    /// A mouse button was pressed or released.
    MouseButton { button: MouseButton, pressed: bool },
    /// Scroll wheel delta.
    Wheel { dx: f32, dy: f32 },
    /// A key was pressed or released. `code` is the DOM `KeyboardEvent.code`.
    /// `caps` is the browser's authoritative CapsLock lock state at the moment
    /// of the event (`KeyboardEvent.getModifierState("CapsLock")`), so the
    /// backend never has to infer it — it can't be observed until the first key
    /// event otherwise, which would mis-case letters when CapsLock was already
    /// on at connect time. Used by the VNC engine; RDP lets the host track it.
    Key {
        code: String,
        pressed: bool,
        caps: bool,
    },
    /// The size the client wants the remote desktop to be, in remote pixels:
    /// the room it has, times the density the remote draws at
    /// ([`ServerMsg::Resize`]'s `scale`). Engines that can drive the remote size
    /// act on it (VNC `SetDesktopSize`); the rest ignore it and the client keeps
    /// its scrollbars.
    ///
    /// Not the client's own device pixels, which is what this carried before
    /// clients scaled their output: a desktop sized to those is a desktop drawn
    /// at the host's density rather than its own, which on a Retina host is every
    /// remote's UI at half size.
    ///
    /// This is the only way a client asks for a size, and there is deliberately
    /// no menu of resolutions beside it: a remote's resolution belongs to the
    /// machine running it. Three engines act on it, each where its protocol hands
    /// that decision to the client — VNC's `SetDesktopSize` (TigerVNC-family
    /// servers), continuously; RDP's Display Control channel, on the user's
    /// request; and `rxa`, also on request and narrower still, because a Mac's
    /// own panel is never resized because somebody connected. What `rxa` resizes
    /// is a display the agent *made* to be looked at from here, so the control
    /// appears only while that display is the one being shared.
    Viewport { w: u16, h: u16 },
    /// The density of the screen this client's window is on, in hundredths —
    /// 100 for a 1x screen, 200 for a Retina one. Sent on connect and again
    /// whenever the window moves to a screen of a different density.
    ///
    /// The counterpart to [`ServerMsg::Resize`]'s `scale`, travelling the other
    /// way, and the two are read together: that one says what the remote draws
    /// at, this one says what the client can show. Only the `rxa` engine acts on
    /// it, and only for a display the agent *made* — a Mac's own panel does not
    /// change density because someone connected to it. Every other engine
    /// ignores it.
    ///
    /// It does not change how a client presents what it receives: a client always
    /// lays the remote out at the remote's own point size and lets its host
    /// rasterize that (see `RemoteGeometry` and `applyCanvasCss`). This asks the
    /// remote to *have* the density that makes the result one pixel per pixel,
    /// which is a saving, not a correctness fix — mismatched densities already
    /// look right, they just cost four times the framebuffer or lose sharpness.
    HostScale { scale: u16 },
    /// Re-announce the desktop size and repaint the whole framebuffer.
    /// Injected by the session layer when a client (re)attaches to a running
    /// engine. A client may also send it to recover a canvas that has gone
    /// wrong, which the viewer offers as Remote > Refresh; the SPA has no such
    /// command and never sends this.
    Refresh,
    /// Pick a target from the post-login picker and start a session against it.
    /// Handled by the session layer (spawns the engine for `target`), never
    /// forwarded to an engine. `target` is a `[[targets]]` profile name.
    Connect { target: String },
    /// Tear the current session's engine down and return to the picker
    /// ("switch target"). Handled by the session layer, never forwarded to an
    /// engine.
    Disconnect,
    /// Put `text` on the remote's clipboard (the clipboard panel's "Send", or
    /// the browser's automatic push when the tab regains focus). Ignored by
    /// engines whose target did not opt in (`clipboard` in the target profile).
    Clipboard { text: String },
    /// Ask for the remote's current clipboard text (the panel's "Fetch"); the
    /// engine answers with [`ServerMsg::Clipboard`]. Still worth having
    /// alongside the automatic pushes: a browser attaching mid-session has
    /// missed every one of them. See docs/architecture.md.
    ClipboardRequest,
    /// Share a different one of the remote's displays: the `id` of an entry from
    /// the last [`ServerMsg::Displays`].
    ///
    /// Not the same kind of request as [`ClientMsg::Viewport`] above, and the
    /// distinction is the whole reason both can exist: a remote's *resolution*
    /// belongs to the machine running it, while *which of its screens to look
    /// at* is only ever a question for the person looking. Only `rxa` can answer
    /// it — RDP and VNC each deliver a single framebuffer spanning every remote
    /// screen, with no way to ask for one of them — so those engines never send
    /// a display list and a client with none shows no picker.
    SelectDisplay { id: u32 },
}

/// A dirty rectangle of the framebuffer, sent as one binary WebSocket frame.
/// The payload is an image stream the browser decodes natively — PNG or JPEG,
/// named by the `format` byte so `createImageBitmap` gets the right MIME type.
///
/// The RDP and VNC engines decode a framebuffer and PNG-compress it here
/// ([`Tile::from_rgb`]); the macOS agent chooses PNG or JPEG per tile on the
/// Mac and the gateway relays those bytes untouched ([`Tile::encoded`]), which
/// is why the format travels with the tile instead of being a constant.
///
/// Frame layout (little-endian):
///
/// ```text
/// offset 0: u8  frame kind, always 0x01 (tile)
/// offset 1: u8  format: 1 = PNG, 2 = JPEG
/// offset 2: u16 x
/// offset 4: u16 y
/// offset 6: u16 w
/// offset 8: u16 h
/// offset 10: payload (a PNG or JPEG stream)
/// ```
#[derive(Debug, Clone)]
pub struct Tile {
    /// Payload codec: [`Tile::FORMAT_PNG`] or [`Tile::FORMAT_JPEG`].
    pub format: u8,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    /// The encoded image stream, in `format`.
    pub data: Vec<u8>,
}

impl Tile {
    pub const FRAME_KIND: u8 = 0x01;
    pub const FORMAT_PNG: u8 = 1;
    pub const FORMAT_JPEG: u8 = 2;
    pub const HEADER_LEN: usize = 10;

    /// Build a tile from packed RGB888 pixels, PNG-compressing the payload.
    pub fn from_rgb(x: u16, y: u16, w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Self> {
        let expected = usize::from(w) * usize::from(h) * 3;
        anyhow::ensure!(
            rgb.len() == expected,
            "tile payload is {} bytes, expected {expected} for {w}x{h} RGB",
            rgb.len()
        );
        let data = encode_png(w, h, png::ColorType::Rgb, rgb)?;
        Ok(Self {
            format: Self::FORMAT_PNG,
            x,
            y,
            w,
            h,
            data,
        })
    }

    /// Wrap an already-encoded image stream — the pass-through path for the
    /// macOS agent (see [`crate::rxa`]), which encodes on the Mac so the
    /// gateway never decodes and re-encodes a pixel.
    pub fn encoded(format: u8, x: u16, y: u16, w: u16, h: u16, data: Vec<u8>) -> Self {
        Self {
            format,
            x,
            y,
            w,
            h,
            data,
        }
    }

    /// Serialize into the binary WebSocket frame described above.
    pub fn to_frame(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.data.len());
        out.push(Self::FRAME_KIND);
        out.push(self.format);
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.w.to_le_bytes());
        out.extend_from_slice(&self.h.to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }
}

/// The remote pointer shape, for engines whose server does **not** composite
/// the cursor into the framebuffer and hands the shape over instead (the VNC
/// Cursor pseudo-encoding — see [`crate::vnc`]). The browser draws it locally,
/// anchoring the image so that `(hx, hy)` — the hotspot — lands on the pointer
/// position. RDP never sends one: it renders the pointer into the framebuffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub w: u16,
    pub h: u16,
    /// Hotspot within the image, in cursor pixels.
    pub hx: u16,
    pub hy: u16,
    /// PNG-encoded RGBA image (the alpha channel carries the cursor mask).
    pub png: Vec<u8>,
}

impl CursorShape {
    /// Build from packed RGBA8888 pixels.
    pub fn from_rgba(w: u16, h: u16, hx: u16, hy: u16, rgba: &[u8]) -> anyhow::Result<Self> {
        let expected = usize::from(w) * usize::from(h) * 4;
        anyhow::ensure!(
            rgba.len() == expected,
            "cursor payload is {} bytes, expected {expected} for {w}x{h} RGBA",
            rgba.len()
        );
        let png = encode_png(w, h, png::ColorType::Rgba, rgba)?;
        Ok(Self { w, h, hx, hy, png })
    }
}

/// PNG-encode packed 8-bit pixels. Fast compression: the win over raw is
/// already large for screen content, and this runs on the session's hot path.
fn encode_png(w: u16, h: u16, color: png::ColorType, pixels: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, u32::from(w), u32::from(h));
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(pixels)?;
    writer.finish()?;
    Ok(out)
}

/// The `scale` on [`ServerMsg::Resize`] for a framebuffer whose pixels *are* the
/// points of the desktop it shows: every engine but `rxa`, which reports the
/// density of the Mac display it captures.
///
/// A remote with no density of its own is presented one point per pixel, which is
/// what a VNC or RDP desktop expects — its own DPI settings decide how large its
/// UI is, and a client must not second-guess them by drawing the whole desktop at
/// half size because the host screen happens to be Retina.
pub const UNSCALED: f32 = 1.0;

/// One of the remote's displays, as a client lists it for the user to pick from.
///
/// The strings are built by the remote end and passed through: the Mac knows how
/// its displays are named and numbered, and having it say so once keeps the
/// browser panel and the viewer's menu reading the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    /// Opaque to every client — whatever the engine wants back in
    /// [`ClientMsg::SelectDisplay`]. For `rxa` it is a `CGDirectDisplayID`.
    pub id: u32,
    /// Short enough for a menu item: `"Display 2"`, or `"Virtual display"`.
    pub label: String,
    /// The line under it: `"1600×1000 at 2x"`.
    pub detail: String,
    /// The remote's primary screen.
    pub main: bool,
    /// A display the remote made for this purpose rather than one of its own
    /// screens, so a client can say which is which.
    pub virtual_display: bool,
}

/// Server -> browser: screen updates and session status.
///
/// Most variants come from the protocol engine (tiles, resize, error); the two
/// session-status variants ([`ServerMsg::Picker`] / [`ServerMsg::Connected`])
/// come from the session layer (src/session.rs) to tell the browser which
/// post-login state it is in — the target picker, or a live desktop.
#[derive(Debug, Clone)]
pub enum ServerMsg {
    Tile(Tile),
    /// The remote desktop resolution changed. `w`/`h` are framebuffer pixels;
    /// `scale` is how many of them the remote draws per point of its *own*
    /// desktop — 1.0 for a framebuffer whose pixels are its points (VNC, RDP,
    /// and a 1x Mac), 2.0 for a Retina Mac.
    ///
    /// The two travel together because a client cannot present either without
    /// the other: it shows the desktop at `w / scale` points and lets the host
    /// resample that to its own display, which is what keeps a remote the same
    /// physical size on a 1x screen and a Retina one. A size that arrived without
    /// its density would be presented at the wrong size until the next message.
    Resize { w: u16, h: u16, scale: f32 },
    /// The remote pointer shape changed, and with it the fact that **the
    /// browser** owns pointer rendering for this session — a server that
    /// composites the cursor into the framebuffer (RDP, and VNC servers that
    /// ignore the Cursor pseudo-encoding) never sends this, and the browser
    /// keeps its own pointer hidden. `None` means the remote hid the pointer.
    Cursor(Option<CursorShape>),
    /// A fatal session error the client should surface. The session then
    /// returns to the picker, so the browser shows this against the picker.
    Error { message: String },
    /// No target is selected: show the post-login target picker. Sent on attach
    /// to an idle slot, on disconnect ("switch target"), and when an engine
    /// ends (the remote hung up, or a connect failure after its `Error`).
    Picker,
    /// A target session is live: show the desktop. Sent on attach to a running
    /// engine and right after a [`ClientMsg::Connect`]. `name` is the target
    /// profile the session is bound to; `protocol` (`"rdp"`/`"vnc"`/`"rxa"`) and
    /// `resize` let the browser choose its resize behaviour — VNC resizes
    /// automatically with the viewport, RDP only on the user's request (the
    /// floating menu's "Resize to window"). For `rxa` these two settle only half
    /// of it: `resize` is the target's permission, and whether the control
    /// actually appears also depends on the display being shared being one the
    /// agent made, which arrives later in [`ServerMsg::Displays`]. `clipboard` says whether this
    /// target opted into the clipboard bridge, which is what enables the
    /// floating menu's Clipboard button.
    Connected {
        name: String,
        protocol: &'static str,
        resize: bool,
        clipboard: bool,
    },
    /// The remote's displays and which one is being shared, whenever either
    /// changes. Pushed, never requested: a client holds no display state of its
    /// own, so a checkmark follows `active` and a selection that failed leaves
    /// the menu honest rather than showing a screen nobody is looking at.
    ///
    /// An engine that cannot offer a choice never sends this, and its clients
    /// show no display picker at all — see [`ClientMsg::SelectDisplay`].
    Displays {
        /// The `id` of the entry being shared. Not an index, and not necessarily
        /// present in `displays` — a screen can be unplugged between the two.
        active: u32,
        displays: Vec<DisplayInfo>,
    },
    /// Whether the remote runs macOS, discovered by the engine as it connects
    /// and sent once, next to the first [`ServerMsg::Resize`].
    ///
    /// Only a native host reads it, and only to decide whether a local Command
    /// shortcut belongs to the Mac the user is sitting at or the Mac at the far
    /// end. That is the whole reason this exists — which is why it is one bit
    /// discovered from the connection rather than an OS name someone has to
    /// keep correct in the config file.
    RemoteOs { macos: bool },
    /// The remote's clipboard text, either pushed when the engine observes a
    /// change or returned from its cache for [`ClientMsg::ClipboardRequest`].
    /// `requested` distinguishes those paths so an explicit panel read does
    /// not silently replace the browser's local OS clipboard; unsolicited
    /// pushes still drive automatic sync. `changed_at_ms` is retained across
    /// Fetches; `None` means the content predates the session and its real
    /// activity time is unknown.
    Clipboard {
        text: String,
        changed_at_ms: Option<u64>,
        requested: bool,
        /// `Some(len)` when the remote's clipboard was refused for exceeding
        /// [`MAX_CLIPBOARD_BYTES`] — see [`ClipboardSnapshot::oversized_bytes`].
        oversized_bytes: Option<u64>,
    },
}

/// One encoded WebSocket frame, ready to send.
#[derive(Debug)]
pub enum WireFrame {
    Text(String),
    Binary(Vec<u8>),
}

/// JSON shape of the text-frame control messages (`ServerMsg` minus tiles).
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ControlMsg<'a> {
    Resize { w: u16, h: u16, scale: f32 },
    /// `image` is a base64 PNG (the browser wraps it in a `data:` URL), null
    /// when the remote hid the pointer.
    Cursor {
        image: Option<String>,
        w: u16,
        h: u16,
        hx: u16,
        hy: u16,
    },
    Error { message: &'a str },
    Picker,
    Connected {
        name: &'a str,
        protocol: &'a str,
        resize: bool,
        clipboard: bool,
    },
    RemoteOs { macos: bool },
    Clipboard {
        text: &'a str,
        #[serde(rename = "changedAtMs")]
        changed_at_ms: Option<u64>,
        requested: bool,
        #[serde(rename = "oversizedBytes")]
        oversized_bytes: Option<u64>,
    },
    Displays {
        active: u32,
        displays: Vec<WireDisplay<'a>>,
    },
}

/// [`DisplayInfo`] as it goes out: `virtual_display` is `virtual` on the wire,
/// which is a reserved word in Rust and not in JavaScript.
#[derive(Serialize)]
struct WireDisplay<'a> {
    id: u32,
    label: &'a str,
    detail: &'a str,
    main: bool,
    #[serde(rename = "virtual")]
    virtual_display: bool,
}

impl ServerMsg {
    /// Encode for the WebSocket: tiles as binary frames, control as JSON text.
    pub fn encode(&self) -> WireFrame {
        match self {
            ServerMsg::Tile(tile) => WireFrame::Binary(tile.to_frame()),
            ServerMsg::Resize { w, h, scale } => WireFrame::Text(control(&ControlMsg::Resize {
                w: *w,
                h: *h,
                scale: *scale,
            })),
            ServerMsg::Cursor(shape) => WireFrame::Text(control(&match shape {
                Some(c) => ControlMsg::Cursor {
                    image: Some(base64::engine::general_purpose::STANDARD.encode(&c.png)),
                    w: c.w,
                    h: c.h,
                    hx: c.hx,
                    hy: c.hy,
                },
                None => ControlMsg::Cursor {
                    image: None,
                    w: 0,
                    h: 0,
                    hx: 0,
                    hy: 0,
                },
            })),
            ServerMsg::Error { message } => WireFrame::Text(control(&ControlMsg::Error { message })),
            ServerMsg::Picker => WireFrame::Text(control(&ControlMsg::Picker)),
            ServerMsg::Connected {
                name,
                protocol,
                resize,
                clipboard,
            } => WireFrame::Text(control(&ControlMsg::Connected {
                name,
                protocol,
                resize: *resize,
                clipboard: *clipboard,
            })),
            ServerMsg::RemoteOs { macos } => {
                WireFrame::Text(control(&ControlMsg::RemoteOs { macos: *macos }))
            }
            ServerMsg::Displays { active, displays } => {
                WireFrame::Text(control(&ControlMsg::Displays {
                    active: *active,
                    displays: displays
                        .iter()
                        .map(|display| WireDisplay {
                            id: display.id,
                            label: &display.label,
                            detail: &display.detail,
                            main: display.main,
                            virtual_display: display.virtual_display,
                        })
                        .collect(),
                }))
            }
            // The last gate on the browser link, behind each engine's own: an
            // oversized value is reported as its size rather than sent, so no
            // path can put an unbounded string on this link.
            ServerMsg::Clipboard {
                text,
                changed_at_ms,
                requested,
                oversized_bytes,
            } => {
                let refused = (!clipboard_fits(text)).then_some(text.len() as u64);
                WireFrame::Text(control(&ControlMsg::Clipboard {
                    text: if refused.is_some() { "" } else { text },
                    changed_at_ms: *changed_at_ms,
                    requested: *requested,
                    oversized_bytes: oversized_bytes.or(refused),
                }))
            }
        }
    }
}

fn control(msg: &ControlMsg<'_>) -> String {
    // Infallible: ControlMsg is a string/number-only struct enum.
    serde_json::to_string(msg).expect("control message serialization cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deserialize the exact JSON the frontend (`protocol.ts`) sends.
    #[test]
    fn client_messages_deserialize_from_frontend_json() {
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"mouseMove","x":5,"y":6}"#).unwrap(),
            ClientMsg::MouseMove { x: 5, y: 6 }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(
                r#"{"type":"mouseButton","button":"right","pressed":true}"#
            )
            .unwrap(),
            ClientMsg::MouseButton {
                button: MouseButton::Right,
                pressed: true
            }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"wheel","dx":0.0,"dy":-2.5}"#).unwrap(),
            ClientMsg::Wheel { dy, .. } if dy == -2.5
        ));
        match serde_json::from_str::<ClientMsg>(
            r#"{"type":"key","code":"KeyA","pressed":false,"caps":true}"#,
        )
        .unwrap()
        {
            ClientMsg::Key {
                code,
                pressed,
                caps,
            } => {
                assert_eq!(code, "KeyA");
                assert!(!pressed);
                assert!(caps);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"viewport","w":2560,"h":1440}"#).unwrap(),
            ClientMsg::Viewport { w: 2560, h: 1440 }
        ));
        // Viewport dimensions beyond the protocol's u16 range are rejected at
        // the deserialization boundary, not clamped.
        assert!(serde_json::from_str::<ClientMsg>(r#"{"type":"viewport","w":70000,"h":1}"#).is_err());
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"refresh"}"#).unwrap(),
            ClientMsg::Refresh
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"clipboardRequest"}"#).unwrap(),
            ClientMsg::ClipboardRequest
        ));
        match serde_json::from_str::<ClientMsg>(r#"{"type":"clipboard","text":"héllo"}"#).unwrap() {
            ClientMsg::Clipboard { text } => assert_eq!(text, "héllo"),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"selectDisplay","id":2}"#).unwrap(),
            ClientMsg::SelectDisplay { id: 2 }
        ));
        // A display id is opaque and uses the full u32 range: nothing may narrow
        // it on the way in.
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"selectDisplay","id":4294967295}"#)
                .unwrap(),
            ClientMsg::SelectDisplay { id: u32::MAX }
        ));
        assert!(serde_json::from_str::<ClientMsg>(r#"{"type":"selectDisplay","id":-1}"#).is_err());
    }

    // Control messages keep the tagged, camelCase text shape `protocol.ts` expects.
    #[test]
    fn control_messages_encode_to_tagged_camelcase_text() {
        match (ServerMsg::Resize { w: 1280, h: 800, scale: UNSCALED }).encode() {
            WireFrame::Text(json) => {
                assert_eq!(json, r#"{"type":"resize","w":1280,"h":800,"scale":1.0}"#)
            }
            other => panic!("resize should be a text frame: {other:?}"),
        }
        match (ServerMsg::Error { message: "boom".to_owned() }).encode() {
            WireFrame::Text(json) => assert_eq!(json, r#"{"type":"error","message":"boom"}"#),
            other => panic!("error should be a text frame: {other:?}"),
        }
        match (ServerMsg::Connected {
            name: "mac".to_owned(),
            protocol: "rxa",
            resize: false,
            clipboard: true,
        })
        .encode()
        {
            WireFrame::Text(json) => assert_eq!(
                json,
                r#"{"type":"connected","name":"mac","protocol":"rxa","resize":false,"clipboard":true}"#
            ),
            other => panic!("connected should be a text frame: {other:?}"),
        }
        match (ServerMsg::Displays {
            active: 7,
            displays: vec![
                DisplayInfo {
                    id: 7,
                    label: "Display 1".to_owned(),
                    detail: "1920×1080 at 1x".to_owned(),
                    main: true,
                    virtual_display: false,
                },
                DisplayInfo {
                    id: 9,
                    label: "Virtual display".to_owned(),
                    detail: "3200×2000 at 2x".to_owned(),
                    main: false,
                    virtual_display: true,
                },
            ],
        })
        .encode()
        {
            // `virtual` on the wire: reserved in Rust, ordinary in JavaScript.
            WireFrame::Text(json) => assert_eq!(
                json,
                r#"{"type":"displays","active":7,"displays":[{"id":7,"label":"Display 1","detail":"1920×1080 at 1x","main":true,"virtual":false},{"id":9,"label":"Virtual display","detail":"3200×2000 at 2x","main":false,"virtual":true}]}"#
            ),
            other => panic!("displays should be a text frame: {other:?}"),
        }
        // No displays is a shape a client must handle, not one it never sees: a
        // Mac can have every screen unplugged.
        match (ServerMsg::Displays {
            active: 0,
            displays: Vec::new(),
        })
        .encode()
        {
            WireFrame::Text(json) => {
                assert_eq!(json, r#"{"type":"displays","active":0,"displays":[]}"#)
            }
            other => panic!("displays should be a text frame: {other:?}"),
        }
        for macos in [false, true] {
            match (ServerMsg::RemoteOs { macos }).encode() {
                WireFrame::Text(json) => assert_eq!(
                    json,
                    format!(r#"{{"type":"remoteOs","macos":{macos}}}"#)
                ),
                other => panic!("remoteOs should be a text frame: {other:?}"),
            }
        }
        match (ServerMsg::Clipboard {
            text: "hi \"there\"".to_owned(),
            changed_at_ms: Some(1_721_234_567_890),
            requested: false,
            oversized_bytes: None,
        })
        .encode()
        {
            WireFrame::Text(json) => {
                assert_eq!(
                    json,
                    r#"{"type":"clipboard","text":"hi \"there\"","changedAtMs":1721234567890,"requested":false,"oversizedBytes":null}"#
                );
            }
            other => panic!("clipboard should be a text frame: {other:?}"),
        }
        match (ServerMsg::Clipboard {
            text: String::new(),
            changed_at_ms: None,
            requested: true,
            oversized_bytes: None,
        })
        .encode()
        {
            WireFrame::Text(json) => {
                assert_eq!(
                    json,
                    r#"{"type":"clipboard","text":"","changedAtMs":null,"requested":true,"oversizedBytes":null}"#
                );
            }
            other => panic!("clipboard should be a text frame: {other:?}"),
        }
    }

    // Nothing may put an unbounded string on the browser link. Refused rather
    // than truncated: the browser is told the size so it can say so, where the
    // first 64 KiB of a copy could not be told from all of it.
    #[test]
    fn oversized_clipboard_text_is_refused_with_its_size() {
        assert!(clipboard_fits(&"a".repeat(MAX_CLIPBOARD_BYTES)));
        assert!(!clipboard_fits(&"a".repeat(MAX_CLIPBOARD_BYTES + 1)));
        // Bytes, not characters: two-byte chars hit the ceiling twice as fast.
        assert!(!clipboard_fits(&"é".repeat(MAX_CLIPBOARD_BYTES)));

        let oversized = MAX_CLIPBOARD_BYTES + 10;
        match (ServerMsg::Clipboard {
            text: "x".repeat(oversized),
            changed_at_ms: Some(42),
            requested: true,
            oversized_bytes: None,
        })
        .encode()
        {
            WireFrame::Text(json) => assert_eq!(
                json,
                format!(
                    r#"{{"type":"clipboard","text":"","changedAtMs":42,"requested":true,"oversizedBytes":{oversized}}}"#
                )
            ),
            other => panic!("clipboard should be a text frame: {other:?}"),
        }

        // An engine that already refused it says so itself, and that size is
        // kept rather than recomputed from the empty text it sent.
        match (ServerMsg::Clipboard {
            text: String::new(),
            changed_at_ms: Some(42),
            requested: false,
            oversized_bytes: Some(209_715_200),
        })
        .encode()
        {
            WireFrame::Text(json) => assert_eq!(
                json,
                r#"{"type":"clipboard","text":"","changedAtMs":42,"requested":false,"oversizedBytes":209715200}"#
            ),
            other => panic!("clipboard should be a text frame: {other:?}"),
        }
    }

    // Empty text alone means "the remote has copied nothing", so the oversized
    // marker is what keeps the two apart.
    #[test]
    fn an_oversized_snapshot_is_distinguishable_from_an_empty_one() {
        let oversized = ClipboardSnapshot::oversized(209_715_200, None);
        assert!(oversized.text.is_empty());
        assert_eq!(oversized.oversized_bytes, Some(209_715_200));
        assert!(oversized.changed_at_ms.is_some(), "still clipboard activity");

        let unobserved = ClipboardSnapshot::unobserved();
        assert!(unobserved.text.is_empty());
        assert_eq!(unobserved.oversized_bytes, None);
        assert_eq!(unobserved.changed_at_ms, None);
    }

    #[test]
    fn repeated_clipboard_activity_advances_the_timestamp_even_for_identical_text() {
        let first = ClipboardSnapshot {
            text: "same text".to_owned(),
            changed_at_ms: Some(rxa_proto::unix_time_ms().saturating_add(1_000)),
            oversized_bytes: None,
        };
        let second = ClipboardSnapshot::changed("same text".to_owned(), Some(&first));
        assert_eq!(second.text, first.text);
        assert!(
            second.changed_at_ms > first.changed_at_ms,
            "activity identity comes from its timestamp, not only its text"
        );
    }

    // The cursor control message: base64 PNG plus geometry, and an explicit
    // null image for "the remote hid the pointer".
    #[test]
    fn cursor_control_message_carries_a_base64_png_or_null() {
        let shape = CursorShape::from_rgba(1, 1, 3, 4, &[255, 0, 0, 255]).unwrap();
        let expected = base64::engine::general_purpose::STANDARD.encode(&shape.png);
        match (ServerMsg::Cursor(Some(shape))).encode() {
            WireFrame::Text(json) => assert_eq!(
                json,
                format!(r#"{{"type":"cursor","image":"{expected}","w":1,"h":1,"hx":3,"hy":4}}"#)
            ),
            other => panic!("cursor should be a text frame: {other:?}"),
        }
        match (ServerMsg::Cursor(None)).encode() {
            WireFrame::Text(json) => assert_eq!(
                json,
                r#"{"type":"cursor","image":null,"w":0,"h":0,"hx":0,"hy":0}"#
            ),
            other => panic!("cursor should be a text frame: {other:?}"),
        }
    }

    #[test]
    fn cursor_with_wrong_payload_length_is_rejected() {
        assert!(CursorShape::from_rgba(2, 2, 0, 0, &[0u8; 12]).is_err());
    }

    // The binary layout `protocol.ts` (decodeTileFrame) parses.
    #[test]
    fn tile_frame_layout_is_kind_format_le_coords_payload() {
        let tile = Tile {
            format: Tile::FORMAT_PNG,
            x: 0x0102,
            y: 0x0304,
            w: 2,
            h: 1,
            data: vec![10, 20, 30, 40, 50, 60],
        };
        let frame = tile.to_frame();
        assert_eq!(frame[0], Tile::FRAME_KIND);
        assert_eq!(frame[1], Tile::FORMAT_PNG);
        assert_eq!(&frame[2..4], &[0x02, 0x01]); // x, little-endian
        assert_eq!(&frame[4..6], &[0x04, 0x03]); // y
        assert_eq!(&frame[6..8], &[2, 0]); // w
        assert_eq!(&frame[8..10], &[1, 0]); // h
        assert_eq!(&frame[10..], &[10, 20, 30, 40, 50, 60]);

        match (ServerMsg::Tile(tile)).encode() {
            WireFrame::Binary(bytes) => assert_eq!(bytes, frame),
            other => panic!("tile should be a binary frame: {other:?}"),
        }
    }

    // The pass-through path: the macOS agent's already-encoded bytes reach the
    // browser byte for byte, with the format byte it chose.
    #[test]
    fn encoded_tile_passes_the_payload_and_format_through_untouched() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        let tile = Tile::encoded(Tile::FORMAT_JPEG, 0x0102, 0x0304, 320, 64, jpeg.clone());
        let frame = tile.to_frame();
        assert_eq!(frame[0], Tile::FRAME_KIND);
        assert_eq!(frame[1], Tile::FORMAT_JPEG);
        assert_eq!(&frame[2..4], &[0x02, 0x01]); // x, little-endian
        assert_eq!(&frame[4..6], &[0x04, 0x03]); // y
        assert_eq!(&frame[6..8], &[0x40, 0x01]); // w = 320
        assert_eq!(&frame[8..10], &[64, 0]); // h
        assert_eq!(&frame[Tile::HEADER_LEN..], jpeg.as_slice());

        // A PNG the agent encoded itself takes the same path, differing only in
        // the format byte — the gateway looks inside neither.
        let png = vec![0x89, b'P', b'N', b'G'];
        let tile = Tile::encoded(Tile::FORMAT_PNG, 0, 0, 1, 1, png.clone());
        assert_eq!(tile.to_frame()[1], Tile::FORMAT_PNG);
        assert_eq!(&tile.to_frame()[Tile::HEADER_LEN..], png.as_slice());
    }

    // from_rgb still stamps PNG, so RDP and VNC are unaffected by the new field.
    #[test]
    fn from_rgb_still_marks_its_payload_as_png() {
        let tile = Tile::from_rgb(0, 0, 2, 2, &[0u8; 12]).unwrap();
        assert_eq!(tile.format, Tile::FORMAT_PNG);
        assert_eq!(tile.to_frame()[1], Tile::FORMAT_PNG);
    }

    /// A desktop-like strip: horizontal gradient, repeated rows.
    fn gradient_rgb(w: u16, h: u16) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for _ in 0..h {
            for x in 0..w {
                let v = (x % 256) as u8;
                rgb.extend_from_slice(&[v, v / 2, 255 - v]);
            }
        }
        rgb
    }

    #[test]
    fn screen_content_compresses_to_png_and_roundtrips() {
        let (w, h) = (320, 64);
        let rgb = gradient_rgb(w, h);
        let tile = Tile::from_rgb(7, 9, w, h, &rgb).unwrap();
        assert!(
            tile.data.len() < rgb.len() / 4,
            "PNG should compress a gradient well: {} vs raw {}",
            tile.data.len(),
            rgb.len()
        );

        // Decode the PNG back and verify the pixels survived.
        let decoder = png::Decoder::new(std::io::Cursor::new(tile.data.as_slice()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (u32::from(w), u32::from(h)));
        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert_eq!(&buf[..info.buffer_size()], rgb.as_slice());
    }

    // The binary tile frame's reason to exist: it must beat the old
    // base64-in-JSON baseline by a wide margin for screen-like content.
    #[test]
    fn tile_frame_beats_old_base64_json_baseline() {
        let (w, h) = (1280, 64);
        let rgb = gradient_rgb(w, h);
        let frame = Tile::from_rgb(0, 0, w, h, &rgb).unwrap().to_frame();
        // Old wire cost: RGBA (4 bytes/px) -> base64 (4/3) + ~90 bytes of JSON.
        let old = usize::from(w) * usize::from(h) * 4 * 4 / 3 + 90;
        assert!(
            frame.len() * 10 < old,
            "expected >10x reduction: {} vs baseline {old}",
            frame.len()
        );
    }

    #[test]
    fn tiny_tile_is_still_a_valid_png() {
        // 2x2 of "noise" — PNG's fixed overhead dominates here, which is
        // accepted: one decode path beats saving a few dozen bytes.
        let rgb = [1u8, 200, 3, 250, 5, 90, 7, 160, 9, 30, 11, 220];
        let tile = Tile::from_rgb(0, 0, 2, 2, &rgb).unwrap();
        assert_eq!(&tile.data[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn tile_with_wrong_payload_length_is_rejected() {
        assert!(Tile::from_rgb(0, 0, 2, 2, &[0u8; 5]).is_err());
    }
}
