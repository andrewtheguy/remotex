//! Client wire types: tagged JSON for control and input, binary batches for still
//! tiles and H.264 access units, and binary frames for Opus audio. WebSocket
//! ordering is required because resize messages change the coordinate space of
//! following tiles — and because an access unit means nothing out of sequence.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Transport policy shared by all engines: a dirty rectangle taller than this
/// is split into strips before being sent, so a full-screen repaint doesn't
/// produce one huge WebSocket message.
pub const STRIP_ROWS: u16 = 64;

/// Version of [`ClientMsg`], [`ControlMsg`], and the binary layouts. The native
/// viewer requires an exact match. Bump it when an older peer would otherwise
/// fail without a useful compatibility error; clients ignore additive control
/// tags they do not know.
pub const PROTOCOL_VERSION: u32 = 10;

/// Ceiling on one clipboard transfer, in bytes, in either direction.
///
/// Text over this is refused, not truncated: a truncated paste looks exactly
/// like a complete one, so neither end can tell the rest is missing until
/// something downstream is quietly wrong. A refusal is reported as one (the
/// panels name the size and the limit), so the surprise happens at the
/// clipboard, where it can be understood. Clipboard text rides the same link as
/// live frames, so an accidental 200 MB copy must not stall a session.
pub const MAX_CLIPBOARD_BYTES: usize = 65_536;

/// Whether `text` fits one clipboard transfer. `str::len` is already UTF-8
/// bytes, which is exactly what [`MAX_CLIPBOARD_BYTES`] bounds.
pub fn clipboard_fits(text: &str) -> bool {
    text.len() <= MAX_CLIPBOARD_BYTES
}

/// A display's backing scale as it travels on the wire: hundredths of a
/// captured pixel per point of the desktop being captured — 100 for a 1× panel,
/// 200 for a Retina one.
pub const SCALE_ONE: u16 = 100;

/// The largest scale worth believing. macOS has only ever shipped 1× and 2×
/// panels; this leaves room for one more doubling and rejects the rest.
const SCALE_MAX: u16 = 4 * SCALE_ONE;

/// A wire `scale` as the ratio clients divide the framebuffer by.
///
/// Anything outside `SCALE_ONE..=SCALE_MAX` — a zero from a source that could
/// not read the display's mode, a number no panel has — reads as 1×, which is
/// the answer that leaves the framebuffer alone. A scale below 1 is as wrong as
/// one above 4: it would blow the desktop up rather than shrink it.
pub fn scale_ratio(scale: u16) -> f32 {
    if (SCALE_ONE..=SCALE_MAX).contains(&scale) {
        f32::from(scale) / f32::from(SCALE_ONE)
    } else {
        1.0
    }
}

/// Wall-clock milliseconds for clipboard activity timestamps. Saturation only
/// matters after the year 584,554,051 or if the system clock predates Unix.
pub fn unix_time_ms() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis();
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

/// Timestamp a newly observed clipboard change without moving backwards.
///
/// Advancing by at least one millisecond distinguishes repeated activity even
/// when the text is identical or the wall clock has not advanced.
pub fn next_clipboard_time(previous: Option<u64>) -> u64 {
    let now = unix_time_ms();
    previous.map_or(now, |last| now.max(last.saturating_add(1)))
}

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
        next_clipboard_time(previous.and_then(|snapshot| snapshot.changed_at_ms))
    }
}

/// A mouse button, matching the DOM `MouseEvent.button` numbering.
///
/// `Back` and `Forward` are the side buttons of a five-button mouse. No engine
/// acts on them today — RDP and VNC both carry no equivalent and drop them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

/// What a wheel delta is measured in — the DOM's `deltaMode`, carried rather
/// than normalised because only the client knows whether its scroll came from a
/// trackpad (pixels) or a notched wheel (lines). Read only when VNC is talking
/// to an Apple subtype, the one server whose scroll step has been measured; RDP
/// and generic RFB spend any nonzero delta as a single notch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WheelUnit {
    Pixel,
    Line,
    Page,
}

/// Browser -> server: input events captured over the remote canvas.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMsg {
    /// Pointer moved to framebuffer coordinates (x, y).
    MouseMove { x: i32, y: i32 },
    /// A mouse button was pressed or released. `clicks` is the client's own click
    /// count for the press — `MouseEvent.detail` in the browser — which still
    /// rides the wire for any engine that has somewhere to put it, but neither
    /// current engine does: RDP and VNC carry button state alone and leave the
    /// guest to count.
    MouseButton {
        button: MouseButton,
        pressed: bool,
        clicks: u8,
    },
    /// Scroll wheel delta, in `unit`. An Apple VNC target is sent as many
    /// wheel-button pulses as the distance is worth there; every other target
    /// gets one notch per event and leaves the scaling to the guest.
    Wheel { dx: f32, dy: f32, unit: WheelUnit },
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
    /// Requested desktop size in remote pixels: available client points
    /// multiplied by the scale in [`ServerMsg::Resize`]. Applied by any engine the
    /// target opted into resize for, and dropped by every other. How often one
    /// arrives — per window change, or only when the user asks — is the client's
    /// own choice and nothing this end distinguishes.
    Viewport { w: u16, h: u16 },
    /// Restore the engine's configured or created default size. This carries no
    /// dimensions so a pinch-zoom client need not invent a desktop shape.
    DefaultSize,
    /// The density of the screen this client's window is on, in hundredths —
    /// 100 for a 1x screen, 200 for a Retina one. Sent on connect and again
    /// whenever the window moves to a screen of a different density.
    ///
    /// A request that the remote render at this density, which one engine can
    /// answer: RDP with `resize` asks the host for twice the pixels at 200% UI
    /// scaling. It quantizes to 1x or 2x at a midpoint and reports what it got
    /// back through [`ServerMsg::Resize`]. RDP without `resize` and VNC ignore
    /// it — clients send it unconditionally rather than asking what the engine
    /// is, so being ignored is not a client error.
    HostScale { scale: u16 },
    /// Re-announce the desktop size and repaint the whole framebuffer.
    /// Injected by the session layer when a client (re)attaches to a running
    /// engine. A client may also send it to recover a canvas that has gone
    /// wrong, which the viewer offers as Remote > Refresh; the SPA has no such
    /// command and never sends this.
    Refresh,
    /// Clear this attachment's tile-cache table and repaint. Unlike
    /// [`ClientMsg::Refresh`], this repairs disagreement about cache slots.
    CacheReset,
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
    /// Share the display identified by the last [`ServerMsg::Displays`].
    ///
    /// Acted on by the VNC engine's Apple dialect and by nothing else: RDP and plain
    /// VNC each deliver one framebuffer spanning every remote screen and have no
    /// message for this. A client that never receives [`ServerMsg::Displays`] never
    /// has an id to name here, which is how the panel stays hidden on those engines.
    SelectDisplay { id: u32 },
    /// Start or stop audio delivery for this attachment.
    Audio { enabled: bool },
}

