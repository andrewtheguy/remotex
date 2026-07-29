//! Wire protocol shared (in shape) with the frontend `src/protocol.ts`.
//!
//! `ClientMsg` flows browser -> server (input events) as JSON text frames.
//! Server -> browser, the transport is split by weight (see
//! docs/architecture.md):
//!
//! - **Screen updates** are binary WebSocket frames, each one a *batch* of
//!   records rather than a single tile — see [`batch`]. A payload is a WebP
//!   image the client decodes natively — lossless for screen content, lossy for
//!   the photographic tiles the macOS agent classifies as such.
//!   Binary replaced base64 RGBA inside JSON text, which inflated the
//!   bottleneck backend->browser link by ~4.3x (4 bytes/px, +33% base64);
//!   batching then replaced one frame per tile, which cost a WebSocket frame, a
//!   client event and a separate decode for every strip of a repaint.
//! - **Control messages** (`resize`, `error`, `cursor`, …) are rare and small;
//!   they stay JSON text frames with a `type` tag. `cursor` carries a base64
//!   PNG — a pointer shape is a couple of hundred bytes and changes a handful
//!   of times a session, so it is not worth a second binary frame kind, and it
//!   is the one place PNG survives the move to WebP (see [`CursorShape`]).
//!
//! Ordering between the two is the WebSocket's, and it is load-bearing: a
//! `resize` reallocates the client's canvas, so a tile that arrived before it
//! must be *sent* before it. [`crate::wire`] is what preserves that while
//! batching.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Transport policy shared by all engines: a dirty rectangle taller than this
/// is split into strips before being sent, so a full-screen repaint doesn't
/// produce one huge WebSocket message.
pub const STRIP_ROWS: u16 = 64;

/// The revision of everything in this file: [`ClientMsg`], `ControlMsg`, and
/// the [`batch`] frame layout. Served from `GET /api/config` so a client that
/// isn't shipped with the gateway can refuse a version it cannot speak.
///
/// The SPA doesn't check it — it is served by this same binary, so it cannot
/// disagree. The macOS viewer is a separate artifact and does, and
/// `apps/remotex-viewer/Sources/App/ProductInfo.swift` carries the number it
/// accepts. Bump this only for a change that would break a client compiled
/// against the old shape; a purely additive control message is not one, because
/// clients are required to ignore tags they don't know.
///
/// 3 was the batch envelope: binary frames stopped being one tile each. 4 is
/// WebP: one codec replaced PNG *and* JPEG, so a `TILE` record's format byte has
/// exactly one valid value. That byte alone cannot protect a stale client —
/// rejecting an unknown format drops the whole frame silently, which looks like a
/// dead session rather than a version mismatch — so this is what makes the failure
/// legible.
///
/// **Audio did not bump it, and that was a decision rather than an oversight.** The
/// [`audio`] frame kind and its two messages are additive *and* opt-in: nothing
/// arrives until a client sends [`ClientMsg::Audio`], which only a browser does. So
/// a viewer's socket carries the same bytes it did before audio existed, and since
/// the viewer compares this number for **equality** and ships as its own DMG, a bump
/// would have cost every installed copy a reinstall to gain nothing.
pub const PROTOCOL_VERSION: u32 = 4;

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
    /// Put the remote desktop back at whatever size the *far side* considers its
    /// default: a target's `width`/`height` for VNC and RDP, and for `rxa` the
    /// point size the agent created its display at.
    ///
    /// The contrast with [`ClientMsg::Viewport`] above is the whole reason both
    /// exist. A `Viewport` is a size the client worked out from the room it has,
    /// which presumes the client's window is a shape a desktop can usefully be.
    /// A phone's is not — a portrait window asks for a tall, narrow desktop no
    /// desktop OS lays out well, and rotating it asks for a different one — so a
    /// client reading the desktop through pinch zoom carries no number worth
    /// sending. Deferring to the far side is the only form of the request it can
    /// make honestly, and it deliberately carries no size for the same reason:
    /// the default is known where it lives, and the client is the one place that
    /// does not know it.
    ///
    /// Not merely "send nothing", which was the other candidate and is not
    /// equivalent. A remote's size outlives the client that set it — most
    /// sharply for `rxa`, where macOS remembers and restores the mode a display
    /// identity was last put in (see the agent's `virtual_display_initial_size`),
    /// so a session that stretched that display leaves it stretched for whoever
    /// connects next. Declining to ask inherits that; this repairs it.
    ///
    /// Gated on the target's `resize` opt-in by every engine, exactly as
    /// `Viewport` is, and subject to the same per-protocol narrowing — `rxa`
    /// still only resizes a display the agent made.
    DefaultSize,
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
    /// "I lost the tiles you told me to remember." Empties the server's slot table
    /// and repaints, so the next tiles arrive as payloads rather than references.
    ///
    /// A client sends this when it cannot decode a tile it was told to cache, or
    /// when a reference names a slot it does not hold. Both leave the two ends
    /// disagreeing about what the client has, and **nothing else can repair it** —
    /// which is why this exists instead of reusing [`ClientMsg::Refresh`]. A
    /// `Refresh` is routed to the *engine* ([`crate::session`]); the outbound task
    /// that owns the slot table never sees it, so the engine would repaint,
    /// the repaint's tiles would still be believed cached, references would be sent
    /// again, and the client would miss again — a livelock at full-repaint
    /// bandwidth. This is handled by the socket's own bridge instead, which bumps
    /// the table's epoch *and* asks the engine to repaint.
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
    /// Start or stop sending this attachment's audio. Handled by the session layer,
    /// never forwarded to an engine (see [`crate::session::SessionManager::set_audio`]).
    ///
    /// **Audio is opt-in for a reason beyond taste, and it is why
    /// [`PROTOCOL_VERSION`] did not have to move for it.** The macOS viewer checks
    /// that number for equality and ships separately, so a bump costs every
    /// installed copy a reinstall. Nothing but a browser sends this, so a viewer's
    /// socket carries exactly the bytes it carried before audio existed — there is
    /// no new wire for it to refuse. Guacamole makes the same arrangement from the
    /// other end: its client declares the audio mimetypes it can decode, and a
    /// client that declares none gets a stream carrying nothing.
    ///
    /// The browser sends this from the floating menu's Audio button, so the enabling
    /// message is always inside a user gesture — which is also what lets it create
    /// an `AudioContext` a policy will let play.
    Audio { enabled: bool },
}

/// The layout of a server -> client binary frame: a **batch** of records.
///
/// One frame carries however many screen updates were ready at once, which is
/// what a full repaint needs — at [`CELL_W`]×[`CELL_H`] a 1600×1000 desktop is
/// 80 cells, and 80 WebSocket frames cost 80 client events and 80 separately
/// scheduled decodes to paint one picture.
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
/// ```
///
/// Why each of the header's four bytes earns its place:
///
/// - **kind `0x02`** retires the old `0x01` outright rather than extending it, so
///   a client built against the single-tile frame rejects this as unknown and goes
///   black instead of drawing garbage out of a misread header.
/// - **flags** is one reserved byte, and receivers *reject* a non-zero value
///   rather than ignoring it. Ignoring would make the byte useless later: a client
///   that skips a flag it does not know cannot be told anything by it.
/// - **record count**, even though records are self-delimiting and parsing could
///   simply run to the end of the buffer. A truncated frame would then paint a
///   silently short batch; with a count it is a detectable error.
pub mod batch {
    pub const FRAME_KIND: u8 = 0x02;
    pub const HEADER_LEN: usize = 4;

    pub const OP_TILE: u8 = 0x01;
    pub const OP_TILE_REF: u8 = 0x02;

    /// Bytes a `TILE` record costs besides its payload.
    pub const TILE_HEADER_LEN: usize = 16;
    /// A whole `TILE_REF` record.
    pub const TILE_REF_LEN: usize = 7;

    /// `slot` meaning "draw this and do not remember it".
    ///
    /// Needed so one enormous photographic tile cannot evict a screenful of
    /// useful small ones, and so a three-pixel caret rectangle need not consume a
    /// slot at all.
    pub const NO_SLOT: u16 = 0xFFFF;

    /// How many tile slots a client keeps.
    ///
    /// Part of the wire contract, not a server-side tuning knob: a client sizes
    /// its cache by this, and a `slot` at or above it (other than [`NO_SLOT`]) is
    /// a malformed record rather than something to grow an array for. That makes a
    /// client's memory a function of the protocol instead of a function of what a
    /// server chooses to send it.
    ///
    /// 256 because both clients cache *encoded* payloads, so the cost is bytes
    /// received rather than pixels decoded — at the per-slot ceiling below, 8 MiB
    /// worst case, and a small fraction of that in practice.
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
/// Deliberately the same four-byte header as [`batch`], so a reader of one finds
/// nothing surprising in the other, and for the same two reasons: a reserved flags
/// byte a receiver *rejects* rather than ignores, and a count that makes a truncated
/// frame detectable rather than a silently short one.
///
/// **Why lengths at all**, when a WebSocket message is already delimited: an Opus
/// packet does not carry its own size, and one frame holds several. Nine or ten of
/// them for the tested host's 32 KiB wave buffers — 20 ms each — which is what keeps
/// this at ~5 frames a second instead of the 50 a packet-per-frame design would
/// send.
///
/// Audio shares the desktop socket rather than getting one of its own, following
/// Guacamole, and [`crate::wire`] is where that is made to cost as little as it can:
/// an audio frame does not wait behind a batch still being built.
pub mod audio {
    pub const FRAME_KIND: u8 = 0x03;
    pub const HEADER_LEN: usize = 4;
    /// Bytes each packet costs besides its own bytes.
    pub const PACKET_HEADER_LEN: usize = 2;

    /// Serialize `packets` into one audio frame.
    ///
    /// Both `u16` fields are checked rather than truncated, and each panic names the
    /// invariant it belongs to, because silently wrapping either would produce a frame
    /// a client parses successfully and wrongly. Neither is reachable from the encoder:
    /// `opus_stream::MAX_PACKET_BYTES` caps a packet at 4000, and a wave buffer holding
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
                .expect("an opus packet is at most u16::MAX bytes");
            frame.extend_from_slice(&size.to_le_bytes());
            frame.extend_from_slice(packet);
        }
        frame
    }
}

/// The canonical tile grid, in framebuffer pixels.
///
/// Origin is pinned at (0,0) and these are compile-time constants: a grid derived
/// from the desktop size would shift the meaning of every cell whenever the
/// desktop resized, and cells are about to become cache identities.
///
/// **320×64 is measured, not guessed** — see `encode_cost_against_hash_cost`
/// below. Covering one 3200×64 strip, sending cells instead of the whole strip
/// costs 1.36× the bytes when every cell genuinely changed, and breaks even when
/// about 70% of them did; so it wins in every case short of near-total change.
/// The two obvious alternatives are worse for reasons worth recording, because
/// both look free until measured:
///
/// - **Narrower** is expensive fast. 128 wide costs 2.01× on the same content and
///   needs fewer than half its cells skippable to break even. An encoder pays a
///   fixed per-stream cost and loses horizontal redundancy in every one of them.
/// - **Shorter is not the cheap axis.** 3200×16 costs 1.56–2.57×, often worse than
///   any narrowing, because filters predict from the row *above*: a short stream
///   throws away vertical prediction exactly as a narrow one throws away
///   horizontal. Neither axis is free.
///
/// Those ratios were measured with PNG, which is no longer the codec
/// (`encode_webp`). They are left as they were because the *shape* of the
/// argument is what chose 320, and WebP does not change it — it too pays a fixed
/// per-stream cost, and `webp_cost_against_png_cost` measures that cost as the
/// dominant term at small sizes rather than a marginal one. Re-deriving the exact
/// break-even under WebP would only be worth it if the cell size were up for
/// revision.
///
/// 20480 px also stays far clear of the agent's `MIN_LOSSY_PIXELS` (32×32), so its
/// per-tile codec classifier still has enough pixels to judge.
///
/// # Where this does *not* apply
///
/// All of the above answers "if damage is split into cells, how big should a cell
/// be". It says nothing about whether damage *should* be split into cells, and for
/// the gateway's own engines the answer turned out to be no: RDP reports damage
/// with a median area of 1295 pixels, 92% of it smaller than one cell, so snapping
/// outward onto this grid cost 8.9× the bytes (see [`crate::tiles`] and
/// `tests/rdp_bytes_probe.rs`). Those engines compare against a shadow copy of what
/// the client holds instead, and this grid is left for damage that genuinely
/// arrives coarse — the macOS agent's, which is reported in full-width strips of a
/// 3200-pixel desktop.
pub const CELL_W: u16 = 320;
/// See [`CELL_W`]. Also the height a dirty rectangle is split at, which is what
/// [`STRIP_ROWS`] used to mean on its own.
pub const CELL_H: u16 = STRIP_ROWS;

/// A dirty rectangle of the framebuffer, carried as one `TILE` record inside a
/// [`batch`] frame. The payload is a WebP stream the client decodes natively.
///
/// The RDP and VNC engines decode a framebuffer and compress it here
/// ([`Tile::from_rgb`], always lossless); the macOS agent encodes on the Mac and
/// the gateway relays those bytes untouched ([`Tile::encoded`]), choosing lossless
/// or lossy per tile from the content.
///
/// The `format` byte therefore has one valid value today. It is kept rather than
/// dropped because it is the seam a second payload kind would arrive through —
/// `docs/roadmap.md` puts H.264 next — and one byte per tile is not worth
/// reclaiming and then re-adding.
#[derive(Debug, Clone)]
pub struct Tile {
    /// Payload codec. Always [`Tile::FORMAT_WEBP`]; see the note above.
    pub format: u8,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    /// The encoded image stream, in `format`.
    pub data: Vec<u8>,
}

impl Tile {
    /// WebP, lossless or lossy — the container is the same either way, so the
    /// client needs no signal to tell them apart and the classifier's choice never
    /// reaches the wire.
    ///
    /// 3 rather than reusing 1: 1 and 2 meant PNG and JPEG, and a value a stale
    /// client *recognises* is worse than one it rejects. Reusing 1 would have it
    /// hand WebP bytes to a PNG decoder, fail per tile, and ask for a cache reset
    /// on every one — a reset storm rather than a clean refusal.
    pub const FORMAT_WEBP: u8 = 3;