/// The layout of a server -> client binary frame: a **batch** of records.
///
/// ```text
/// offset 0: u8  frame kind, always 0x02 (batch)
/// offset 1: u8  flags, always 0 — a receiver rejects anything else
/// offset 2: u16 record count
/// offset 4: records, back to back
///
/// record = u8 op | body   (little-endian throughout)
///
/// 0x01 TILE      u8 format | u16 slot | u16 x | u16 y | u16 w | u16 h | u32 len | payload[len]
/// 0x02 TILE_REF  u16 slot | u16 x | u16 y
/// 0x03 VIDEO     u8 stream | u16 x | u16 y | u16 w | u16 h | u32 len | payload[len]
/// ```
///
/// Receivers reject nonzero flags. The record count makes truncation detectable.
pub mod batch {
    pub const FRAME_KIND: u8 = 0x02;
    pub const HEADER_LEN: usize = 4;

    pub const OP_TILE: u8 = 0x01;
    pub const OP_TILE_REF: u8 = 0x02;
    pub const OP_VIDEO: u8 = 0x03;

    /// Bytes a `TILE` record costs besides its payload.
    pub const TILE_HEADER_LEN: usize = 16;
    /// A whole `TILE_REF` record.
    pub const TILE_REF_LEN: usize = 7;
    /// Bytes a `VIDEO` record costs besides its payload.
    pub const VIDEO_HEADER_LEN: usize = 14;

    /// The most H.264 streams one session may run at once, and so the range of a
    /// `VIDEO` record's `stream` byte.
    ///
    /// A client may size a decoder table by it. The gateway's own cap on concurrent
    /// regions is `crate::regions::MAX_STREAMS`, which is smaller; this is the wire's
    /// bound rather than the policy's.
    pub const MAX_STREAMS: u8 = 16;

    /// `slot` meaning "draw this and do not remember it".
    ///
    /// Needed so one enormous photographic tile cannot evict a screenful of
    /// useful small ones, and so a three-pixel caret rectangle need not consume a
    /// slot at all.
    pub const NO_SLOT: u16 = 0xFFFF;

    /// Number of encoded-payload cache slots in the wire contract.
    pub const SLOT_COUNT: u16 = 256;

    /// The largest payload worth a slot.
    ///
    /// A slot spent on one screen-sized photograph is a slot not spent on the
    /// dozens of small tiles a returning menu or a blinking caret is made of, and
    /// large payloads are the least likely to recur byte for byte anyway.
    pub const MAX_CACHED_BYTES: usize = 32 * 1024;
}

/// The layout of a server -> client **audio** frame: one wave buffer's worth of
/// Opus packets.
///
/// ```text
/// offset 0: u8  frame kind, always 0x03 (audio)
/// offset 1: u8  flags, always 0 — a receiver rejects anything else
/// offset 2: u16 packet count
/// offset 4: packets, each u16 length | length bytes  (little-endian throughout)
/// ```
///
/// Receivers reject nonzero flags. Packet lengths delimit multiple packets within
/// one WebSocket frame — Opus packets, or one wave buffer's worth of PCM.
pub mod audio {
    pub const FRAME_KIND: u8 = 0x03;
    pub const HEADER_LEN: usize = 4;
    /// Bytes each packet costs besides its own bytes.
    pub const PACKET_HEADER_LEN: usize = 2;

    /// Serialize `packets` into one audio frame.
    ///
    /// Both `u16` fields are checked rather than truncated, and each panic names the
    /// invariant it belongs to, because silently wrapping either would produce a frame
    /// a client parses successfully and wrongly. Neither is reachable: both streams
    /// cap a packet below 65 535 bytes — `opus_stream`'s `MAX_PACKET_BYTES` at 4000,
    /// and `pcm_stream`'s at the length field itself, which is the one place a
    /// *remote's* buffer size could otherwise reach this — and a wave buffer holding
    /// 65 535 packets of 20 ms would be twenty minutes of audio in one buffer.
    pub fn frame(packets: &[Vec<u8>]) -> Vec<u8> {
        let len: usize = packets.iter().map(|p| PACKET_HEADER_LEN + p.len()).sum();
        let mut frame = Vec::with_capacity(HEADER_LEN + len);
        frame.push(FRAME_KIND);
        frame.push(0); // flags
        let count = u16::try_from(packets.len())
            .expect("an audio frame carries at most u16::MAX packets");
        frame.extend_from_slice(&count.to_le_bytes());
        for packet in packets {
            let size = u16::try_from(packet.len())
                .expect("an opus or pcm packet is at most u16::MAX bytes");
            frame.extend_from_slice(&size.to_le_bytes());
            frame.extend_from_slice(packet);
        }
        frame
    }
}

/// Canonical 320×64 tile grid in framebuffer pixels, anchored at (0,0).
///
/// Damage is still reported by RDP and VNC in their own rectangles and is still
/// *sent* in those rectangles — nothing snaps outward to the grid, which would
/// mean shipping pixels that did not change. What the grid gives is a stable
/// **identity**: [`crate::tiles::Rect::cells`] splits a rectangle at these lines
/// so the same region of the screen always lands under the same
/// [`crate::tiles::Rect::cell_key`], however differently the two protocols happen
/// to describe it from one frame to the next. That identity is what the render
/// dial's `motion` type counts churn against.
pub const CELL_W: u16 = 320;
/// See [`CELL_W`].
pub const CELL_H: u16 = STRIP_ROWS;

/// A dirty rectangle of the framebuffer, carried as one `TILE` record inside a
/// [`batch`] frame. The payload is an image stream the client decodes natively —
/// PNG or JPEG, named by the `format` byte so `createImageBitmap` gets the right
/// MIME type.
///
/// The RDP and VNC engines decode a framebuffer and compress it here: lossless
/// PNG ([`Tile::from_rgb`], the default) or, for a target on the fixed-quality
/// dial, JPEG ([`Tile::from_rgb_jpeg`]) or WebP ([`Tile::from_rgb_webp`]). A
/// pass-through path also exists for a source that hands over frames already
/// encoded ([`Tile::encoded`]), which no current engine uses; either way the
/// format travels with the tile instead of being a constant.
#[derive(Debug, Clone)]
pub struct Tile {
    /// Payload codec: [`Tile::FORMAT_PNG`], [`Tile::FORMAT_JPEG`] or
    /// [`Tile::FORMAT_WEBP`]. Every one of them is a self-contained picture; a frame
    /// that only means something in sequence is a [`VideoUnit`] and not a tile at all.
    pub format: u8,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    /// The encoded image stream, in `format`.
    pub data: Vec<u8>,
}