    /// Build a tile from packed RGB888 pixels, WebP-compressing the payload.
    pub fn from_rgb(x: u16, y: u16, w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Self> {
        let expected = usize::from(w) * usize::from(h) * 3;
        anyhow::ensure!(
            rgb.len() == expected,
            "tile payload is {} bytes, expected {expected} for {w}x{h} RGB",
            rgb.len()
        );
        let data = encode_webp(w, h, rgb)?;
        Ok(Self {
            format: Self::FORMAT_WEBP,
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
    /// Still PNG after tiles moved to WebP, and deliberately: a shape is a few
    /// hundred bytes sent a handful of times a session, so the codec saves nothing
    /// measurable — while the macOS agent's shapes come out of AppKit
    /// (`NSBitmapImageRep`, which cannot write WebP) and are relayed here
    /// unmodified, so switching would mean re-encoding them for no gain.
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
        let png = encode_cursor_png(w, h, rgba)?;
        Ok(Self { w, h, hx, hy, png })
    }
}

/// PNG-encode a cursor's packed RGBA8888 pixels.
///
/// The only PNG left in the gateway. Tiles are WebP ([`encode_webp`]); this stays
/// because [`CursorShape::png`] rides the JSON control channel as base64 and the
/// agent's own shapes arrive as PNG from AppKit.
fn encode_cursor_png(w: u16, h: u16, rgba: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, u32::from(w), u32::from(h));
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(out)
}

/// WebP's maximum dimension in either axis, from the format itself (14 bits).
///
/// PNG had no such limit, so this is a new way for an encode to fail. It is
/// unreachable in practice — a `u16` framebuffer could in principle be wider, and
/// no desktop is — but it would end the session ([`crate::encode`] treats a failed
/// encode as fatal, because the shadow has already recorded those pixels as sent),
/// so it is checked here and reported rather than left to libwebp's
/// `VP8_ENC_ERROR_BAD_DIMENSION`.
const WEBP_MAX_DIMENSION: u16 = 16383;

/// The lossless effort libwebp is asked for, chosen by measurement.
///
/// `method` is its speed/size dial and `quality`, in lossless mode, is an effort
/// dial rather than a fidelity one. Both are pinned at the cheap end because the
/// extra compression above them did not pay for itself against the budget these
/// encodes had — which was an engine's own protocol-read loop:
///
/// Bytes against `png::Compression::Fast`, from `webp_cost_against_png_cost` on two
/// real screenshots — one a working window, one a desktop mostly covered by a
/// photographic wallpaper. Content matters more than shape does:
///
/// | config | UI window | photo wallpaper | time vs PNG-Fast |
/// |---|---|---|---|
/// | `m0 q20` | **0.64-0.81x** | 0.83-0.88x | 5-34x |
/// | `m2 q50` | 0.53-0.73x | 0.66-0.77x | 19-72x |
///
/// So the swap buys 19-36% of tile bytes on the content the RDP and VNC engines
/// mostly carry, and rather less on a photograph — which is the right way round,
/// since a photograph is what the macOS agent's classifier sends down its lossy
/// branch instead. The 27-47% on offer at `m2` costs an order of magnitude more
/// time: 3.1ms for one 320x64 cell.
///
/// **One half of that trade changed, and it is not the half that matters.** 3.1ms was
/// ruled out because it was 3.1ms an engine's protocol-read loop had to stand still
/// for, and it no longer runs there — [`crate::encode`] moved these encodes onto a
/// bounded set of workers. But relocating a cost is not removing one: `m2` is still
/// 7.7x the CPU per encode, `ENCODE_DEPTH` can only overlap that up to the core
/// count, and on the widest bands it would make a repaint *slower* rather than
/// faster. So this is not the lever for a screen with a large moving area, which is
/// the case anyone arrives here wanting to fix.
///
/// Where it does pay is the small end, per the note below: nearly free under roughly
/// 512 pixels, which is most of what the gateway's engines actually send. That makes
/// a **size-tiered** effort the shape of any future change here, not a new constant.
///
/// Two things to get right when reading the numbers above. The size gain's baseline is
/// the row above and not PNG: against the `m0 q20` that ships, `m2 q50` is about
/// 10-20% fewer bytes (0.53-0.73x against 0.64-0.81x), where the 27-47% figure is
/// against PNG-Fast, a codec this tree no longer has. And the effort costs no fidelity
/// whatever — `quality` is an effort dial here, the mode is still lossless — which is
/// what distinguishes it from reaching for the lossy branch to save the same bytes.
///
/// Two things the same tables say, for whoever revisits this:
///
/// - **The cost is mostly fixed per encode, not per pixel.** A 16x16 tile costs
///   60µs and a 320x64 one — 80x the pixels — costs 402µs. That is what makes the
///   *ratio* worst on small rectangles even though their absolute cost is trivial,
///   and it is a second reason the gateway does not snap damage onto a grid (see
///   [`CELL_W`]).
/// - **Higher effort is nearly free at the smallest sizes**, for that same reason:
///   at 16x16, `m2 q50` costs 87µs against `m0 q20`'s 60µs and gives 0.53x rather
///   than 0.76x. The crossover is sharp — by 64x20 the same swap costs 3.5x the
///   time for 7% more compression — so a size-tiered effort is a real but *bounded*
///   win, worth having only below roughly 512 pixels. Left out of this change so the
///   codec swap can be validated on its own.
///
/// Timings are from an arm64 Mac. The x86-64 host the gateway is deployed to came
/// out faster in every ratio (5.4x rather than 6.8x at 320x64), with byte counts
/// identical, as they must be.
const WEBP_LOSSLESS_METHOD: i32 = 0;
const WEBP_LOSSLESS_EFFORT: f32 = 20.0;

/// WebP-encode packed RGB888 losslessly.
///
/// Every caller is a screen tile, so this is lossless without a choice: the
/// gateway's engines decode a framebuffer and must reproduce it exactly. Only the
/// macOS agent encodes lossily, on the Mac, from content it classified.
///
/// Two hazards in the `webp` crate that the shape of this function is answering:
/// `Encoder::from_rgb` *panics* on a buffer shorter than `w * h * 3`, so the length
/// check above every call is load-bearing rather than tidy; and `Encoder::encode`
/// and `encode_lossless` both `unwrap()` internally, so `encode_advanced` is the
/// only entry point that can report failure — which matters because
/// `[profile.release] panic = "abort"` makes a panic here unrecoverable.
fn encode_webp(w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(w > 0 && h > 0, "cannot encode a {w}x{h} tile");
    anyhow::ensure!(
        w <= WEBP_MAX_DIMENSION && h <= WEBP_MAX_DIMENSION,
        "tile {w}x{h} exceeds WebP's {WEBP_MAX_DIMENSION}px limit"
    );
    let expected = usize::from(w) * usize::from(h) * 3;
    anyhow::ensure!(
        rgb.len() == expected,
        "tile payload is {} bytes, expected {expected} for {w}x{h} RGB",
        rgb.len()
    );
    let mut config = webp::WebPConfig::new()
        .map_err(|()| anyhow::anyhow!("libwebp rejected its own default config"))?;
    config.lossless = 1;
    config.quality = WEBP_LOSSLESS_EFFORT;
    config.method = WEBP_LOSSLESS_METHOD;
    // No libwebp worker thread. A tile is small, and the parallelism that pays is
    // one band per worker ([`crate::encode`]) rather than one tile split across
    // threads — libwebp's own would compete with that for the same cores.
    config.thread_level = 0;
    let encoded = webp::Encoder::from_rgb(rgb, u32::from(w), u32::from(h))
        .encode_advanced(&config)
        .map_err(|e| anyhow::anyhow!("WebP encode failed for {w}x{h}: {e:?}"))?;
    // Copied out rather than held: `WebPMemory` is neither `Send` nor `Sync`, and
    // a tile crosses tasks on its way to the socket.
    Ok(encoded.to_vec())
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
    ///
    /// `audio` is the same kind of permission: it says this session *can* carry the
    /// remote's sound, so a browser may ask for it with [`ClientMsg::Audio`] (see
    /// docs/remote-audio.md). It does not mean any is playing, or that the remote's
    /// audio channel is even up — from this end those are indistinguishable.
    Connected {
        name: String,
        protocol: &'static str,
        resize: bool,
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
    /// How to decode the audio frames that follow, sent once when a client's
    /// [`ClientMsg::Audio`] subscription starts — and *before* the first packet,
    /// because a decoder cannot be configured by the audio it is meant to decode.
    ///
    /// `sample_rate` and `channels` are the *stream's*, which is 48 kHz whatever
    /// rate the remote negotiated: libopus encodes at 48 kHz and nothing else.
    /// `head` is `OpusHead` (RFC 7845 §5.1) — base64 on the wire, and exactly the
    /// byte string WebCodecs wants as an `AudioDecoderConfig.description`. It is
    /// what carries the encoder's pre-skip, so a decoder discards its own delay
    /// instead of playing it as leading silence.
    AudioFormat {
        codec: &'static str,
        sample_rate: u32,
        channels: u16,
        head: Vec<u8>,
    },
    /// One wave buffer's worth of Opus packets, framed by [`audio::frame`].
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
        /// base64 `OpusHead`, for the same reason `cursor`'s PNG is base64: a text
        /// frame cannot carry bytes, and this is 19 of them once a session.
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
            ServerMsg::Tile(_) | ServerMsg::Audio(_) => return None,
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
                clipboard,
                audio,
            } => control(&ControlMsg::Connected {
                name,
                protocol,
                resize: *resize,
                clipboard: *clipboard,
                audio: *audio,
            }),
            ServerMsg::RemoteOs { macos } => control(&ControlMsg::RemoteOs { macos: *macos }),
            ServerMsg::AudioFormat {
                codec,
                sample_rate,
                channels,
                head,
            } => control(&ControlMsg::AudioFormat {
                codec,
                sample_rate: *sample_rate,
                channels: *channels,
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
        // The audio subscription, which is also the FAB's enable/disable control.
        // Both directions matter: nothing sends this but a browser, and a viewer
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
            head: head.clone(),
        })
        .text_frame()
        .expect("the format must be a text frame");
        assert_eq!(
            json,
            r#"{"type":"audioFormat","codec":"opus","sampleRate":48000,"channels":2,"head":"T3B1c0hlYWQBAjgBRKwAAAAAAA=="}"#
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
            protocol: "rxa",
            resize: false,
            clipboard: true,
            audio: false,
        })
        .text_frame()
        {
            Some(json) => assert_eq!(
                json,
                r#"{"type":"connected","name":"mac","protocol":"rxa","resize":false,"clipboard":true,"audio":false}"#
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
            format: Tile::FORMAT_WEBP,
            x: 0x0102,
            y: 0x0304,
            w: 2,
            h: 1,
            data: vec![10, 20, 30, 40, 50, 60],
        };
        let mut out = Vec::new();
        tile.write_record(batch::NO_SLOT, &mut out);
        assert_eq!(out[0], batch::OP_TILE);
        assert_eq!(out[1], Tile::FORMAT_WEBP);
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
    // browser byte for byte. The gateway never looks inside a payload, so this is
    // the one place the wire carries bytes this crate did not produce.
    #[test]
    fn encoded_tile_passes_the_payload_and_format_through_untouched() {
        let webp = vec![
            b'R', b'I', b'F', b'F', 0x10, 0, 0, 0, b'W', b'E', b'B', b'P', b'V', b'P', b'8', b'L',
        ];
        let tile = Tile::encoded(Tile::FORMAT_WEBP, 0x0102, 0x0304, 320, 64, webp.clone());
        let mut out = Vec::new();
        tile.write_record(batch::NO_SLOT, &mut out);
        assert_eq!(out[0], batch::OP_TILE);
        assert_eq!(out[1], Tile::FORMAT_WEBP);
        assert_eq!(&out[4..6], &[0x02, 0x01]); // x, little-endian
        assert_eq!(&out[6..8], &[0x04, 0x03]); // y
        assert_eq!(&out[8..10], &[0x40, 0x01]); // w = 320
        assert_eq!(&out[10..12], &[64, 0]); // h
        assert_eq!(&out[batch::TILE_HEADER_LEN..], webp.as_slice());
    }

    #[test]
    fn from_rgb_marks_its_payload_as_webp() {
        let tile = Tile::from_rgb(0, 0, 2, 2, &[0u8; 12]).unwrap();
        assert_eq!(tile.format, Tile::FORMAT_WEBP);
        let mut out = Vec::new();
        tile.write_record(batch::NO_SLOT, &mut out);
        assert_eq!(out[1], Tile::FORMAT_WEBP);
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

    /// Also the guard on the shipped config: a `lossless` flag that libwebp did
    /// not honour would show up here and nowhere else, and the failure mode is
    /// silent — slightly wrong pixels that no other test looks at.
    #[test]
    fn screen_content_compresses_and_roundtrips_losslessly() {
        let (w, h) = (320, 64);
        let rgb = gradient_rgb(w, h);
        let tile = Tile::from_rgb(7, 9, w, h, &rgb).unwrap();
        assert!(
            tile.data.len() < rgb.len() / 4,
            "WebP should compress a gradient well: {} vs raw {}",
            tile.data.len(),
            rgb.len()
        );

        let image = webp::Decoder::new(&tile.data).decode().expect("payload decodes");
        assert_eq!((image.width(), image.height()), (u32::from(w), u32::from(h)));
        // An RGB source must not come back with an alpha channel: the viewer's
        // decoder discards alpha on the stated grounds that tiles are opaque
        // (`TileDecoder.swift`), so a payload carrying it would decode to the
        // wrong pixels rather than fail.
        assert!(!image.is_alpha());
        assert_eq!(&*image, rgb.as_slice());
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
    fn tiny_tile_is_still_a_valid_webp() {
        // 2x2 of "noise" — the container's fixed overhead dominates here, which is
        // accepted: one decode path beats saving a few dozen bytes.
        let rgb = [1u8, 200, 3, 250, 5, 90, 7, 160, 9, 30, 11, 220];
        let tile = Tile::from_rgb(0, 0, 2, 2, &rgb).unwrap();
        assert_eq!(&tile.data[..4], b"RIFF");
        assert_eq!(&tile.data[8..12], b"WEBP");
    }

    // WebP cannot describe a tile wider or taller than 16383, where PNG could.
    // Unreachable from any real desktop, but the failure ends the session rather
    // than dropping a tile, so it has to be an error and not a panic.
    #[test]
    fn a_tile_beyond_webps_dimension_limit_is_rejected() {
        let w = WEBP_MAX_DIMENSION + 1;
        let rgb = vec![0u8; usize::from(w) * 3];
        let err = Tile::from_rgb(0, 0, w, 1, &rgb).unwrap_err().to_string();
        assert!(err.contains("16383"), "unhelpful error: {err}");
        // The limit itself still encodes, so the bound is not off by one.
        let rgb = vec![0u8; usize::from(WEBP_MAX_DIMENSION) * 3];
        assert!(Tile::from_rgb(0, 0, WEBP_MAX_DIMENSION, 1, &rgb).is_ok());
    }

    #[test]
    fn a_zero_sized_tile_is_rejected_rather_than_handed_to_libwebp() {
        // The `webp` crate does not reject these; libwebp fails them deep inside
        // with BAD_DIMENSION, and `Encoder::from_rgb` panics on a short buffer.
        assert!(Tile::from_rgb(0, 0, 0, 4, &[]).is_err());
        assert!(Tile::from_rgb(0, 0, 4, 0, &[]).is_err());
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

    /// Uniform noise: the one case with no structure for either codec to exploit,
    /// and therefore the honest worst case for encode *time*.
    fn noise_rgb(w: u16, h: u16) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..usize::from(w) * usize::from(h) * 3)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 33) as u8
            })
            .collect()
    }

    /// A real screen, decoded to packed RGB888 from the PNG at
    /// `REMOTEX_BENCH_IMAGE`.
    ///
    /// **There is deliberately no synthetic fallback**, and that is the most
    /// important thing in this module for anyone extending the bench below. The
    /// obvious generated fixtures — [`flat_ui_rgb`], [`gradient_rgb`], the agent's
    /// `photo` — are all exactly *periodic*, and WebP lossless resolves any of them
    /// to about a hundred bytes at any size, because its backward references match
    /// the whole image against its first period. Measured that way WebP appears to
    /// beat PNG by 60× on a 3200×64 strip, which is fiction. Those fixtures are
    /// fine for [`encode_cost_against_hash_cost`], which compares PNG against
    /// itself, and worthless for comparing one codec to another.
    ///
    /// So this reads real pixels. Any screenshot of a desktop will do:
    ///
    /// ```sh
    /// ssh mac screencapture -x -t png /tmp/shot.png
    /// scp 'mac:/tmp/shot.png' tmp/bench/shot.png
    /// REMOTEX_BENCH_IMAGE=tmp/bench/shot.png \
    ///   cargo test --release --lib -- --ignored --nocapture webp_cost
    /// ```
    fn screenshot_rgb() -> Option<(u16, u16, Vec<u8>, String)> {
        let path = std::env::var("REMOTEX_BENCH_IMAGE").ok()?;
        let file = std::fs::File::open(&path).expect("REMOTEX_BENCH_IMAGE is not readable");
        let mut reader = png::Decoder::new(std::io::BufReader::new(file))
            .read_info()
            .expect("REMOTEX_BENCH_IMAGE is not a PNG");
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("PNG did not decode");
        assert_eq!(info.bit_depth, png::BitDepth::Eight, "expected an 8-bit PNG");
        let rgb = match info.color_type {
            png::ColorType::Rgb => buf[..info.buffer_size()].to_vec(),
            png::ColorType::Rgba => buf[..info.buffer_size()]
                .chunks_exact(4)
                .flat_map(|px| [px[0], px[1], px[2]])
                .collect(),
            other => panic!("expected an RGB or RGBA PNG, got {other:?}"),
        };
        let (w, h) = (info.width as u16, info.height as u16);
        assert_eq!(rgb.len(), usize::from(w) * usize::from(h) * 3);
        Some((w, h, rgb, path))
    }