impl Tile {
    pub const FORMAT_PNG: u8 = 1;
    pub const FORMAT_JPEG: u8 = 2;
    pub const FORMAT_WEBP: u8 = 3;

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

    /// Build a tile from packed RGB888 pixels, JPEG-compressing the payload at a
    /// fixed `quality` (1–100). The lossy counterpart to [`Tile::from_rgb`], taken
    /// by an engine only when its target set `render_type = "fixed-quality"`,
    /// `render_subtype = "jpeg"` (see [`crate::config::RenderType`]); the format
    /// byte carries the choice, so no client is told anything new.
    ///
    /// Every tile goes to JPEG here — there is no content classifier — so flat UI
    /// and text soften along with everything else. That is the documented trade of
    /// the fixed dial; a classifying subtype is a separate, future render subtype.
    pub fn from_rgb_jpeg(x: u16, y: u16, w: u16, h: u16, rgb: &[u8], quality: u8) -> anyhow::Result<Self> {
        let expected = usize::from(w) * usize::from(h) * 3;
        anyhow::ensure!(
            rgb.len() == expected,
            "tile payload is {} bytes, expected {expected} for {w}x{h} RGB",
            rgb.len()
        );
        let data = encode_jpeg(w, h, rgb, quality)?;
        Ok(Self {
            format: Self::FORMAT_JPEG,
            x,
            y,
            w,
            h,
            data,
        })
    }

    /// Build a tile from packed RGB888 pixels, WebP-compressing the payload at a
    /// fixed `quality` (1–100). The other lossy counterpart to [`Tile::from_rgb`],
    /// taken when a target set `render_subtype = "webp"`: typically ~30% fewer
    /// bytes than [`Tile::from_rgb_jpeg`] at a matched quality. Both clients decode
    /// WebP natively, so the only difference on the wire is the format byte.
    ///
    /// Like the JPEG path there is no classifier — every tile goes to WebP, so flat
    /// UI and text soften too.
    pub fn from_rgb_webp(x: u16, y: u16, w: u16, h: u16, rgb: &[u8], quality: u8) -> anyhow::Result<Self> {
        let expected = usize::from(w) * usize::from(h) * 3;
        anyhow::ensure!(
            rgb.len() == expected,
            "tile payload is {} bytes, expected {expected} for {w}x{h} RGB",
            rgb.len()
        );
        let data = encode_webp(w, h, rgb, quality)?;
        Ok(Self {
            format: Self::FORMAT_WEBP,
            x,
            y,
            w,
            h,
            data,
        })
    }

    /// Wrap an already-encoded payload, for a caller that did the encoding itself.
    ///
    /// The pass-through for a source that hands over frames already encoded, so the
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

    /// What this tile will cost inside a batch, payload included.
    pub fn record_len(&self) -> usize {
        batch::TILE_HEADER_LEN + self.data.len()
    }

    /// Append this tile as a `TILE` record. `slot` is where the client should
    /// remember it, or [`batch::NO_SLOT`] not to.
    ///
    /// Appends rather than returning a buffer because a batch is built by writing
    /// records one after another into one allocation.
    pub fn write_record(&self, slot: u16, out: &mut Vec<u8>) {
        out.push(batch::OP_TILE);
        out.push(self.format);
        out.extend_from_slice(&slot.to_le_bytes());
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.w.to_le_bytes());
        out.extend_from_slice(&self.h.to_le_bytes());
        // u32, not u16: a full-width Retina strip has been measured at ~192 KB,
        // and a length field that cannot describe the payload is not a saving.
        out.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.data);
    }
}

/// Append a `TILE_REF` record: redraw whatever the client has in `slot` at
/// `(x, y)`. Seven bytes in place of a payload.
pub fn write_tile_ref(slot: u16, x: u16, y: u16, out: &mut Vec<u8>) {
    out.push(batch::OP_TILE_REF);
    out.extend_from_slice(&slot.to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
}

/// One H.264 access unit for one region of the framebuffer, carried as a `VIDEO`
/// record inside a [`batch`] frame.
///
/// A record of its own rather than a fourth [`Tile`] format, because it is not the
/// same kind of thing. A tile is a self-contained picture: independent, reorderable,
/// cacheable, and droppable once something covers it. This is one link in a chain,
/// where losing any link decodes wrongly until the next keyframe. Making it a
/// different record is what keeps [`crate::wire`]'s cache and coverage rules from
/// ever having to ask whether they apply.
///
/// The contract every client implements:
///
/// - The payload is **one whole access unit** — every NAL unit of exactly one frame,
///   Annex-B, delimited by start codes. Never a partial one, never two.
/// - SPS and PPS accompany every keyframe, so a client can build its decoder from any
///   keyframe it sees and needs nothing out of band. A stream begins with one, and a
///   repaint, a resize, a client coming back or a region that grew produces another.
/// - `stream` names which decoder this belongs to. A session may run several at once
///   — one per moving region under `render_motion_subtype = "h264"`, exactly one
///   under `render_type = "video"` — and ids are reused as regions come and go, so a
///   record whose `(w, h)` differs from the last one on the same id means that
///   decoder is starting over on a differently sized picture.
/// - `(x, y, w, h)` is the **true region rectangle**, in framebuffer pixels. The
///   decoded picture may be one pixel wider and/or taller, because H.264 needs even
///   sides and a region at the edge of an odd desktop does not have them: a client
///   draws the top-left `w`×`h` of what it decodes, at `(x, y)`, and ignores the rest.
/// - Every access unit matters and their order matters — including their order
///   against the tiles around them, since a still tile covering the same pixels is
///   how a settled region is restored to full quality.
#[derive(Debug, Clone)]
pub struct VideoUnit {
    /// Which of this session's streams, `0..batch::MAX_STREAMS`.
    pub stream: u8,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    /// Whether a decoder that has seen nothing before this can start here. Not on the
    /// wire: a client reads it out of the bitstream, and the gateway keeps it for the
    /// totals, where keyframe bytes against total bytes is the whole measurement of
    /// whether a stream is winning.
    pub keyframe: bool,
    /// The Annex-B access unit.
    pub data: Vec<u8>,
}

impl VideoUnit {
    /// What this unit will cost inside a batch, payload included.
    pub fn record_len(&self) -> usize {
        batch::VIDEO_HEADER_LEN + self.data.len()
    }

    /// Append this unit as a `VIDEO` record.
    pub fn write_record(&self, out: &mut Vec<u8>) {
        out.push(batch::OP_VIDEO);
        out.push(self.stream);
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.w.to_le_bytes());
        out.extend_from_slice(&self.h.to_le_bytes());
        // u32 for the same reason a tile's length is: a keyframe of a 4K desktop runs
        // to hundreds of kilobytes, and a length field that cannot describe the
        // payload is not a saving.
        out.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.data);
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
    ///
    /// PNG rather than the tile codec because [`CursorShape::png`] rides the JSON
    /// control channel as base64, and the macOS agent's own shapes arrive PNG from
    /// AppKit and are relayed unmodified.
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
///
/// Shared by [`Tile::from_rgb`] (RGB screen tiles) and [`CursorShape::from_rgba`]
/// (RGBA cursor shapes), so a PNG tile from the gateway is byte-for-byte the same
/// shape a decoder sees from the agent's own PNG branch.
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

/// JPEG-encode packed RGB888 at a fixed `quality` (1–100). The lossy tile path
/// ([`Tile::from_rgb_jpeg`]); JPEG embeds its own quantization tables, so the
/// quality rides no wire and the decoder needs no telling. Salvaged from the
/// deleted agent's encoder (commit 8990971).
fn encode_jpeg(w: u16, h: u16, rgb: &[u8], quality: u8) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder
        .encode(rgb, w, h, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| anyhow::anyhow!("JPEG encode failed: {e}"))?;
    Ok(out)
}