    /// Cut one `w`×`h` tile out of a `sw`-wide RGB888 image at `(x, y)`.
    fn crop_rgb(src: &[u8], sw: u16, x: u16, y: u16, w: u16, h: u16) -> Vec<u8> {
        let stride = usize::from(sw) * 3;
        let mut out = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for row in 0..usize::from(h) {
            let start = (usize::from(y) + row) * stride + usize::from(x) * 3;
            out.extend_from_slice(&src[start..start + usize::from(w) * 3]);
        }
        out
    }

    /// Up to `limit` `w`×`h` tiles spread evenly over the image, and how many the
    /// image holds in total — a strided sample of what a full repaint would send.
    fn sample_tiles(
        src: &[u8],
        sw: u16,
        sh: u16,
        w: u16,
        h: u16,
        limit: usize,
    ) -> (Vec<Vec<u8>>, usize) {
        let (cols, rows) = (usize::from(sw / w), usize::from(sh / h));
        let total = cols * rows;
        let step = total.div_ceil(limit.max(1)).max(1);
        let tiles = (0..total)
            .step_by(step)
            .map(|i| {
                let (cx, cy) = (i % cols, i / cols);
                crop_rgb(src, sw, (cx as u16) * w, (cy as u16) * h, w, h)
            })
            .collect();
        (tiles, total)
    }

    /// WebP-encode packed RGB888 with a hand-built config.
    ///
    /// `encode_advanced` is the only encode entry point in the `webp` crate that
    /// returns a `Result`: `Encoder::encode` and `Encoder::encode_lossless` both
    /// `unwrap()` internally, and `[profile.release] panic = "abort"` makes that
    /// unrecoverable. `Encoder::from_rgb` panics on a buffer shorter than
    /// `w * h * 3`, so the length is asserted before it is handed over.
    ///
    /// In lossless mode libwebp reads `quality` as an effort dial, not a fidelity
    /// one, and `method` is the speed/size trade (0 selects its low-effort path).
    fn webp_rgb(w: u16, h: u16, rgb: &[u8], lossless: bool, quality: f32, method: i32) -> Vec<u8> {
        assert_eq!(rgb.len(), usize::from(w) * usize::from(h) * 3);
        let mut config = webp::WebPConfig::new().expect("libwebp rejected its own defaults");
        config.lossless = i32::from(lossless);
        config.quality = quality;
        config.method = method;
        // No worker thread, matching `encode_webp`: a tile is too small for
        // libwebp's own threads to beat one worker per band.
        config.thread_level = 0;
        webp::Encoder::from_rgb(rgb, u32::from(w), u32::from(h))
            .encode_advanced(&config)
            .map(|mem| mem.to_vec())
            .unwrap_or_else(|e| panic!("webp encode failed for {w}x{h}: {e:?}"))
    }

    /// Decode and compare, so a "lossless" config that is quietly lossy cannot be
    /// printed as a win.
    fn assert_webp_roundtrips(w: u16, h: u16, rgb: &[u8], encoded: &[u8], label: &str) {
        let image = webp::Decoder::new(encoded)
            .decode()
            .unwrap_or_else(|| panic!("{label}: {w}x{h} payload did not decode"));
        assert_eq!(
            (image.width(), image.height()),
            (u32::from(w), u32::from(h)),
            "{label}: {w}x{h} decoded to the wrong size"
        );
        assert!(!image.is_alpha(), "{label}: an RGB source gained an alpha channel");
        assert_eq!(&*image, rgb, "{label}: {w}x{h} did not roundtrip losslessly");
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
    /// Run it in **release**: an encode is several times slower in a debug build,
    /// which would flatter the hash and slander the grid.
    ///
    /// The prose below still says PNG, and the ratios in [`CELL_W`]'s documentation
    /// were measured with it, but `Tile::from_rgb` is WebP now — so a fresh run
    /// prints WebP's numbers. Both are what the tile path actually costs at the time
    /// of running, which is what this measures; see [`CELL_W`] for why the cell size
    /// was not re-derived.
    ///
    /// ```sh
    /// cargo test --release --lib -- --ignored --nocapture encode_cost
    /// ```
    #[test]
    #[ignore = "prints timings; run explicitly in release"]
    fn encode_cost_against_hash_cost() {
        use std::time::Instant;

        // A full-width Retina strip, the unit the rxa agent ships today.
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

    /// Write the WebP fixtures the macOS viewer's tests decode.
    ///
    /// They are checked in rather than generated in Swift because **ImageIO cannot
    /// write WebP** — `CGImageDestinationCopyTypeIdentifiers()` has no
    /// `org.webmproject.webp`, though `CGImageSource` reads it, which is why the
    /// viewer itself is unaffected. Those tests used to encode their own payloads
    /// through `CGImageDestination` precisely so no encoder's choices got frozen
    /// into a fixture; that is no longer available, so the encoder that produces
    /// them is this one — the same one production uses, which is the next best
    /// thing and arguably better: it is the real wire payload.
    ///
    /// Run after changing [`encode_webp`]'s config, or when a test needs a shape or
    /// colour that is not here yet (it will say so by name):
    ///
    /// ```sh
    /// cargo test --lib -- --ignored --nocapture swift_webp_fixtures
    /// ```
    #[test]
    #[ignore = "writes files into the viewer's test bundle; run explicitly"]
    fn write_swift_webp_fixtures() {
        let dir = std::path::Path::new("apps/remotex-viewer/Tests/Fixtures");
        std::fs::create_dir_all(dir).expect("fixture directory");

        // Solid 2x2s for `tileRecord`, whose `red:` argument selects between them.
        // The other two channels are fixed, so a test comparing decoded bytes is
        // comparing the thing it named.
        for red in [0x11u8, 0x22, 0xFF] {
            let rgb: Vec<u8> = std::iter::repeat_n([red, 0x20, 0x40], 4).flatten().collect();
            let name = format!("solid-2x2-{red:02x}.webp");
            std::fs::write(dir.join(&name), encode_webp(2, 2, &rgb).unwrap()).unwrap();
            println!("wrote {name}");
        }

        // Top half red, bottom half blue: the asymmetry is the point, since it is
        // what catches a decoder that flips rows. Also stands in for a payload
        // whose size disagrees with its record header.
        let mut rgb = Vec::new();
        for y in 0..8u16 {
            for _ in 0..8 {
                rgb.extend_from_slice(if y < 4 { &[0xFF, 0x00, 0x00] } else { &[0x00, 0x00, 0xFF] });
            }
        }
        std::fs::write(dir.join("topdown-8x8.webp"), encode_webp(8, 8, &rgb).unwrap()).unwrap();
        println!("wrote topdown-8x8.webp");

        // The two layouts ImageIO can hand back for a WebP: three channels, and
        // four when the bitstream carries alpha. `TileDecoder` has to normalise both
        // to four bytes per pixel, and this is the only place the alpha case exists
        // — nothing in production encodes it, which is exactly why it is a fixture.
        let opaque: Vec<u8> = std::iter::repeat_n([0x30u8, 0x60, 0x90], 16).flatten().collect();
        std::fs::write(dir.join("opaque-4x4.webp"), encode_webp(4, 4, &opaque).unwrap()).unwrap();
        println!("wrote opaque-4x4.webp");

        let rgba: Vec<u8> = std::iter::repeat_n([0x30u8, 0x60, 0x90, 0x80], 16).flatten().collect();
        let mut config = webp::WebPConfig::new().unwrap();
        config.lossless = 1;
        config.quality = WEBP_LOSSLESS_EFFORT;
        config.method = WEBP_LOSSLESS_METHOD;
        config.thread_level = 0;
        let alpha = webp::Encoder::from_rgba(&rgba, 4, 4)
            .encode_advanced(&config)
            .expect("alpha fixture encodes");
        std::fs::write(dir.join("alpha-4x4.webp"), &*alpha).unwrap();
        println!("wrote alpha-4x4.webp");

        // A *lossy* payload, which no other fixture is: the agent's classifier sends
        // photographic tiles down that branch, and lossy WebP is a VP8 bitstream
        // where everything else here is VP8L. Two different decoders on the other
        // side, and only one of them was being exercised — so a viewer that could
        // not read VP8 would have shown blank tiles on exactly the content the
        // classifier picks, with every test passing.
        let mut rgb = Vec::new();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for y in 0..64u16 {
            for x in 0..64u16 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let jitter = ((state >> 56) as i32 - 128) / 5;
                // Ramps chosen so that neither wraps across 64 pixels: red climbs
                // with x and green with y, monotonically, which is what lets the
                // Swift side check the orientation from two corners.
                for base in [i32::from(x) * 3, i32::from(y) * 3 + 30, 100] {
                    rgb.push((base + jitter).clamp(0, 255) as u8);
                }
            }
        }
        let mut lossy_config = webp::WebPConfig::new().unwrap();
        lossy_config.lossless = 0;
        lossy_config.quality = 80.0;
        lossy_config.method = WEBP_LOSSLESS_METHOD;
        lossy_config.thread_level = 0;
        let lossy = webp::Encoder::from_rgb(&rgb, 64, 64)
            .encode_advanced(&lossy_config)
            .expect("lossy fixture encodes");
        // Not `assert_ne!` against the source pixels: those are raw RGB and this is
        // an encoded container, so they differ however the config was set — a
        // lossless encode would have passed that check just as well. The chunk
        // identifier is the thing that actually distinguishes the two bitstreams.
        assert_eq!(&lossy[12..16], b"VP8 ", "the lossy fixture is not a lossy bitstream");
        std::fs::write(dir.join("lossy-64x64.webp"), &*lossy).unwrap();
        println!("wrote lossy-64x64.webp");

        // Every fixture is read back and checked against what its name claims,
        // because the Swift side cannot tell the difference. `alpha-4x4` in
        // particular is only worth having if the bitstream really does carry alpha:
        // uniform alpha is exactly the sort of thing an encoder may decide to drop,
        // and a fixture that silently became opaque would leave `TileDecoder`'s
        // four-bytes-per-pixel normalisation untested while looking covered.
        //
        // The codec chunk is checked for the same reason: `VP8L` and `VP8 ` are two
        // different bitstreams decoded by two different paths on the other side, so a
        // config change that silently flipped one is a fixture that stops covering
        // what its test claims.
        for (name, w, h, wants_alpha, chunk) in [
            ("solid-2x2-11.webp", 2u32, 2u32, false, b"VP8L"),
            ("solid-2x2-22.webp", 2, 2, false, b"VP8L"),
            ("solid-2x2-ff.webp", 2, 2, false, b"VP8L"),
            ("topdown-8x8.webp", 8, 8, false, b"VP8L"),
            ("opaque-4x4.webp", 4, 4, false, b"VP8L"),
            ("alpha-4x4.webp", 4, 4, true, b"VP8L"),
            ("lossy-64x64.webp", 64, 64, false, b"VP8 "),
        ] {
            let bytes = std::fs::read(dir.join(name)).unwrap();
            assert!(bytes.len() >= 16, "{name} is too short to be a WebP");
            assert_eq!(&bytes[..4], b"RIFF", "{name} is not a RIFF container");
            assert_eq!(&bytes[8..12], b"WEBP", "{name} is not a WebP");
            assert_eq!(&bytes[12..16], chunk, "{name} is not the codec its test expects");
            let image = webp::Decoder::new(&bytes)
                .decode()
                .unwrap_or_else(|| panic!("{name} does not decode"));
            assert_eq!((image.width(), image.height()), (w, h), "{name} is the wrong size");
            assert_eq!(image.is_alpha(), wants_alpha, "{name}'s alpha channel is not as named");
        }
    }

    /// What WebP costs against PNG on this protocol's own content, in bytes and in
    /// time. The decision record for replacing the tile codec.
    ///
    /// A sibling of [`encode_cost_against_hash_cost`] rather than an extension of
    /// it: that test's numbers are quoted verbatim in [`CELL_W`]'s documentation and
    /// have to stay readable on their own.
    ///
    /// Ignored and mostly assertion-free for the same reasons as its sibling — it
    /// prints, it does not judge, because a timing assertion on a shared machine is
    /// a flaky test. The one thing it *does* assert is that every payload from a
    /// lossless config decodes back to the original pixels: a config that is
    /// quietly lossy would otherwise print as the winner.
    ///
    /// Run it in **release**. `png` at `Compression::Fast` is several times slower
    /// in a debug build, so a debug run flatters WebP and decides nothing:
    ///
    /// ```sh
    /// cargo test --release --lib -- --ignored --nocapture webp_cost
    /// ```
    ///
    /// Measured on **real screen pixels** ([`screenshot_rgb`]), tiled the way an
    /// engine would tile them, because generated fixtures give answers off by
    /// orders of magnitude — see that function for what went wrong the first time.
    ///
    /// Three questions, in the order they have to be answered:
    ///
    /// 1. **Which lossless config?** Section 1 sweeps `method` × effort at one shape.
    /// 2. **Does it hold across shapes?** Section 2 re-runs the shortlist over the
    ///    whole shape range, summing a sampled tiling of the screenshot — so the
    ///    byte column is a repaint total, not one lucky tile. Gateway damage is
    ///    *small*: RDP's median is 1295 px, 92% of it under one cell (see
    ///    [`CELL_W`]), so the small end is the binding case, not the 3200-wide
    ///    strip.
    /// 3. **Can JPEG go?** Section 3 puts WebP lossy against `jpeg-encoder` at the
    ///    quality the agent ships. If WebP is competitive the wire drops to one
    ///    codec. Section 4 repeats both against uniform noise, the worst case
    ///    either codec can be handed.
    #[test]
    #[ignore = "prints timings; run explicitly in release"]
    fn webp_cost_against_png_cost() {
        use std::time::Instant;

        /// How many times to repeat a pass over a tile sample.
        ///
        /// Budgeted in *pixels*, not passes: one pass already encodes every tile in
        /// the sample, so a wide shape needs very few repeats to be timed as well as
        /// a small one needs many. Fixing the repeat count instead makes the slowest
        /// config at the widest shape dominate the whole run.
        fn passes_for(tiles: usize, w: u16, h: u16) -> u32 {
            let per_pass = tiles as u64 * u64::from(w) * u64::from(h);
            (4_000_000 / per_pass.max(1)).clamp(1, 50) as u32
        }

        fn time<T>(runs: u32, mut f: impl FnMut() -> T) -> std::time::Duration {
            let started = Instant::now();
            for _ in 0..runs {
                std::hint::black_box(f());
            }
            started.elapsed() / runs
        }

        let Some((sw, sh, screen, path)) = screenshot_rgb() else {
            println!(
                "\nSkipped: set REMOTEX_BENCH_IMAGE to a PNG screenshot. Generated\n\
                 fixtures are periodic and would overstate WebP by ~60x — see\n\
                 screenshot_rgb's documentation."
            );
            return;
        };
        println!("\n=== source: {path}, {sw}x{sh} ===");

        // Gateway-realistic rects first (small and numerous), then the agent's cell,
        // then a wide strip. Widths beyond the screenshot are skipped, not clamped.
        const SHAPES: [(u16, u16); 8] = [
            (16, 16),
            (32, 40),
            (64, 20),
            (120, 11),
            (240, 64),
            (320, 64),
            (640, 64),
            (1600, 64),
        ];
        /// `(method, effort)` pairs carried into section 2: fastest, middle, and the
        /// crate's own default effort.
        const SHORTLIST: [(i32, f32); 3] = [(0, 20.0), (2, 50.0), (4, 75.0)];
        /// Tiles per shape. A 16×16 tiling of a 1600×1000 screen is 6250 tiles, and
        /// timing every one of them at 15 configs would take minutes for a ratio the
        /// sample already settles. Every table below prints the sample and the total.
        const SAMPLE: usize = 24;

        println!("\n=== 1. lossless config sweep at 320x64, against png::Compression::Fast ===");
        let (cells, total) = sample_tiles(&screen, sw, sh, 320, 64, SAMPLE);
        let runs = passes_for(cells.len(), 320, 64);
        let png: usize = cells.iter().map(|c| png_fast(320, 64, c).len()).sum();
        let png_time = time(runs, || {
            cells.iter().map(|c| png_fast(320, 64, c).len()).sum::<usize>()
        });
        println!(
            "  {} of {total} tiles, {png} png bytes, {:.1}µs/tile",
            cells.len(),
            png_time.as_secs_f64() * 1e6 / cells.len() as f64
        );
        println!("  config      bytes   vs png   µs/tile   vs png");
        for method in [0, 1, 2, 3, 4] {
            for effort in [20.0f32, 50.0, 75.0] {
                let bytes: usize = cells
                    .iter()
                    .map(|c| {
                        let data = webp_rgb(320, 64, c, true, effort, method);
                        assert_webp_roundtrips(320, 64, c, &data, "lossless sweep");
                        data.len()
                    })
                    .sum();
                let took = time(runs, || {
                    cells
                        .iter()
                        .map(|c| webp_rgb(320, 64, c, true, effort, method).len())
                        .sum::<usize>()
                });
                println!(
                    "  m{method} q{effort:<3.0}  {bytes:>7}  {:>6.2}x  {:>7.1}  {:>6.2}x",
                    bytes as f64 / png as f64,
                    took.as_secs_f64() * 1e6 / cells.len() as f64,
                    took.as_secs_f64() / png_time.as_secs_f64().max(f64::EPSILON),
                );
            }
        }

        println!("\n=== 2. the shortlist across every shape (sampled repaint totals) ===");
        println!("  shape      tiles     png B  µs/tile   config     B   vs png  µs/tile   vs png");
        for (w, h) in SHAPES {
            if w > sw || h > sh {
                println!("  {w}x{h}: larger than the screenshot, skipped");
                continue;
            }
            let (tiles, total) = sample_tiles(&screen, sw, sh, w, h, SAMPLE);
            let runs = passes_for(tiles.len(), w, h);
            let png: usize = tiles.iter().map(|t| png_fast(w, h, t).len()).sum();
            let png_time = time(runs, || {
                tiles.iter().map(|t| png_fast(w, h, t).len()).sum::<usize>()
            });
            let per_tile = |d: std::time::Duration| d.as_secs_f64() * 1e6 / tiles.len() as f64;
            let mut shape = format!("{w}x{h}");
            let mut count = format!("{}/{total}", tiles.len());
            for (method, effort) in SHORTLIST {
                let bytes: usize = tiles
                    .iter()
                    .map(|t| {
                        let data = webp_rgb(w, h, t, true, effort, method);
                        assert_webp_roundtrips(w, h, t, &data, "shape sweep");
                        data.len()
                    })
                    .sum();
                let took = time(runs, || {
                    tiles
                        .iter()
                        .map(|t| webp_rgb(w, h, t, true, effort, method).len())
                        .sum::<usize>()
                });
                println!(
                    "  {shape:<9} {count:>7} {png:>9} {:>8.1}   m{method} q{effort:<3.0} \
                     {bytes:>7}  {:>6.2}x {:>8.1}  {:>6.2}x",
                    per_tile(png_time),
                    bytes as f64 / png as f64,
                    per_tile(took),
                    took.as_secs_f64() / png_time.as_secs_f64().max(f64::EPSILON),
                );
                // The png columns belong to the shape, not to each config row.
                shape.clear();
                count.clear();
            }
        }

        println!("\n=== 3. lossy: WebP q80 against jpeg-encoder q80, on the same tiles ===");
        println!("  shape      tiles    jpeg B  µs/tile   config     B  vs jpeg  µs/tile  vs jpeg");
        for (w, h) in [(320u16, 64u16), (640, 64)] {
            let (tiles, total) = sample_tiles(&screen, sw, sh, w, h, SAMPLE);
            let runs = passes_for(tiles.len(), w, h);
            let jpeg: usize = tiles.iter().map(|t| jpeg_q80(w, h, t).len()).sum();
            let jpeg_time = time(runs, || {
                tiles.iter().map(|t| jpeg_q80(w, h, t).len()).sum::<usize>()
            });
            let per_tile = |d: std::time::Duration| d.as_secs_f64() * 1e6 / tiles.len() as f64;
            let mut shape = format!("{w}x{h}");
            let mut count = format!("{}/{total}", tiles.len());
            for method in [0, 2, 4] {
                let bytes: usize = tiles
                    .iter()
                    .map(|t| webp_rgb(w, h, t, false, 80.0, method).len())
                    .sum();
                let took = time(runs, || {
                    tiles
                        .iter()
                        .map(|t| webp_rgb(w, h, t, false, 80.0, method).len())
                        .sum::<usize>()
                });
                println!(
                    "  {shape:<9} {count:>7} {jpeg:>9} {:>8.1}   m{method} q80  {bytes:>7}  \
                     {:>6.2}x {:>8.1}  {:>6.2}x",
                    per_tile(jpeg_time),
                    bytes as f64 / jpeg as f64,
                    per_tile(took),
                    took.as_secs_f64() / jpeg_time.as_secs_f64().max(f64::EPSILON),
                );
                shape.clear();
                count.clear();
            }
        }

        // Whether the agent's classifier still earns its keep, which the codec swap
        // put back in question. Its premise is "many distinct colours means lossy is
        // much smaller", and that was measured against PNG — which cannot exploit a
        // smooth ramp, where WebP lossless can. So on real screen content: how many
        // tiles would the lossy branch actually make smaller, and by how much at
        // best? An oracle rather than the classifier itself, so this needs no copy
        // of it: the gap between "always lossless" and "best of the two per tile" is
        // the most any classifier could win.
        println!("\n=== 3b. is a lossy branch worth having on real screen content? ===");
        for (w, h) in [(320u16, 64u16)] {
            let (tiles, total) = sample_tiles(&screen, sw, sh, w, h, SAMPLE);
            let mut lossless_total = 0usize;
            let mut lossy_total = 0usize;
            let mut oracle_total = 0usize;
            let mut lossy_wins = 0usize;
            for tile in &tiles {
                let lossless = webp_rgb(w, h, tile, true, WEBP_LOSSLESS_EFFORT, WEBP_LOSSLESS_METHOD)
                    .len();
                let lossy = webp_rgb(w, h, tile, false, 80.0, WEBP_LOSSLESS_METHOD).len();
                lossless_total += lossless;
                lossy_total += lossy;
                oracle_total += lossless.min(lossy);
                if lossy < lossless {
                    lossy_wins += 1;
                }
            }
            println!("  {w}x{h}, {} of {total} tiles", tiles.len());
            println!("    always lossless   {lossless_total:>8}");
            println!("    always lossy      {lossy_total:>8}  ({:.2}x)", lossy_total as f64 / lossless_total as f64);
            println!(
                "    best of the two   {oracle_total:>8}  ({:.2}x)  — lossy smaller on {lossy_wins}/{} tiles",
                oracle_total as f64 / lossless_total as f64,
                tiles.len()
            );
        }

        println!("\n=== 4. worst case: uniform noise, nothing for either codec to find ===");
        for (w, h) in [(320u16, 64u16)] {
            let rgb = noise_rgb(w, h);
            let runs = passes_for(1, w, h);
            let png = png_fast(w, h, &rgb);
            let png_time = time(runs, || png_fast(w, h, &rgb));
            println!(
                "  {w}x{h} ({} raw): png {} bytes, {:.1}µs",
                rgb.len(),
                png.len(),
                png_time.as_secs_f64() * 1e6
            );
            for (method, effort) in SHORTLIST {
                let data = webp_rgb(w, h, &rgb, true, effort, method);
                assert_webp_roundtrips(w, h, &rgb, &data, "noise");
                let took = time(runs, || webp_rgb(w, h, &rgb, true, effort, method));
                println!(
                    "    lossless m{method} q{effort:<3.0} {:>7} {:>6.2}x  {:>8.1}µs  {:>6.2}x",
                    data.len(),
                    data.len() as f64 / png.len() as f64,
                    took.as_secs_f64() * 1e6,
                    took.as_secs_f64() / png_time.as_secs_f64().max(f64::EPSILON),
                );
            }
            let jpeg = jpeg_q80(w, h, &rgb);
            let jpeg_time = time(runs, || jpeg_q80(w, h, &rgb));
            println!(
                "    jpeg q80          {:>7}          {:>8.1}µs",
                jpeg.len(),
                jpeg_time.as_secs_f64() * 1e6
            );
            for method in [0, 2, 4] {
                let data = webp_rgb(w, h, &rgb, false, 80.0, method);
                let took = time(runs, || webp_rgb(w, h, &rgb, false, 80.0, method));
                println!(
                    "    lossy    m{method} q80  {:>7} {:>6.2}x  {:>8.1}µs  {:>6.2}x  (vs jpeg)",
                    data.len(),
                    data.len() as f64 / jpeg.len() as f64,
                    took.as_secs_f64() * 1e6,
                    took.as_secs_f64() / jpeg_time.as_secs_f64().max(f64::EPSILON),
                );
            }
        }
    }

    /// The codec this protocol used before WebP, at the compression it used, as the
    /// baseline the bench measures against.
    ///
    /// Spelled out here rather than reached through `Tile::from_rgb`, which is what
    /// it originally did — and which silently stopped being a baseline the moment
    /// `from_rgb` became WebP itself. The symptom was a table of `1.00x` ratios and
    /// byte-identical columns across seven shapes, which is at least loud; a bench
    /// whose control group is the thing under test is worth guarding against.
    fn png_fast(w: u16, h: u16, rgb: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, u32::from(w), u32::from(h));
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgb).unwrap();
        writer.finish().unwrap();
        out
    }

    /// The agent's current lossy encoder at the quality it ships (`LOSSY_QUALITY`
    /// in `crates/rxa-agent/src/encode.rs`), so section 3 above compares against
    /// what is actually deployed rather than a default.
    fn jpeg_q80(w: u16, h: u16, rgb: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 80)
            .encode(rgb, w, h, jpeg_encoder::ColorType::Rgb)
            .expect("jpeg encode failed");
        out
    }
}