/// WebP-encode packed RGB888 at a fixed `quality` (1–100). The lossy path for the
/// `webp` subtype ([`Tile::from_rgb_webp`]).
///
/// All the CPU the encode can use: `libwebp` is compiled with the target's SIMD
/// (SSE/AVX on x86, NEON on Apple silicon) by the `cc` build, and `thread_level`
/// lets one encode fan out across cores on top of the tile-level parallelism
/// `TileSink` already has. `method` stays at libwebp's default 4 — the
/// speed/size balance — so a hot-path encode does not stall on a slower search.
fn encode_webp(w: u16, h: u16, rgb: &[u8], quality: u8) -> anyhow::Result<Vec<u8>> {
    let mut config = webp::WebPConfig::new().map_err(|()| anyhow::anyhow!("WebP config init failed"))?;
    config.quality = f32::from(quality);
    config.method = 4;
    config.thread_level = 1;
    let encoder = webp::Encoder::from_rgb(rgb, u32::from(w), u32::from(h));
    let mem = encoder
        .encode_advanced(&config)
        .map_err(|e| anyhow::anyhow!("WebP encode failed: {e:?}"))?;
    Ok(mem.to_vec())
}

/// The `scale` on [`ServerMsg::Resize`] for a framebuffer whose pixels *are* the
/// points of the desktop it shows: VNC always, and RDP until a client asks the
/// remote for a density and the remote agrees.
///
/// A remote with no density of its own is presented one point per pixel, and a
/// client must not second-guess that by drawing the whole desktop at half size
/// because the host screen happens to be Retina. What makes a scale of 2.0 honest
/// instead is that the remote was *told*: RDP declares a density to the host, so
/// the extra pixels carry a UI drawn twice as large rather than the same UI
/// stretched.
pub const UNSCALED: f32 = 1.0;

/// One of the remote's displays, as a client lists it for the user to pick from.
///
/// The strings are built by the remote end and passed through: the Mac knows how
/// its displays are named and numbered, and having it say so once keeps the
/// browser panel and the viewer's menu reading the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    /// Opaque to every client — whatever the engine wants back in
    /// [`ClientMsg::SelectDisplay`]. On the Apple dialect it is a
    /// `CGDirectDisplayID`, except for `0xffffffff`, which the engine uses for its
    /// own "All Displays" entry (and which is Apple's own sentinel for that).
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
    /// One H.264 access unit for one region. Like a tile this has no text encoding
    /// and is not a control message: it is a binary record, and [`crate::wire`] is
    /// what puts it in a batch — in its place among the tiles, which is load-bearing.
    Video(VideoUnit),
    /// The remote desktop resolution changed. `w`/`h` are framebuffer pixels;
    /// `scale` is how many of them the remote draws per point of its *own*
    /// desktop — 1.0 for a framebuffer whose pixels are its points (VNC, a 1x
    /// Mac, an RDP host rendering at 100%), 2.0 for a Retina Mac or an RDP host
    /// that accepted a 200% request.
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
    /// A live target and its client-visible capabilities. `audio` reports
    /// capability, not whether sound is arriving.
    ///
    /// `resize` and `auto_resize` are two permissions, not one with a shortcut:
    /// the first is whether a client may resize the remote when the user asks, the
    /// second whether it may hand the size to its window and let every drag report.
    /// Only plain `vnc` gets the second — see [`crate::config::TargetConfig::auto_resize`].
    /// `auto_resize` is required rather than defaulted, which is what moved
    /// [`PROTOCOL_VERSION`] to 9: version 8 shipped this message without it, so a
    /// gateway that omits it must be refused by version rather than decoded into a
    /// target that silently cannot follow the window.
    Connected {
        name: String,
        protocol: &'static str,
        resize: bool,
        auto_resize: bool,
        clipboard: bool,
        audio: bool,
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
    /// How to play what follows, sent before the first packet.
    ///
    /// `codec` is `opus` — a WebCodecs codec string, with the RFC 7845 `OpusHead`
    /// in `head` and `sample_rate` the 48 kHz it was resampled to — or
    /// `pcm-s16le`, which is not a WebCodecs codec at all: `head` is then empty,
    /// `sample_rate` is the remote's own, and the packets are interleaved signed
    /// 16-bit little-endian samples for the client to play directly.
    ///
    /// `packet_frames` is the one thing a client cannot work out for itself: 960
    /// on Opus, and 0 on passthrough, whose packets are whatever length the
    /// remote's buffers were and so carry their own.
    AudioFormat {
        codec: &'static str,
        sample_rate: u32,
        channels: u16,
        packet_frames: u32,
        head: Vec<u8>,
    },
    /// One wave buffer's worth of audio packets, framed by [`audio::frame`].
    ///
    /// Like a tile, this has no text encoding and is not a control message: it is a
    /// binary frame, and [`crate::wire`] is what turns it into one.
    Audio(Vec<Vec<u8>>),
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
        // `rename_all` on this enum renames the variants, not their fields, so
        // every camelCase key on the wire is spelled here — see `changedAtMs`.
        #[serde(rename = "autoResize")]
        auto_resize: bool,
        clipboard: bool,
        audio: bool,
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
    AudioFormat {
        codec: &'a str,
        #[serde(rename = "sampleRate")]
        sample_rate: u32,
        channels: u16,
        #[serde(rename = "packetFrames")]
        packet_frames: u32,
        /// base64 decoder configuration, for the same reason `cursor`'s PNG is
        /// base64: a text frame cannot carry bytes, and this is tens of them
        /// once a session.
        head: String,
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
    /// The JSON text frame for a control message, or `None` for a tile.
    ///
    /// `None` rather than a panic or a placeholder because a tile genuinely has no
    /// standalone encoding any more: it only exists as a record inside a batch,
    /// and only [`crate::wire`] knows which slot to give it. Making that a
    /// type-level fact is what stops a future caller sending one on its own.
    pub fn text_frame(&self) -> Option<String> {
        Some(match self {
            ServerMsg::Tile(_) | ServerMsg::Video(_) | ServerMsg::Audio(_) => return None,
            ServerMsg::Resize { w, h, scale } => control(&ControlMsg::Resize {
                w: *w,
                h: *h,
                scale: *scale,
            }),
            ServerMsg::Cursor(shape) => control(&match shape {
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
            }),
            ServerMsg::Error { message } => control(&ControlMsg::Error { message }),
            ServerMsg::Picker => control(&ControlMsg::Picker),
            ServerMsg::Connected {
                name,
                protocol,
                resize,
                auto_resize,
                clipboard,
                audio,
            } => control(&ControlMsg::Connected {
                name,
                protocol,
                resize: *resize,
                auto_resize: *auto_resize,
                clipboard: *clipboard,
                audio: *audio,
            }),
            ServerMsg::RemoteOs { macos } => control(&ControlMsg::RemoteOs { macos: *macos }),
            ServerMsg::AudioFormat {
                codec,
                sample_rate,
                channels,
                packet_frames,
                head,
            } => control(&ControlMsg::AudioFormat {
                codec,
                sample_rate: *sample_rate,
                channels: *channels,
                packet_frames: *packet_frames,
                head: base64::engine::general_purpose::STANDARD.encode(head),
            }),
            ServerMsg::Displays { active, displays } => control(&ControlMsg::Displays {
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
            }),
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
                control(&ControlMsg::Clipboard {
                    text: if refused.is_some() { "" } else { text },
                    changed_at_ms: *changed_at_ms,
                    requested: *requested,
                    oversized_bytes: oversized_bytes.or(refused),
                })
            }
        })
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
                r#"{"type":"mouseButton","button":"right","pressed":true,"clicks":2}"#
            )
            .unwrap(),
            ClientMsg::MouseButton {
                button: MouseButton::Right,
                pressed: true,
                clicks: 2
            }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(
                r#"{"type":"wheel","dx":0.0,"dy":-2.5,"unit":"pixel"}"#
            )
            .unwrap(),
            ClientMsg::Wheel { dy, unit, .. } if dy == -2.5 && unit == WheelUnit::Pixel
        ));
        // The side buttons a five-button mouse has, which no engine acts on
        // today.
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(
                r#"{"type":"mouseButton","button":"forward","pressed":true,"clicks":1}"#
            )
            .unwrap(),
            ClientMsg::MouseButton {
                button: MouseButton::Forward,
                ..
            }
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
        // The sizeless request beside it, which carries no dimensions at all:
        // what "default" means is the far side's to say.
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"defaultSize"}"#).unwrap(),
            ClientMsg::DefaultSize
        ));
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
        // The audio subscription, which is also each client's enable/disable control.
        // Both directions matter: a client that never sends it hears nothing, and one
        // that never sends it is why [`PROTOCOL_VERSION`] did not have to move.
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"audio","enabled":true}"#).unwrap(),
            ClientMsg::Audio { enabled: true }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"audio","enabled":false}"#).unwrap(),
            ClientMsg::Audio { enabled: false }
        ));
        // No default: "audio" with nothing said about it would otherwise mean
        // whichever of on and off serde picked.
        assert!(serde_json::from_str::<ClientMsg>(r#"{"type":"audio"}"#).is_err());
    }

    /// The audio pair, which is one text frame and one binary frame — and the split
    /// is the point: a decoder is configured by the first and fed by the second, so
    /// a client that received them in the other order would decode nothing.
    #[test]
    fn the_audio_format_is_text_and_the_packets_are_not() {
        let head = crate::opus_stream::opus_head(crate::audio::PCM_CD_QUALITY, 312);
        let json = (ServerMsg::AudioFormat {
            codec: "opus",
            sample_rate: 48_000,
            channels: 2,
            packet_frames: 960,
            head: head.clone(),
        })
        .text_frame()
        .expect("the format must be a text frame");
        assert_eq!(
            json,
            r#"{"type":"audioFormat","codec":"opus","sampleRate":48000,"channels":2,"packetFrames":960,"head":"T3B1c0hlYWQBAjgBRKwAAAAAAA=="}"#
        );
        // And the base64 is really OpusHead, not a placeholder that happens to
        // decode: a client configures a decoder from these bytes.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode("T3B1c0hlYWQBAjgBRKwAAAAAAA==")
            .expect("valid base64");
        assert_eq!(decoded, head);
        assert_eq!(&decoded[0..8], b"OpusHead");

        assert!(
            ServerMsg::Audio(vec![vec![1, 2, 3]]).text_frame().is_none(),
            "packets are a binary frame, like a tile"
        );
    }

    /// The same message for passthrough, pinned separately because every field but
    /// the shape of it differs: a codec string WebCodecs does not know, the
    /// remote's own rate rather than 48 kHz, no decoder configuration, and no
    /// packet length. Two clients parse this, and neither may assume Opus.
    ///
    /// The empty `head` is the load-bearing one. It is what a client reads as
    /// "there is nothing to configure" — and an encoder is the only thing that
    /// would ever put bytes there, so a non-empty one here would mean the
    /// passthrough path had grown one.
    #[test]
    fn the_audio_format_describes_passthrough_without_a_decoder() {
        let (_stream, head) =
            crate::pcm_stream::PcmStream::new(crate::audio::PCM_CD_QUALITY).expect("a stream");
        assert!(head.is_empty());
        let json = (ServerMsg::AudioFormat {
            codec: crate::pcm_stream::PCM_CODEC,
            sample_rate: crate::audio::PCM_CD_QUALITY.sample_rate,
            channels: 2,
            packet_frames: 0,
            head,
        })
        .text_frame()
        .expect("the format must be a text frame");
        assert_eq!(
            json,
            r#"{"type":"audioFormat","codec":"pcm-s16le","sampleRate":44100,"channels":2,"packetFrames":0,"head":""}"#
        );
    }

    // Control messages keep the tagged, camelCase text shape `protocol.ts` expects.
    #[test]
    fn control_messages_encode_to_tagged_camelcase_text() {
        match (ServerMsg::Resize { w: 1280, h: 800, scale: UNSCALED }).text_frame() {
            Some(json) => {
                assert_eq!(json, r#"{"type":"resize","w":1280,"h":800,"scale":1.0}"#)
            }
            None => panic!("resize must be a text frame"),
        }
        match (ServerMsg::Error { message: "boom".to_owned() }).text_frame() {
            Some(json) => assert_eq!(json, r#"{"type":"error","message":"boom"}"#),
            None => panic!("error must be a text frame"),
        }
        match (ServerMsg::Connected {
            name: "mac".to_owned(),
            protocol: "vnc",
            resize: false,
            auto_resize: false,
            clipboard: true,
            audio: false,
        })
        .text_frame()
        {
            Some(json) => assert_eq!(
                json,
                r#"{"type":"connected","name":"mac","protocol":"vnc","resize":false,"autoResize":false,"clipboard":true,"audio":false}"#
            ),
            None => panic!("connected must be a text frame"),
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
        .text_frame()
        {
            // `virtual` on the wire: reserved in Rust, ordinary in JavaScript.
            Some(json) => assert_eq!(
                json,
                r#"{"type":"displays","active":7,"displays":[{"id":7,"label":"Display 1","detail":"1920×1080 at 1x","main":true,"virtual":false},{"id":9,"label":"Virtual display","detail":"3200×2000 at 2x","main":false,"virtual":true}]}"#
            ),
            None => panic!("displays must be a text frame"),
        }
        // No displays is a shape a client must handle, not one it never sees: a
        // Mac can have every screen unplugged.
        match (ServerMsg::Displays {
            active: 0,
            displays: Vec::new(),
        })
        .text_frame()
        {
            Some(json) => {
                assert_eq!(json, r#"{"type":"displays","active":0,"displays":[]}"#)
            }
            None => panic!("displays must be a text frame"),
        }
        for macos in [false, true] {
            match (ServerMsg::RemoteOs { macos }).text_frame() {
                Some(json) => assert_eq!(
                    json,
                    format!(r#"{{"type":"remoteOs","macos":{macos}}}"#)
                ),
                None => panic!("remoteOs must be a text frame"),
            }
        }
        match (ServerMsg::Clipboard {
            text: "hi \"there\"".to_owned(),
            changed_at_ms: Some(1_721_234_567_890),
            requested: false,
            oversized_bytes: None,
        })
        .text_frame()
        {
            Some(json) => {
                assert_eq!(
                    json,
                    r#"{"type":"clipboard","text":"hi \"there\"","changedAtMs":1721234567890,"requested":false,"oversizedBytes":null}"#
                );
            }
            None => panic!("clipboard must be a text frame"),
        }
        match (ServerMsg::Clipboard {
            text: String::new(),
            changed_at_ms: None,
            requested: true,
            oversized_bytes: None,
        })
        .text_frame()
        {
            Some(json) => {
                assert_eq!(
                    json,
                    r#"{"type":"clipboard","text":"","changedAtMs":null,"requested":true,"oversizedBytes":null}"#
                );
            }
            None => panic!("clipboard must be a text frame"),
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
        .text_frame()
        {
            Some(json) => assert_eq!(
                json,
                format!(
                    r#"{{"type":"clipboard","text":"","changedAtMs":42,"requested":true,"oversizedBytes":{oversized}}}"#
                )
            ),
            None => panic!("clipboard must be a text frame"),
        }

        // An engine that already refused it says so itself, and that size is
        // kept rather than recomputed from the empty text it sent.
        match (ServerMsg::Clipboard {
            text: String::new(),
            changed_at_ms: Some(42),
            requested: false,
            oversized_bytes: Some(209_715_200),
        })
        .text_frame()
        {
            Some(json) => assert_eq!(
                json,
                r#"{"type":"clipboard","text":"","changedAtMs":42,"requested":false,"oversizedBytes":209715200}"#
            ),
            None => panic!("clipboard must be a text frame"),
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
            changed_at_ms: Some(unix_time_ms().saturating_add(1_000)),
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
        match (ServerMsg::Cursor(Some(shape))).text_frame() {
            Some(json) => assert_eq!(
                json,
                format!(r#"{{"type":"cursor","image":"{expected}","w":1,"h":1,"hx":3,"hy":4}}"#)
            ),
            None => panic!("cursor must be a text frame"),
        }
        match (ServerMsg::Cursor(None)).text_frame() {
            Some(json) => assert_eq!(
                json,
                r#"{"type":"cursor","image":null,"w":0,"h":0,"hx":0,"hy":0}"#
            ),
            None => panic!("cursor must be a text frame"),
        }
    }

    #[test]
    fn cursor_with_wrong_payload_length_is_rejected() {
        assert!(CursorShape::from_rgba(2, 2, 0, 0, &[0u8; 12]).is_err());
    }

    // A tile has no standalone frame any more, only a record inside a batch. The
    // type says so, which is what keeps a caller from sending one on its own.
    #[test]
    fn a_tile_has_no_text_encoding() {
        let tile = Tile::from_rgb(0, 0, 1, 1, &[0, 0, 0]).unwrap();
        assert!((ServerMsg::Tile(tile)).text_frame().is_none());
    }

    // The record layout `protocol.ts` (decodeBatchFrame) and `BatchFrame.swift`
    // parse.
    #[test]
    fn tile_record_layout_is_op_format_slot_le_coords_len_payload() {
        let tile = Tile {
            format: Tile::FORMAT_PNG,
            x: 0x0102,
            y: 0x0304,
            w: 2,
            h: 1,
            data: vec![10, 20, 30, 40, 50, 60],
        };
        let mut out = Vec::new();
        tile.write_record(batch::NO_SLOT, &mut out);
        assert_eq!(out[0], batch::OP_TILE);
        assert_eq!(out[1], Tile::FORMAT_PNG);
        assert_eq!(&out[2..4], &[0xFF, 0xFF]); // slot: NO_SLOT
        assert_eq!(&out[4..6], &[0x02, 0x01]); // x, little-endian
        assert_eq!(&out[6..8], &[0x04, 0x03]); // y
        assert_eq!(&out[8..10], &[2, 0]); // w
        assert_eq!(&out[10..12], &[1, 0]); // h
        assert_eq!(&out[12..16], &[6, 0, 0, 0]); // payload length, u32
        assert_eq!(&out[16..], &[10, 20, 30, 40, 50, 60]);
        assert_eq!(out.len(), tile.record_len());
        assert_eq!(batch::TILE_HEADER_LEN, 16);

        // A real slot only changes those two bytes.
        let mut out = Vec::new();
        tile.write_record(9, &mut out);
        assert_eq!(&out[2..4], &[9, 0]);
    }

    #[test]
    fn tile_ref_record_is_seven_bytes_of_slot_and_position() {
        let mut out = Vec::new();
        write_tile_ref(0x0102, 0x0304, 0x0506, &mut out);
        assert_eq!(out[0], batch::OP_TILE_REF);
        assert_eq!(&out[1..3], &[0x02, 0x01]); // slot
        assert_eq!(&out[3..5], &[0x04, 0x03]); // x
        assert_eq!(&out[5..7], &[0x06, 0x05]); // y
        assert_eq!(out.len(), batch::TILE_REF_LEN);
    }

    // The pass-through path: the macOS agent's already-encoded bytes reach the
    // browser byte for byte, with the format byte it chose.
    #[test]
    fn encoded_tile_passes_the_payload_and_format_through_untouched() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        let tile = Tile::encoded(Tile::FORMAT_JPEG, 0x0102, 0x0304, 320, 64, jpeg.clone());
        let mut out = Vec::new();
        tile.write_record(batch::NO_SLOT, &mut out);
        assert_eq!(out[0], batch::OP_TILE);
        assert_eq!(out[1], Tile::FORMAT_JPEG);
        assert_eq!(&out[4..6], &[0x02, 0x01]); // x, little-endian
        assert_eq!(&out[6..8], &[0x04, 0x03]); // y
        assert_eq!(&out[8..10], &[0x40, 0x01]); // w = 320
        assert_eq!(&out[10..12], &[64, 0]); // h
        assert_eq!(&out[batch::TILE_HEADER_LEN..], jpeg.as_slice());

        // A PNG the agent encoded itself takes the same path, differing only in
        // the format byte — the gateway looks inside neither.
        let png = vec![0x89, b'P', b'N', b'G'];
        let tile = Tile::encoded(Tile::FORMAT_PNG, 0, 0, 1, 1, png.clone());
        let mut out = Vec::new();
        tile.write_record(batch::NO_SLOT, &mut out);
        assert_eq!(out[1], Tile::FORMAT_PNG);
        assert_eq!(&out[batch::TILE_HEADER_LEN..], png.as_slice());
    }

    // from_rgb still stamps PNG, so RDP and VNC are unaffected by the new field.
    #[test]
    fn from_rgb_still_marks_its_payload_as_png() {
        let tile = Tile::from_rgb(0, 0, 2, 2, &[0u8; 12]).unwrap();
        assert_eq!(tile.format, Tile::FORMAT_PNG);
        let mut out = Vec::new();
        tile.write_record(batch::NO_SLOT, &mut out);
        assert_eq!(out[1], Tile::FORMAT_PNG);
    }

    // The lossy path stamps JPEG and produces a real JFIF stream (SOI marker).
    #[test]
    fn from_rgb_jpeg_marks_its_payload_as_jpeg() {
        let (w, h) = (16, 16);
        let tile = Tile::from_rgb_jpeg(0, 0, w, h, &vec![0u8; usize::from(w) * usize::from(h) * 3], 60).unwrap();
        assert_eq!(tile.format, Tile::FORMAT_JPEG);
        assert_eq!(&tile.data[..2], &[0xFF, 0xD8], "JPEG start-of-image marker");
        let mut out = Vec::new();
        tile.write_record(batch::NO_SLOT, &mut out);
        assert_eq!(out[1], Tile::FORMAT_JPEG);
    }

    /// A high-entropy, photographic-like strip: PNG cannot compress it, which is
    /// exactly where a lossy codec earns its keep (a smooth gradient is the
    /// opposite case — PNG wins it, so it is no test of the JPEG path).
    fn noisy_rgb(w: u16, h: u16) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for y in 0..u32::from(h) {
            for x in 0..u32::from(w) {
                let n = x
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(y.wrapping_mul(40_503))
                    .rotate_left(13);
                rgb.extend_from_slice(&[n as u8, (n >> 8) as u8, (n >> 16) as u8]);
            }
        }
        rgb
    }

    // The whole point of the dial: a photographic strip is far smaller as JPEG
    // than as lossless PNG, and a lower quality is smaller still.
    #[test]
    fn jpeg_is_smaller_than_png_on_photographic_content() {
        let (w, h) = (320, 64);
        let rgb = noisy_rgb(w, h);
        let png = Tile::from_rgb(0, 0, w, h, &rgb).unwrap();
        let jpeg = Tile::from_rgb_jpeg(0, 0, w, h, &rgb, 60).unwrap();
        assert!(
            jpeg.data.len() < png.data.len(),
            "JPEG should beat PNG on a gradient: {} vs {}",
            jpeg.data.len(),
            png.data.len()
        );
        let lower = Tile::from_rgb_jpeg(0, 0, w, h, &rgb, 20).unwrap();
        assert!(
            lower.data.len() < jpeg.data.len(),
            "lower quality should be smaller: {} vs {}",
            lower.data.len(),
            jpeg.data.len()
        );
    }

    // A payload whose length disagrees with its geometry is rejected, same as the
    // PNG constructor.
    #[test]
    fn from_rgb_jpeg_rejects_a_mismatched_payload() {
        assert!(Tile::from_rgb_jpeg(0, 0, 2, 2, &[0u8; 11], 60).is_err());
    }

    // The WebP path stamps WebP and produces a real RIFF/WEBP container.
    #[test]
    fn from_rgb_webp_marks_its_payload_as_webp() {
        let (w, h) = (16, 16);
        let tile = Tile::from_rgb_webp(0, 0, w, h, &vec![0u8; usize::from(w) * usize::from(h) * 3], 60).unwrap();
        assert_eq!(tile.format, Tile::FORMAT_WEBP);
        assert_eq!(&tile.data[..4], b"RIFF", "RIFF container");
        assert_eq!(&tile.data[8..12], b"WEBP", "WebP form type");
        let mut out = Vec::new();
        tile.write_record(batch::NO_SLOT, &mut out);
        assert_eq!(out[1], Tile::FORMAT_WEBP);
    }

    // The reason to offer WebP at all: on photographic content it beats both PNG
    // (lossless) and JPEG at a matched quality, and a lower quality is smaller.
    #[test]
    fn webp_is_smaller_than_png_and_jpeg_on_photographic_content() {
        let (w, h) = (320, 64);
        let rgb = noisy_rgb(w, h);
        let png = Tile::from_rgb(0, 0, w, h, &rgb).unwrap();
        let jpeg = Tile::from_rgb_jpeg(0, 0, w, h, &rgb, 60).unwrap();
        let webp = Tile::from_rgb_webp(0, 0, w, h, &rgb, 60).unwrap();
        assert!(
            webp.data.len() < png.data.len(),
            "WebP should beat PNG: {} vs {}",
            webp.data.len(),
            png.data.len()
        );
        assert!(
            webp.data.len() < jpeg.data.len(),
            "WebP should beat JPEG at the same quality: {} vs {}",
            webp.data.len(),
            jpeg.data.len()
        );
        let lower = Tile::from_rgb_webp(0, 0, w, h, &rgb, 20).unwrap();
        assert!(
            lower.data.len() < webp.data.len(),
            "lower quality should be smaller: {} vs {}",
            lower.data.len(),
            webp.data.len()
        );
    }

    #[test]
    fn from_rgb_webp_rejects_a_mismatched_payload() {
        assert!(Tile::from_rgb_webp(0, 0, 2, 2, &[0u8; 11], 60).is_err());
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

    // The binary tile record's reason to exist: it must beat the old
    // base64-in-JSON baseline by a wide margin for screen-like content.
    #[test]
    fn tile_record_beats_old_base64_json_baseline() {
        let (w, h) = (1280, 64);
        let rgb = gradient_rgb(w, h);
        let mut frame = Vec::new();
        Tile::from_rgb(0, 0, w, h, &rgb)
            .unwrap()
            .write_record(batch::NO_SLOT, &mut frame);
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

    /// Flat UI: a few colours and hard edges, which is what most of a desktop is.
    /// Deliberately the same shape of content as the agent's classifier test, so
    /// the two halves of the system are judged on the same material.
    fn flat_ui_rgb(w: u16, h: u16) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for y in 0..h {
            for x in 0..w {
                if (y / 16) % 2 == 0 && (x / 7) % 3 == 0 {
                    rgb.extend_from_slice(&[20, 20, 24]); // "text"
                } else {
                    rgb.extend_from_slice(&[246, 246, 248]);
                }
            }
        }
        rgb
    }

    /// What a change-detection hash costs against what it skips, and what a
    /// narrower cell costs when its pixels really did change.
    ///
    /// Ignored and assertion-free on purpose: it prints, it does not judge. There
    /// is no benchmark harness here and a timing assertion on a shared machine is
    /// a flaky test — but these numbers decide two real design questions, so
    /// guessing at them is worse than a test nobody runs by accident:
    ///
    /// 1. **Does a change-detection gate pay for itself?** Only if hashing is much
    ///    cheaper than the encode it skips.
    /// 2. **How wide should a cell be?** A narrower cell skips more often but pays
    ///    PNG's per-stream overhead more times, and gets less redundancy to
    ///    compress within each stream. The ratio printed here is what a grid costs
    ///    in the case where it wins nothing, so it sets the skip rate the grid has
    ///    to achieve before it is worth having at all.
    ///
    /// Run it in **release**: `png` at `Compression::Fast` is several times slower
    /// in a debug build, which would flatter the hash and slander the grid.
    ///
    /// ```sh
    /// cargo test --release --lib -- --ignored --nocapture encode_cost
    /// ```
    #[test]
    #[ignore = "prints timings; run explicitly in release"]
    fn encode_cost_against_hash_cost() {
        use std::time::Instant;

        // A full-width Retina strip.
        let (sw, sh) = (3200u16, 64u16);
        let runs = 20;

        for (label, make) in [
            ("flat UI ", flat_ui_rgb as fn(u16, u16) -> Vec<u8>),
            ("gradient", gradient_rgb as fn(u16, u16) -> Vec<u8>),
        ] {
            let strip = make(sw, sh);

            let started = Instant::now();
            for _ in 0..runs {
                std::hint::black_box(xxhash_rust::xxh3::xxh3_64(&strip));
            }
            let hash = started.elapsed() / runs;

            let started = Instant::now();
            for _ in 0..runs {
                std::hint::black_box(Tile::from_rgb(0, 0, sw, sh, &strip).unwrap());
            }
            let whole = started.elapsed() / runs;
            let strip_bytes = Tile::from_rgb(0, 0, sw, sh, &strip).unwrap().data.len();

            println!(
                "\n{label}  {sw}x{sh} strip: {strip_bytes} bytes, encode {whole:?}, \
                 hash {hash:?} ({:.0}x cheaper)",
                whole.as_secs_f64() / hash.as_secs_f64().max(f64::EPSILON),
            );
            println!("  cell      tiles  bytes   vs strip  encode     vs strip  break-even");

            // Both axes, because they are not equivalent: PNG filters and
            // compresses *along rows*, so cutting the width throws away redundancy
            // inside every stream, while cutting the height keeps each row whole
            // and only pays the per-stream overhead again.
            for (cw, ch) in [
                (3200u16, 32u16),
                (3200, 16),
                (800, 64),
                (640, 64),
                (320, 64),
                (320, 32),
                (256, 64),
                (128, 64),
            ] {
                let tiles = usize::from(sw / cw) * usize::from(sh / ch);
                let cell = make(cw, ch);
                let started = Instant::now();
                for _ in 0..runs {
                    for _ in 0..tiles {
                        std::hint::black_box(Tile::from_rgb(0, 0, cw, ch, &cell).unwrap());
                    }
                }
                let split = started.elapsed() / runs;
                let cell_bytes = Tile::from_rgb(0, 0, cw, ch, &cell).unwrap().data.len();
                let total = cell_bytes * tiles;
                // How many tiles may change before sending tiles costs more than
                // sending the whole strip. Below this the grid wins.
                let break_even = (strip_bytes as f64 / cell_bytes as f64).min(tiles as f64);
                println!(
                    "  {cw:>4}x{ch:<3}  {tiles:>5}  {total:>6}  {:>7.2}x  {split:>9?}  \
                     {:>6.2}x  {break_even:>5.1}/{tiles}",
                    total as f64 / strip_bytes as f64,
                    split.as_secs_f64() / whole.as_secs_f64().max(f64::EPSILON),
                );
            }
        }
    }
}
