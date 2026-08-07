//! VNC client, in two dialects that share everything below the handshake.
//!
//! **RFB 3.8**, which is every VNC server including a Mac's under `subtype =
//! "ard"`: classic or Apple DH authentication. A Mac also accepts Apple's
//! metadata and pasteboard messages on this transport, exposing the Mac's physical
//! display list, selection and density. After the first layout, a second encoding
//! request switches the rectangles from raw to zlib.
//!
//! **RFB 003.889**, Apple's own revision, under `subtype =
//! "ard-high-performance"`: the same RFB messages carried inside an AES-128-CBC
//! record layer ([`crate::vnc_record`]), alongside Apple's control messages
//! ([`crate::vnc_apple`]). This mode requests one virtual display at the target's
//! configured `width` and `height`. Its pasteboard is a separate Apple protocol
//! rather than RFB Extended Clipboard. See docs/apple-vnc-889.md.
//!
//! The transport difference is contained in three places and nowhere else:
//! [`Dialect`] (which banner and ClientInit byte), the two preface functions after
//! ServerInit, and the optional record wrapper. One read loop, one input path, one
//! Apple metadata path and one tile path serve both.

use std::collections::HashMap;
use std::sync::Arc;

use aes::Aes128;
use des::Des;
use des::cipher::generic_array::GenericArray;
use des::cipher::{BlockEncrypt as _, KeyInit as _};
use md5::{Digest as _, Md5};
use num_bigint::BigUint;
use rand::Rng as _;
use log::{debug, info, warn};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc};

use crate::config::{Subtype, TargetConfig};
use crate::encode::TileSink;
use crate::engine::{self, clamp_u16, host_port};
use crate::keymap;
use crate::protocol::{
    self, ClientMsg, ClipboardSnapshot, CursorShape, DisplayInfo, MAX_CLIPBOARD_BYTES,
    MAX_CURSOR_DIM, MouseButton, ServerMsg, UNSCALED, WheelUnit, clipboard_fits,
};
use crate::tiles::{self, Rect, Shadow};
use crate::vnc_apple::{self, CursorCache};
use crate::vnc_encodings::{Decoded, Decoders, Payload};
use crate::vnc_apple_clipboard;
use crate::vnc_clipboard;
use crate::vnc_record::{self, Keys, RecordReader, RecordWriter};

const SECURITY_NONE: u8 = 1;
const SECURITY_VNC_AUTH: u8 = 2;
/// Apple's Diffie-Hellman authentication, the one RFB security type that
/// carries a *user name* — see [`ard_authenticate`] for why a Mac target needs
/// it and what happens without it.
const SECURITY_ARD: u8 = 30;
/// Largest DH key length accepted from the server, in bytes. macOS sends 128
/// (a 1024-bit prime); the cap is what keeps a bogus length from turning into
/// a huge allocation and a very slow modular exponentiation.
const MAX_ARD_KEY_BYTES: usize = 512;
/// Smallest DH key length accepted, in bytes. The server picks the group, and
/// what rides inside it is an account password, so a small prime is not a
/// server being frugal — it is a shared secret anyone watching the wire can
/// recover.
///
/// 128 bytes: the 1024 bits macOS 26 sends, with no room below it. The 512-bit
/// group Apple's own documentation calls the "older, less secure method" is
/// therefore refused rather than downgraded to, which is the point. If a Mac old
/// enough to still offer it ever turns up, this is what it will fail on, and the
/// error says so — a refusal being the honest answer for a group that would put
/// an account password behind precomputation anyone can afford.
const MIN_ARD_KEY_BYTES: usize = 128;
/// Apple's credential blob: `username[64]`, then `password[64]`, each
/// null-terminated, the remainder random.
const ARD_CREDENTIALS_LEN: usize = 128;
const ARD_FIELD_LEN: usize = 64;
const ENCODING_RAW: i32 = 0;
/// CopyRect: two `u16`s naming where in the framebuffer this rectangle's pixels
/// already are, and no pixels at all.
const ENCODING_COPY_RECT: i32 = 1;
/// RRE: a background colour and a run of coloured sub-rectangles over it.
const ENCODING_RRE: i32 = 2;
/// Hextile: RRE applied to each 16x16 tile of the rectangle in turn.
const ENCODING_HEXTILE: i32 = 5;
/// ZRLE: 64x64 tiles, run-length encoded or palettised, inside a deflate stream.
/// The best of the lossless standard encodings and the one RFC 6143 defines for the
/// job.
const ENCODING_ZRLE: i32 = 16;
/// Standard RFB zlib: `u32 length` then that many bytes of one deflate stream
/// shared by every rectangle on the connection. Not a vendor encoding and not
/// Apple's alone, though Apple's High Performance mode is where it arrived here
/// first — see [`vnc_apple::ENCODINGS_WITH_ZLIB`].
pub(crate) const ENCODING_ZLIB: i32 = 6;
/// Cursor pseudo-encoding: the server hands over the pointer shape (pixels +
/// a 1-bit mask, the rect's x/y being the hotspot) instead of drawing it into
/// the framebuffer.
const ENCODING_CURSOR: i32 = -239;
/// DesktopSize pseudo-encoding: the server announces a new framebuffer size.
const ENCODING_DESKTOP_SIZE: i32 = -223;
/// ExtendedDesktopSize pseudo-encoding: size announcements with a screen
/// layout, and the server's declaration that it accepts SetDesktopSize.
const ENCODING_EXTENDED_DESKTOP_SIZE: i32 = -308;
/// LastRect pseudo-encoding: this update has no more rectangles, whatever its
/// header's count said. Servers use it to start sending an update before they know
/// how many rectangles it will hold, declaring `0xffff` of them.
const ENCODING_LAST_RECT: i32 = -224;
/// Fence pseudo-encoding: the server may send a marker down the stream and ask for
/// it back, which is how it measures the round trip and sizes its congestion window.
///
/// Advertised for [`ENCODING_CONTINUOUS_UPDATES`]'s sake rather than for its own:
/// with updates arriving unasked, echoing fences is the only thing left telling the
/// server how fast this end is actually keeping up.
const ENCODING_FENCE: i32 = -312;
/// ContinuousUpdates pseudo-encoding: ask the server to send framebuffer updates
/// for a region as it changes, rather than one per request.
const ENCODING_CONTINUOUS_UPDATES: i32 = -313;
/// EndOfContinuousUpdates, both a support announcement and the acknowledgement of a
/// disable. Server message type; there is no client message with this number.
const MSG_END_OF_CONTINUOUS_UPDATES: u8 = 150;
/// ServerFence and ClientFence share a message type in the two directions.
const MSG_FENCE: u8 = 248;
/// A fence the server wants echoed. Nothing else in the flags word obliges a
/// client, and the two it may keep are [`FENCE_BLOCK_BEFORE`] and
/// [`FENCE_BLOCK_AFTER`].
const FENCE_REQUEST: u32 = 1 << 31;
const FENCE_BLOCK_BEFORE: u32 = 1 << 0;
const FENCE_BLOCK_AFTER: u32 = 1 << 1;
/// Longest fence payload the extension defines. A server sending more is malformed;
/// the excess is consumed to keep the stream in step and left out of the echo.
const MAX_FENCE_PAYLOAD: usize = 64;
/// Bytes per pixel of the format we force with SetPixelFormat.
pub(crate) const BPP: usize = 4;
/// Cap on server-sent reason/name strings, so a bogus length can't OOM us.
const MAX_STRING: u32 = 1024;
/// Cap on an Apple cursor rect's compressed payload. Its size is not implied by
/// the rect header the way a raw cursor's is — a *select* carries zeroed geometry
/// — so the length has to be bounded on its own.
const MAX_CURSOR_BYTES: u64 = 1 << 20;

type Reader = BufReader<OwnedReadHalf>;

/// Which RFB dialect a target's subtype puts on the wire.
///
/// One value, read once from the config, standing in for what would otherwise be
/// a subtype test at four points in the handshake. Everything after ServerInit is
/// decided by which preface function ran, not by re-asking this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dialect {
    /// RFB 3.8, used by generic VNC and Apple Screen Sharing Standard mode
    /// (`subtype = "ard"`). Apple authentication and metadata are layered above.
    Rfb38,
    /// Apple's RFB 003.889 and the record layer that goes up after ServerInit.
    Apple889,
}

impl Dialect {
    fn of(subtype: Option<Subtype>) -> Self {
        match subtype {
            Some(Subtype::ArdHighPerformance) => Dialect::Apple889,
            Some(Subtype::Ard) | None => Dialect::Rfb38,
        }
    }

    /// The version this client answers the server's greeting with.
    fn banner(self) -> &'static [u8; 12] {
        match self {
            Dialect::Rfb38 => b"RFB 003.008\n",
            Dialect::Apple889 => b"RFB 003.889\n",
        }
    }

    /// The ClientInit byte. Nominally RFB's shared-session flag; Apple's server
    /// wants a particular value there and the bits above the low one are what tell
    /// it a viewer speaking its own revision is on the other end.
    fn client_init(self) -> u8 {
        match self {
            // Share the session: don't kick other clients. The single-session
            // policy lives in this program, not on the VNC server.
            Dialect::Rfb38 => 1,
            Dialect::Apple889 => 0xc1,
        }
    }
}

/// The session's byte source.
///
/// `Plain` is the socket as RFB has always been read. `Records` is the same
/// socket with Apple's record layer peeled off — and because that peeling is an
/// [`AsyncRead`], every `read_u8`/`read_exact` above here is identical in both
/// dialects and a rectangle whose pixels span four records is still one
/// `read_exact`.
enum Downlink {
    Plain(Reader),
    /// Boxed because a record reader is an order of magnitude larger than a bare
    /// one (two AES key schedules and a staging buffer), and every plain-RFB
    /// session would otherwise carry that on the stack for nothing.
    Records(Box<RecordReader<Reader>>),
}

impl AsyncRead for Downlink {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Downlink::Plain(r) => std::pin::Pin::new(r).poll_read(cx, buf),
            Downlink::Records(r) => std::pin::Pin::new(r).poll_read(cx, buf),
        }
    }
}

/// Everything the session sends, one complete client message per call.
///
/// A message sink rather than an [`AsyncWrite`], because on the 003.889 wire the
/// framing unit *is* a message: one record carries exactly one of them. Two
/// messages written back to back would land in a single record and the server
/// would read the first and discard the second — which is why
/// [`translate_input`] returns a list rather than a buffer.
struct Uplink {
    /// Boxed rather than a type parameter: this is written once per input event,
    /// so a vtable hop costs nothing measurable, and it keeps [`Shared`] and every
    /// rect handler free of a `W`.
    sock: Box<dyn AsyncWrite + Send + Unpin>,
    /// `None` until the record layer is up, which on the plain dialect is never.
    records: Option<RecordWriter>,
}

impl Uplink {
    fn plain(sock: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self {
            sock: Box::new(sock),
            records: None,
        }
    }

    fn records(sock: impl AsyncWrite + Send + Unpin + 'static, keys: Keys) -> Self {
        Self {
            sock: Box::new(sock),
            records: Some(RecordWriter::new(keys)),
        }
    }

    async fn send(&mut self, msg: &[u8]) -> anyhow::Result<()> {
        let Self { sock, records } = self;
        match records {
            Some(records) => sock.write_all(records.frame(msg)?).await?,
            None => sock.write_all(msg).await?,
        }
        Ok(())
    }
}

type SharedUplink = Arc<Mutex<Uplink>>;

/// Send one complete message.
async fn send(uplink: &SharedUplink, msg: &[u8]) -> anyhow::Result<()> {
    uplink.lock().await.send(msg).await
}

/// Send several, in order, stopping at the first failure. The lock is taken once
/// so nothing can interleave between them — a wheel notch's press and release
/// must not be split by a pointer move.
async fn send_all(uplink: &SharedUplink, msgs: &[Vec<u8>]) -> anyhow::Result<()> {
    let mut uplink = uplink.lock().await;
    for msg in msgs {
        uplink.send(msg).await?;
    }
    Ok(())
}

/// One screen in the server's ExtendedDesktopSize layout. Only the id and
/// flags matter here: SetDesktopSize echoes them back with new dimensions.
#[derive(Debug, Clone, Copy)]
struct Screen {
    id: u32,
    flags: u32,
}

/// Desktop geometry, shared between the read loop (which learns about
/// resizes and server support) and the input side (which requests them).
/// The lock is never held across an await.
#[derive(Debug)]
struct DesktopState {
    /// Current framebuffer size, in pixels.
    size: (u16, u16),
    /// Pixels per point: how large `size` should be *shown*, as opposed to how
    /// many pixels it has.
    ///
    /// Always [`UNSCALED`] on non-Apple RFB, where a framebuffer is just its pixels
    /// and no server says otherwise. Apple's display layout does say otherwise —
    /// a Retina screen renders at twice its logical size — and reporting only the
    /// pixel count there would give the browser a canvas at half the size the Mac
    /// thinks it is.
    scale: f32,
    /// The density of the screen the client's window is on, from
    /// [`ClientMsg::HostScale`]. 1.0 until the client says otherwise.
    ///
    /// Only High Performance resize spends it: a virtual display renders `points ×
    /// host_density` pixels, so a Retina client gets a Retina desktop. `scale` is
    /// what the *remote* granted; the two disagree exactly while a density change
    /// is in flight.
    host_density: f32,
    /// First screen of the server's layout. `Some` only once the server has
    /// sent an ExtendedDesktopSize rect — its declaration that SetDesktopSize
    /// is supported; nothing is requested before that.
    screen: Option<Screen>,
    /// A browser viewport report that arrived before support was declared,
    /// replayed on the first ExtendedDesktopSize rect.
    pending: Option<(u16, u16)>,
}

impl DesktopState {
    /// The size and scale, as a client is told them.
    fn resize_msg(&self) -> ServerMsg {
        ServerMsg::Resize {
            w: self.size.0,
            h: self.size.1,
            scale: self.scale,
        }
    }
}

type SharedDesktop = Arc<std::sync::Mutex<DesktopState>>;

/// The Mac's screens and which one is being shared. Empty on non-Apple RFB and
/// until the first layout arrives.
#[derive(Debug, Default)]
struct DisplayState {
    displays: Vec<DisplayInfo>,
    /// Union area the next non-incremental Apple update is expected to paint.
    /// A combined framebuffer may include gaps which never arrive as rectangles.
    repaint_pixels: u64,
    /// The entry a client's checkmark sits on: a screen id, or
    /// [`DisplayState::COMBINED`].
    ///
    /// Only ever written from a layout, which is the Mac naming the screen it is
    /// sending. So a selection the Mac declines leaves the menu agreeing with what
    /// is on the canvas rather than with what was clicked — client state is never
    /// optimistic here, see [`ServerMsg::Displays`].
    active: u32,
}

impl DisplayState {
    /// The list entry for every screen at once, which is the state a session
    /// starts in and the only one a client cannot name by `CGDirectDisplayID`.
    ///
    /// `0xffffffff` because that is already the sentinel Apple's own wire uses for
    /// it, in both directions: the `combine_all_displays` request and the
    /// `current_display` a layout answers with.
    const COMBINED: u32 = u32::MAX;

    /// The message that tells a client the list and the selection, or `None` while
    /// there is nothing to choose between.
    fn displays_msg(&self) -> Option<ServerMsg> {
        (!self.displays.is_empty()).then(|| ServerMsg::Displays {
            active: self.active,
            displays: self.displays.clone(),
        })
    }
}

type SharedDisplay = Arc<std::sync::Mutex<DisplayState>>;

/// Apple's display/cursor decoding state, owned by the read loop.
///
/// Not in [`Shared`]: the cursor cache is touched by nothing else, and a lock on
/// the pixel path to say so would be a lock that never contends. The zlib stream
/// used to live here too; it moved to [`vnc_encodings::Decoders`] when encoding 6
/// stopped being the Apple dialect's alone.
#[derive(Default)]
struct Apple {
    cursors: CursorCache,
    /// True for High Performance mode, whose setup requested a virtual display.
    /// Layout records do not carry this fact themselves.
    virtual_display: bool,
    /// Whether zlib has been asked for yet.
    ///
    /// It cannot be in the first `SetEncodings` — see
    /// [`vnc_apple::ENCODINGS_WITH_ZLIB`] — so it is asked for in a second one, once
    /// the Mac has reported its displays and there is nothing left to lose by it.
    /// Once, hence the flag: a layout arrives at every login and lock.
    ///
    /// Both subtypes do this. The upgrade rides on the display layout, which plain
    /// `ard` reports just as High Performance does, so gating it by subtype only cost
    /// bandwidth — measured at 6.19 MB of raw against 3.38 MB of zlib for the same
    /// 800x600 desktop, on a mode whose framebuffer is a physical screen and can be
    /// far larger than that.
    asked_for_zlib: bool,
}

impl Apple {
    /// The read loop's starting state for either Apple subtype.
    ///
    /// `high_performance` settles one thing only — whether a virtual display was
    /// asked for. It must not reach [`Apple::asked_for_zlib`]: presetting that flag
    /// is how a subtype opts *out* of compression, and neither should.
    fn new(high_performance: bool) -> Self {
        Self { virtual_display: high_performance, ..Self::default() }
    }
}

/// The pixels the browser has already been sent, so an update carrying none of
/// them costs nothing and one carrying a few is sent as those few.
///
/// Shared because the two halves of the session both have a say: the read loop
/// compares every rect against it, and the input side forgets it on `Refresh`.
/// The lock is never held across an await, as with every other lock here.
type SharedShadow = Arc<std::sync::Mutex<Shadow>>;

/// What the browser should draw for the pointer, tracked so a browser that
/// (re)attaches mid-session gets it replayed — the server only sends the shape
/// when it changes, which may have been long before this browser showed up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum CursorState {
    /// No Cursor rect has arrived: the server is compositing the pointer into
    /// the framebuffer itself, so the browser must not draw one.
    #[default]
    ServerDrawn,
    /// The server owns the shape and has currently hidden the pointer.
    Hidden,
    /// The latest shape the server sent.
    Shape(CursorShape),
}

type SharedCursor = Arc<std::sync::Mutex<CursorState>>;

/// Both ends of the clipboard bridge, shared between the read loop (which
/// fills `remote` and learns the server's capabilities) and the input side
/// (which answers a Fetch from `remote` and records `local`).
///
/// Standard RFB has no "read the clipboard" request — the server pushes whenever
/// the remote clipboard changes — so `remote` keeps the latest text to answer
/// [`ClientMsg::ClipboardRequest`]. Apple's pasteboard does have a fetch; a
/// request is forwarded there and its reply refreshes this same cache.
#[derive(Debug, Default)]
struct ClipboardState {
    /// What the remote last sent. `None` means nothing has been copied there
    /// this session.
    remote: Option<ClipboardSnapshot>,
    /// What the browser last sent, held until the server asks for it. Only the
    /// extended path defers like that; the latin-1 fallback writes immediately
    /// and never reads this.
    local: Option<String>,
    /// What the server said it can do, from its Extended Clipboard caps.
    /// `None` until caps arrive, which is also how "the server does not speak
    /// the extension, use latin-1" is spelled — see [`crate::vnc_clipboard`].
    server: Option<vnc_clipboard::Caps>,
    /// Opaque value echoed by Apple's pasteboard messages. Zero until the Mac
    /// supplies one, which is also the value its first fetch uses.
    apple_session_id: u32,
    /// Browser reads waiting for the next native Apple pasteboard response.
    /// The panel issues only one at a time, but count them so the wire remains
    /// correct if another client does not make that UI guarantee.
    apple_requests: usize,
}

type SharedClipboard = Arc<std::sync::Mutex<ClipboardState>>;

/// Connect to the VNC host, then drive the session until it ends.
///
/// `input_rx` carries browser input; `frame_tx` carries screen updates back.
/// Either closing (browser gone / VNC ended) tears the session down.
///
/// A thin wrapper so the shutdown cannot be missed — see [`crate::rdp::run`], which has
/// the same shape for the same reason: the engine thread's runtime dies with this
/// function, and the sink forwards from a task of its own.
pub async fn run(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
    feedback: Arc<crate::feedback::LinkFeedback>,
) {
    let sink = TileSink::new("vnc", frame_tx, config.render_plan(), feedback);
    session(config, input_rx, &sink).await;
    sink.finish().await;
}

async fn session(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    sink: &TileSink,
) {
    // The budget covers the RFB handshake, which can stall on a host that accepts
    // the connection and then says nothing — no socket timeout catches that. The
    // TCP connect has its own deadline inside the helper, so a slow one is
    // reported as what it is rather than as a handshake that ran long.
    let dest = host_port(&config.host, config.port);
    let Some(connected) = engine::connect_and_handshake(
        "vnc",
        &dest,
        engine::HANDSHAKE_TIMEOUT,
        sink,
        |stream| connect(&config, stream),
    )
    .await
    else {
        return;
    };

    let Connected { downlink, uplink, width, height, macos, apple, poll } = connected;
    info!("vnc: connected, desktop {width}x{height} (macos={macos})");
    if sink
        .msg(ServerMsg::Resize {
            w: width,
            h: height,
            scale: UNSCALED,
        })
        .await
        .is_err()
    {
        return; // browser already gone
    }
    if sink.msg(ServerMsg::RemoteOs { macos }).await.is_err() {
        return; // browser already gone
    }

    if let Err(e) = active_loop(
        downlink,
        uplink,
        (width, height),
        Flags {
            macos,
            resize: config.resize,
            clipboard: config.clipboard,
            default_size: (config.width, config.height),
            apple,
            high_performance: Dialect::of(config.subtype) == Dialect::Apple889,
            poll,
        },
        input_rx,
        sink.clone(),
    )
    .await
    {
        warn!("vnc: session error: {e:#}");
        let _ = sink
            .msg(ServerMsg::Error {
                message: format!("VNC session ended: {e}"),
            })
            .await;
    }
    info!("vnc: session terminated");
}

/// The per-session switches [`active_loop`] needs: one discovered from the
/// handshake, the rest read off the target profile.
struct Flags {
    macos: bool,
    resize: bool,
    clipboard: bool,
    /// The target's configured `width`/`height`, which is what
    /// [`ClientMsg::DefaultSize`] resolves to here. Carried rather than read from
    /// the config at the point of use because [`active_loop`] is given the
    /// handshaken link and these switches, not the profile behind them.
    ///
    /// Generic VNC and Standard `ard` consult it only when a client asks.
    /// High Performance mode also uses it during setup for its virtual display;
    /// retaining it here keeps `DefaultSize` consistent with that configured mode.
    default_size: (u16, u16),
    /// Whether Apple's metadata encodings were negotiated, giving the read loop
    /// its zlib stream, cursor cache and display list to report. Both Apple
    /// subtypes negotiate them; only one uses the 003.889 record transport.
    apple: bool,
    /// Whether this is Apple's High Performance mode. It requests a virtual display
    /// during setup and asks for zlib after the first layout; plain `ard` does
    /// neither.
    high_performance: bool,
    /// Whether the client drives the update cycle — see [`Connected::poll`].
    poll: bool,
}

/// What the read loop needs to know about the dialect it is reading. Two bools
/// with names on them, because at the call site they are indistinguishable.
#[derive(Clone, Copy)]
struct ReadFlags {
    clipboard: bool,
    poll: bool,
}

/// An established, handshaken RFB link, plus what the handshake revealed about
/// the far side. A named struct rather than a tuple nobody can read at the call
/// site.
struct Connected {
    downlink: Downlink,
    uplink: Uplink,
    width: u16,
    height: u16,
    /// Whether the server is macOS Screen Sharing — see [`is_macos_server`].
    macos: bool,
    /// Whether the preface negotiated Apple's display/cursor encodings.
    apple: bool,
    /// Whether the client drives the update cycle: one request, one update, repeat.
    ///
    /// True on both dialects, and on the 003.889 one that is a measurement rather
    /// than a default. The reference says `AutoFrameBufferUpdate` switches the
    /// server to sending on its own and that a client should then stop asking;
    /// macOS 26 does not — armed or not, it answers a non-incremental request and
    /// is otherwise silent, even while the screen is changing under a moving
    /// pointer. A client that took the reference at its word would paint one frame
    /// and then freeze.
    poll: bool,
}

/// ServerInit, as much of it as anything here uses.
struct ServerInit {
    width: u16,
    height: u16,
}

impl ServerInit {
    fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }
}

/// RFB version/security handshake → ClientInit/ServerInit → the dialect's
/// preface, on a connected socket.
///
/// Reads as the sequence it is, with the one branch at the end: everything above
/// that point is common to both dialects, including Apple's authentication, which
/// plain `subtype = "ard"` uses on the 3.8 wire.
///
/// The TCP connect happens in [`run`] (see [`engine::connect_and_handshake`]) so
/// its deadline and this handshake's are sequential rather than nested. The
/// 003.889 preface waits for the server's rekey, which puts *that* wait inside the
/// same budget — a Mac that authenticates and then says nothing is reported as a
/// handshake that ran long, not as a live session with a blank canvas.
async fn connect(config: &TargetConfig, stream: tokio::net::TcpStream) -> anyhow::Result<Connected> {
    let dialect = Dialect::of(config.subtype);
    let (read_half, mut sock) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let minor = read_version(&mut reader).await?;
    sock.write_all(dialect.banner()).await?;

    let types = read_security_types(&mut reader).await?;
    let macos = is_macos_server(minor, &types);
    let chosen = choose_security(&types, config.subtype, &config.vnc_password)?;
    if macos && chosen != SECURITY_ARD {
        // Said once, at the only moment it can still be acted on, because the
        // symptom is otherwise unreadable: a login screen that will not accept
        // the account already signed in on that Mac.
        warn!(
            "vnc: this server is a Mac and the target has no Apple subtype — macOS answers \
             an anonymous viewer with a new login window on a virtual display rather than its \
             own screen. Set subtype = \"ard\" with a macOS account's username and password \
             to share the screen."
        );
    }
    sock.write_all(&[chosen]).await?;

    let wrap_key = authenticate(&mut reader, &mut sock, config, chosen).await?;
    read_security_result(&mut reader).await?;
    sock.write_all(&[dialect.client_init()]).await?;
    let server = read_server_init(&mut reader).await?;

    match dialect {
        Dialect::Rfb38 => rfb38_preface(reader, sock, server, macos, config).await,
        Dialect::Apple889 => apple_preface(reader, sock, server, macos, wrap_key, config).await,
    }
}

/// The server's version greeting, answered by the caller. Returns the minor
/// number, which is one of the two things that identifies a Mac.
async fn read_version<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<u32> {
    let mut greeting = [0u8; 12];
    reader.read_exact(&mut greeting).await?;
    let (major, minor) =
        parse_version(&greeting).ok_or_else(|| anyhow::anyhow!("not an RFB server: {greeting:?}"))?;
    anyhow::ensure!(
        major > 3 || (major == 3 && minor >= 8),
        "unsupported RFB version {major}.{minor} (this client requires 3.8+)"
    );
    Ok(minor)
}

/// The security types on offer. An empty list is not an empty list — it is RFB's
/// way of refusing the connection, with the reason following it.
async fn read_security_types<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    let count = reader.read_u8().await?;
    if count == 0 {
        anyhow::bail!(
            "VNC server refused the connection: {}",
            read_string(reader).await?
        );
    }
    let mut types = vec![0u8; usize::from(count)];
    reader.read_exact(&mut types).await?;
    Ok(types)
}

/// Run the chosen security type's exchange.
///
/// Returns the record layer's initial wrap key, which only Apple's DH branch
/// produces and only the 003.889 dialect goes on to use. It is `MD5(shared)` —
/// the very digest that encrypted the credentials — so the plain path has always
/// computed it and thrown it away.
async fn authenticate<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    sock: &mut W,
    config: &TargetConfig,
    chosen: u8,
) -> anyhow::Result<Option<[u8; 16]>> {
    match chosen {
        SECURITY_ARD => Ok(Some(
            ard_authenticate(reader, sock, &config.username, &config.password).await?,
        )),
        SECURITY_VNC_AUTH => {
            let mut challenge = [0u8; 16];
            reader.read_exact(&mut challenge).await?;
            sock.write_all(&auth_response(&config.vnc_password, &challenge))
                .await?;
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// SecurityResult, which RFB 3.8 sends for every type including None.
async fn read_security_result<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<()> {
    if reader.read_u32().await? != 0 {
        anyhow::bail!(
            "VNC authentication failed: {}",
            read_string(reader).await?
        );
    }
    Ok(())
}

/// ServerInit: desktop size, the server's native pixel format (ignored — we
/// override it), and the desktop name.
async fn read_server_init<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<ServerInit> {
    let width = reader.read_u16().await?;
    let height = reader.read_u16().await?;
    let mut native_format = [0u8; 16];
    reader.read_exact(&mut native_format).await?;
    // Read outside the `debug!`, which does not evaluate its arguments when the
    // level is off — leaving the name field on the stream and every rectangle after
    // it misaligned.
    let name = read_bytes(reader).await?;
    debug!("vnc: server desktop {}", describe_desktop(&name));
    anyhow::ensure!(width > 0 && height > 0, "server reported a {width}x{height} desktop");
    Ok(ServerInit { width, height })
}

/// Describe ServerInit's name field, which on Apple's revision is not a name.
///
/// A Mac prefixes it with 22 bytes: a zero marker, a `u32` of session flags, and a
/// 16-byte capability bitmap, with the UTF-8 name after all of it. Printing the lot
/// as a string gave a log line of mojibake with the real name buried in it, and
/// hid the flags — of which one, `0x04`, would mean a whole negotiation follows
/// ServerInit that this client does not implement. Saying so is the point of
/// reading them; nothing here is acted on.
///
/// Anything that is not shaped like that is a name, which is what every other
/// server sends.
fn describe_desktop(field: &[u8]) -> String {
    if field.len() < 22 || field[0] != 0 {
        return format!("{:?}", String::from_utf8_lossy(field));
    }
    let flags = u32::from_be_bytes(field[2..6].try_into().expect("four bytes of flags"));
    let name = String::from_utf8_lossy(&field[22..]);
    let mut named: Vec<&str> = [(0x01, "observe"), (0x02, "may-control"), (0x08, "no-virtual-display")]
        .into_iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, name)| name)
        .collect();
    // Called out rather than listed with the rest: a server that offers it expects
    // a SessionInfo/SessionCommand/SessionResult exchange before anything else, and
    // the symptom of not answering is a session that stops here in silence.
    if flags & 0x04 != 0 {
        named.push("SESSION-SELECT, which this client does not implement");
    }
    format!("{name:?} (Apple flags {flags:#010x}: {})", named.join(", "))
}

/// The RFB 3.8 tail: force our pixel format and the encoding set.
async fn rfb38_preface(
    reader: Reader,
    sock: OwnedWriteHalf,
    server: ServerInit,
    macos: bool,
    config: &TargetConfig,
) -> anyhow::Result<Connected> {
    let mut uplink = Uplink::plain(sock);
    let apple = config.subtype == Some(Subtype::Ard);
    if apple {
        // Native Standard announces itself and its control mode before enabling
        // pasteboard monitoring. Without this prelude the Mac accepts writes and
        // explicit fetches but does not emit clipboard-change status messages.
        uplink.send(&vnc_apple::viewer_info()).await?;
        uplink.send(&vnc_apple::set_mode_control()).await?;
    }
    uplink.send(&set_pixel_format()).await?;
    uplink
        .send(&set_encodings(&rfb38_encoding_list(
            apple,
            config.resize,
            config.clipboard,
        )))
        .await?;
    if apple && config.clipboard {
        uplink.send(&vnc_apple_clipboard::auto_pasteboard(true)).await?;
    }

    Ok(Connected {
        downlink: Downlink::Plain(reader),
        uplink,
        width: server.width,
        height: server.height,
        macos,
        apple,
        poll: true,
    })
}

fn rfb38_encoding_list(apple: bool, resize: bool, clipboard: bool) -> Vec<i32> {
    if apple {
        // A Mac sends the same display layout and accepts the same display picker
        // on its downgraded 3.8 wire. Keep this measured list exact and zlib-free —
        // zlib here costs the layout, so both subtypes ask for it in the second
        // `SetEncodings` a layout triggers — and note that the native pasteboard is
        // negotiated by `AutoPasteboard`, not an RFB encoding.
        return vnc_apple::ENCODINGS.to_vec();
    }

    // A preference order, because a server reads it as one: it encodes with the
    // first entry it supports and keeps that choice for the session.
    //
    // CopyRect leads because it is not a competitor. It carries no pixels, so a
    // server does not pick it *instead* of something — it uses it for scrolls and
    // window moves whatever else it chose. ZRLE is first among the pixel encodings:
    // it takes the redundancy out tile by tile before deflate sees the bytes, so it
    // beats plain zlib on interface content, and RFC 6143 defines it, so a modern
    // server has it. zlib next for the servers that do not. Hextile and RRE are the
    // uncompressed fallbacks, in the order of how much they usually save. Raw last —
    // the encoding every server has and none should choose.
    //
    // Deliberately absent: Tight and TightPNG are vendor encodings, JPEG and H.264
    // are lossy, and a gateway that re-encodes every tile for the browser anyway
    // gains nothing from pixels that have already lost information. Advertising an
    // encoding is a promise to decode it.
    //
    // Cursor is unconditional (the browser can always draw a pointer). The resize
    // pseudo-encodings are advertised only when the target opts in; without them
    // the server never announces support and keeps its connect-time size.
    //
    // ContinuousUpdates and Fence are unconditional and go together. The first asks
    // the server to send updates for the whole desktop as it changes instead of once
    // per request, which removes a round trip from every frame — and with it the
    // request cadence that was this engine's only pacing, which is what the second is
    // for: the server measures the link by fences it asks this end to echo, and
    // cannot do that unless the pseudo-encoding is in this list. A server with
    // neither is unaffected: it says nothing, and the polling loop below never stops.
    let mut encodings = vec![
        ENCODING_COPY_RECT,
        ENCODING_ZRLE,
        ENCODING_ZLIB,
        ENCODING_HEXTILE,
        ENCODING_RRE,
        ENCODING_RAW,
        ENCODING_CURSOR,
        ENCODING_CONTINUOUS_UPDATES,
        ENCODING_FENCE,
    ];
    if resize {
        encodings.push(ENCODING_EXTENDED_DESKTOP_SIZE);
        encodings.push(ENCODING_DESKTOP_SIZE);
    }
    if clipboard {
        // Extended Clipboard is the only way generic RFB carries anything outside
        // latin-1. A server that ignores it never sends caps and the fallback stays
        // in use.
        encodings.push(vnc_clipboard::ENCODING);
    }
    encodings
}

/// The virtual display a High Performance session opens with.
///
/// A resizable target ignores its configured `width`/`height` here and opens at
/// the dynamic backing ceiling, the way Apple's own client does: the size the
/// session should be is the client window's, which arrives as a viewport report
/// moments later, and shrinking a large display keeps the window layout that
/// opening small destroys. The configured size is the opening mode only where no
/// window will ever report one — `resize = false`, the fixed-size and mobile
/// profile.
fn opening_mode(config: &TargetConfig) -> vnc_apple::VirtualMode {
    if config.resize {
        vnc_apple::maximum_mode()
    } else {
        vnc_apple::virtual_display_mode((config.width, config.height), 1.0)
    }
}

/// The RFB 003.889 tail: Apple's cleartext prelude, the wait for the rekey, then
/// the encrypted preface and the arming that replaces polling.
///
/// The one function in this file that knows the record layer is switched on here,
/// which is deliberate: it runs before [`Connected`] exists, so there is no input
/// task and no second holder of the writer. The alternative — noticing the rekey
/// inside [`read_loop`] — has a race with no fix, because the server rotates
/// *both* its own ciphers the instant it sends the rekey: a mouse move delivered
/// in the window before this side catches up goes out in cleartext to a server
/// that is already decrypting, and the session is unrecoverable. Doing it here
/// makes that structurally impossible rather than unlikely.
async fn apple_preface(
    mut reader: Reader,
    mut sock: OwnedWriteHalf,
    server: ServerInit,
    macos: bool,
    wrap_key: Option<[u8; 16]>,
    config: &TargetConfig,
) -> anyhow::Result<Connected> {
    let wrap_key = wrap_key.ok_or_else(|| {
        anyhow::anyhow!(
            "Apple's protocol revision needs its DH authentication, which this server did not offer"
        )
    })?;

    // With clipboard enabled, the native control prelude is written back to back
    // before encryption. The server emits the rekey as soon as encryption starts,
    // so anything that waited for a reply in between would risk writing cleartext
    // to a server that had already switched. ViewerInfo's body is the measured
    // fixed numeric form, not the mis-sized string form in the reverse-engineered
    // reference.
    // ViewerInfo + SetMode are required for automatic pasteboard notifications on
    // the live High Performance server, just as they are in Standard mode. The
    // AutoPasteboard enable itself must also be cleartext: sending it as the first
    // encrypted record is accepted without error but produces no status or data.
    if config.clipboard {
        sock.write_all(&vnc_apple::viewer_info()).await?;
        sock.write_all(&vnc_apple::set_mode_control()).await?;
        sock.write_all(&vnc_apple_clipboard::auto_pasteboard(true)).await?;
    }
    sock.write_all(&vnc_apple::set_encryption_start()).await?;
    sock.write_all(&vnc_apple::set_encryption_stop()).await?;

    let keys = await_rekey(&mut reader, &wrap_key).await?;
    info!("vnc: Apple record layer active");

    let mut uplink = Uplink::records(sock, keys);
    // High Performance mode is a virtual-display session. Request its mode before
    // the pixel format and encoding list. The same message is resent for viewport
    // reports when resize is permitted; its dynamic-resolution flag is set here
    // regardless, so every fresh session restores the Mac's checkbox to on.
    // At 1x always: no client has attached to say what its screen's density is —
    // that arrives as the first `hostScale` and re-requests the mode if it differs.
    uplink
        .send(&vnc_apple::set_display_configuration(opening_mode(config)))
        .await?;
    uplink.send(&set_pixel_format()).await?;
    uplink.send(&set_encodings(vnc_apple::ENCODINGS)).await?;
    // Arm the server's sender. On this Mac it does *not* take over the update
    // cycle — see [`Connected::poll`] — so what it is still sent for is the
    // server-driven cursor shapes, which the reference says stop flowing across a
    // login or lock without it. Cheap, and re-sent on every layout for the same
    // reason. The non-incremental request that pairs with it is [`active_loop`]'s
    // opening kick, which is the next thing on the wire.
    uplink
        .send(&vnc_apple::auto_framebuffer_update(server.size()))
        .await?;

    Ok(Connected {
        downlink: Downlink::Records(Box::new(RecordReader::new(reader, keys))),
        uplink,
        width: server.width,
        height: server.height,
        macos,
        apple: true,
        poll: true,
    })
}

/// Read cleartext server messages until the rekey arrives, and return the key and
/// IV it carried.
///
/// Nothing legitimate can precede it: no pixel format, no encodings and no update
/// request have been sent, so the server has nothing else to say. Anything that
/// does turn up is named in the error rather than skipped — the metadata burst
/// that follows a rekey is already inside the record layer, so a rectangle here is
/// not a burst arriving early, it is a stream that has gone somewhere unexpected.
async fn await_rekey<R: AsyncRead + Unpin>(
    reader: &mut R,
    wrap_key: &[u8; 16],
) -> anyhow::Result<Keys> {
    loop {
        match reader.read_u8().await? {
            // FramebufferUpdate, which is how the rekey travels.
            0 => {
                reader.read_u8().await?; // padding
                let rects = reader.read_u16().await?;
                // An update with no rectangles at all is empty, not an error.
                if rects == 0 {
                    continue;
                }
                let mut header = [0u8; 8];
                reader.read_exact(&mut header).await?;
                let encoding = reader.read_i32().await?;
                anyhow::ensure!(
                    encoding == vnc_apple::ENCODING_REKEY,
                    "the server sent encoding {encoding} before the record layer was up"
                );
                let mut body = [0u8; vnc_record::REKEY_LEN];
                reader.read_exact(&mut body).await?;
                // Everything after this rectangle is ciphertext, so a further
                // rectangle in the same update cannot be read at all. Named rather
                // than attempted.
                anyhow::ensure!(
                    rects == 1,
                    "the server put {} more rectangle(s) after the rekey",
                    rects - 1
                );
                let (generation, keys) = vnc_record::unwrap_rekey(wrap_key, &body);
                debug!("vnc: rekey generation {generation}");
                return Ok(keys);
            }
            // Bell. Nothing to ring, and no reason to end the session over it.
            2 => {}
            other => anyhow::bail!(
                "the server sent message type {other} before the record layer was up"
            ),
        }
    }
}

/// Drive the active session: framebuffer updates out, browser input in.
async fn active_loop<R: AsyncRead + Unpin + Send + 'static>(
    downlink: R,
    uplink: Uplink,
    size: (u16, u16),
    flags: Flags,
    mut input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    sink: TileSink,
) -> anyhow::Result<()> {
    let Flags {
        macos,
        resize,
        clipboard: clipboard_enabled,
        default_size,
        apple,
        high_performance,
        poll,
    } = flags;
    // The uplink is shared: the read loop answers the server (update requests,
    // re-arming), the input side sends pointer/key/display messages.
    let uplink: SharedUplink = Arc::new(Mutex::new(uplink));
    let desktop: SharedDesktop = Arc::new(std::sync::Mutex::new(DesktopState {
        size,
        scale: UNSCALED,
        host_density: 1.0,
        screen: None,
        pending: None,
    }));
    let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
    let clipboard: SharedClipboard = Arc::new(std::sync::Mutex::new(ClipboardState::default()));
    let shadow: SharedShadow = Arc::new(std::sync::Mutex::new({
        let mut shadow = Shadow::new("vnc", size.0, size.1);
        shadow.classify_cells(sink.wants_cells());
        shadow
    }));
    let display: SharedDisplay = Arc::new(std::sync::Mutex::new(DisplayState::default()));
    let shared = Shared {
        uplink: Arc::clone(&uplink),
        desktop: Arc::clone(&desktop),
        cursor: Arc::clone(&cursor),
        clipboard: Arc::clone(&clipboard),
        shadow: Arc::clone(&shadow),
        display: Arc::clone(&display),
    };

    // Kick off the update cycle with one full (non-incremental) request. On the
    // 003.889 wire this is also the second half of the arming pair the preface
    // began, which is why it is unconditional.
    send(&uplink, &update_request(false, size)).await?;

    let mut read_task = tokio::spawn(read_loop(
        downlink,
        shared,
        ReadFlags {
            clipboard: clipboard_enabled,
            poll,
        },
        apple.then(|| Apple::new(high_performance)),
        sink.clone(),
    ));

    // RFB pointer events always carry position + full button mask, so both are
    // tracked across browser events (which report only the changed part).
    let mut button_mask = 0u8;
    let mut last_pos = (size.0 / 2, size.1 / 2);
    // The keysym actually sent for each pressed DOM code, so a key released
    // after Shift is let go still releases the shifted keysym it was pressed
    // with (down/up symmetry). Doubles as the live Shift state. CapsLock is not
    // tracked here — every key event carries the browser's authoritative lock
    // state (see [`ClientMsg::Key`]).
    let mut pressed_keys: HashMap<String, u32> = HashMap::new();
    let mut wheel = Wheel::new(apple);
    let buttons = Buttons::new(high_performance);

    let result = loop {
        tokio::select! {
            res = &mut read_task => {
                return res.map_err(|e| anyhow::anyhow!("read task failed: {e}"))?;
            }
            input = input_rx.recv() => {
                let Some(input) = input else {
                    info!("vnc: input channel closed; session shut down");
                    break Ok(());
                };
                // Viewport reports drive dynamic resize, not an input event;
                // dropped entirely unless the target opted in. `DefaultSize` is
                // the same request with the size supplied from here instead of by
                // the client — see [`ClientMsg::DefaultSize`] — so the two resolve
                // to a size first and share the one call, which is also how the
                // second inherits the stash-until-supported and drop-the-no-op
                // behaviour `request_resize` already has. `HostScale` is that
                // request with no size at all: only a High Performance virtual
                // display can render the same points at a new density, so only it
                // listens, and like RDP it listens only where `resize` is granted.
                let ask = match input {
                    ClientMsg::Viewport { w, h } => Some(ResizeAsk::Viewport((w, h))),
                    ClientMsg::DefaultSize => Some(ResizeAsk::Points(default_size)),
                    ClientMsg::HostScale { scale } if high_performance && resize => {
                        let density = crate::protocol::scale_ratio(scale);
                        let mut d = desktop.lock().unwrap();
                        let changed = (d.host_density - density).abs() > 0.005;
                        d.host_density = density;
                        changed.then_some(ResizeAsk::Density)
                    }
                    _ => None,
                };
                let sent = if let Some(ask) = ask {
                    if resize {
                        request_resize(&uplink, &desktop, ask, high_performance).await
                    } else {
                        Ok(())
                    }
                } else if matches!(input, ClientMsg::Refresh) {
                    // A (re)attached browser needs the desktop size and a full
                    // repaint, and the repaint is still asked of the *server*
                    // rather than answered from the shadow below. The shadow
                    // holds what the browser was sent, which is not the same
                    // thing as the remote's current pixels — the session layer
                    // drops frames while nobody is attached, so it goes stale
                    // exactly across a detach. Answering locally would trade the
                    // server's ground truth for bytes on the LAN hop, which is
                    // not the link this is trying to save.
                    //
                    // So: forget what the browser had. Everything the
                    // non-incremental update brings back is then new, which is
                    // the truth — a browser that just attached has nothing.
                    shadow.lock().unwrap().forget();
                    // The repaint that follows re-sends every pixel at the base
                    // encode, which settles every debt and makes every cell's
                    // history a single redraw rather than motion.
                    sink.reset_render();
                    let (size, resize_msg) = {
                        let d = desktop.lock().unwrap();
                        (d.size, d.resize_msg())
                    };
                    if let Err(e) = sink.msg(resize_msg).await {
                        break Err(e);
                    }
                    if let Err(e) = sink.msg(ServerMsg::RemoteOs { macos }).await {
                        break Err(e);
                    }
                    // The pointer shape is not part of a repaint — the server
                    // resends it only when it changes — so replay the cached
                    // one, or the fresh browser would draw no pointer at all.
                    if let Some(msg) = cursor_msg(&cursor)
                        && let Err(e) = sink.msg(msg).await
                    {
                        break Err(e);
                    }
                    // The display list is the same story: the Mac reports it when
                    // its layout changes, which may have been long before this
                    // browser arrived, and a client holds no display state of its
                    // own to fall back on.
                    let displays_msg = display.lock().unwrap().displays_msg();
                    if let Some(msg) = displays_msg
                        && let Err(e) = sink.msg(msg).await
                    {
                        break Err(e);
                    }
                    send(&uplink, &update_request(false, size)).await
                } else if matches!(input, ClientMsg::ClipboardRequest) {
                    if clipboard_enabled && apple {
                        // Unlike standard RFB, Apple's pasteboard can be read on
                        // demand. Answer from the cache first so a silent Mac
                        // cannot strand the browser's read, then fetch so a later
                        // response refreshes that cache and the open panel.
                        request_apple_clipboard(&clipboard, &uplink, &sink).await
                    } else if clipboard_enabled {
                        // Standard RFB can only answer from the buffer the read
                        // loop fills. Empty means nothing has been copied there
                        // yet during this session.
                        let snapshot = clipboard
                            .lock()
                            .unwrap()
                            .remote
                            .clone()
                            .unwrap_or_else(ClipboardSnapshot::unobserved);
                        if let Err(e) = sink
                            .msg(ServerMsg::Clipboard {
                                text: snapshot.text,
                                changed_at_ms: snapshot.changed_at_ms,
                                requested: true,
                                oversized_bytes: snapshot.oversized_bytes,
                            })
                            .await
                        {
                            break Err(e);
                        }
                        Ok(())
                    } else {
                        Ok(())
                    }
                } else if let ClientMsg::Clipboard { text } = &input {
                    if clipboard_enabled && !clipboard_fits(text) {
                        // Refused, as the RDP engine does: the remote
                        // keeps what it had rather than being handed a partial
                        // copy that looks whole. Also keeps an oversized string
                        // out of `state.local`, which the deferred Provide can
                        // be asked for long after the copy.
                        warn!(
                            "vnc: refusing {} bytes to the remote clipboard, over the {MAX_CLIPBOARD_BYTES} byte limit",
                            text.len()
                        );
                        Ok(())
                    } else if clipboard_enabled && apple {
                        let session_id = {
                            let mut state = clipboard.lock().unwrap();
                            state.local = Some(text.to_owned());
                            state.apple_session_id
                        };
                        match vnc_apple_clipboard::send(session_id, text) {
                            Ok(msg) => send(&uplink, &msg).await,
                            Err(e) => Err(e),
                        }
                    } else if clipboard_enabled {
                        // Extended when the server offered it, which is the
                        // only path that carries anything outside latin-1.
                        // Deferred by design: advertise now, hand the text over
                        // when the remote actually pastes and asks for it.
                        let extended = {
                            let mut state = clipboard.lock().unwrap();
                            state.local = Some(text.to_owned());
                            state
                                .server
                                .is_some_and(|caps| caps.handles(vnc_clipboard::ACTION_NOTIFY))
                        };
                        if extended {
                            let notify = vnc_clipboard::notify(vnc_clipboard::FORMAT_TEXT);
                            send(&uplink, &cut_text_extended(&notify)).await
                        } else {
                            // Unreachable None: the branch above refused
                            // anything over the ceiling.
                            match client_cut_text(text) {
                                Some(msg) => send(&uplink, &msg).await,
                                None => Ok(()),
                            }
                        }
                    } else {
                        Ok(())
                    }
                } else if let ClientMsg::SelectDisplay { id } = input {
                    // Handled here rather than in `translate_input`, which is a
                    // pure function of the input and has no way to record what was
                    // asked for. Only an Apple target can act on it: generic RFB
                    // exposes one framebuffer, while a Mac accepts this extension
                    // on both supported transports.
                    if apple {
                        let known =
                            display.lock().unwrap().displays.iter().any(|d| d.id == id);
                        if known {
                            // `COMBINED` is this gateway's own list entry, not a
                            // screen the Mac named, so it maps back to the
                            // `combine_all_displays` byte rather than to an id.
                            let pick = (id != DisplayState::COMBINED).then_some(id);
                            debug!("vnc: asking the Mac for display {pick:?}");
                            // Queue the repaint while the selection is still the
                            // message in front of the Mac. Asking only after its
                            // answering layout is too late on macOS 26: the layout
                            // and resize arrive, but the request then earns only
                            // later damage. A client has just cleared its resized
                            // framebuffer, so a quiet screen stays black. The Mac
                            // accepts the old framebuffer bounds here and applies
                            // the request to the screen it is switching to.
                            let size = desktop.lock().unwrap().size;
                            send_all(
                                &uplink,
                                &[
                                    vnc_apple::set_display_message(pick),
                                    update_request(false, size).to_vec(),
                                ],
                            )
                            .await
                        } else {
                            // A screen that has been unplugged since the list was
                            // sent. Dropped rather than forwarded, so the Mac is
                            // not asked to bind to something that is gone and the
                            // checkmark stays where it is.
                            debug!("vnc: ignoring a selection of unknown display {id}");
                            Ok(())
                        }
                    } else {
                        Ok(())
                    }
                } else {
                    let msgs = translate_input(
                        input,
                        &buttons,
                        &mut button_mask,
                        &mut last_pos,
                        &mut pressed_keys,
                        &mut wheel,
                    );
                    send_all(&uplink, &msgs).await
                };
                // Break instead of `?`: the error must pass the trailing
                // read_task.abort() on its way out.
                if let Err(e) = sent {
                    break Err(e);
                }
            }
        }
    };
    read_task.abort();
    result
}

/// The unit a resize request states its size in. Everything resolves to logical
/// points first, because that is the one unit all three speak: a viewport report
/// is pixels at the scale this end last announced, the configured default is
/// points outright, and a density change carries no size at all.
enum ResizeAsk {
    /// A browser viewport report: pixels at the announced scale.
    Viewport((u16, u16)),
    /// The target's configured size: logical points.
    Points((u16, u16)),
    /// No new size — the client's screen changed density, so the current size is
    /// re-expressed at the new [`DesktopState::host_density`].
    Density,
}

/// Handle a browser viewport report (dynamic resize).
///
/// A High Performance Mac owns a virtual display, so replacing its one-mode
/// `SetDisplayConfiguration` is the resize request — and the mode it requests is
/// the resolved points at the client screen's density, which is how moving the
/// window to a Retina display re-renders the same desktop at 2x. Generic VNC uses
/// `SetDesktopSize` once the server declares support via an ExtendedDesktopSize
/// rect; until then, its report is stashed for replay. It has no density to
/// apply, so its points are its pixels.
async fn request_resize(
    uplink: &SharedUplink,
    desktop: &SharedDesktop,
    ask: ResizeAsk,
    high_performance: bool,
) -> anyhow::Result<()> {
    let msg = {
        let mut d = desktop.lock().unwrap();
        let to_points = |px: (u16, u16)| {
            let point = |v: u16| (f32::from(v) / d.scale).round().max(1.0) as u16;
            (point(px.0), point(px.1))
        };
        let want = match ask {
            ResizeAsk::Viewport((0, _) | (_, 0)) => return Ok(()),
            ResizeAsk::Viewport(px) => to_points(px),
            ResizeAsk::Points(points) => points,
            ResizeAsk::Density => to_points(d.size),
        };
        let msg = if high_performance {
            let mode = vnc_apple::virtual_display_mode(want, d.host_density);
            // A no-op needs the density to agree too: a 3840×2160 desktop moving
            // from 1x to 2x keeps every pixel and still needs the new mode sent.
            if mode.pixels == d.size && (d.scale - d.host_density).abs() < 0.005 {
                return Ok(());
            }
            vnc_apple::set_display_configuration(mode)
        } else if want == d.size {
            // The browser is back at the current size; drop any stale stash
            // so a later support declaration doesn't replay it.
            d.pending = None;
            return Ok(());
        } else {
            match d.screen {
                Some(screen) => set_desktop_size(want, screen).to_vec(),
                None => {
                    d.pending = Some(want);
                    return Ok(());
                }
            }
        };
        debug!(
            "vnc: requesting {} resize to {}x{} points at {}x",
            if high_performance { "Apple virtual-display" } else { "desktop" },
            want.0,
            want.1,
            if high_performance { d.host_density } else { 1.0 },
        );
        msg
    };
    send(uplink, &msg).await
}

/// Everything the read loop and the rect handlers under it share with the input
/// side. Grouped because it all travels together and none of it is optional.
#[derive(Clone)]
struct Shared {
    uplink: SharedUplink,
    desktop: SharedDesktop,
    cursor: SharedCursor,
    clipboard: SharedClipboard,
    shadow: SharedShadow,
    display: SharedDisplay,
}

/// Read server messages forever, forwarding framebuffer updates as tiles.
///
/// `apple` is `Some` when either Apple subtype negotiated the Mac's metadata
/// encodings. The transport may be plain RFB 3.8 or 003.889 records.
async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    shared: Shared,
    flags: ReadFlags,
    mut apple: Option<Apple>,
    sink: TileSink,
) -> anyhow::Result<()> {
    let ReadFlags { clipboard: clipboard_enabled, poll } = flags;
    let Shared { uplink, desktop, clipboard, display, .. } = &shared;
    let mut full_repaint: Option<FullRepaint> = None;
    // The connection's decoder state: the deflate streams and whatever else an
    // encoding carries from one rectangle to the next.
    let mut decoders = Decoders::default();
    // Whether the server is pushing updates unasked, and whether it has ever said it
    // could. Two flags rather than one because the extension uses the same message
    // for both answers: the first is the support announcement, and any later one is
    // the acknowledgement of a disable — which this client never asks for, so seeing
    // a second means the server has stopped on its own and the polling loop has to
    // take over again.
    let mut continuous = false;
    let mut continuous_supported = false;
    loop {
        // Raced against the next message rather than awaited on its own, so a paced
        // video stream still hands over pixels the mirror is holding when the remote
        // has gone quiet — see `TileSink::due_at` for why that is correctness rather
        // than smoothness. `None` on every still target, which leaves this exactly as
        // it was.
        //
        // Here and only here: at the *top* of the loop this is a message boundary, so
        // a flush cannot land between the rectangles of one FramebufferUpdate and cut
        // a frame in half. And a cancelled one-byte read is safe to retry — a byte is
        // either taken or it is not, so unlike a multi-byte `read_exact` there is no
        // half-read state for `select!` to strand.
        // A clean mirror parks on the round-returned signal instead of forever:
        // while a round is away being encoded the live table is empty and `due_at`
        // cannot see the damage that lands meanwhile, so the round's return is what
        // re-arms this.
        let video_flush = async {
            match sink.due_at().await {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => sink.round_returned().await,
            }
        };
        let read = tokio::select! {
            byte = reader.read_u8() => byte,
            () = video_flush => {
                sink.frame().await?;
                continue;
            }
        };
        let msg_type = match read {
            Ok(t) => t,
            // A clean hang-up is an event the user should be told about — a
            // stopped server, a Mac logged out, `vncserver -kill`. Returning
            // `Ok` here meant `run` skipped its error branch and the browser got
            // a bare picker with no explanation, or worse, the *previous*
            // error still sitting on it. Deliberate teardown does not come
            // through here; it leaves through the input branch.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                info!("vnc: server closed the connection");
                return Err(anyhow::anyhow!("the VNC server closed the connection"));
            }
            // What a host that was switched off or cut off looks like, now that
            // the socket has keepalive on it (see [`crate::engine`]). Worth its
            // own words: the raw form of this is "read server message:
            // Connection timed out (os error 60)".
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err(anyhow::anyhow!(
                    "the remote host stopped answering (no reply for {}s)",
                    engine::keepalive_budget().as_secs()
                ));
            }
            // The record layer's own refusals, which have already been phrased for
            // a person. Passed through rather than wrapped in "read server
            // message", which would bury them.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                return Err(anyhow::anyhow!("{e}"));
            }
            Err(e) => return Err(anyhow::anyhow!("read server message: {e}")),
        };
        match msg_type {
            // FramebufferUpdate
            0 => {
                reader.read_u8().await?; // padding
                // `0xffff` here means "as many as it takes, ended by a LastRect" —
                // an update a server starts sending before it knows how long it
                // will be. macOS uses it for the metadata burst, so on the Apple
                // dialect this is the normal form rather than a curiosity. The count
                // still bounds the loop, so a server that promises a LastRect and
                // never sends one is stopped by the same code either way.
                let rects = reader.read_u16().await?;
                let mut resized = false;
                let mut full_repaint_owed = false;
                for _ in 0..rects {
                    let effect = read_rect(
                        &mut reader,
                        &shared,
                        &mut apple,
                        &mut decoders,
                        clipboard_enabled,
                        &sink,
                    )
                    .await?;
                    resized |= effect.resized;
                    full_repaint_owed |= effect.full_repaint_owed;
                    if let (Some(repaint), Some(rect)) = (&mut full_repaint, effect.pixels) {
                        repaint.accept(rect);
                    }
                    if effect.last {
                        break;
                    }
                }
                // One FramebufferUpdate is one frame's worth of damage, however many
                // rectangles it was described in — the cleanest frame boundary either
                // protocol offers, and where a video stream is told to encode what it
                // has. After the loop rather than inside it, so a `LastRect` breaking
                // out still reaches it.
                sink.frame().await?;
                let size = desktop.lock().unwrap().size;
                // The enabled region is part of the request, so a resize invalidates
                // it: without this the server would go on pushing updates for a
                // rectangle the desktop no longer has.
                if continuous && resized {
                    send(uplink, &enable_continuous_updates(true, size)).await?;
                }
                // With the server pushing, asking for an *incremental* update is the
                // one thing that has to stop — it is the round trip per frame this
                // removes. Non-incremental requests are unaffected and still go where
                // they went: this gateway needs a full repaint that no amount of
                // waiting for damage will produce, on a reattach, a resize, or a
                // CopyRect whose source it never learned.
                let poll = poll && !continuous;
                if full_repaint_owed {
                    // Layout metadata and empty updates can arrive before the
                    // pixels this request earns. Hold the polling loop until the
                    // actual display regions have arrived or the bounded request
                    // budget is exhausted, so an incremental request cannot
                    // immediately replace this full one on macOS.
                    let expected = display.lock().unwrap().repaint_pixels;
                    full_repaint = Some(FullRepaint::new(expected));
                    send(uplink, &update_request(false, size)).await?;
                } else {
                    if let Some(repaint) = &mut full_repaint {
                        repaint.finish_update();
                    }
                    if full_repaint.as_ref().is_some_and(FullRepaint::complete) {
                        full_repaint = None;
                        if poll {
                            send(uplink, &update_request(true, size)).await?;
                        }
                    } else if full_repaint.is_some() {
                        send(uplink, &update_request(false, size)).await?;
                    } else if poll || resized {
                        send(uplink, &update_request(poll && !resized, size)).await?;
                    }
                }
            }
            // SetColourMapEntries — can't happen for the true-colour format we
            // set, but consume it correctly rather than desyncing the stream.
            1 => {
                reader.read_u8().await?; // padding
                reader.read_u16().await?; // first colour index
                let colours = reader.read_u16().await?;
                discard(&mut reader, u64::from(colours) * 6).await?;
            }
            // Bell — nothing to ring in the browser (yet).
            2 => {}
            // ServerCutText — the remote's clipboard changed. Pushed to the
            // browser as it arrives *and* stashed, because the two serve
            // different readers: the push drives automatic sync, the stash
            // answers a Fetch from a browser that attached later and so never
            // saw the push. Drained and dropped when the target didn't opt in.
            3 => {
                let mut padding = [0u8; 3];
                reader.read_exact(&mut padding).await?;
                // Signed: a negative length marks an Extended Clipboard
                // message, whose body is a flags word and an action rather
                // than latin-1 text.
                let signed = reader.read_i32().await?;
                let len = u64::from(signed.unsigned_abs());
                if !clipboard_enabled {
                    discard(&mut reader, len).await?;
                    continue;
                }
                // Discard an oversized announcement and report its size instead
                // of the first 64 KiB, which would look like the whole thing.
                // The body is consumed either way: the stream position must stay
                // exact whatever the server sends.
                if len > MAX_CLIPBOARD_BYTES as u64 {
                    discard(&mut reader, len).await?;
                    debug!(
                        "vnc: remote clipboard is {len} bytes, over the {MAX_CLIPBOARD_BYTES} byte limit"
                    );
                    let snapshot = {
                        let mut state = clipboard.lock().unwrap();
                        let snapshot = ClipboardSnapshot::oversized(len, state.remote.as_ref());
                        state.remote = Some(snapshot.clone());
                        snapshot
                    };
                    if sink
                        .msg(ServerMsg::Clipboard {
                            text: snapshot.text,
                            changed_at_ms: snapshot.changed_at_ms,
                            requested: false,
                            oversized_bytes: snapshot.oversized_bytes,
                        })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    continue;
                }
                let mut bytes = vec![0u8; len as usize];
                reader.read_exact(&mut bytes).await?;

                if signed < 0 {
                    if extended_cut_text(&bytes, uplink, clipboard, &sink).await? {
                        return Ok(()); // browser link gone
                    }
                    continue;
                }

                let text = latin1_to_string(&bytes);
                debug!("vnc: remote clipboard updated, {} bytes", bytes.len());
                let snapshot = {
                    let mut state = clipboard.lock().unwrap();
                    let snapshot = ClipboardSnapshot::changed(text, state.remote.as_ref());
                    state.remote = Some(snapshot.clone());
                    snapshot
                };
                if sink
                    .msg(ServerMsg::Clipboard {
                        text: snapshot.text,
                        changed_at_ms: snapshot.changed_at_ms,
                        requested: false,
                        oversized_bytes: snapshot.oversized_bytes,
                    })
                    .await
                    .is_err()
                {
                    return Ok(()); // browser link gone; the session layer handles it
                }
            }
            // EndOfContinuousUpdates, which carries nothing: the message is the
            // whole content, and which of its two meanings it has depends only on
            // whether one has arrived before.
            //
            // The first is the server answering the SetEncodings that advertised the
            // pseudo-encoding — the only way it ever says it supports the extension —
            // and is answered by turning it on. A later one is the acknowledgement of
            // a disable, which this client never sends, so the honest reading is that
            // the server has stopped pushing; polling resumes and one request is sent
            // to restart the cycle it had replaced.
            MSG_END_OF_CONTINUOUS_UPDATES => {
                let size = desktop.lock().unwrap().size;
                if continuous_supported {
                    info!("vnc: the server ended continuous updates; polling again");
                    continuous = false;
                    if poll {
                        send(uplink, &update_request(true, size)).await?;
                    }
                    continue;
                }
                info!("vnc: the server offers continuous updates; enabling them");
                continuous_supported = true;
                continuous = true;
                send(uplink, &enable_continuous_updates(true, size)).await?;
            }
            // ServerFence: a marker the server sends down the stream and asks back,
            // which is how it measures this end and paces itself. Echoed here, on the
            // read task, so the answer is not queued behind anything the input side is
            // doing — a fence that waited would report a link slower than it is.
            MSG_FENCE => {
                let mut padding = [0u8; 3];
                reader.read_exact(&mut padding).await?;
                let flags = reader.read_u32().await?;
                let len = usize::from(reader.read_u8().await?);
                let mut payload = vec![0u8; len];
                reader.read_exact(&mut payload).await?;
                if flags & FENCE_REQUEST == 0 {
                    // A fence this end never asked for. Not fatal — nothing here is
                    // waiting on it — but worth a line, because it means the server
                    // believes it is answering something.
                    debug!("vnc: ignoring an unrequested server fence");
                    continue;
                }
                payload.truncate(MAX_FENCE_PAYLOAD);
                // Only the two flags this loop actually honours are claimed back;
                // `SyncNext` in particular is not implemented and must not be echoed
                // as though it were.
                let flags = flags & (FENCE_BLOCK_BEFORE | FENCE_BLOCK_AFTER);
                send(uplink, &client_fence(flags, &payload)).await?;
            }
            // Apple's pasteboard status. `cmd = 2` says the remote clipboard
            // changed and must be fetched; `cmd = 3` asks for the browser's last
            // clipboard again. Other status values are session/heartbeat notices
            // with no clipboard action.
            0x14 if apple.is_some() => {
                reader.read_u8().await?; // padding
                let len = reader.read_u16().await?;
                let mut body = vec![0u8; usize::from(len)];
                reader.read_exact(&mut body).await?;
                if body.len() < 4 {
                    warn!("vnc: ignoring an Apple status with a {len}-byte body");
                    continue;
                }
                let command = u16::from_be_bytes([body[2], body[3]]);
                match command {
                    2 if clipboard_enabled => {
                        let session_id = clipboard.lock().unwrap().apple_session_id;
                        send(uplink, &vnc_apple_clipboard::fetch(session_id)).await?;
                    }
                    3 if clipboard_enabled => {
                        let local = {
                            let state = clipboard.lock().unwrap();
                            state
                                .local
                                .as_ref()
                                .map(|text| (state.apple_session_id, text.clone()))
                        };
                        if let Some((session_id, text)) = local {
                            match vnc_apple_clipboard::send(session_id, &text) {
                                Ok(msg) => send(uplink, &msg).await?,
                                Err(e) => warn!("vnc: could not answer Apple pasteboard request: {e:#}"),
                            }
                        }
                    }
                    _ => debug!("vnc: Apple status command {command}"),
                }
            }
            // Apple's compressed pasteboard archive, fetched after the status
            // above. The record layer is a byte stream here, so a payload split
            // across records is reassembled by `read_exact` without special cases.
            0x1f if apple.is_some() => {
                let mut raw = [0u8; 15];
                reader.read_exact(&mut raw).await?;
                let header = vnc_apple_clipboard::header(&raw);
                clipboard.lock().unwrap().apple_session_id = header.session_id;
                let compressed = u64::from(header.compressed);
                if compressed > vnc_apple_clipboard::MAX_COMPRESSED_BYTES {
                    discard(&mut reader, compressed).await?;
                    if !clipboard_enabled {
                        continue;
                    }
                    let requested = {
                        let mut state = clipboard.lock().unwrap();
                        let requested = state.apple_requests > 0;
                        state.apple_requests = state.apple_requests.saturating_sub(1);
                        requested
                    };
                    let snapshot = {
                        let mut state = clipboard.lock().unwrap();
                        let snapshot = ClipboardSnapshot::oversized(
                            u64::from(header.uncompressed),
                            state.remote.as_ref(),
                        );
                        state.remote = Some(snapshot.clone());
                        snapshot
                    };
                    if emit_clipboard(&sink, snapshot, requested).await {
                        return Ok(());
                    }
                    continue;
                }
                let mut bytes = vec![0u8; compressed as usize];
                reader.read_exact(&mut bytes).await?;
                if !clipboard_enabled {
                    continue;
                }
                let requested = {
                    let mut state = clipboard.lock().unwrap();
                    let requested = state.apple_requests > 0;
                    state.apple_requests = state.apple_requests.saturating_sub(1);
                    requested
                };
                match vnc_apple_clipboard::parse(header, &bytes) {
                    Ok(vnc_apple_clipboard::Incoming::Text(text)) => {
                        debug!("vnc: remote Apple clipboard updated, {} bytes", text.len());
                        let snapshot = {
                            let mut state = clipboard.lock().unwrap();
                            let snapshot = ClipboardSnapshot::changed(text, state.remote.as_ref());
                            state.remote = Some(snapshot.clone());
                            snapshot
                        };
                        if emit_clipboard(&sink, snapshot, requested).await {
                            return Ok(());
                        }
                    }
                    Ok(vnc_apple_clipboard::Incoming::Oversized(bytes)) => {
                        let snapshot = {
                            let mut state = clipboard.lock().unwrap();
                            let snapshot = ClipboardSnapshot::oversized(bytes, state.remote.as_ref());
                            state.remote = Some(snapshot.clone());
                            snapshot
                        };
                        if emit_clipboard(&sink, snapshot, requested).await {
                            return Ok(());
                        }
                    }
                    Ok(vnc_apple_clipboard::Incoming::NoText) => {
                        debug!("vnc: Apple pasteboard carries no text flavor");
                        if requested {
                            let snapshot = clipboard
                                .lock()
                                .unwrap()
                                .remote
                                .clone()
                                .unwrap_or_else(ClipboardSnapshot::unobserved);
                            if emit_clipboard(&sink, snapshot, true).await {
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => {
                        warn!("vnc: unreadable Apple pasteboard: {e:#}");
                        if requested {
                            let snapshot = clipboard
                                .lock()
                                .unwrap()
                                .remote
                                .clone()
                                .unwrap_or_else(ClipboardSnapshot::unobserved);
                            if emit_clipboard(&sink, snapshot, true).await {
                                return Ok(());
                            }
                        }
                    }
                }
            }
            // Apple's own server messages, which arrive alongside the rectangles on
            // the 003.889 wire and end the session if they are not stepped over.
            //
            // `0x04` ServerAck and `0x07` NOP carry no body at all. The metadata
            // encodings each *also* come as a bare message whose type is the
            // encoding's low byte — `0x451` as `0x51`, `0x453` as `0x53` — with the
            // same `u16` length prefix, and a live session sends both forms of the
            // same content. Read for their length and dropped, exactly as the
            // rectangle forms are: nothing here is acted on, but walking past by the
            // wrong number of bytes would desync everything after it.
            0x04 | 0x07 if apple.is_some() => {}
            0x51 | 0x53 | 0x55 | 0x56 if apple.is_some() => {
                let len = reader.read_u16().await?;
                debug!("vnc: Apple message type {msg_type:#04x}, {len} bytes");
                discard(&mut reader, u64::from(len)).await?;
            }
            other => anyhow::bail!("unknown server message type {other}"),
        }
    }
}

/// Forward one already-recorded remote clipboard snapshot. Returns whether the
/// browser link is gone, matching the read loop's other sink helpers.
async fn emit_clipboard(sink: &TileSink, snapshot: ClipboardSnapshot, requested: bool) -> bool {
    sink.msg(ServerMsg::Clipboard {
        text: snapshot.text,
        changed_at_ms: snapshot.changed_at_ms,
        requested,
        oversized_bytes: snapshot.oversized_bytes,
    })
    .await
    .is_err()
}

/// Answer an Apple clipboard read immediately, then refresh it asynchronously
/// from the Mac. The cache reply is the browser request's guaranteed response;
/// the fetch may be ignored by a server or arrive later as a second update.
async fn request_apple_clipboard(
    clipboard: &SharedClipboard,
    uplink: &SharedUplink,
    sink: &TileSink,
) -> anyhow::Result<()> {
    let (session_id, snapshot) = {
        let mut state = clipboard.lock().unwrap();
        state.apple_requests = state.apple_requests.saturating_add(1);
        (
            state.apple_session_id,
            state
                .remote
                .clone()
                .unwrap_or_else(ClipboardSnapshot::unobserved),
        )
    };
    sink.msg(ServerMsg::Clipboard {
        text: snapshot.text,
        changed_at_ms: snapshot.changed_at_ms,
        requested: true,
        oversized_bytes: snapshot.oversized_bytes,
    })
    .await?;
    send(uplink, &vnc_apple_clipboard::fetch(session_id)).await
}

/// Handle one Extended Clipboard message from the server.
///
/// Returns whether the browser link is gone, which is the caller's cue to stop.
/// Everything here is a reply to the server, so it writes rather than returns.
async fn extended_cut_text(
    body: &[u8],
    uplink: &SharedUplink,
    clipboard: &SharedClipboard,
    sink: &TileSink,
) -> anyhow::Result<bool> {
    let message = match vnc_clipboard::parse(body) {
        Ok(message) => message,
        Err(e) => {
            // One malformed clipboard message is not worth the session. The
            // stream stayed in sync (the length told us how much to consume),
            // so the next copy can still work.
            warn!("vnc: unreadable extended clipboard message: {e:#}");
            return Ok(false);
        }
    };

    match message {
        // The server's opening move. Record what it can do, then answer with
        // ours — until this arrives the engine assumes latin-1.
        vnc_clipboard::Incoming::Caps(caps) => {
            debug!(
                "vnc: extended clipboard available (actions {:#x}, formats {:#x})",
                caps.actions, caps.formats
            );
            clipboard.lock().unwrap().server = Some(caps);
            send(uplink, &cut_text_extended(&vnc_clipboard::caps())).await?;
        }
        // The remote copied something. Ask for it, so the browser gets it
        // without anyone pressing Fetch.
        vnc_clipboard::Incoming::Notify(formats) => {
            if formats & vnc_clipboard::FORMAT_TEXT != 0 {
                let request = vnc_clipboard::request(vnc_clipboard::FORMAT_TEXT);
                send(uplink, &cut_text_extended(&request)).await?;
            } else {
                // An image or file copy, or `formats == 0` for a clipboard
                // that was cleared. Either way the remote no longer holds the
                // text we cached, so drop it — a later Fetch answering with it
                // would be reporting a clipboard that has moved on.
                //
                // Not forwarded as an empty push: the browser would clear an
                // open panel over what may be a screenshot copy. Leaving the
                // panel as it is until something asks is the quieter half of
                // the same truth, and Fetch now answers correctly.
                debug!("vnc: remote copied a format the browser cannot hold");
                let mut state = clipboard.lock().unwrap();
                state.remote = Some(ClipboardSnapshot::changed(
                    String::new(),
                    state.remote.as_ref(),
                ));
            }
        }
        // The answer to that request, or — when there is too much of it to
        // carry — the size it would have been. Both are clipboard activity the
        // panel reports; only one of them has text in it.
        vnc_clipboard::Incoming::Provide(Some(text)) => {
            debug!("vnc: remote clipboard updated, {} bytes (utf-8)", text.len());
            let snapshot = {
                let mut state = clipboard.lock().unwrap();
                let snapshot = ClipboardSnapshot::changed(text, state.remote.as_ref());
                state.remote = Some(snapshot.clone());
                snapshot
            };
            if sink
                .msg(ServerMsg::Clipboard {
                    text: snapshot.text,
                    changed_at_ms: snapshot.changed_at_ms,
                    requested: false,
                    oversized_bytes: snapshot.oversized_bytes,
                })
                .await
                .is_err()
            {
                return Ok(true);
            }
        }
        vnc_clipboard::Incoming::Provide(None) => {}
        // Refused, and reported as the size it was: the panel says so instead of
        // showing the first 64 KiB as though it were the whole clipboard.
        vnc_clipboard::Incoming::Oversized(bytes) => {
            debug!(
                "vnc: remote clipboard is {bytes} bytes, over the {MAX_CLIPBOARD_BYTES} byte limit"
            );
            let snapshot = {
                let mut state = clipboard.lock().unwrap();
                let snapshot = ClipboardSnapshot::oversized(bytes, state.remote.as_ref());
                state.remote = Some(snapshot.clone());
                snapshot
            };
            if sink
                .msg(ServerMsg::Clipboard {
                    text: snapshot.text,
                    changed_at_ms: snapshot.changed_at_ms,
                    requested: false,
                    oversized_bytes: snapshot.oversized_bytes,
                })
                .await
                .is_err()
            {
                return Ok(true);
            }
        }
        // The server wants what the browser has. This is the deferred half of
        // a browser copy: we advertised with a notify, it asks here.
        vnc_clipboard::Incoming::Request(formats) => {
            let text = clipboard.lock().unwrap().local.clone();
            if let Some(text) = text
                && formats & vnc_clipboard::FORMAT_TEXT != 0
            {
                debug!("vnc: handing {} bytes to the remote's paste", text.len());
                let provide = vnc_clipboard::provide(&text)?;
                send(uplink, &cut_text_extended(&provide)).await?;
            }
        }
        // "What do you have?" — answered with a notify either way, since
        // silence would leave the server waiting.
        vnc_clipboard::Incoming::Peek => {
            let formats = match clipboard.lock().unwrap().local {
                Some(_) => vnc_clipboard::FORMAT_TEXT,
                None => 0,
            };
            send(uplink, &cut_text_extended(&vnc_clipboard::notify(formats))).await?;
        }
        vnc_clipboard::Incoming::Unknown(action) => {
            debug!("vnc: ignoring extended clipboard action {action:#x}");
        }
    }
    Ok(false)
}

/// Coverage of one non-incremental framebuffer request.
///
/// Apple sends a combined desktop as one rectangle per display, sometimes with
/// metadata or damage updates between them. Rectangle union is tracked exactly so
/// overlapping damage cannot masquerade as the full repaint.
///
/// Eight full requests tolerate the metadata and small-damage bursts measured
/// around a display switch without letting a server that never repaints stall the
/// polling loop forever.
const FULL_REPAINT_UPDATE_BUDGET: u8 = 8;

#[derive(Debug)]
struct FullRepaint {
    expected_pixels: u64,
    regions: Vec<Rect>,
    updates_left: u8,
    coverage_complete: bool,
}

impl FullRepaint {
    fn new(expected_pixels: u64) -> Self {
        Self {
            expected_pixels,
            regions: Vec::new(),
            updates_left: FULL_REPAINT_UPDATE_BUDGET,
            coverage_complete: expected_pixels == 0,
        }
    }

    fn accept(&mut self, rect: Rect) {
        if self.complete() || self.regions.iter().any(|region| region.contains(&rect)) {
            return;
        }
        self.regions.push(rect);
        self.coverage_complete = union_pixels(&self.regions) >= self.expected_pixels;
    }

    fn finish_update(&mut self) {
        if !self.complete() {
            self.updates_left = self.updates_left.saturating_sub(1);
        }
    }

    fn complete(&self) -> bool {
        self.coverage_complete || self.updates_left == 0
    }
}

/// Area of the union of inclusive rectangles.
fn union_pixels(regions: &[Rect]) -> u64 {
    let mut xs = regions
        .iter()
        .flat_map(|rect| [u32::from(rect.left), u32::from(rect.right) + 1])
        .collect::<Vec<_>>();
    xs.sort_unstable();
    xs.dedup();

    xs.windows(2)
        .map(|x| {
            let mut ys = regions
                .iter()
                .filter(|rect| u32::from(rect.left) < x[1] && u32::from(rect.right) + 1 > x[0])
                .map(|rect| (u32::from(rect.top), u32::from(rect.bottom) + 1))
                .collect::<Vec<_>>();
            ys.sort_unstable();
            let mut height = 0u64;
            let mut merged: Option<(u32, u32)> = None;
            for (top, bottom) in ys {
                match merged {
                    Some((start, end)) if top <= end => merged = Some((start, end.max(bottom))),
                    Some((start, end)) => {
                        height += u64::from(end - start);
                        merged = Some((top, bottom));
                    }
                    None => merged = Some((top, bottom)),
                }
            }
            if let Some((start, end)) = merged {
                height += u64::from(end - start);
            }
            u64::from(x[1] - x[0]) * height
        })
        .sum()
}

/// What reading one rectangle did, beyond whatever it painted.
#[derive(Debug, Default, Clone, Copy)]
struct RectEffect {
    /// The desktop changed size, so what the browser holds is stale.
    resized: bool,
    /// A layout invalidated the framebuffer, so the next poll must be full even
    /// when the layout's backing size did not change.
    full_repaint_owed: bool,
    /// Pixel rectangle consumed, whether or not the shadow needed to forward it.
    pixels: Option<Rect>,
    /// A `LastRect`: this update ends here, whatever its header's count claimed.
    last: bool,
}

impl RectEffect {
    const NOTHING: Self = Self {
        resized: false,
        full_repaint_owed: false,
        pixels: None,
        last: false,
    };
    const LAST: Self = Self { last: true, ..Self::NOTHING };

    const FULL_REPAINT: Self = Self {
        resized: false,
        full_repaint_owed: true,
        pixels: None,
        last: false,
    };

    const fn resized(resized: bool) -> Self {
        Self { resized, ..Self::NOTHING }
    }

    const fn pixels(rect: Rect) -> Self {
        Self { pixels: Some(rect), ..Self::NOTHING }
    }
}

/// Read one FramebufferUpdate rectangle — pixels compared against what the
/// browser holds and forwarded as tiles, or one of the pseudo-encodings that
/// carry a cursor, a size or a display layout instead.
async fn read_rect<R: AsyncRead + Unpin>(
    reader: &mut R,
    shared: &Shared,
    apple: &mut Option<Apple>,
    decoders: &mut Decoders,
    clipboard_enabled: bool,
    sink: &TileSink,
) -> anyhow::Result<RectEffect> {
    let Shared { uplink, desktop, cursor, shadow, .. } = shared;
    let x = reader.read_u16().await?;
    let y = reader.read_u16().await?;
    let w = reader.read_u16().await?;
    let h = reader.read_u16().await?;
    let encoding = reader.read_i32().await?;
    // How this rectangle's pixels arrive. Decided here so the bounds check and the
    // tile path stay one path for all of them.
    let payload;
    match encoding {
        ENCODING_RAW => payload = Payload::Raw,
        ENCODING_COPY_RECT => payload = Payload::CopyRect,
        ENCODING_RRE => payload = Payload::Rre,
        ENCODING_HEXTILE => payload = Payload::Hextile,
        ENCODING_ZRLE => payload = Payload::Zrle,
        // Cursor: the rect header carries the hotspot (x, y) and the shape
        // size, never a framebuffer position — so it skips the bounds check
        // and tile path below entirely.
        ENCODING_CURSOR => {
            read_cursor(reader, cursor, (x, y, w, h), sink).await?;
            return Ok(RectEffect::NOTHING);
        }
        // No payload at all: the rectangle's presence is the whole message.
        ENCODING_LAST_RECT => return Ok(RectEffect::LAST),
        // DesktopSize: the rect itself is the announcement; no payload.
        //
        // Non-Apple RFB only, and the guard is not decoration: it carries no density,
        // so applying one with Apple metadata would overwrite a scale learned from a
        // display layout with `UNSCALED` and double the desktop's apparent size. It
        // *is* advertised there — [`vnc_apple::ENCODINGS`] must contain it or no
        // layout arrives at all — so this arm is reached in practice, and dropping
        // the rect is right: the layout carries the same size and the density with
        // it, and one arrives with every geometry change.
        ENCODING_DESKTOP_SIZE => {
            if apple.is_some() {
                debug!("vnc: ignoring a DesktopSize rect; the display layout is authoritative");
                return Ok(RectEffect::NOTHING);
            }
            return apply_resize(desktop, shadow, (w, h), UNSCALED, sink).await.map(RectEffect::resized);
        }
        ENCODING_EXTENDED_DESKTOP_SIZE if apple.is_none() => {
            return read_extended_desktop_size(reader, uplink, desktop, shadow, (x, y, w, h), sink)
                .await
                .map(RectEffect::resized);
        }
        // Ungated, like [`ENCODING_RAW`]: every generic target is offered zlib too,
        // and an Apple server cannot send what its own measured list omits.
        ENCODING_ZLIB => payload = Payload::Zlib,
        vnc_apple::ENCODING_CURSOR_IMAGE if apple.is_some() => {
            read_cursor_image(reader, apple, cursor, (x, y), (w, h), sink).await?;
            return Ok(RectEffect::NOTHING);
        }
        vnc_apple::ENCODING_DISPLAY_LAYOUT if apple.is_some() => {
            let first = apple.as_ref().is_some_and(|a| !a.asked_for_zlib);
            let virtual_display = apple.as_ref().is_some_and(|a| a.virtual_display);
            if let Some(a) = apple.as_mut() {
                a.asked_for_zlib = true;
            }
            read_display_layout(
                reader,
                shared,
                first,
                virtual_display,
                clipboard_enabled && virtual_display,
                sink,
            )
            .await?;
            // Finish consuming this FramebufferUpdate before asking for the full
            // repaint. If the layout handler asks here, the poll loop queues its
            // normal incremental request directly behind it. macOS keeps only the
            // later request, so a freshly cleared framebuffer receives damage
            // rectangles instead of its full contents.
            return Ok(RectEffect::FULL_REPAINT);
        }
        // Where the pointer is, which the rect header carries and nothing else does.
        // Advertised because the layout depends on the exact list, and ignored
        // because a client draws the pointer where it last put it.
        vnc_apple::ENCODING_CURSOR_POS if apple.is_some() => return Ok(RectEffect::NOTHING),
        // The Mac's keyboard and its hardware, neither of which this gateway acts on.
        // All three frame themselves the same way — a `u16` saying how much follows —
        // so one rule steps over all of them, and reading that length is the whole
        // point: the RFB stream above the record layer has no framing of its own, so
        // walking past by the wrong number of bytes desyncs everything after it.
        vnc_apple::ENCODING_VENDOR_KEYSYMS
        | vnc_apple::ENCODING_KEYBOARD_SOURCE
        | vnc_apple::ENCODING_DEVICE_INFO
            if apple.is_some() =>
        {
            let len = reader.read_u16().await?;
            discard(reader, u64::from(len)).await?;
            return Ok(RectEffect::NOTHING);
        }
        // Two more that frame themselves differently, so they cannot share the rule
        // above. `DisplayInfo` is in [`vnc_apple::ENCODINGS`] and so must be
        // steppable — advertising an encoding is a promise to be able to; `UserInfo`
        // is not advertised and is handled anyway, on the same grounds as
        // `DeviceInfo`. Neither was ever seen on macOS 26.
        //
        // `DisplayInfo` is the older display list — a header of four `u16`s, then
        // 0x1c bytes per screen — and carries no density, which is why the layout is
        // used instead even if this does turn up.
        vnc_apple::ENCODING_DISPLAY_INFO if apple.is_some() => {
            let mut head = [0u8; 8];
            reader.read_exact(&mut head).await?;
            let count = u64::from(u16::from_be_bytes([head[4], head[5]]));
            discard(reader, count * 0x1c).await?;
            return Ok(RectEffect::NOTHING);
        }
        // `UserInfo` is the logged-in account and its avatar: a counted name, then a
        // counted (zlib'd PNG) image.
        vnc_apple::ENCODING_USER_INFO if apple.is_some() => {
            let name = u64::from(reader.read_u16().await?);
            discard(reader, name).await?;
            let image = u64::from(reader.read_u32().await?);
            reader.read_u32().await?; // the image's encoding, which is not read
            discard(reader, image).await?;
            return Ok(RectEffect::NOTHING);
        }
        // A second rekey. The key could be recovered — the wrap key rotates to the
        // last content key — but installing it means swapping the ciphers on both
        // halves of a running session at the same instant, and the read and write
        // halves are in different tasks. Named and closed instead: macOS sends one
        // rekey per session, so if this is ever seen the log says what to build.
        vnc_apple::ENCODING_REKEY if apple.is_some() => {
            anyhow::bail!("the server re-keyed mid-session, which this client does not implement")
        }
        other => anyhow::bail!("server sent encoding {other}, which was not advertised"),
    }

    let size = desktop.lock().unwrap().size;
    // Bounds-check before allocating: a rect outside the announced desktop is
    // a protocol violation (and would let a bad length drive the allocation).
    anyhow::ensure!(
        u32::from(x) + u32::from(w) <= u32::from(size.0)
            && u32::from(y) + u32::from(h) <= u32::from(size.1),
        "rect {w}x{h}+{x}+{y} exceeds the {}x{} desktop",
        size.0,
        size.1
    );
    // Read the payload before deciding a rectangle of no pixels has nothing to do.
    // An encoding that frames itself — a length word, a subrect count, a source
    // position — sends that framing whatever its geometry says, and the RFB stream
    // has no framing of its own above the record layer, so stepping past by the
    // wrong number of bytes desyncs everything after it.
    let decoded = decoders
        .decode(reader, payload, shadow, sink.copies().then_some((x, y)), w, h)
        .await?;
    let Some(rect) = Rect::from_size(x, y, w, h) else {
        return Ok(RectEffect::NOTHING);
    };
    let rgb = match decoded {
        Decoded::Pixels(rgb) => rgb,
        // A CopyRect this client can do itself. The shadow has already moved its
        // own copy of the pixels, so the record is all that is owed — and it goes
        // through `msg` rather than the tile path because that is the queue whose
        // order against the tiles is the contract a copy reads the canvas under.
        Decoded::Copied(src) => {
            sink.msg(ServerMsg::Copy(protocol::CopyRect {
                sx: src.left,
                sy: src.top,
                x,
                y,
                w,
                h,
            }))
            .await?;
            return Ok(RectEffect::pixels(rect));
        }
        // A CopyRect onto pixels identical to the ones already there.
        Decoded::Unchanged => return Ok(RectEffect::pixels(rect)),
        // A CopyRect whose source this side never learned. Guessing would leave
        // wrong pixels on screen until something else happened to change that area;
        // one full request makes the source known instead.
        Decoded::Unavailable => return Ok(RectEffect::FULL_REPAINT),
    };

    // What of this rect the browser does not already have. A server that
    // re-sends unchanged pixels — and they do, on a cursor crossing a window
    // boundary or a client asking for a full update — stops costing the browser
    // link anything here.
    let Some(changed) = shadow.lock().unwrap().accept(rect, &rgb) else {
        return Ok(RectEffect::pixels(rect));
    };

    // Cropped out of the rect just read rather than out of the shadow: the bytes
    // are the same and this needs no lock. Its own buffer per piece, since the
    // encoder reads it after this function has returned and `rgb` is gone.
    sink.damage(&changed, |piece| {
        let mut pixels = Vec::new();
        tiles::crop(&rgb, rect, piece, &mut pixels);
        pixels
    })
    .await?;
    Ok(RectEffect::pixels(rect))
}

/// Handle a Cursor rect: `w * h` pixels in the negotiated format, followed by
/// a 1-bit-per-pixel transparency mask (rows padded to whole bytes, MSB first,
/// 1 = opaque). The hotspot rides in the rect's x/y. A 0x0 rect means the
/// server hid the pointer.
///
/// Receiving one at all is the server's admission that it is *not* drawing the
/// pointer into the framebuffer, so the shape is cached and forwarded to the
/// browser, which takes over rendering from here.
async fn read_cursor<R: AsyncRead + Unpin>(
    reader: &mut R,
    cursor: &SharedCursor,
    (hx, hy, w, h): (u16, u16, u16, u16),
    sink: &TileSink,
) -> anyhow::Result<()> {
    let (state, msg) = if w == 0 || h == 0 {
        debug!("vnc: server hid the pointer");
        (CursorState::Hidden, ServerMsg::Cursor(None))
    } else {
        let pixels_len = usize::from(w) * usize::from(h) * BPP;
        let mask_len = usize::from(w).div_ceil(8) * usize::from(h);
        if w > MAX_CURSOR_DIM || h > MAX_CURSOR_DIM {
            // Drop the shape but not the admission behind it: the server has
            // handed pointer drawing over, so report a hidden pointer and let
            // the browser draw its own arrow instead of nothing at all.
            warn!("vnc: ignoring an oversized {w}x{h} cursor");
            discard(reader, (pixels_len + mask_len) as u64).await?;
            (CursorState::Hidden, ServerMsg::Cursor(None))
        } else {
            let mut pixels = vec![0u8; pixels_len];
            reader.read_exact(&mut pixels).await?;
            let mut mask = vec![0u8; mask_len];
            reader.read_exact(&mut mask).await?;
            // Framebuffer pixels, per the pseudo-encoding's own convention.
            let shape =
                CursorShape::from_rgba(w, h, hx, hy, false, &masked_bgrx_to_rgba(&pixels, &mask, w))?;
            debug!("vnc: cursor {w}x{h} hotspot ({hx},{hy}), {} bytes", shape.png.len());
            (CursorState::Shape(shape.clone()), ServerMsg::Cursor(Some(shape)))
        }
    };
    *cursor.lock().unwrap() = state;
    sink.msg(msg).await
}

/// The [`ServerMsg`] that reproduces the current pointer state for a browser
/// that just attached, or `None` while the server is still drawing it itself.
fn cursor_msg(cursor: &SharedCursor) -> Option<ServerMsg> {
    match &*cursor.lock().unwrap() {
        CursorState::ServerDrawn => None,
        CursorState::Hidden => Some(ServerMsg::Cursor(None)),
        CursorState::Shape(shape) => Some(ServerMsg::Cursor(Some(shape.clone()))),
    }
}

/// Handle an ExtendedDesktopSize rect. The rect header is repurposed by the
/// extension: x = reason (0 server, 1 our SetDesktopSize, 2 another client),
/// y = status when the reason is 1 (0 = ok), w/h = the framebuffer size; the
/// payload is the screen layout. Receiving one at all is the server's
/// declaration that SetDesktopSize is supported.
async fn read_extended_desktop_size<R: AsyncRead + Unpin>(
    reader: &mut R,
    uplink: &SharedUplink,
    desktop: &SharedDesktop,
    shadow: &SharedShadow,
    (reason, status, w, h): (u16, u16, u16, u16),
    sink: &TileSink,
) -> anyhow::Result<bool> {
    let screens = reader.read_u8().await?;
    let mut padding = [0u8; 3];
    reader.read_exact(&mut padding).await?;
    let mut first = None;
    for i in 0..screens {
        let id = reader.read_u32().await?;
        discard(reader, 8).await?; // x, y, width, height — layout is unused
        let flags = reader.read_u32().await?;
        if i == 0 {
            first = Some(Screen { id, flags });
        }
    }

    let pending = {
        let mut d = desktop.lock().unwrap();
        if first.is_some() {
            d.screen = first;
        }
        d.pending.take()
    };

    let resized = if reason == 1 && status != 0 {
        // Our SetDesktopSize was rejected; the size on the rect is unchanged.
        warn!("vnc: server rejected SetDesktopSize (status {status})");
        false
    } else {
        apply_resize(desktop, shadow, (w, h), UNSCALED, sink).await?
    };

    // Replay a viewport report that arrived before support was declared.
    if let Some(want) = pending {
        let msg = {
            let d = desktop.lock().unwrap();
            (want != d.size)
                .then(|| d.screen.map(|screen| set_desktop_size(want, screen)))
                .flatten()
        };
        if let Some(msg) = msg {
            debug!("vnc: requesting desktop resize to {}x{} (replayed)", want.0, want.1);
            send(uplink, &msg).await?;
        }
    }
    Ok(resized)
}

/// Apply a server-announced framebuffer size: update the shared geometry and
/// forward it to the browser. Returns whether anything actually changed.
///
/// `scale` is how large those pixels should look — [`UNSCALED`] on generic RFB,
/// which has no way to say otherwise, and the Mac's own ratio when Apple metadata
/// was negotiated. A scale change with no size change still counts: the same pixels
/// shown at a different size is a different canvas.
async fn apply_resize(
    desktop: &SharedDesktop,
    shadow: &SharedShadow,
    new: (u16, u16),
    scale: f32,
    sink: &TileSink,
) -> anyhow::Result<bool> {
    anyhow::ensure!(
        new.0 > 0 && new.1 > 0,
        "server resized the desktop to {}x{}",
        new.0,
        new.1
    );
    let (was, resize_msg) = {
        let mut d = desktop.lock().unwrap();
        if d.size == new && d.scale == scale {
            return Ok(false);
        }
        let was = (d.size, d.scale);
        d.size = new;
        d.scale = scale;
        (was, d.resize_msg())
    };
    // The old pixels describe a framebuffer that no longer exists, and the
    // browser is about to reallocate its canvas.
    shadow.lock().unwrap().resize(new.0, new.1);
    // The cell grid is anchored at (0,0) in framebuffer pixels, so a new size makes
    // every key name somewhere else.
    sink.reset_render();
    info!(
        "vnc: desktop resized from {}x{} at {}x to {}x{} at {scale}x",
        was.0.0, was.0.1, was.1, new.0, new.1
    );
    sink.msg(resize_msg).await?;
    Ok(true)
}

/// Handle an Apple `CursorImage` rect: a shape stored once under an id, then
/// re-selected by that id every time the pointer changes shape.
///
/// The whole body is read before anything is decided, so an oversized or unknown
/// shape costs the stream nothing — the alternative is a partially consumed
/// rectangle, which desyncs everything after it.
async fn read_cursor_image<R: AsyncRead + Unpin>(
    reader: &mut R,
    apple: &mut Option<Apple>,
    cursor: &SharedCursor,
    hotspot: (u16, u16),
    size: (u16, u16),
    sink: &TileSink,
) -> anyhow::Result<()> {
    let id = reader.read_u32().await?;
    let len = reader.read_u32().await?;
    anyhow::ensure!(
        u64::from(len) <= MAX_CURSOR_BYTES,
        "a cursor rect claims {len} compressed bytes, past the {MAX_CURSOR_BYTES} ceiling"
    );
    let mut deflated = vec![0u8; len as usize];
    reader.read_exact(&mut deflated).await?;

    let apple = apple.as_mut().expect("cursor images are the Apple dialect's alone");
    let shape = match apple.cursors.accept(id, hotspot, size, &deflated) {
        Err(error) => {
            // Cursor stores are individually compressed, so this body has been
            // fully consumed and a bad one has no state the next shape depends on.
            // Keep the last usable pointer instead of ending the desktop session.
            warn!("vnc: ignoring cursor image {id}: {error:#}");
            return Ok(());
        }
        Ok(vnc_apple::Cursor::Shape(shape)) => shape,
        // Nothing to draw and nothing to say: the pointer keeps the shape it has,
        // which is closer to the truth than blanking it.
        Ok(vnc_apple::Cursor::Unchanged) => return Ok(()),
    };
    *cursor.lock().unwrap() = CursorState::Shape(shape.clone());
    sink.msg(ServerMsg::Cursor(Some(shape))).await
}

/// Handle an `AppleDisplayLayout` rect: the Mac's screens, and the geometry it is
/// rendering them at.
///
/// Three things follow from one of these, and the third is the one that is easy to
/// miss. The framebuffer may have changed size. The display list may have changed.
/// And the server's *arming* has been dropped — a layout is emitted at a login, a
/// lock and a fast-user-switch as well as at a real geometry change, and after any
/// of them the server stops sending on its own. Not re-arming does not look like an
/// error: the desktop keeps painting and the pointer silently freezes on whatever
/// shape it last had.
async fn read_display_layout<R: AsyncRead + Unpin>(
    reader: &mut R,
    shared: &Shared,
    ask_for_zlib: bool,
    virtual_display: bool,
    rearm_pasteboard: bool,
    sink: &TileSink,
) -> anyhow::Result<bool> {
    let Shared { uplink, desktop, shadow, display, .. } = shared;
    let declared = reader.read_u16().await?;
    // Two fewer than declared, which is the count the Mac actually sends — see
    // [`vnc_apple::parse_layout`], where the reason and the measurement are.
    anyhow::ensure!(
        declared >= 4,
        "a display layout declared {declared} bytes, less than its own length prefix"
    );
    let mut payload = declared.to_be_bytes().to_vec();
    payload.resize(usize::from(declared) - 2, 0);
    reader.read_exact(&mut payload[2..]).await?;
    let layout = if virtual_display {
        vnc_apple::parse_virtual_display_layout(&payload)?
    } else {
        vnc_apple::parse_layout(&payload)?
    };

    let resized = apply_resize(desktop, shadow, layout.backing, layout.scale(), sink).await?;

    // The Mac says which screen it is sending, so nothing here has to be inferred
    // from what was asked for. `current` is a screen id, or `None` for the combined
    // view of all of them — which is what a session starts on, and which
    // [`DisplayState::COMBINED`] is the client-facing name for.
    let msg = {
        let mut state = display.lock().unwrap();
        let mut infos = layout.infos();
        // With more than one screen there is a combined view to go back to, and it
        // has to be listed or a client that picks a screen can never leave it. With
        // one screen there is nothing to combine, and the entry would be the same
        // picture under a second name.
        if infos.len() > 1 {
            // No size on this one, deliberately. The framebuffer is only the union
            // of every screen while the combined view is the one selected; ask for a
            // single screen and the next layout reports that screen's size instead,
            // so any number here would be wrong half the time.
            let detail = format!("{} screens side by side", infos.len());
            infos.insert(
                0,
                DisplayInfo {
                    id: DisplayState::COMBINED,
                    label: "All Displays".into(),
                    detail,
                    main: false,
                    virtual_display: false,
                },
            );
        }
        let active = layout.current.unwrap_or(DisplayState::COMBINED);
        state.repaint_pixels = layout.repaint_pixels();
        let changed = state.displays != infos || state.active != active;
        state.displays = infos;
        state.active = active;
        // Sent only on a change, since a client holds no display state of its own
        // and the checkmark is the only thing telling it what it is looking at. Most
        // layouts change neither half — one arrives at every login and lock — and
        // say nothing new.
        changed.then(|| state.displays_msg()).flatten()
    };
    if let Some(msg) = msg {
        sink.msg(msg).await?;
    }

    let size = desktop.lock().unwrap().size;
    let mut uplink = uplink.lock().await;
    // Now that the Mac has said what it has, ask for compression. This has to wait
    // for a layout: zlib in the *first* `SetEncodings` costs the layout entirely, and
    // asking again here keeps the display state and merely changes encoder. Sent
    // before the re-arm so the update that follows is the compressed one. Both
    // subtypes reach here — a layout is what the upgrade waits on, not a dialect.
    if ask_for_zlib {
        debug!("vnc: display layout received, asking for zlib");
        uplink.send(&set_encodings(vnc_apple::ENCODINGS_WITH_ZLIB)).await?;
    }
    // The initial virtual-display layout can arrive after the cleartext enable.
    // Repeat it here, after the Mac has answered that setup, and on later layouts
    // just as cursor arming is repeated. The command is idempotent.
    if rearm_pasteboard {
        uplink.send(&vnc_apple_clipboard::auto_pasteboard(true)).await?;
    }
    // Re-arm, on every layout and not only on a change of geometry.
    //
    // Logged with the geometry it arms for: this is the one message that tells the
    // Mac what to stream, so an arming that disagrees with the desktop the gateway
    // just adopted is what a resize going wrong looks like from here.
    debug!("vnc: arming auto framebuffer updates for {}x{}", size.0, size.1);
    uplink.send(&vnc_apple::auto_framebuffer_update(size)).await?;
    Ok(resized)
}

/// Scroll intent turned into RFB wheel pulses, carrying the sub-pulse remainder
/// between events.
///
/// RFB has no scroll magnitude: a wheel is buttons 4-7, and the only thing a
/// client can vary is how many times it pulses one. Apple's own protocol is no
/// better — its `0x10` input event carries a button/scroll *mask* too — so
/// Screen Sharing.app is pulsing as well, and a pulse count is the whole of the
/// vocabulary here.
///
/// How far one pulse scrolls is the server's business, and macOS is *far* more
/// frugal with it than the desktop convention: measured against a live Mac, a
/// pulse is worth about two pixels, where an X11 server hands the pulse to a
/// toolkit that spends it as a notch's worth. That is why spending any nonzero
/// delta as exactly one pulse, which is what remotex used to do and what noVNC
/// and RealVNC still do, makes a Mac crawl: one flick of a wheel is ~600px of
/// intent and bought six pixels of scrolling.
///
/// Only the Apple subtypes are converted proportionally, because the Mac is the
/// only server whose price has been measured. Charging a generic server the same
/// way overshoots — an X11 desktop asked for a distance in pulses it spends a
/// notch apiece on scrolls in lurches — so those keep the one-pulse convention
/// every other client follows and every such server is tuned for.
enum Wheel {
    /// One pulse per event, whatever the delta.
    Notch,
    /// Pulses proportional to the distance asked for, holding the sub-pulse
    /// remainder per axis between events.
    Apple { pending: (f32, f32) },
}

impl Wheel {
    /// The line height a `line` delta is worth. Trackpads report distance and
    /// notched wheels report lines, and pixels are the common unit.
    const LINE_PX: f32 = 16.0;
    /// A `page` delta, in lines: a screenful, which is what the DOM means by it.
    const PAGE_LINES: f32 = 20.0;
    /// The most one event may spend, in pixels of intent: a single absurd delta —
    /// a flick, or a client that reports a whole document — must not turn into
    /// thousands of pointer events queued ahead of everything else on the uplink.
    /// Well above the ~400px an accelerated flick reports at its peak.
    const MAX_PX: f32 = 512.0;
    /// A pulse on macOS Screen Sharing. Measured, not derived: nothing in either
    /// protocol says what a pulse is worth, and this is the value at which a
    /// flick moves a Mac about as far as it moves the local screen.
    const APPLE_PX_PER_PULSE: f32 = 2.0;

    /// `apple` is the two Apple subtypes, the servers whose pulse has been
    /// measured — not merely a macOS server, since a Mac reached as plain `vnc`
    /// has not been.
    fn new(apple: bool) -> Self {
        if apple {
            Self::Apple { pending: (0.0, 0.0) }
        } else {
            Self::Notch
        }
    }

    /// Whole pulses to send for one wheel event, as (horizontal, vertical).
    fn pulses(&mut self, dx: f32, dy: f32, unit: WheelUnit) -> (i32, i32) {
        let px = |delta: f32| match unit {
            WheelUnit::Pixel => delta,
            WheelUnit::Line => delta * Self::LINE_PX,
            WheelUnit::Page => delta * Self::PAGE_LINES * Self::LINE_PX,
        };
        match self {
            Self::Notch => (notch(dx), notch(dy)),
            Self::Apple { pending } => {
                let step = Self::APPLE_PX_PER_PULSE;
                let max = Self::MAX_PX / step;
                (
                    Self::spend(&mut pending.0, px(dx) / step, max),
                    Self::spend(&mut pending.1, px(dy) / step, max),
                )
            }
        }
    }

    /// Add one axis' worth of intent and take the whole pulses out of it. The
    /// fraction stays behind, so a slow trackpad glide of deltas too small to be
    /// a pulse on their own still adds up to one.
    fn spend(pending: &mut f32, add: f32, max: f32) -> i32 {
        if add == 0.0 || !add.is_finite() {
            return 0;
        }
        // A reversal starts over rather than first paying off the remainder of
        // the direction the user just left, which would swallow the flick back.
        if pending.signum() != add.signum() {
            *pending = 0.0;
        }
        *pending += add;
        let whole = pending.trunc();
        if whole.abs() >= max {
            // Capped: the surplus is dropped rather than kept, so a flick cannot
            // leave pulses trickling out under the next few events. At the cap
            // exactly as much as beyond it — an event spending every pulse it is
            // allowed has nothing left over by definition, and treating that one
            // as uncapped would carry a fraction the next event could round up
            // into a pulse the cap was there to refuse.
            *pending = 0.0;
            return (whole.signum() * max) as i32;
        }
        *pending -= whole;
        whole as i32
    }
}

/// The pointer-mask bit each mouse button sets, by server dialect.
///
/// RFB's convention is bit 1 = left, bit 2 = middle, bit 3 = right, and every
/// server here honours it — including Apple's Standard mode, measured on macOS
/// 26.6 by holding each button through a live session and reading
/// `CGEventSource.buttonState` on the Mac. High Performance mode's agent reads
/// the same mask positionally instead, as CGMouseButton numbers: bit 2 =
/// *right*, bit 3 = *middle*. A right-click sent by the book therefore lands on
/// the virtual display as a middle-click — the button macOS does nothing with —
/// which is what a dead right button in that mode was. The two bits are swapped
/// for that subtype alone. See docs/apple-vnc-889.md.
enum Buttons {
    /// The RFB convention: bit 2 = middle, bit 3 = right.
    Rfb,
    /// Apple High Performance's positional reading: bit 2 = right, bit 3 = middle.
    HighPerformance,
}

impl Buttons {
    fn new(high_performance: bool) -> Self {
        if high_performance {
            Self::HighPerformance
        } else {
            Self::Rfb
        }
    }

    /// The mask bit this button sets, or `None` for a button no server reads.
    /// RFB's mask has bits for buttons 8 and 9, but no server agrees on what
    /// they mean and the ones remotex talks to ignore them. `Back` and
    /// `Forward` are dropped rather than sent as a scroll notch, which is what
    /// those bits are on every server that does read them.
    fn bit(&self, button: MouseButton) -> Option<u8> {
        match (self, button) {
            (_, MouseButton::Left) => Some(0x01),
            (Self::Rfb, MouseButton::Middle) => Some(0x02),
            (Self::Rfb, MouseButton::Right) => Some(0x04),
            (Self::HighPerformance, MouseButton::Middle) => Some(0x04),
            (Self::HighPerformance, MouseButton::Right) => Some(0x02),
            (_, MouseButton::Back | MouseButton::Forward) => None,
        }
    }
}

/// One pulse in the direction of a nonzero delta: the RFB convention, where the
/// magnitude a client reports is dropped and the server decides how far a scroll
/// goes.
fn notch(delta: f32) -> i32 {
    if !delta.is_finite() || delta == 0.0 {
        0
    } else if delta > 0.0 {
        1
    } else {
        -1
    }
}

/// Translate one browser input message into RFB client messages, updating the
/// tracked pointer state.
///
/// A *list* of messages, not one buffer of them. A wheel notch is a press and a
/// release, and on the 003.889 wire each has to go in a record of its own: two
/// concatenated into one record means the server reads the press and drops the
/// release, leaving a wheel button held down. Keeping them separate here is what
/// makes that impossible rather than remembered.
fn translate_input(
    input: ClientMsg,
    buttons: &Buttons,
    button_mask: &mut u8,
    last_pos: &mut (u16, u16),
    pressed_keys: &mut HashMap<String, u32>,
    wheel: &mut Wheel,
) -> Vec<Vec<u8>> {
    match input {
        ClientMsg::MouseMove { x, y } => {
            *last_pos = (clamp_u16(x), clamp_u16(y));
            vec![pointer_event(*button_mask, *last_pos).to_vec()]
        }
        // `clicks` goes nowhere: RFB carries a button mask alone, and the guest
        // counts the clicks itself from the events it receives.
        ClientMsg::MouseButton { button, pressed, .. } => {
            let Some(bit) = buttons.bit(button) else {
                return Vec::new();
            };
            if pressed {
                *button_mask |= bit;
            } else {
                *button_mask &= !bit;
            }
            vec![pointer_event(*button_mask, *last_pos).to_vec()]
        }
        ClientMsg::Wheel { dx, dy, unit } => {
            // A wheel pulse is a press+release of buttons 4-7 (mask bits 3-6):
            // 4 = up, 5 = down, 6 = left, 7 = right. How many of them this delta
            // is worth is [`Wheel`]'s business — the magnitude has nowhere else
            // to go on this wire.
            let (px, py) = wheel.pulses(dx, dy, unit);
            // Debug rather than trace: what a client actually reports per notch
            // is the input to every constant above, and it varies by browser,
            // by pointing device and by platform.
            debug!("vnc: wheel dx={dx} dy={dy} {unit:?} -> {px} + {py} pulses");
            let mut out = Vec::new();
            for (pulses, negative_bit, positive_bit) in [(py, 0x08, 0x10), (px, 0x20, 0x40)] {
                let bit = if pulses > 0 { positive_bit } else { negative_bit };
                for _ in 0..pulses.abs() {
                    out.push(pointer_event(*button_mask | bit, *last_pos).to_vec());
                    out.push(pointer_event(*button_mask, *last_pos).to_vec());
                }
            }
            out
        }
        ClientMsg::Key {
            code,
            pressed,
            caps,
        } => {
            // CapsLock is never forwarded: leaving the server's Lock modifier
            // off keeps our pre-resolved keysym from being re-cased by
            // "Shift+Lock" keymap ambiguity. Case is applied here instead, from
            // the browser-reported `caps` state carried on every key event.
            if code == "CapsLock" {
                return Vec::new();
            }
            if pressed {
                // Resolve the symbol against the live modifier state so the
                // shifted keysym (`A`, `!`) is sent, not the base one. CapsLock
                // affects letters only, XORed with Shift.
                let shift_down = pressed_keys.contains_key("ShiftLeft")
                    || pressed_keys.contains_key("ShiftRight");
                let is_letter = matches!(code.as_bytes(), [b'K', b'e', b'y', b'A'..=b'Z']);
                let shift = if is_letter { shift_down ^ caps } else { shift_down };
                match keymap::keysym(&code, shift) {
                    Some(sym) => {
                        pressed_keys.insert(code, sym);
                        vec![key_event(true, sym).to_vec()]
                    }
                    None => {
                        debug!("vnc: unmapped key code {code}");
                        Vec::new()
                    }
                }
            } else {
                // Release exactly what was pressed; fall back to the unshifted
                // keysym for a release with no matching press.
                match pressed_keys
                    .remove(&code)
                    .or_else(|| keymap::keysym(&code, false))
                {
                    Some(sym) => vec![key_event(false, sym).to_vec()],
                    None => {
                        debug!("vnc: unmapped key code {code}");
                        Vec::new()
                    }
                }
            }
        }
        // Intercepted by the input loop (request_resize) before translation.
        ClientMsg::Viewport { .. } | ClientMsg::DefaultSize => Vec::new(),
        // Intercepted by the input loop (full repaint) before translation.
        ClientMsg::Refresh => Vec::new(),
        // Intercepted by the input loop (the clipboard bridge, which needs the
        // shared buffer and the tile sink) before translation.
        ClientMsg::Clipboard { .. } | ClientMsg::ClipboardRequest => Vec::new(),
        // Session-control messages act on the slot, not an engine — the ws
        // bridge handles them and they never reach here. `CacheReset` is one of
        // them: it empties that socket's tile cache and injects its own `Refresh`.
        ClientMsg::Connect { .. }
        | ClientMsg::Disconnect
        | ClientMsg::CacheReset
        | ClientMsg::PaintAck { .. } => Vec::new(),
        // Intercepted by the input loop, which is where the requested screen is
        // checked — see the `SelectDisplay` branch there. Generic RFB has nothing
        // for it; the Apple extension supplies the selectable list on either
        // transport.
        ClientMsg::SelectDisplay { .. } => Vec::new(),
        // Intercepted by the input loop where it means something — a High
        // Performance virtual display with resize granted re-renders at the new
        // density. Everywhere else there is nothing to act on: RFB has no backing
        // scale, and a VNC server's framebuffer is already the pixels it has.
        // Clients send this unconditionally rather than asking what the engine
        // is, so it is ignored here rather than treated as a client error.
        ClientMsg::HostScale { .. } => Vec::new(),
    }
}

// ── RFB message builders (all integers big-endian, per RFC 6143) ────────────

/// SetPixelFormat: 32 bpp, depth 24, little-endian, true colour, 8 bits per
/// channel with red<<16 / green<<8 / blue<<0 — i.e. raw pixels arrive as
/// B, G, R, pad bytes, which [`bgrx_to_rgb`] repacks for the tile encoder.
fn set_pixel_format() -> [u8; 20] {
    let mut msg = [0u8; 20];
    msg[0] = 0; // message type
    // msg[1..4]: padding
    msg[4] = 32; // bits per pixel
    msg[5] = 24; // depth
    msg[6] = 0; // big-endian flag: off
    msg[7] = 1; // true-colour flag: on
    msg[8..10].copy_from_slice(&255u16.to_be_bytes()); // red max
    msg[10..12].copy_from_slice(&255u16.to_be_bytes()); // green max
    msg[12..14].copy_from_slice(&255u16.to_be_bytes()); // blue max
    msg[14] = 16; // red shift
    msg[15] = 8; // green shift
    msg[16] = 0; // blue shift
    // msg[17..20]: padding
    msg
}

/// SetEncodings for the given encoding list.
fn set_encodings(encodings: &[i32]) -> Vec<u8> {
    let mut msg = vec![2u8, 0];
    msg.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
    for &encoding in encodings {
        msg.extend_from_slice(&encoding.to_be_bytes());
    }
    msg
}

/// FramebufferUpdateRequest for the whole desktop.
fn update_request(incremental: bool, size: (u16, u16)) -> [u8; 10] {
    let mut msg = [0u8; 10];
    msg[0] = 3; // message type
    msg[1] = u8::from(incremental);
    // msg[2..6]: x, y = 0
    msg[6..8].copy_from_slice(&size.0.to_be_bytes());
    msg[8..10].copy_from_slice(&size.1.to_be_bytes());
    msg
}

/// EnableContinuousUpdates for the whole desktop.
///
/// Sent only after the server has announced support by sending
/// [`MSG_END_OF_CONTINUOUS_UPDATES`], and re-sent whenever the desktop changes size:
/// the region is part of the request, so a server told about the old one would go on
/// pushing updates for a rectangle that no longer exists.
fn enable_continuous_updates(enable: bool, size: (u16, u16)) -> [u8; 10] {
    let mut msg = [0u8; 10];
    msg[0] = MSG_END_OF_CONTINUOUS_UPDATES; // the client message shares the number
    msg[1] = u8::from(enable);
    // msg[2..6]: x, y = 0
    msg[6..8].copy_from_slice(&size.0.to_be_bytes());
    msg[8..10].copy_from_slice(&size.1.to_be_bytes());
    msg
}

/// ClientFence: the server's own marker handed straight back.
///
/// BlockBefore and BlockAfter need nothing done to honour them — this loop reads and
/// acts on one message at a time, in order — so the whole of the obligation is the
/// echo, and the flags this end does not implement are dropped from it rather than
/// claimed.
fn client_fence(flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut msg = vec![MSG_FENCE, 0, 0, 0];
    msg.extend_from_slice(&flags.to_be_bytes());
    msg.push(payload.len() as u8);
    msg.extend_from_slice(payload);
    msg
}

/// SetDesktopSize: ask the server to re-render at the given framebuffer size,
/// laid out as a single screen echoing the server's screen id and flags.
fn set_desktop_size(size: (u16, u16), screen: Screen) -> [u8; 24] {
    let mut msg = [0u8; 24];
    msg[0] = 251; // message type
    // msg[1]: padding
    msg[2..4].copy_from_slice(&size.0.to_be_bytes());
    msg[4..6].copy_from_slice(&size.1.to_be_bytes());
    msg[6] = 1; // number of screens
    // msg[7]: padding
    msg[8..12].copy_from_slice(&screen.id.to_be_bytes());
    // msg[12..16]: screen x, y = 0
    msg[16..18].copy_from_slice(&size.0.to_be_bytes());
    msg[18..20].copy_from_slice(&size.1.to_be_bytes());
    msg[20..24].copy_from_slice(&screen.flags.to_be_bytes());
    msg
}

/// KeyEvent.
fn key_event(down: bool, keysym: u32) -> [u8; 8] {
    let mut msg = [0u8; 8];
    msg[0] = 4; // message type
    msg[1] = u8::from(down);
    // msg[2..4]: padding
    msg[4..8].copy_from_slice(&keysym.to_be_bytes());
    msg
}

/// PointerEvent.
fn pointer_event(button_mask: u8, pos: (u16, u16)) -> [u8; 6] {
    let mut msg = [0u8; 6];
    msg[0] = 5; // message type
    msg[1] = button_mask;
    msg[2..4].copy_from_slice(&pos.0.to_be_bytes());
    msg[4..6].copy_from_slice(&pos.1.to_be_bytes());
    msg
}

/// ClientCutText: put `text` on the remote's clipboard.
///
/// RFB cut text is latin-1 ([`latin1_from_str`]). `None` over
/// [`MAX_CLIPBOARD_BYTES`]: the caller has already refused by then, and an
/// encoder that quietly truncated instead would be the one place a partial
/// paste could still reach a remote.
fn client_cut_text(text: &str) -> Option<Vec<u8>> {
    if !clipboard_fits(text) {
        return None;
    }
    let bytes = latin1_from_str(text);
    let mut msg = vec![6u8, 0, 0, 0]; // message type + 3 padding
    msg.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    msg.extend_from_slice(&bytes);
    Some(msg)
}

/// ClientCutText carrying an Extended Clipboard body.
///
/// Same message type as [`client_cut_text`]; the negative length is the whole
/// signal that the payload is a flags word rather than latin-1 text.
fn cut_text_extended(body: &[u8]) -> Vec<u8> {
    let mut msg = vec![6u8, 0, 0, 0]; // message type + 3 padding
    msg.extend_from_slice(&(-(body.len() as i32)).to_be_bytes());
    msg.extend_from_slice(body);
    msg
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Decode RFB cut text (latin-1) into a `String`: every byte is the codepoint
/// of the same value.
fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// Encode a `String` as RFB cut text (latin-1).
///
/// Anything outside latin-1 becomes `?`, which is what noVNC does and all the
/// baseline protocol can carry — RFB's UTF-8 clipboard lives in the Extended
/// Clipboard pseudo-encoding, which this client does not negotiate.
///
/// Length is [`client_cut_text`]'s business: latin-1 spends one byte per char
/// where UTF-8 spends at least one, so text that fits the ceiling as UTF-8
/// cannot exceed it here.
fn latin1_from_str(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

/// Classic VNC authentication: DES-ECB over the 16-byte challenge, keyed by
/// the first 8 bytes of the password (zero-padded) with the bit order of each
/// key byte reversed — the RFB spec's non-standard DES key convention.
fn auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (slot, byte) in key.iter_mut().zip(password.bytes()) {
        *slot = byte.reverse_bits();
    }
    let cipher = Des::new(GenericArray::from_slice(&key));
    let mut response = *challenge;
    for block in response.chunks_exact_mut(8) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    response
}

/// Pick the RFB security type to answer with.
///
/// The target's subtype decides, not which credential fields happen to be
/// filled: `Ard` is a declaration that the far end is a Mac and that the
/// credentials name an account there, while a plain `vnc` target has only
/// `vnc_password`, a secret belonging to the *machine* that tells the server
/// nothing about who is connecting. On a Mac that difference decides which
/// screen you get — see [`ard_authenticate`] — so a subtype the server cannot
/// answer is an error rather than a silent fall back to the anonymous path.
fn choose_security(
    types: &[u8],
    subtype: Option<Subtype>,
    vnc_password: &str,
) -> anyhow::Result<u8> {
    // Both Apple subtypes authenticate the same way and neither falls back: the
    // credentials are a macOS account's, and there is nothing else on the list that
    // could carry them. The subtype names itself in the refusal, since the two are
    // configured differently and the reader needs to know which one they wrote.
    if let Some(subtype) = subtype.filter(|s| s.apple_authentication()) {
        anyhow::ensure!(
            types.contains(&SECURITY_ARD),
            "the target is subtype {:?}, whose authentication this server does not \
             offer (types {types:?}) — it is not macOS Screen Sharing",
            subtype.name()
        );
        return Ok(SECURITY_ARD);
    }
    if !vnc_password.is_empty() && types.contains(&SECURITY_VNC_AUTH) {
        return Ok(SECURITY_VNC_AUTH);
    }
    if types.contains(&SECURITY_NONE) {
        return Ok(SECURITY_NONE);
    }
    anyhow::ensure!(
        !types.contains(&SECURITY_VNC_AUTH),
        "VNC server requires a password but the target has no vnc_password configured"
    );
    anyhow::bail!(
        "no supported VNC security type (server offers {types:?}; \
         this client speaks None, VncAuth and Apple's DH authentication)"
    )
}

/// Apple's Diffie-Hellman authentication (RFB security type 30): the server
/// sends a generator, a key length, the prime modulus and its public key; the
/// answer is the credentials encrypted under the shared secret, then our own
/// public key.
///
/// This is the only way to tell a Mac **who** is connecting, and that is the
/// whole reason it is here. Authenticated with a password alone, a connection
/// is `uid -2` — nobody — and macOS answers an anonymous viewer by creating a
/// *new login-window session on a virtual display* rather than sharing the
/// screen, leaving the client on a login screen it can never get past while the
/// signed-in user's session carries on beside it. Named, the same connection
/// resolves to that user's own session: measured on macOS 26, where
/// `screensharingd` logged `uid 501 createLoginWindow 0` and attached to the
/// console. So for a Mac target the credentials are the *account's*, not the
/// Screen Sharing password's.
async fn ard_authenticate<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    username: &str,
    password: &str,
) -> anyhow::Result<[u8; 16]> {
    let generator = reader.read_u16().await?;
    let key_len = usize::from(reader.read_u16().await?);
    anyhow::ensure!(
        (MIN_ARD_KEY_BYTES..=MAX_ARD_KEY_BYTES).contains(&key_len),
        "VNC server offered a {key_len}-byte DH key, outside the \
         {MIN_ARD_KEY_BYTES}..={MAX_ARD_KEY_BYTES} accepted"
    );
    let mut prime = vec![0u8; key_len];
    reader.read_exact(&mut prime).await?;
    let mut peer_public = vec![0u8; key_len];
    reader.read_exact(&mut peer_public).await?;
    // A zero modulus is not a weak group but an arithmetic impossibility, and
    // `BigUint::modpow` answers it with a panic rather than a value — so it has
    // to be refused here, before the exchange, and not inside it.
    anyhow::ensure!(
        prime.iter().any(|&b| b != 0),
        "VNC server offered a zero DH prime"
    );
    // And the server's public key has to be a real member of that group. The
    // degenerate ones — 0, 1, and p-1 — each collapse the shared secret to a
    // value anyone watching the exchange can work out for themselves, which
    // costs the account password its only cover on the wire. Parsed twice
    // rather than threaded into [`ard_exchange`], so the arithmetic stays a
    // pure function of bytes and this stays where the rest of the refusals are.
    let modulus = BigUint::from_bytes_be(&prime);
    let peer = BigUint::from_bytes_be(&peer_public);
    anyhow::ensure!(
        peer > BigUint::from(1u8) && peer < modulus - 1u8,
        "VNC server offered a degenerate DH public key"
    );
    debug!("vnc: Apple DH authentication as {username:?}, {}-bit prime", key_len * 8);

    let mut rng = rand::rng();
    let mut private = vec![0u8; key_len];
    let mut filler = [0u8; ARD_CREDENTIALS_LEN];
    rng.fill_bytes(&mut private);
    rng.fill_bytes(&mut filler);

    let (secret, public) = ard_exchange(generator, &prime, &peer_public, &private);
    let key = ard_wrap_key(&secret);
    let credentials = ard_credentials(username, password, filler)?;
    writer.write_all(&ard_encrypt(&key, &credentials)).await?;
    writer.write_all(&public).await?;
    Ok(key)
}

/// The Diffie-Hellman half: the shared secret and the public key to send with
/// it, both left-padded to the server's key length.
///
/// The padding is not cosmetic. The secret is hashed as bytes, so a secret that
/// happens to be numerically small has to carry its leading zeros or the two
/// ends derive different keys — an authentication that fails once in a few
/// hundred connections rather than never.
fn ard_exchange(
    generator: u16,
    prime: &[u8],
    peer_public: &[u8],
    private: &[u8],
) -> (Vec<u8>, Vec<u8>) {
    let modulus = BigUint::from_bytes_be(prime);
    let private = BigUint::from_bytes_be(private);
    let public = BigUint::from(generator).modpow(&private, &modulus);
    let secret = BigUint::from_bytes_be(peer_public).modpow(&private, &modulus);
    let pad = |value: BigUint| {
        let bytes = value.to_bytes_be();
        let mut padded = vec![0u8; prime.len().saturating_sub(bytes.len())];
        padded.extend_from_slice(&bytes);
        padded
    };
    (pad(secret), pad(public))
}

/// Pack the credentials the way Apple's server expects to find them: the user
/// name at 0 and the password at 64, each null-terminated, everything else left
/// as the random filler it arrived as (so identical credentials do not encrypt
/// to identical ciphertext).
fn ard_credentials(
    username: &str,
    password: &str,
    filler: [u8; ARD_CREDENTIALS_LEN],
) -> anyhow::Result<[u8; ARD_CREDENTIALS_LEN]> {
    let mut blob = filler;
    for (offset, field, what) in [
        (0, username, "username"),
        (ARD_FIELD_LEN, password, "password"),
    ] {
        let bytes = field.as_bytes();
        anyhow::ensure!(
            bytes.len() < ARD_FIELD_LEN,
            "the target's {what} is {} bytes; Apple's DH authentication carries at most {}",
            bytes.len(),
            ARD_FIELD_LEN - 1
        );
        blob[offset..offset + bytes.len()].copy_from_slice(bytes);
        blob[offset + bytes.len()] = 0;
    }
    Ok(blob)
}

/// The AES-128 key derived from a Diffie-Hellman shared secret: its MD5.
///
/// Named, rather than computed inside [`ard_encrypt`], because it is not private
/// to the credential encryption: on the 003.889 wire the same digest is the
/// record layer's first wrap key (see [`crate::vnc_record`]). One derivation, two
/// readers, and no chance of them drifting apart.
fn ard_wrap_key(secret: &[u8]) -> [u8; 16] {
    Md5::digest(secret).into()
}

/// Encrypt the credential blob under the shared secret: AES-128 in ECB mode,
/// keyed by [`ard_wrap_key`]. ECB is Apple's choice, not one available to
/// us — the blob is exactly eight blocks and the server decrypts them
/// independently.
fn ard_encrypt(key: &[u8; 16], credentials: &[u8; ARD_CREDENTIALS_LEN]) -> Vec<u8> {
    use aes::cipher::{BlockCipherEncrypt as _, KeyInit as _};

    let cipher = Aes128::new(key.into());
    let mut out = credentials.to_vec();
    for block in out.chunks_exact_mut(16) {
        cipher.encrypt_block((&mut *block).try_into().expect("16-byte AES block"));
    }
    out
}

/// Parse the 12-byte RFB greeting `RFB xxx.yyy\n` into (major, minor).
fn parse_version(greeting: &[u8; 12]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(greeting).ok()?;
    let rest = text.strip_prefix("RFB ")?.strip_suffix('\n')?;
    let (major, minor) = rest.split_once('.')?;
    if major.len() != 3 || minor.len() != 3 {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Whether the far end is macOS Screen Sharing, from what it said during the
/// handshake. Apple's server announces its own protocol revision, RFB 003.889,
/// and offers Apple's security types (30 = ARD, 35 = Mac authentication)
/// alongside the standard ones — no other server does either.
///
/// A third-party VNC server running on a Mac looks like any other server here
/// and is reported as not-macOS. What that costs is the browser client's
/// Command-key convention, not correctness, which is why guessing from a desktop
/// name is not worth it.
fn is_macos_server(minor: u32, security_types: &[u8]) -> bool {
    minor == 889 || security_types.iter().any(|t| matches!(t, 30 | 35))
}

/// Repack a cursor's BGRX pixels into RGBA, folding the RFB 1-bit mask into
/// the alpha channel: rows are padded to whole bytes and scanned MSB first,
/// with a set bit meaning opaque. Pixels outside the mask are cleared to fully
/// transparent black rather than just alpha-zeroed, so the cursor PNG's filtering has a
/// flat area to compress and no stale colour can bleed through a viewer that
/// ignores alpha.
fn masked_bgrx_to_rgba(bgrx: &[u8], mask: &[u8], w: u16) -> Vec<u8> {
    let stride = usize::from(w).div_ceil(8);
    let mut rgba = Vec::with_capacity(bgrx.len());
    for (i, px) in bgrx.chunks_exact(BPP).enumerate() {
        let (row, col) = (i / usize::from(w), i % usize::from(w));
        let opaque = mask
            .get(row * stride + col / 8)
            .is_some_and(|byte| byte >> (7 - col % 8) & 1 == 1);
        if opaque {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        } else {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        }
    }
    rgba
}

/// Read a u32-length-prefixed latin-1 string (a failure reason), truncated to
/// [`MAX_STRING`] with the excess drained off the stream.
async fn read_string<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<String> {
    Ok(read_bytes(reader).await?.iter().map(|&b| char::from(b)).collect())
}

/// The same field, undecoded. ServerInit's is not latin-1 and not always a string
/// at all — see [`describe_desktop`].
async fn read_bytes<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    let len = reader.read_u32().await?;
    let keep = len.min(MAX_STRING);
    let mut buf = vec![0u8; keep as usize];
    reader.read_exact(&mut buf).await?;
    discard(reader, u64::from(len - keep)).await?;
    Ok(buf)
}

/// Drain and drop exactly `n` bytes.
async fn discard<R: AsyncRead + Unpin>(reader: &mut R, n: u64) -> anyhow::Result<()> {
    let copied = tokio::io::copy(&mut reader.take(n), &mut tokio::io::sink()).await?;
    anyhow::ensure!(copied == n, "connection closed while skipping {n} bytes");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WheelUnit;

    // Vectors generated from a reference VNC auth implementation
    // (node:crypto des-ecb) with the challenge 00 01 .. 0f.
    #[test]
    fn auth_response_matches_reference_implementation() {
        let challenge: [u8; 16] = std::array::from_fn(|i| i as u8);
        let cases = [
            ("secret42", "c6e31ed26154432307b32f3f00a3e6a1"),
            // Longer than 8 bytes: only the first 8 are used.
            ("longpassword", "5931256585fd62106d317e09fc963baf"),
            // Shorter than 8 bytes: zero-padded.
            ("ab", "fe01155de95da3e28adf6cc730f06f08"),
        ];
        for (password, expected_hex) in cases {
            let response = auth_response(password, &challenge);
            let hex: String = response.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(hex, expected_hex, "password {password:?}");
        }
    }

    #[test]
    fn auth_response_truncation_boundary() {
        // "longpass" and "longpassword" share the first 8 bytes, so their
        // responses must be identical; a 9th significant byte would differ.
        let challenge = [7u8; 16];
        assert_eq!(
            auth_response("longpass", &challenge),
            auth_response("longpassword", &challenge)
        );
        assert_ne!(
            auth_response("longpas", &challenge),
            auth_response("longpass", &challenge)
        );
    }

    /// The macOS 26 offer, exactly as the test VM sent it.
    const MACOS_TYPES: [u8; 5] = [30, 33, 36, 2, 35];

    #[test]
    fn the_subtype_decides_the_authentication() {
        assert_eq!(choose_security(&MACOS_TYPES, Some(Subtype::Ard), "pw").unwrap(), SECURITY_ARD);
        // The very same server, answered anonymously, because that is what a
        // target without the subtype asked for — and it is what costs you the
        // Mac's own screen.
        assert_eq!(choose_security(&MACOS_TYPES, None, "pw").unwrap(), SECURITY_VNC_AUTH);
        // A subtype the server cannot answer is a configuration error, not a
        // reason to authenticate as nobody.
        let err = choose_security(&[SECURITY_VNC_AUTH, SECURITY_NONE], Some(Subtype::Ard), "pw")
            .unwrap_err();
        assert!(format!("{err:#}").contains("not macOS Screen Sharing"), "{err:#}");

        // The high-performance subtype authenticates identically — the dialect
        // above it differs, the security type does not — and names itself when the
        // server cannot answer.
        assert_eq!(
            choose_security(&MACOS_TYPES, Some(Subtype::ArdHighPerformance), "").unwrap(),
            SECURITY_ARD
        );
        let err = choose_security(&[SECURITY_NONE], Some(Subtype::ArdHighPerformance), "")
            .unwrap_err();
        assert!(format!("{err:#}").contains("\"ard-high-performance\""), "{err:#}");
    }

    #[test]
    fn the_dialect_follows_the_subtype() {
        assert_eq!(Dialect::of(None), Dialect::Rfb38);
        assert_eq!(Dialect::of(Some(Subtype::Ard)), Dialect::Rfb38);
        assert_eq!(
            Dialect::of(Some(Subtype::ArdHighPerformance)),
            Dialect::Apple889
        );
        // The two bytes that are the whole visible difference on the wire.
        assert_eq!(Dialect::Rfb38.banner(), b"RFB 003.008\n");
        assert_eq!(Dialect::Apple889.banner(), b"RFB 003.889\n");
        assert_eq!(Dialect::Rfb38.client_init(), 1);
        assert_eq!(Dialect::Apple889.client_init(), 0xc1);
    }

    #[test]
    fn security_falls_back_the_way_it_always_did() {
        assert_eq!(choose_security(&[SECURITY_NONE], None, "").unwrap(), SECURITY_NONE);
        // A password with no VncAuth on offer is not a failure: an open server
        // is still an open server.
        assert_eq!(choose_security(&[SECURITY_NONE], None, "pw").unwrap(), SECURITY_NONE);
        let err = choose_security(&[SECURITY_VNC_AUTH], None, "").unwrap_err();
        assert!(format!("{err:#}").contains("requires a password"), "{err:#}");
        let err = choose_security(&[19], None, "pw").unwrap_err();
        assert!(format!("{err:#}").contains("no supported VNC security type"), "{err:#}");
    }

    #[test]
    fn ard_credentials_are_packed_where_apple_reads_them() {
        let blob = ard_credentials("andrew", "hunter2", [0xaa; ARD_CREDENTIALS_LEN]).unwrap();
        assert_eq!(&blob[..7], b"andrew\0");
        assert_eq!(&blob[64..72], b"hunter2\0");
        // Everything else is left as the random filler it came in as, so the
        // same credentials do not encrypt to the same ciphertext twice.
        assert!(blob[7..64].iter().all(|&b| b == 0xaa));
        assert!(blob[72..].iter().all(|&b| b == 0xaa));

        // 63 bytes plus the terminator is the whole field; 64 cannot be told
        // apart from an unterminated one.
        assert!(ard_credentials(&"a".repeat(63), "pw", [0; ARD_CREDENTIALS_LEN]).is_ok());
        let err = ard_credentials(&"a".repeat(64), "pw", [0; ARD_CREDENTIALS_LEN]).unwrap_err();
        assert!(format!("{err:#}").contains("at most 63"), "{err:#}");
    }

    #[test]
    fn ard_encrypts_each_block_independently() {
        // ECB, and the test is the property that names it: two identical
        // plaintext blocks encrypt identically. CBC or CTR would not.
        let mut credentials = [0u8; ARD_CREDENTIALS_LEN];
        credentials[..16].copy_from_slice(&[9u8; 16]);
        credentials[16..32].copy_from_slice(&[9u8; 16]);
        let out = ard_encrypt(&ard_wrap_key(b"shared secret"), &credentials);
        assert_eq!(out.len(), ARD_CREDENTIALS_LEN);
        assert_eq!(out[..16], out[16..32]);
        assert_ne!(out[..16], credentials[..16], "the blob is not sent in clear");
    }

    /// A worked Diffie-Hellman exchange, small enough to check by hand: the two
    /// sides must reach the same secret, and it must be padded to the server's
    /// key length rather than trimmed to its own.
    #[test]
    fn ard_exchange_agrees_with_the_server_and_pads_to_the_key_length() {
        // p = 4099, g = 2, and a private key on each side.
        let prime = 4099u32.to_be_bytes();
        let ours = [0, 0, 0, 7u8];
        let theirs = 11u32;
        let server_public = 2u32.pow(theirs).rem_euclid(4099).to_be_bytes();

        let (secret, public) = ard_exchange(2, &prime, &server_public, &ours);
        assert_eq!(secret.len(), prime.len(), "left-padded to the key length");
        assert_eq!(public.len(), prime.len());
        // What the server derives from our public key must be the same secret.
        let mirror = ard_exchange(2, &prime, &public, &theirs.to_be_bytes()).0;
        assert_eq!(secret, mirror);
        // And a secret that is numerically small keeps its leading zeros: the
        // bytes are what gets hashed, so trimming them would derive a different
        // AES key at one end.
        assert_eq!(secret[0], 0);
    }

    /// The whole exchange, played from the server's side: feed it what macOS
    /// sends, then finish the key agreement with the server's own private key
    /// and decrypt what the client wrote. Recovering the credentials proves the
    /// field order, the padding, the key derivation and the cipher mode all at
    /// once — nothing else in this module can say that.
    #[tokio::test]
    async fn a_full_dh_exchange_hands_the_server_the_credentials_back() {
        use aes::cipher::{BlockCipherDecrypt as _, KeyInit as _};

        // The group macOS sends, and now the smallest this client accepts: 128
        // bytes of it.
        let key_len = MIN_ARD_KEY_BYTES;
        let prime = {
            let mut bytes = vec![0xffu8; key_len];
            bytes[key_len - 1] = 0x97; // 2^1024 - 105, prime
            bytes
        };
        let server_private = vec![0x5au8; key_len];
        let modulus = BigUint::from_bytes_be(&prime);
        let server_public =
            BigUint::from(2u16).modpow(&BigUint::from_bytes_be(&server_private), &modulus);

        let mut offer = Vec::new();
        offer.extend_from_slice(&2u16.to_be_bytes()); // generator
        offer.extend_from_slice(&u16::try_from(key_len).unwrap().to_be_bytes());
        offer.extend_from_slice(&prime);
        let mut public_bytes = server_public.to_bytes_be();
        public_bytes.splice(..0, std::iter::repeat_n(0, key_len - public_bytes.len()));
        offer.extend_from_slice(&public_bytes);

        let mut sent = Vec::new();
        ard_authenticate(&mut offer.as_slice(), &mut sent, "andrew", "hunter2")
            .await
            .unwrap();
        assert_eq!(sent.len(), ARD_CREDENTIALS_LEN + key_len);

        let (ciphertext, client_public) = sent.split_at(ARD_CREDENTIALS_LEN);
        let secret = BigUint::from_bytes_be(client_public)
            .modpow(&BigUint::from_bytes_be(&server_private), &modulus);
        let mut secret_bytes = secret.to_bytes_be();
        secret_bytes.splice(..0, std::iter::repeat_n(0, key_len - secret_bytes.len()));
        let cipher = Aes128::new(&Md5::digest(&secret_bytes));
        let mut plain = ciphertext.to_vec();
        for block in plain.chunks_exact_mut(16) {
            cipher.decrypt_block((&mut *block).try_into().unwrap());
        }
        assert_eq!(&plain[..7], b"andrew\0");
        assert_eq!(&plain[64..72], b"hunter2\0");
    }

    /// The server chooses the group and we have to live in it, so every way that
    /// choice can be unusable is refused before any arithmetic: a prime small
    /// enough to break — with an account password riding inside it — a zero one,
    /// which `BigUint::modpow` answers with a panic rather than a number, and a
    /// public key whose shared secret anyone could predict.
    #[tokio::test]
    async fn a_degenerate_dh_group_is_refused_before_the_exchange() {
        let offer_with = |key_len: usize, prime_byte: u8, peer: &[u8]| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&2u16.to_be_bytes());
            bytes.extend_from_slice(&u16::try_from(key_len).unwrap().to_be_bytes());
            bytes.extend(std::iter::repeat_n(prime_byte, key_len)); // prime
            let mut public = vec![0u8; key_len - peer.len()];
            public.extend_from_slice(peer);
            bytes.extend_from_slice(&public);
            bytes
        };
        let offer = |key_len: usize, prime_byte: u8| offer_with(key_len, prime_byte, &[3]);
        let authenticate = async |bytes: Vec<u8>| {
            let mut sent = Vec::new();
            let result = ard_authenticate(&mut bytes.as_slice(), &mut sent, "andrew", "pw").await;
            assert!(sent.is_empty(), "nothing may be sent under a bad group");
            format!("{:#}", result.unwrap_err())
        };

        let too_small = authenticate(offer(MIN_ARD_KEY_BYTES - 1, 0xff)).await;
        assert!(too_small.contains("outside the"), "{too_small}");
        // Named for what it is rather than left to the arithmetic above: 64
        // bytes is the 512-bit group Apple used to use, and refusing it is the
        // reason the floor exists.
        let legacy = authenticate(offer(64, 0xff)).await;
        assert!(legacy.contains("outside the"), "{legacy}");
        let too_large = authenticate(offer(MAX_ARD_KEY_BYTES + 1, 0xff)).await;
        assert!(too_large.contains("outside the"), "{too_large}");
        // Long enough, and still no group at all.
        let zero = authenticate(offer(MIN_ARD_KEY_BYTES, 0)).await;
        assert!(zero.contains("zero DH prime"), "{zero}");

        // A sound group, and a public key that gives the secret away: 0 and 1
        // fix it outright, and p-1 leaves it one of two values.
        let p_minus_one = {
            let mut bytes = vec![0xffu8; MIN_ARD_KEY_BYTES];
            *bytes.last_mut().unwrap() = 0xfe;
            bytes
        };
        for peer in [
            vec![0u8],
            vec![1u8],
            p_minus_one,                   // leaves the secret one of two values
            vec![0xff; MIN_ARD_KEY_BYTES], // p itself, whose secret is always 0
        ] {
            let msg = authenticate(offer_with(MIN_ARD_KEY_BYTES, 0xff, &peer)).await;
            assert!(msg.contains("degenerate DH public key"), "peer {peer:02x?}: {msg}");
        }
        // The neighbours of those values are accepted: the check is a floor and
        // a ceiling, not a filter on anything that looks unusual.
        let mut sent = Vec::new();
        ard_authenticate(
            &mut offer_with(MIN_ARD_KEY_BYTES, 0xff, &[2]).as_slice(),
            &mut sent,
            "andrew",
            "pw",
        )
        .await
        .unwrap();
        assert_eq!(sent.len(), ARD_CREDENTIALS_LEN + MIN_ARD_KEY_BYTES);
    }

    #[test]
    fn version_parses_and_gates() {
        assert_eq!(parse_version(b"RFB 003.008\n"), Some((3, 8)));
        assert_eq!(parse_version(b"RFB 003.889\n"), Some((3, 889))); // macOS
        assert_eq!(parse_version(b"RFB 004.001\n"), Some((4, 1))); // RealVNC
        assert_eq!(parse_version(b"HTTP/1.1 200"), None);
        assert_eq!(parse_version(b"RFB 03.008\n\n"), None);
    }

    #[test]
    fn a_mac_is_recognized_from_its_handshake() {
        // macOS Screen Sharing, exactly as macOS 26 answered on the test VM:
        // Apple's revision, and Apple's security types around the standard
        // one. Either signal alone is enough.
        assert!(is_macos_server(889, &[30, 33, 36, 2, 35]));
        assert!(is_macos_server(889, &[2]));
        assert!(is_macos_server(8, &[30, 2]));
        assert!(is_macos_server(8, &[35]));

        // Everyone else — the first line is what the test Linux box answered.
        assert!(!is_macos_server(8, &[2]));
        assert!(!is_macos_server(8, &[1, 2, 16, 18]));
        assert!(!is_macos_server(1, &[1]));
    }

    #[test]
    fn pixel_format_is_bgrx_little_endian_true_colour() {
        let msg = set_pixel_format();
        assert_eq!(msg[0], 0);
        assert_eq!((msg[4], msg[5]), (32, 24)); // bpp, depth
        assert_eq!((msg[6], msg[7]), (0, 1)); // little-endian, true-colour
        assert_eq!(&msg[8..14], &[0, 255, 0, 255, 0, 255]); // maxima
        assert_eq!(&msg[14..17], &[16, 8, 0]); // shifts
    }

    #[test]
    fn client_cut_text_is_type_6_with_a_big_endian_length() {
        let msg = client_cut_text("hi").expect("fits");
        assert_eq!(msg[0], 6);
        assert_eq!(&msg[1..4], &[0, 0, 0]); // padding
        assert_eq!(&msg[4..8], &2u32.to_be_bytes()); // length, big-endian
        assert_eq!(&msg[8..], b"hi");

        // Empty text is a well-formed message, not a skipped one — clearing the
        // remote clipboard is a legitimate thing to ask for.
        let msg = client_cut_text("").expect("fits");
        assert_eq!(msg.len(), 8);
        assert_eq!(&msg[4..8], &0u32.to_be_bytes());
    }

    #[test]
    fn cut_text_is_latin1_with_a_question_mark_for_the_rest() {
        // Latin-1 survives; anything above U+00FF degrades to '?'.
        let msg = client_cut_text("café ☕").expect("fits");
        assert_eq!(&msg[8..], &[b'c', b'a', b'f', 0xE9, b' ', b'?']);

        // Round trip: what a server echoes back decodes to the same latin-1.
        assert_eq!(latin1_to_string(&msg[8..]), "café ?");
        // Every byte maps to the codepoint of the same value, 0x80..0x9F
        // included (latin-1, not Windows-1252).
        assert_eq!(latin1_to_string(&[0x00, 0x80, 0xFF]), "\u{0}\u{80}\u{ff}");
    }

    // Refused, not truncated: this encoder is the last place a partial paste
    // could still reach a remote, and one byte of latin-1 per char means it
    // cannot silently overshoot either.
    #[test]
    fn cut_text_over_the_ceiling_is_refused() {
        assert_eq!(client_cut_text(&"a".repeat(MAX_CLIPBOARD_BYTES + 1)), None);
        // Measured in UTF-8 bytes, so multi-byte characters hit it sooner than
        // their latin-1 '?' would suggest.
        assert_eq!(client_cut_text(&"☕".repeat(MAX_CLIPBOARD_BYTES)), None);

        // At the ceiling it still encodes, so the boundary is inclusive.
        let msg = client_cut_text(&"a".repeat(MAX_CLIPBOARD_BYTES)).expect("fits");
        assert_eq!(msg.len(), 8 + MAX_CLIPBOARD_BYTES);
        assert_eq!(&msg[4..8], &(MAX_CLIPBOARD_BYTES as u32).to_be_bytes());
    }

    #[test]
    fn raw_only_encoding_set() {
        assert_eq!(set_encodings(&[ENCODING_RAW]), vec![2, 0, 0, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn resize_encoding_set_appends_the_pseudo_encodings() {
        let msg = set_encodings(&[
            ENCODING_RAW,
            ENCODING_CURSOR,
            ENCODING_EXTENDED_DESKTOP_SIZE,
            ENCODING_DESKTOP_SIZE,
        ]);
        assert_eq!(&msg[..4], &[2, 0, 0, 4]);
        assert_eq!(&msg[4..8], &0i32.to_be_bytes());
        assert_eq!(&msg[8..12], &(-239i32).to_be_bytes());
        assert_eq!(&msg[12..16], &(-308i32).to_be_bytes());
        assert_eq!(&msg[16..20], &(-223i32).to_be_bytes());
    }

    /// A server reads the list as a preference order, so the order is the decision.
    /// Every pixel encoding in it must also have an arm in `read_rect`: advertising
    /// one is a promise to decode it.
    #[tokio::test]
    async fn the_generic_encoding_list_is_in_preference_order() {
        assert_eq!(
            rfb38_encoding_list(false, false, false),
            vec![
                ENCODING_COPY_RECT,
                ENCODING_ZRLE,
                ENCODING_ZLIB,
                ENCODING_HEXTILE,
                ENCODING_RRE,
                ENCODING_RAW,
                ENCODING_CURSOR,
                ENCODING_CONTINUOUS_UPDATES,
                ENCODING_FENCE,
            ]
        );

        // Every pixel encoding advertised is one this side can be handed. A rect
        // header alone is enough to prove it: an unrecognised encoding bails with
        // "not advertised" before any payload is read, and the promised ones do not.
        //
        // Pixel encodings are the non-negative ones. The pseudo-encodings are
        // excluded because a server never sends one as a rectangle at all — the
        // clipboard's arrives as a ServerCutText, not here.
        let pixel_encodings = rfb38_encoding_list(false, true, true)
            .into_iter()
            .filter(|encoding| *encoding >= 0);
        for encoding in pixel_encodings {
            let mut wire = vec![0u8, 0];
            wire.extend_from_slice(&1u16.to_be_bytes());
            wire.extend_from_slice(&[0u8; 8]); // a 0x0 rect at the origin
            wire.extend_from_slice(&encoding.to_be_bytes());

            let (uplink, _sent) = test_uplink();
            let (sink, _rx) = test_sink();
            let shared = test_shared(
                uplink,
                shared_desktop((2, 2), None, None),
                test_shadow((2, 2)),
            );
            let err = read_loop(
                std::io::Cursor::new(wire),
                shared,
                ReadFlags { clipboard: true, poll: false },
                None,
                sink,
            )
            .await
            .unwrap_err();
            assert!(
                !format!("{err:#}").contains("not advertised"),
                "encoding {encoding} is advertised but not decoded: {err:#}"
            );
        }
    }

    /// Compression is not a High Performance feature. The upgrade waits on a display
    /// layout, and plain `ard` reports one, so the only thing the subtype settles is
    /// the virtual display. Gating zlib by subtype cost 6.19 MB of raw where zlib
    /// sent 3.38 MB of the same 800x600 desktop, and Standard mode's framebuffer is
    /// a physical screen — 3200x1800 on the Mac this was measured against.
    #[test]
    fn both_apple_subtypes_start_out_wanting_zlib() {
        for high_performance in [false, true] {
            let apple = Apple::new(high_performance);
            assert!(
                !apple.asked_for_zlib,
                "high_performance={high_performance} skipped the zlib upgrade"
            );
            assert_eq!(apple.virtual_display, high_performance);
        }
    }

    /// The *first* `SetEncodings` only. zlib in this list costs the display layout,
    /// which is why it is absent here and asked for again once a layout has arrived —
    /// see [`both_apple_subtypes_start_out_wanting_zlib`].
    #[test]
    fn standard_ard_uses_the_apple_metadata_list_without_zlib() {
        let encodings = rfb38_encoding_list(true, false, true);
        assert_eq!(encodings, vnc_apple::ENCODINGS);
        assert!(encodings.contains(&vnc_apple::ENCODING_DISPLAY_LAYOUT));
        assert!(!encodings.contains(&ENCODING_ZLIB));
        assert!(!encodings.contains(&vnc_clipboard::ENCODING));
    }

    // ── Cursor pseudo-encoding ──────────────────────────────────────────────

    #[test]
    fn cursor_mask_becomes_alpha_and_masked_out_pixels_are_cleared() {
        // 3x2 cursor: mask rows are padded to a whole byte, MSB first.
        // Row 0: 101xxxxx, row 1: 010xxxxx.
        let bgrx: Vec<u8> = (0..6).flat_map(|i| [i * 3, i * 3 + 1, i * 3 + 2, 0]).collect();
        let rgba = masked_bgrx_to_rgba(&bgrx, &[0b1010_0000, 0b0100_0000], 3);
        assert_eq!(
            rgba,
            vec![
                2, 1, 0, 255, // (0,0) opaque, BGRX -> RGBA
                0, 0, 0, 0, // (1,0) transparent
                8, 7, 6, 255, // (2,0) opaque
                0, 0, 0, 0, // (0,1) transparent
                14, 13, 12, 255, // (1,1) opaque
                0, 0, 0, 0, // (2,1) transparent
            ]
        );
    }

    /// Decode a cursor's PNG back to RGBA for assertions.
    fn decode_rgba(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgba);
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    #[tokio::test]
    async fn cursor_rect_is_cached_and_forwarded_as_an_rgba_png() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (sink, mut rx) = test_sink();
        // 2x1: an opaque red pixel then a masked-out one.
        let mut payload = vec![0, 0, 255, 0, 9, 9, 9, 0]; // BGRX
        payload.push(0b1000_0000); // mask row
        let mut reader = payload.as_slice();

        read_cursor(&mut reader, &cursor, (1, 2, 2, 1), &sink).await.unwrap();

        let shape = match forwarded(&sink, &mut rx).await.unwrap() {
            ServerMsg::Cursor(Some(shape)) => shape,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!((shape.w, shape.h, shape.hx, shape.hy), (2, 1, 1, 2));
        assert_eq!(decode_rgba(&shape.png), (2, 1, vec![255, 0, 0, 255, 0, 0, 0, 0]));
        // Cached for replay to a browser that attaches later.
        match cursor_msg(&cursor) {
            Some(ServerMsg::Cursor(Some(cached))) => assert_eq!(cached, shape),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_cursor_rect_hides_the_pointer() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (sink, mut rx) = test_sink();
        // No payload at all for a 0x0 rect.
        read_cursor(&mut [].as_slice(), &cursor, (0, 0, 0, 0), &sink).await.unwrap();
        assert!(matches!(forwarded(&sink, &mut rx).await, Some(ServerMsg::Cursor(None))));
        // Hidden is still browser-drawn state, so it replays on reattach —
        // unlike ServerDrawn, which must stay silent.
        assert!(matches!(cursor_msg(&cursor), Some(ServerMsg::Cursor(None))));
    }

    #[tokio::test]
    async fn oversized_cursor_is_drained_and_hides_the_pointer() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (sink, mut rx) = test_sink();
        let (w, h) = (MAX_CURSOR_DIM + 1, 1);
        let mut payload = vec![0u8; usize::from(w) * BPP + usize::from(w).div_ceil(8)];
        // A trailing byte stands in for the next rect: it must survive.
        payload.push(0xAB);
        let mut reader = payload.as_slice();

        read_cursor(&mut reader, &cursor, (0, 0, w, h), &sink).await.unwrap();
        assert_eq!(reader, &[0xAB]);
        // The shape is dropped, but the server still isn't drawing the pointer,
        // so the browser is told to fall back rather than left with nothing.
        assert!(matches!(forwarded(&sink, &mut rx).await, Some(ServerMsg::Cursor(None))));
        assert!(matches!(cursor_msg(&cursor), Some(ServerMsg::Cursor(None))));
    }

    #[tokio::test]
    async fn a_bad_apple_cursor_does_not_end_or_poison_the_session() {
        use flate2::{Compress, Compression, FlushCompress};

        fn store(id: u32, raw: &[u8]) -> Vec<u8> {
            let mut deflate = Compress::new(Compression::default(), true);
            let mut compressed = Vec::with_capacity(raw.len() + 128);
            deflate
                .compress_vec(raw, &mut compressed, FlushCompress::Sync)
                .unwrap();
            let mut body = id.to_be_bytes().to_vec();
            body.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
            body.extend_from_slice(&compressed);
            body
        }

        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (sink, mut rx) = test_sink();
        let mut apple = Some(Apple::default());

        // 1x1 BGRX plus its separate alpha byte, each in a complete cursor-local
        // zlib stream.
        let first = store(1000, &[0, 0, 255, 0, 255]);
        read_cursor_image(&mut first.as_slice(), &mut apple, &cursor, (0, 0), (1, 1), &sink)
            .await
            .unwrap();
        assert!(matches!(forwarded(&sink, &mut rx).await, Some(ServerMsg::Cursor(Some(_)))));

        let mut bad = 1001u32.to_be_bytes().to_vec();
        bad.extend_from_slice(&3u32.to_be_bytes());
        bad.extend_from_slice(&[1, 2, 3]);
        read_cursor_image(&mut bad.as_slice(), &mut apple, &cursor, (0, 0), (1, 1), &sink)
            .await
            .expect("one malformed cursor is not a session error");
        assert!(forwarded(&sink, &mut rx).await.is_none());

        let after = store(1002, &[255, 0, 0, 0, 255]);
        read_cursor_image(&mut after.as_slice(), &mut apple, &cursor, (0, 0), (1, 1), &sink)
            .await
            .expect("the next independent cursor stream still decodes");
        assert!(matches!(forwarded(&sink, &mut rx).await, Some(ServerMsg::Cursor(Some(_)))));
    }

    #[test]
    fn no_cursor_rect_leaves_pointer_rendering_to_the_server() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        assert!(cursor_msg(&cursor).is_none());
    }

    #[test]
    fn set_desktop_size_encodes_a_single_screen() {
        let msg = set_desktop_size((1920, 1200), Screen { id: 0x0A0B0C0D, flags: 1 });
        assert_eq!(msg[0], 251); // message type
        assert_eq!(msg[1], 0); // padding
        assert_eq!(&msg[2..6], &[0x07, 0x80, 0x04, 0xB0]); // 1920, 1200
        assert_eq!((msg[6], msg[7]), (1, 0)); // one screen + padding
        assert_eq!(&msg[8..12], &[0x0A, 0x0B, 0x0C, 0x0D]); // screen id
        assert_eq!(&msg[12..16], &[0; 4]); // screen x, y = 0
        assert_eq!(&msg[16..20], &[0x07, 0x80, 0x04, 0xB0]); // screen w, h
        assert_eq!(&msg[20..24], &[0, 0, 0, 1]); // flags echoed
    }

    #[test]
    fn update_request_covers_the_desktop() {
        assert_eq!(
            update_request(true, (1280, 800)),
            [3, 1, 0, 0, 0, 0, 0x05, 0x00, 0x03, 0x20]
        );
        assert_eq!(update_request(false, (1, 1))[1], 0);
    }

    #[test]
    fn pointer_and_key_events_encode_big_endian() {
        assert_eq!(pointer_event(0x05, (0x0102, 0x0304)), [5, 5, 1, 2, 3, 4]);
        assert_eq!(key_event(true, 0xFF0D), [4, 1, 0, 0, 0, 0, 0xFF, 0x0D]);
        assert_eq!(key_event(false, 0x61), [4, 0, 0, 0, 0, 0, 0, 0x61]);
    }

    #[test]
    fn buttons_accumulate_in_the_mask_and_wheel_pulses() {
        let mut mask = 0u8;
        let mut pos = (10, 20);
        let mut keys = HashMap::new();
        // A generic server, whose pulse is a whole notch, so one scroll event is
        // one pulse and the mask is what this test is left looking at.
        let mut wheel = Wheel::new(false);

        let bytes = translate_input(
            ClientMsg::MouseButton {
                button: MouseButton::Left,
                pressed: true,
                clicks: 1,
            },
            &Buttons::Rfb,
            &mut mask,
            &mut pos,
            &mut keys,
            &mut wheel,
        );
        assert_eq!(bytes, vec![pointer_event(0x01, (10, 20)).to_vec()]);

        // A move while the button is held keeps it in the mask (drag).
        let bytes = translate_input(
            ClientMsg::MouseMove { x: 30, y: 40 },
            &Buttons::Rfb,
            &mut mask,
            &mut pos,
            &mut keys,
            &mut wheel,
        );
        assert_eq!(bytes, vec![pointer_event(0x01, (30, 40)).to_vec()]);

        // Scroll down = button 5 (0x10) press + release, on top of the held mask.
        // Three lines of intent is exactly one pulse here.
        let bytes = translate_input(
            ClientMsg::Wheel { dx: 0.0, dy: 3.0, unit: WheelUnit::Line },
            &Buttons::Rfb,
            &mut mask,
            &mut pos,
            &mut keys,
            &mut wheel,
        );
        // Two *separate* messages, not one buffer of both: on the 003.889 wire
        // each has to go in a record of its own or the release is dropped and the
        // wheel button stays down.
        assert_eq!(
            bytes,
            vec![
                pointer_event(0x11, (30, 40)).to_vec(),
                pointer_event(0x01, (30, 40)).to_vec(),
            ]
        );

        let bytes = translate_input(
            ClientMsg::MouseButton {
                button: MouseButton::Left,
                pressed: false,
                clicks: 1,
            },
            &Buttons::Rfb,
            &mut mask,
            &mut pos,
            &mut keys,
            &mut wheel,
        );
        assert_eq!(bytes, vec![pointer_event(0x00, (30, 40)).to_vec()]);
    }

    /// High Performance mode's agent reads the mask as CGMouseButton numbers,
    /// so right and middle ride each other's RFB bits there — and only there.
    /// Measured on macOS 26.6: see [`Buttons`].
    #[test]
    fn high_performance_swaps_the_middle_and_right_mask_bits() {
        for (buttons, right_bit, middle_bit) in [
            (Buttons::Rfb, 0x04u8, 0x02u8),
            (Buttons::HighPerformance, 0x02, 0x04),
        ] {
            for (button, bit) in [
                (MouseButton::Right, right_bit),
                (MouseButton::Middle, middle_bit),
            ] {
                let mut mask = 0u8;
                let mut pos = (10, 20);
                let bytes = translate_input(
                    ClientMsg::MouseButton { button, pressed: true, clicks: 1 },
                    &buttons,
                    &mut mask,
                    &mut pos,
                    &mut HashMap::new(),
                    &mut Wheel::new(true),
                );
                assert_eq!(bytes, vec![pointer_event(bit, (10, 20)).to_vec()]);
            }
            // Left is bit 1 in both dialects.
            let mut mask = 0u8;
            let mut pos = (10, 20);
            let bytes = translate_input(
                ClientMsg::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                    clicks: 1,
                },
                &buttons,
                &mut mask,
                &mut pos,
                &mut HashMap::new(),
                &mut Wheel::new(true),
            );
            assert_eq!(bytes, vec![pointer_event(0x01, (10, 20)).to_vec()]);
        }
    }

    /// Pulses for one wheel event, as (horizontal, vertical).
    fn scroll(wheel: &mut Wheel, dx: f32, dy: f32, unit: WheelUnit) -> (i32, i32) {
        wheel.pulses(dx, dy, unit)
    }

    #[test]
    fn only_an_apple_target_is_charged_by_the_distance() {
        // Measured against a live Mac: a pulse is worth about 2px there, so
        // ~120px of intent — one notch of a physical wheel, as browsers report
        // it — is 60 of them. Spending it as one pulse is the whole reason a Mac
        // used to crawl.
        let mut apple = Wheel::new(true);
        assert_eq!(scroll(&mut apple, 0.0, 120.0, WheelUnit::Pixel).1, 60);
        // Every other server keeps the convention it is tuned for: one pulse,
        // whatever distance was asked for. Charging an X11 desktop by the
        // distance scrolls it in lurches.
        let mut generic = Wheel::new(false);
        assert_eq!(scroll(&mut generic, 0.0, 120.0, WheelUnit::Pixel).1, 1);
        assert_eq!(scroll(&mut generic, 0.0, 4.0, WheelUnit::Pixel).1, 1);
        assert_eq!(scroll(&mut generic, 0.0, 0.0, WheelUnit::Pixel).1, 0);
        assert_eq!(scroll(&mut generic, 0.0, f32::NAN, WheelUnit::Pixel).1, 0);
    }

    #[test]
    fn scroll_direction_picks_the_wheel_button() {
        // Up is negative in the DOM and button 4; down is button 5.
        let mut apple = Wheel::new(true);
        assert_eq!(scroll(&mut apple, 0.0, -32.0, WheelUnit::Pixel).1, -16);
        assert_eq!(scroll(&mut apple, 48.0, 0.0, WheelUnit::Pixel).0, 24);
        let mut generic = Wheel::new(false);
        assert_eq!(scroll(&mut generic, 0.0, -32.0, WheelUnit::Pixel).1, -1);
        assert_eq!(scroll(&mut generic, 48.0, 0.0, WheelUnit::Pixel).0, 1);
    }

    #[test]
    fn sub_pulse_glides_accumulate_instead_of_vanishing() {
        // A trackpad reports deltas too small to be a pulse each. Dropping them
        // would scroll never.
        let mut wheel = Wheel::new(true);
        assert_eq!(scroll(&mut wheel, 0.0, 1.5, WheelUnit::Pixel).1, 0);
        assert_eq!(scroll(&mut wheel, 0.0, 1.5, WheelUnit::Pixel).1, 1);
    }

    #[test]
    fn a_reversal_does_not_pay_off_the_old_directions_remainder() {
        let mut wheel = Wheel::new(true);
        assert_eq!(scroll(&mut wheel, 0.0, 1.5, WheelUnit::Pixel).1, 0);
        // Flicking back scrolls back immediately rather than first burning the
        // three quarters of a downward pulse left over.
        assert_eq!(scroll(&mut wheel, 0.0, -2.5, WheelUnit::Pixel).1, -1);
    }

    #[test]
    fn one_absurd_delta_cannot_flood_the_uplink() {
        // The cap is a distance; the Mac's frugal pulse is what makes the count
        // it comes to large.
        let mut wheel = Wheel::new(true);
        let cap = (Wheel::MAX_PX / Wheel::APPLE_PX_PER_PULSE) as i32;
        assert_eq!(scroll(&mut wheel, 0.0, 100_000.0, WheelUnit::Pixel).1, cap);
        // The surplus is dropped, not left trickling into later events.
        assert_eq!(scroll(&mut wheel, 0.0, 1.0, WheelUnit::Pixel).1, 0);
        // A delta a client should never send at all buys nothing.
        assert_eq!(scroll(&mut wheel, f32::NAN, f32::INFINITY, WheelUnit::Pixel), (0, 0));
    }

    #[test]
    fn a_delta_barely_over_the_cap_leaves_no_fraction_behind() {
        // One pulse past the cap: the whole pulses come to exactly the cap, and
        // the fraction over it is surplus like any other. Keeping it would let
        // the next event round up into a pulse the cap exists to refuse — the
        // one window where "capped" and "spent everything" look alike.
        let mut wheel = Wheel::new(true);
        let cap = (Wheel::MAX_PX / Wheel::APPLE_PX_PER_PULSE) as i32;
        let over = Wheel::MAX_PX + Wheel::APPLE_PX_PER_PULSE / 2.0;
        assert!(over < Wheel::MAX_PX + Wheel::APPLE_PX_PER_PULSE);
        assert_eq!(scroll(&mut wheel, 0.0, over, WheelUnit::Pixel).1, cap);
        // Half a pulse on its own, with nothing carried in to round it up.
        let half = Wheel::APPLE_PX_PER_PULSE / 2.0;
        assert_eq!(scroll(&mut wheel, 0.0, half, WheelUnit::Pixel).1, 0);
    }

    #[test]
    fn line_and_page_deltas_are_sized_in_lines() {
        let mut wheel = Wheel::new(true);
        // Firefox reports notches in lines rather than pixels: three lines is
        // 48px of intent.
        assert_eq!(scroll(&mut wheel, 0.0, 3.0, WheelUnit::Line).1, 24);
        // A page is a screenful — 20 lines — and a Mac charges 160 pulses for it.
        assert_eq!(scroll(&mut wheel, 0.0, 1.0, WheelUnit::Page).1, 160);
    }

    // ── Resize state machine (no sockets: in-memory uplink, slice reader) ───

    /// An [`AsyncWrite`] whose bytes a test can read back while [`Uplink`] still
    /// owns it. Cloneable for exactly that reason: `Uplink` boxes its socket, so
    /// there is no getting at it afterwards.
    #[derive(Clone, Default)]
    struct Wire(Arc<std::sync::Mutex<Vec<u8>>>);

    impl AsyncWrite for Wire {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A plain uplink and the buffer behind it.
    fn test_uplink() -> (SharedUplink, Wire) {
        let wire = Wire::default();
        (
            Arc::new(Mutex::new(Uplink::plain(wire.clone()))),
            wire,
        )
    }

    /// An uplink that frames what it sends into Apple records, and the buffer
    /// behind it. `keys` is handed back so a test can read the records again.
    fn test_records_uplink(keys: Keys) -> (SharedUplink, Wire) {
        let wire = Wire::default();
        (
            Arc::new(Mutex::new(Uplink::records(wire.clone(), keys))),
            wire,
        )
    }

    fn written(wire: &Wire) -> Vec<u8> {
        wire.0.lock().unwrap().clone()
    }

    fn shared_desktop(
        size: (u16, u16),
        screen: Option<Screen>,
        pending: Option<(u16, u16)>,
    ) -> SharedDesktop {
        Arc::new(std::sync::Mutex::new(DesktopState {
            size,
            scale: UNSCALED,
            host_density: 1.0,
            screen,
            pending,
        }))
    }

    /// The shared state the rect handlers take, with only the desktop and shadow
    /// meant to be looked at.
    fn test_shared(uplink: SharedUplink, desktop: SharedDesktop, shadow: SharedShadow) -> Shared {
        Shared {
            uplink,
            desktop,
            cursor: Arc::new(std::sync::Mutex::new(CursorState::default())),
            clipboard: Arc::new(std::sync::Mutex::new(ClipboardState::default())),
            shadow,
            display: Arc::new(std::sync::Mutex::new(DisplayState::default())),
        }
    }

    // A server that hangs up mid-session has to reach `run`'s error branch. Ending
    // the read loop with `Ok` instead skips it, and the browser lands on a bare
    // picker — or on whatever error was already sitting there.
    /// A shadow of whatever size the test's desktop is; most of these tests never
    /// put a pixel through it.
    fn test_shadow(size: (u16, u16)) -> SharedShadow {
        Arc::new(std::sync::Mutex::new(Shadow::new("vnc", size.0, size.1)))
    }

    /// A sink and the frame channel behind it.
    fn test_sink() -> (TileSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, frame_rx) = mpsc::channel(8);
        let plan = crate::config::RenderPlan::Tiles {
            base: crate::config::TileCodec::Png,
            motion: None,
            debug: false,
            adaptive: None,
        };
        let feedback = Arc::new(crate::feedback::LinkFeedback::new());
        (TileSink::new("vnc", frame_tx, plan, feedback), frame_rx)
    }

    /// What the sink has forwarded so far, or `None` for nothing.
    ///
    /// The flush is the point: a [`TileSink`] forwards from a task of its own, so a
    /// bare `try_recv` would race it and read `None` for a message that is on its
    /// way. `None` here means the engine sent nothing, which is what these tests
    /// mean when they assert it.
    async fn forwarded(
        sink: &TileSink,
        rx: &mut mpsc::Receiver<ServerMsg>,
    ) -> Option<ServerMsg> {
        sink.flush().await;
        rx.try_recv().ok()
    }

    #[tokio::test]
    async fn a_server_that_hangs_up_is_reported_instead_of_ending_quietly() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // A clean FIN, which is what a stopped server sends.
            drop(stream);
        });

        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (read_half, write_half) = client.into_split();
        let (sink, _frames) = test_sink();
        let err = read_loop(
            BufReader::new(read_half),
            test_shared(
                Arc::new(Mutex::new(Uplink::plain(write_half))),
                shared_desktop((1280, 800), None, None),
                test_shadow((1280, 800)),
            ),
            ReadFlags { clipboard: false, poll: true },
            None,
            sink,
        )
        .await
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("closed the connection"), "{msg}");
        server.await.unwrap();
    }

    /// Payload of an ExtendedDesktopSize rect declaring one screen.
    fn eds_payload(screen: Screen) -> Vec<u8> {
        let mut p = vec![1, 0, 0, 0]; // one screen + padding
        p.extend_from_slice(&screen.id.to_be_bytes());
        p.extend_from_slice(&[0u8; 8]); // screen x, y, w, h (layout unused)
        p.extend_from_slice(&screen.flags.to_be_bytes());
        p
    }

    #[tokio::test]
    async fn request_resize_stashes_until_support_and_skips_noops() {
        let (uplink, wire) = test_uplink();
        let desktop = shared_desktop((1024, 768), None, None);

        // Matching the current size or a zero dimension: no-ops.
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1024, 768)), false).await.unwrap();
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((0, 600)), false).await.unwrap();
        assert!(desktop.lock().unwrap().pending.is_none());
        assert!(written(&wire).is_empty());

        // Support not declared yet: stashed, nothing on the wire.
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((800, 600)), false).await.unwrap();
        assert_eq!(desktop.lock().unwrap().pending, Some((800, 600)));
        assert!(written(&wire).is_empty());

        // Browser back at the current size: the stale stash is dropped.
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1024, 768)), false).await.unwrap();
        assert!(desktop.lock().unwrap().pending.is_none());

        // Support declared: SetDesktopSize goes out immediately.
        let screen = Screen { id: 7, flags: 0 };
        desktop.lock().unwrap().screen = Some(screen);
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((800, 600)), false).await.unwrap();
        assert_eq!(written(&wire), set_desktop_size((800, 600), screen));
    }

    #[tokio::test]
    async fn high_performance_resize_sends_a_full_dynamic_configuration() {
        let (uplink, wire) = test_uplink();
        let desktop = shared_desktop((1024, 768), None, None);

        request_resize(&uplink, &desktop, ResizeAsk::Viewport((800, 600)), true).await.unwrap();

        assert_eq!(
            written(&wire),
            vnc_apple::set_display_configuration(vnc_apple::virtual_display_mode(
                (800, 600),
                1.0
            ))
        );
        assert!(desktop.lock().unwrap().pending.is_none());
    }

    #[test]
    fn a_resizable_session_opens_at_the_ceiling_not_the_configured_size() {
        let target = |resize: bool| -> TargetConfig {
            toml::from_str(&format!(
                "name = \"t\"\nprotocol = \"vnc\"\nsubtype = \"ard-high-performance\"\n\
                 host = \"h\"\nwidth = 1600\nheight = 1000\nresize = {resize}"
            ))
            .unwrap()
        };

        // The window's size arrives as a viewport report; opening small first
        // squeezes every remote window together, so open at the maximum.
        assert_eq!(opening_mode(&target(true)), vnc_apple::maximum_mode());
        assert_eq!(opening_mode(&target(true)).pixels, (3840, 2160));

        // No window will ever report a size: the configured one is the mode.
        assert_eq!(
            opening_mode(&target(false)),
            vnc_apple::virtual_display_mode((1600, 1000), 1.0)
        );
    }

    #[tokio::test]
    async fn a_density_change_re_renders_the_same_points_at_the_new_density() {
        let (uplink, wire) = test_uplink();
        // A 1600×1000 1x desktop whose client window just moved to a 2x screen.
        let desktop = shared_desktop((1600, 1000), None, None);
        desktop.lock().unwrap().host_density = 2.0;

        request_resize(&uplink, &desktop, ResizeAsk::Density, true).await.unwrap();
        assert_eq!(
            written(&wire),
            vnc_apple::set_display_configuration(vnc_apple::virtual_display_mode(
                (1600, 1000),
                2.0
            )),
            "current points, twice the pixels"
        );
    }

    #[tokio::test]
    async fn a_viewport_report_is_read_at_the_announced_scale() {
        let (uplink, wire) = test_uplink();
        // Steady state on a Retina client: the desktop is 3200×2000 pixels shown
        // at 2x, and the browser reports its viewport pre-multiplied by that
        // announced scale. The same size must not re-request anything.
        let desktop = shared_desktop((3200, 2000), None, None);
        {
            let mut d = desktop.lock().unwrap();
            d.scale = 2.0;
            d.host_density = 2.0;
        }

        request_resize(&uplink, &desktop, ResizeAsk::Viewport((3200, 2000)), true).await.unwrap();
        assert!(written(&wire).is_empty(), "the browser is at the current size");

        // A genuinely new window size, still pre-multiplied: 1600×1200 points.
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((3200, 2400)), true).await.unwrap();
        assert_eq!(
            written(&wire),
            vnc_apple::set_display_configuration(vnc_apple::virtual_display_mode(
                (1600, 1200),
                2.0
            ))
        );
    }

    #[tokio::test]
    async fn extended_desktop_size_declares_support_and_replays_pending() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let desktop = shared_desktop((1024, 768), None, Some((800, 600)));
        let screen = Screen { id: 3, flags: 0 };

        // First rect from the server (reason 0), size unchanged.
        let payload = eds_payload(screen);
        let resized = read_extended_desktop_size(
            &mut payload.as_slice(),
            &uplink,
            &desktop,
            &test_shadow((1024, 768)),
            (0, 0, 1024, 768),
            &sink,
        )
        .await
        .unwrap();

        assert!(!resized, "size did not change");
        let (screen_id, pending) = {
            let d = desktop.lock().unwrap();
            (d.screen.map(|s| s.id), d.pending)
        };
        assert_eq!(screen_id, Some(3), "support recorded");
        assert_eq!(pending, None, "stash consumed");
        // No browser resize (same size), but the stashed report replays.
        assert!(forwarded(&sink, &mut rx).await.is_none());
        assert_eq!(written(&wire), set_desktop_size((800, 600), screen));
    }

    #[tokio::test]
    async fn extended_desktop_size_applies_a_change_and_tells_the_browser() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let desktop = shared_desktop((1024, 768), None, None);

        // Our SetDesktopSize succeeded (reason 1, status 0) at 800x600.
        let payload = eds_payload(Screen { id: 1, flags: 0 });
        let resized = read_extended_desktop_size(
            &mut payload.as_slice(),
            &uplink,
            &desktop,
            &test_shadow((1024, 768)),
            (1, 0, 800, 600),
            &sink,
        )
        .await
        .unwrap();

        assert!(resized);
        assert_eq!(desktop.lock().unwrap().size, (800, 600));
        let resize = forwarded(&sink, &mut rx).await;
        assert!(matches!(resize, Some(ServerMsg::Resize { w: 800, h: 600, scale: UNSCALED })));
        assert!(written(&wire).is_empty(), "nothing left to request");
    }

    #[tokio::test]
    async fn rejected_set_desktop_size_leaves_the_size_alone() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let desktop = shared_desktop((1024, 768), Some(Screen { id: 1, flags: 0 }), None);

        // reason 1, status 1 = our request was prohibited.
        let payload = eds_payload(Screen { id: 1, flags: 0 });
        let resized = read_extended_desktop_size(
            &mut payload.as_slice(),
            &uplink,
            &desktop,
            &test_shadow((1024, 768)),
            (1, 1, 640, 480),
            &sink,
        )
        .await
        .unwrap();

        assert!(!resized);
        assert_eq!(desktop.lock().unwrap().size, (1024, 768));
        assert!(forwarded(&sink, &mut rx).await.is_none(), "no resize reported to the browser");
        assert!(written(&wire).is_empty());
    }

    #[tokio::test]
    async fn apply_resize_dedupes_and_rejects_zero_sizes() {
        let (sink, mut rx) = test_sink();
        let desktop = shared_desktop((1024, 768), None, None);
        let shadow = test_shadow((1024, 768));

        // Same size: no change, nothing sent to the browser.
        assert!(!apply_resize(&desktop, &shadow, (1024, 768), UNSCALED, &sink).await.unwrap());
        assert!(forwarded(&sink, &mut rx).await.is_none());

        // A real change updates the state and reaches the browser.
        assert!(apply_resize(&desktop, &shadow, (640, 480), UNSCALED, &sink).await.unwrap());
        assert_eq!(desktop.lock().unwrap().size, (640, 480));
        let resize = forwarded(&sink, &mut rx).await;
        assert!(matches!(resize, Some(ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED })));
        // And the shadow follows it, or the next rect would be compared against a
        // framebuffer that no longer exists.
        assert_eq!(shadow.lock().unwrap().size(), (640, 480));

        // A zero dimension is a protocol violation, not a resize.
        assert!(apply_resize(&desktop, &shadow, (0, 480), UNSCALED, &sink).await.is_err());
    }

    /// Feed one key event through `translate_input`, carrying the browser's
    /// `caps` state (as the wire message does) and sharing the pressed-key map.
    ///
    /// Flattened to a single buffer, which loses nothing: a key is always one
    /// message or none. Only the wheel produces more than one, and its test asserts
    /// on the list.
    fn key(keys: &mut HashMap<String, u32>, code: &str, pressed: bool, caps: bool) -> Vec<u8> {
        let (mut mask, mut pos) = (0u8, (0u16, 0u16));
        translate_input(
            ClientMsg::Key {
                code: code.to_owned(),
                pressed,
                caps,
            },
            &Buttons::Rfb,
            &mut mask,
            &mut pos,
            keys,
            &mut Wheel::new(true),
        )
        .concat()
    }

    #[test]
    fn key_input_maps_to_keysyms_and_drops_unknown_codes() {
        let mut keys = HashMap::new();
        assert_eq!(
            key(&mut keys, "KeyA", true, false),
            key_event(true, 0x61).to_vec()
        );
        assert!(key(&mut keys, "MediaPlayPause", true, false).is_empty());
    }

    #[test]
    fn held_shift_sends_the_shifted_keysym() {
        let mut keys = HashMap::new();
        // Shift down, then a letter and a digit resolve to their shifted form.
        assert_eq!(
            key(&mut keys, "ShiftLeft", true, false),
            key_event(true, 0xFFE1).to_vec()
        );
        assert_eq!(
            key(&mut keys, "KeyA", true, false),
            key_event(true, 0x41).to_vec()
        ); // 'A'
        assert_eq!(
            key(&mut keys, "Digit1", true, false),
            key_event(true, 0x21).to_vec()
        ); // '!'
    }

    #[test]
    fn release_uses_the_keysym_from_press_even_after_shift_is_let_go() {
        let mut keys = HashMap::new();
        key(&mut keys, "ShiftLeft", true, false);
        assert_eq!(
            key(&mut keys, "KeyA", true, false),
            key_event(true, 0x41).to_vec()
        ); // 'A' down
        // Shift released before the letter — the letter must still release 'A',
        // not 'a', or the server leaves the shifted keysym stuck down.
        key(&mut keys, "ShiftLeft", false, false);
        assert_eq!(
            key(&mut keys, "KeyA", false, false),
            key_event(false, 0x41).to_vec()
        );
        assert!(keys.is_empty());
    }

    #[test]
    fn capslock_key_is_never_forwarded() {
        let mut keys = HashMap::new();
        // The CapsLock key itself produces no wire bytes and holds no state.
        assert!(key(&mut keys, "CapsLock", true, true).is_empty());
        assert!(key(&mut keys, "CapsLock", false, true).is_empty());
        assert!(keys.is_empty());
    }

    #[test]
    fn caps_flag_uppercases_letters_only() {
        let mut keys = HashMap::new();
        // With the browser reporting CapsLock on, a plain letter is uppercased.
        assert_eq!(
            key(&mut keys, "KeyA", true, true),
            key_event(true, 0x41).to_vec()
        ); // 'A'
        key(&mut keys, "KeyA", false, true);
        // Digits/symbols are unaffected by CapsLock.
        assert_eq!(
            key(&mut keys, "Digit1", true, true),
            key_event(true, u32::from('1')).to_vec()
        );
    }

    #[test]
    fn caps_and_shift_cancel_for_letters() {
        let mut keys = HashMap::new();
        key(&mut keys, "ShiftLeft", true, true); // shift held, caps on
        // caps XOR shift = off → lowercase letter.
        assert_eq!(
            key(&mut keys, "KeyA", true, true),
            key_event(true, 0x61).to_vec()
        ); // 'a'
    }

    // ── The Apple dialect (no sockets: framed records over a slice) ─────────

    fn apple_keys() -> Keys {
        Keys {
            key: *b"aaaaaaaaaaaaaaaa",
            iv: *b"bbbbbbbbbbbbbbbb",
        }
    }

    /// A cleartext FramebufferUpdate carrying one rekey rectangle, which is how the
    /// record layer's key arrives.
    fn rekey_update(wrap_key: &[u8; 16], keys: Keys) -> Vec<u8> {
        use aes::cipher::{BlockCipherEncrypt as _, KeyInit as _};
        let cipher = Aes128::new(wrap_key.into());
        let wrapped = |mut block: [u8; 16]| {
            cipher.encrypt_block((&mut block).into());
            block
        };

        let mut msg = vec![0u8, 0]; // FramebufferUpdate + padding
        msg.extend_from_slice(&1u16.to_be_bytes()); // one rectangle
        msg.extend_from_slice(&[0u8; 8]); // x, y, w, h all zero
        msg.extend_from_slice(&vnc_apple::ENCODING_REKEY.to_be_bytes());
        msg.extend_from_slice(&1u32.to_be_bytes()); // generation
        msg.extend_from_slice(&wrapped(keys.key));
        msg.extend_from_slice(&wrapped(keys.iv));
        msg
    }

    #[tokio::test]
    async fn the_rekey_is_read_out_of_a_cleartext_rectangle() {
        let wrap = [7u8; 16];
        let wire = rekey_update(&wrap, apple_keys());
        let got = await_rekey(&mut wire.as_slice(), &wrap).await.unwrap();
        assert_eq!(got, apple_keys());

        // A Bell first is tolerated; the rekey behind it is still found.
        let mut wire = vec![2u8];
        wire.extend_from_slice(&rekey_update(&wrap, apple_keys()));
        assert_eq!(
            await_rekey(&mut wire.as_slice(), &wrap).await.unwrap(),
            apple_keys()
        );
    }

    #[tokio::test]
    async fn nothing_may_precede_the_rekey() {
        let wrap = [7u8; 16];

        // A pixel rectangle. Everything after the rekey is ciphertext, so a stream
        // that puts anything else first has gone somewhere this client cannot follow.
        let mut wire = vec![0u8, 0];
        wire.extend_from_slice(&1u16.to_be_bytes());
        wire.extend_from_slice(&[0u8; 8]);
        wire.extend_from_slice(&ENCODING_RAW.to_be_bytes());
        let err = await_rekey(&mut wire.as_slice(), &wrap).await.unwrap_err();
        assert!(format!("{err:#}").contains("before the record layer was up"), "{err:#}");

        // A second rectangle in the same update, which would already be encrypted.
        let mut wire = rekey_update(&wrap, apple_keys());
        wire[2..4].copy_from_slice(&2u16.to_be_bytes());
        let err = await_rekey(&mut wire.as_slice(), &wrap).await.unwrap_err();
        assert!(format!("{err:#}").contains("after the rekey"), "{err:#}");

        // SetColourMapEntries, which cannot arrive before a pixel format is set.
        let err = await_rekey(&mut [1u8, 0, 0, 0, 0, 0].as_slice(), &wrap)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("message type 1"), "{err:#}");
    }

    /// Frame a run of server messages into records, as the Mac would.
    fn framed(msgs: &[Vec<u8>]) -> Vec<u8> {
        let mut writer = RecordWriter::new(apple_keys());
        let mut wire = Vec::new();
        for msg in msgs {
            wire.extend_from_slice(writer.frame(msg).unwrap());
        }
        wire
    }

    /// A FramebufferUpdate of one raw rectangle covering the whole 2x2 desktop.
    fn raw_update() -> Vec<u8> {
        let mut msg = vec![0u8, 0];
        msg.extend_from_slice(&1u16.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes()); // x
        msg.extend_from_slice(&0u16.to_be_bytes()); // y
        msg.extend_from_slice(&2u16.to_be_bytes()); // w
        msg.extend_from_slice(&2u16.to_be_bytes()); // h
        msg.extend_from_slice(&ENCODING_RAW.to_be_bytes());
        // Four BGRX pixels, all distinct so the shadow cannot mistake them for
        // what the browser already has.
        for i in 0..4u8 {
            msg.extend_from_slice(&[i, 0x40, 0x80, 0]);
        }
        msg
    }

    fn raw_rect_update(x: u16, y: u16, w: u16, h: u16, shade: u8) -> Vec<u8> {
        let mut msg = vec![0u8, 0];
        msg.extend_from_slice(&1u16.to_be_bytes());
        for value in [x, y, w, h] {
            msg.extend_from_slice(&value.to_be_bytes());
        }
        msg.extend_from_slice(&ENCODING_RAW.to_be_bytes());
        msg.extend(std::iter::repeat_n(
            [shade, shade, shade, 0],
            usize::from(w) * usize::from(h),
        ).flatten());
        msg
    }

    /// A rectangle header: where it goes, how big it is, and how it is encoded.
    fn geometry(x: u16, y: u16, w: u16, h: u16, encoding: i32) -> Vec<u8> {
        let mut msg = Vec::new();
        for value in [x, y, w, h] {
            msg.extend_from_slice(&value.to_be_bytes());
        }
        msg.extend_from_slice(&encoding.to_be_bytes());
        msg
    }

    use crate::vnc_encodings::deflate_chunk;

    /// A zlib rectangle: the geometry, then a `u32` length and that much of a
    /// deflate stream.
    fn zlib_rect(
        deflate: &mut flate2::Compress,
        (x, y, w, h): (u16, u16, u16, u16),
        pixels: &[u8],
    ) -> Vec<u8> {
        let chunk = deflate_chunk(deflate, pixels);
        let mut msg = geometry(x, y, w, h, ENCODING_ZLIB);
        msg.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
        msg.extend_from_slice(&chunk);
        msg
    }

    /// A CopyRect rectangle: the destination geometry, then the source position.
    fn copy_rect(dst: (u16, u16, u16, u16), src: (u16, u16)) -> Vec<u8> {
        let mut msg = geometry(dst.0, dst.1, dst.2, dst.3, ENCODING_COPY_RECT);
        msg.extend_from_slice(&src.0.to_be_bytes());
        msg.extend_from_slice(&src.1.to_be_bytes());
        msg
    }

    /// A raw rectangle with a colour whose channels all differ, so a swap shows.
    fn raw_rect(x: u16, y: u16, w: u16, h: u16, bgr: [u8; 3]) -> Vec<u8> {
        let mut msg = geometry(x, y, w, h, ENCODING_RAW);
        msg.extend(
            std::iter::repeat_n(
                [bgr[0], bgr[1], bgr[2], 0],
                usize::from(w) * usize::from(h),
            )
            .flatten(),
        );
        msg
    }

    /// Wrap rectangles in one FramebufferUpdate.
    fn update(rects: &[Vec<u8>]) -> Vec<u8> {
        let mut msg = vec![0u8, 0];
        msg.extend_from_slice(&(rects.len() as u16).to_be_bytes());
        for rect in rects {
            msg.extend_from_slice(rect);
        }
        msg
    }

    fn apple_layout_update(current: Option<u32>, backing: (u16, u16)) -> Vec<u8> {
        let mut msg = vec![0u8, 0];
        msg.extend_from_slice(&1u16.to_be_bytes());
        msg.extend_from_slice(&[0u8; 8]);
        msg.extend_from_slice(&vnc_apple::ENCODING_DISPLAY_LAYOUT.to_be_bytes());
        msg.extend_from_slice(&layout_payload(
            current,
            &[(11, backing, backing, 0x01)],
        ));
        msg
    }

    /// The whole read-side design in one test: a rectangle whose bytes are split
    /// across two records reaches the tile path as one rectangle, and nothing above
    /// the record layer knows the records were there.
    /// The same picture in five encodings, and only the first of them is forwarded.
    ///
    /// The shadow suppresses an update that holds nothing new, so four of these
    /// costing nothing *is* the proof that all five decoders produced the same
    /// bytes — no table of expected pixels can go stale against it, and a channel
    /// swapped in one decoder alone cannot pass. The picture is deliberately not
    /// grey and not solid: a wrong byte order or a transposed tile shows up as a
    /// second tile on the channel.
    #[tokio::test]
    async fn the_same_picture_in_five_encodings_is_forwarded_once() {
        // A 2x2 of four different colours, which every encoding below has to spell
        // out in its own way.
        let colours: [[u8; 3]; 4] = [
            [0xf0, 0x00, 0x00],
            [0x00, 0xf0, 0x00],
            [0x00, 0x00, 0xf0],
            [0x10, 0x20, 0x30],
        ];
        let bgrx: Vec<u8> = colours
            .iter()
            .flat_map(|c| [c[2], c[1], c[0], 0])
            .collect();

        let mut rects = Vec::new();
        // Raw.
        let mut raw = geometry(0, 0, 2, 2, ENCODING_RAW);
        raw.extend_from_slice(&bgrx);
        rects.push(raw);

        // Hextile: one tile, raw, since a 2x2 of four colours is what raw is for.
        let mut hextile = geometry(0, 0, 2, 2, ENCODING_HEXTILE);
        hextile.push(0x01);
        hextile.extend_from_slice(&bgrx);
        rects.push(hextile);

        // RRE: any background, then a subrect per pixel.
        let mut rre = geometry(0, 0, 2, 2, ENCODING_RRE);
        rre.extend_from_slice(&4u32.to_be_bytes());
        rre.extend_from_slice(&[0, 0, 0, 0]);
        for (i, colour) in colours.iter().enumerate() {
            rre.extend_from_slice(&[colour[2], colour[1], colour[0], 0]);
            for value in [(i % 2) as u16, (i / 2) as u16, 1, 1] {
                rre.extend_from_slice(&value.to_be_bytes());
            }
        }
        rects.push(rre);

        // ZRLE: one raw tile of CPIXELs, in its own deflate stream.
        let mut zrle_stream = flate2::Compress::new(flate2::Compression::default(), true);
        let mut tile = vec![0u8];
        for colour in &colours {
            tile.extend_from_slice(&[colour[2], colour[1], colour[0]]);
        }
        let mut zrle = geometry(0, 0, 2, 2, ENCODING_ZRLE);
        let chunk = deflate_chunk(&mut zrle_stream, &tile);
        zrle.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
        zrle.extend_from_slice(&chunk);
        rects.push(zrle);

        // zlib: the raw pixels, in a stream of their own.
        let mut zlib_stream = flate2::Compress::new(flate2::Compression::default(), true);
        rects.push(zlib_rect(&mut zlib_stream, (0, 0, 2, 2), &bgrx));

        let (uplink, _sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        // Kept, not discarded: a decoder that bailed on the third encoding would
        // leave the first rectangle's tile sitting there and the count below would
        // still read 1. Running out of stream is the only acceptable way to stop.
        let err = read_loop(
            std::io::Cursor::new(update(&rects)),
            shared,
            ReadFlags { clipboard: false, poll: false },
            None,
            sink.clone(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("closed the connection"), "{err:#}");

        sink.flush().await;
        let mut tiles = 0;
        while let Ok(msg) = rx.try_recv() {
            if matches!(msg, ServerMsg::Tile(_)) {
                tiles += 1;
            }
        }
        assert_eq!(tiles, 1, "five encodings of one picture, one tile");
    }

    /// CopyRect saves the VNC link its pixels, and now the browser link too: the
    /// destination is a thirteen-byte record naming where the client already has
    /// them, not an encode of pixels it is holding.
    #[tokio::test]
    async fn a_copy_rect_reaches_the_browser_as_a_copy() {
        let wire = update(&[
            raw_rect(0, 0, 2, 2, [0x30, 0x20, 0x10]),
            copy_rect((2, 0, 2, 2), (0, 0)),
        ]);

        let (uplink, _sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((4, 2), None, None),
            test_shadow((4, 2)),
        );
        let err = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: false, poll: false },
            None,
            sink.clone(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("closed the connection"), "{err:#}");

        sink.flush().await;
        let mut tiles = Vec::new();
        let mut copies = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ServerMsg::Tile(tile) => tiles.push(tile),
                ServerMsg::Copy(copy) => copies.push(copy),
                _ => {}
            }
        }
        assert_eq!(tiles.len(), 1, "only the painted rect carried pixels");
        assert_eq!((tiles[0].x, tiles[0].y), (0, 0));
        assert_eq!(
            copies,
            vec![protocol::CopyRect { sx: 0, sy: 0, x: 2, y: 0, w: 2, h: 2 }],
            "the copy names the source and lands at the destination"
        );
    }

    /// The plans a copy is not sound on fall back to what this engine always did:
    /// the source read out of the shadow and encoded as a tile. Under a motion
    /// strategy a moving cell owes a cleanup from stashed pixels, and copied-in ones
    /// would be restored away by a debt holding an older picture.
    #[tokio::test]
    async fn a_target_with_a_motion_strategy_still_gets_the_pixels() {
        let wire = update(&[
            raw_rect(0, 0, 2, 2, [0x30, 0x20, 0x10]),
            copy_rect((2, 0, 2, 2), (0, 0)),
        ]);

        let (uplink, _sent) = test_uplink();
        let (frame_tx, mut rx) = mpsc::channel(8);
        let sink = TileSink::new(
            "vnc",
            frame_tx,
            crate::config::RenderPlan::Tiles {
                base: crate::config::TileCodec::Png,
                motion: Some(crate::config::MotionEncode::Tile(
                    crate::config::TileCodec::Jpeg(60),
                )),
                debug: false,
                adaptive: None,
            },
            Arc::new(crate::feedback::LinkFeedback::new()),
        );
        assert!(!sink.copies(), "a motion plan must not be offered copies");
        let shared = test_shared(
            uplink,
            shared_desktop((4, 2), None, None),
            test_shadow((4, 2)),
        );
        let err = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: false, poll: false },
            None,
            sink.clone(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("closed the connection"), "{err:#}");

        sink.flush().await;
        let mut tiles = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ServerMsg::Tile(tile) => tiles.push(tile),
                ServerMsg::Copy(_) => panic!("a motion plan was sent a copy record"),
                _ => {}
            }
        }
        assert_eq!(tiles.len(), 2, "the painted rect and the copy of it, as pixels");
        assert_eq!(
            (tiles[1].x, tiles[1].y, tiles[1].w, tiles[1].h),
            (2, 0, 2, 2),
            "the copy lands at the destination, not the source"
        );
    }

    /// A source the shadow never learned cannot be reproduced, and inventing pixels
    /// would leave them wrong until something else happened to change that area. So
    /// the rectangle costs one non-incremental request instead.
    #[tokio::test]
    async fn a_copy_rect_with_an_unknown_source_asks_for_a_full_repaint() {
        let wire = update(&[copy_rect((2, 0, 2, 2), (0, 0))]);

        let (uplink, sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((4, 2), None, None),
            test_shadow((4, 2)),
        );
        let err = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: false, poll: false },
            None,
            sink.clone(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("closed the connection"), "{err:#}");

        assert_eq!(written(&sent), update_request(false, (4, 2)));
        sink.flush().await;
        assert!(rx.try_recv().is_err(), "and no invented pixels");
    }

    /// A rectangle of no pixels still carries its encoding's framing, and stepping
    /// past that framing is what keeps everything behind it readable.
    ///
    /// The zero-size check used to run *before* the payload was read, so a 0x0 zlib
    /// rectangle left its length word and chunk in the stream and every byte after
    /// it was read as something else.
    #[tokio::test]
    async fn a_zero_sized_rectangle_still_consumes_its_payload() {
        let mut deflate = flate2::Compress::new(flate2::Compression::default(), true);
        let mut wire = vec![0u8, 0];
        wire.extend_from_slice(&2u16.to_be_bytes()); // two rectangles
        wire.extend_from_slice(&zlib_rect(&mut deflate, (0, 0, 0, 0), &[]));
        // The rectangle that has to survive the one before it.
        let mut raw = Vec::new();
        for value in [0u16, 0, 2, 2] {
            raw.extend_from_slice(&value.to_be_bytes());
        }
        raw.extend_from_slice(&ENCODING_RAW.to_be_bytes());
        raw.extend(std::iter::repeat_n([0x30u8, 0x20, 0x10, 0], 4).flatten());
        wire.extend_from_slice(&raw);

        let (uplink, _sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        let err = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: false, poll: false },
            None,
            sink.clone(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("closed the connection"), "{err:#}");

        sink.flush().await;
        match rx.try_recv().expect("the rectangle behind the empty one") {
            ServerMsg::Tile(tile) => {
                assert_eq!((tile.x, tile.y, tile.w, tile.h), (0, 0, 2, 2));
            }
            other => panic!("expected a tile, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rectangle_split_across_records_still_becomes_tiles() {
        let update = raw_update();
        let (a, b) = update.split_at(update.len() - 6);
        let wire = framed(&[a.to_vec(), b.to_vec()]);

        let (uplink, _sent) = test_records_uplink(apple_keys());
        let (sink, mut rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        // Reading past the last record is a clean end of stream, so the loop ends
        // with the hang-up error rather than hanging.
        let err = read_loop(
            RecordReader::new(std::io::Cursor::new(wire), apple_keys()),
            shared,
            ReadFlags { clipboard: false, poll: false },
            Some(Apple::default()),
            sink.clone(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("closed the connection"), "{err:#}");

        // The tile arrived whole, which is the point: one 2x2 rectangle, not two
        // fragments of one.
        sink.flush().await;
        let msg = rx.try_recv().expect("a tile");
        match msg {
            ServerMsg::Tile(tile) => assert_eq!((tile.x, tile.y, tile.w, tile.h), (0, 0, 2, 2)),
            other => panic!("expected a tile, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apple_pasteboard_change_is_fetched_and_forwarded() {
        let mut wire = vec![0x14, 0, 0, 4, 0, 1, 0, 2];
        wire.extend_from_slice(&vnc_apple_clipboard::send(7, "copied on the Mac ✓").unwrap());
        let (uplink, sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );

        let _ = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: true, poll: false },
            Some(Apple { asked_for_zlib: true, ..Apple::default() }),
            sink.clone(),
        )
        .await;

        assert_eq!(written(&sent), vnc_apple_clipboard::fetch(0));
        sink.flush().await;
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerMsg::Clipboard {
                text,
                requested: false,
                oversized_bytes: None,
                ..
            }) if text == "copied on the Mac ✓"
        ));
    }

    #[tokio::test]
    async fn an_apple_clipboard_read_answers_before_its_fetch_returns() {
        let (uplink, sent) = test_uplink();
        let clipboard = Arc::new(std::sync::Mutex::new(ClipboardState {
            remote: Some(ClipboardSnapshot::changed("cached".to_owned(), None)),
            apple_session_id: 7,
            ..ClipboardState::default()
        }));
        let (sink, mut rx) = test_sink();

        request_apple_clipboard(&clipboard, &uplink, &sink).await.unwrap();

        assert_eq!(clipboard.lock().unwrap().apple_requests, 1);
        assert_eq!(written(&sent), vnc_apple_clipboard::fetch(7));
        sink.flush().await;
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerMsg::Clipboard {
                text,
                requested: true,
                oversized_bytes: None,
                ..
            }) if text == "cached"
        ));
    }

    #[tokio::test]
    async fn a_requested_bad_apple_pasteboard_still_answers_from_cache() {
        let mut wire = vec![0x1f, 0, 0, 0];
        wire.extend_from_slice(&7u32.to_be_bytes());
        wire.extend_from_slice(&10u32.to_be_bytes());
        wire.extend_from_slice(&4u32.to_be_bytes());
        wire.extend_from_slice(&[0, 0, 0, 0]);
        let (uplink, _sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        {
            let mut clipboard = shared.clipboard.lock().unwrap();
            clipboard.remote = Some(ClipboardSnapshot::changed("cached".to_owned(), None));
            clipboard.apple_requests = 1;
        }

        let _ = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: true, poll: false },
            Some(Apple { asked_for_zlib: true, ..Apple::default() }),
            sink.clone(),
        )
        .await;

        sink.flush().await;
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerMsg::Clipboard {
                text,
                requested: true,
                oversized_bytes: None,
                ..
            }) if text == "cached"
        ));
    }

    #[tokio::test]
    async fn disabled_apple_pasteboards_do_not_consume_browser_requests() {
        let ordinary = vnc_apple_clipboard::send(7, "ignored").unwrap();
        let oversized_len =
            u32::try_from(vnc_apple_clipboard::MAX_COMPRESSED_BYTES + 1).unwrap();
        let mut oversized = vec![0x1f, 0, 0, 0];
        oversized.extend_from_slice(&9u32.to_be_bytes());
        oversized.extend_from_slice(&oversized_len.to_be_bytes());
        oversized.extend_from_slice(&oversized_len.to_be_bytes());
        oversized.extend(std::iter::repeat_n(0, oversized_len as usize));

        for (wire, session_id) in [(ordinary, 7), (oversized, 9)] {
            let (uplink, _sent) = test_uplink();
            let (sink, _rx) = test_sink();
            let shared = test_shared(
                uplink,
                shared_desktop((2, 2), None, None),
                test_shadow((2, 2)),
            );
            {
                let mut clipboard = shared.clipboard.lock().unwrap();
                clipboard.apple_requests = 2;
            }
            let clipboard = shared.clipboard.clone();

            let _ = read_loop(
                std::io::Cursor::new(wire),
                shared,
                ReadFlags { clipboard: false, poll: false },
                Some(Apple::default()),
                sink,
            )
            .await;

            let clipboard = clipboard.lock().unwrap();
            assert_eq!(clipboard.apple_session_id, session_id);
            assert_eq!(clipboard.apple_requests, 2);
        }
    }

    /// Under `AutoFrameBufferUpdate` the server drives, so a request per update
    /// would race the server's own update schedule.
    #[tokio::test]
    async fn an_armed_session_does_not_poll_for_the_next_update() {
        for poll in [true, false] {
            let wire = framed(&[raw_update()]);
            let (uplink, sent) = test_records_uplink(apple_keys());
            let (sink, _rx) = test_sink();
            let shared = test_shared(
                uplink,
                shared_desktop((2, 2), None, None),
                test_shadow((2, 2)),
            );
            let _ = read_loop(
                RecordReader::new(std::io::Cursor::new(wire), apple_keys()),
                shared,
                ReadFlags { clipboard: false, poll },
                Some(Apple::default()),
                sink,
            )
            .await;
            assert_eq!(
                written(&sent).is_empty(),
                !poll,
                "poll = {poll} should{} have asked for the next update",
                if poll { "" } else { " not" }
            );
        }
    }

    // MARK: continuous updates and fences

    /// A bare `EndOfContinuousUpdates`, which is the whole message.
    fn end_of_continuous_updates() -> Vec<u8> {
        vec![MSG_END_OF_CONTINUOUS_UPDATES]
    }

    /// A ServerFence: three bytes of padding, the flags, and a counted payload.
    fn server_fence(flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut msg = vec![MSG_FENCE, 0, 0, 0];
        msg.extend_from_slice(&flags.to_be_bytes());
        msg.push(payload.len() as u8);
        msg.extend_from_slice(payload);
        msg
    }

    /// The extension's whole handshake: the server's one-message answer to the
    /// SetEncodings that advertised it, and this side turning it on for the desktop
    /// it currently has.
    #[tokio::test]
    async fn a_server_that_offers_continuous_updates_is_asked_for_them() {
        let (uplink, sent) = test_uplink();
        let (sink, _rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((640, 480), None, None),
            test_shadow((640, 480)),
        );
        let _ = read_loop(
            std::io::Cursor::new(end_of_continuous_updates()),
            shared,
            ReadFlags { clipboard: false, poll: true },
            None,
            sink,
        )
        .await;

        assert_eq!(written(&sent), enable_continuous_updates(true, (640, 480)));
    }

    /// The point of the extension: with the server pushing, the round trip per frame
    /// goes away. Nothing but the enable is written, where a polling session answers
    /// every update with a request.
    #[tokio::test]
    async fn a_continuous_session_stops_asking_for_the_next_update() {
        for continuous in [true, false] {
            let mut wire = Vec::new();
            if continuous {
                wire.extend_from_slice(&end_of_continuous_updates());
            }
            wire.extend_from_slice(&raw_update());

            let (uplink, sent) = test_uplink();
            let (sink, _rx) = test_sink();
            let shared = test_shared(
                uplink,
                shared_desktop((2, 2), None, None),
                test_shadow((2, 2)),
            );
            let _ = read_loop(
                std::io::Cursor::new(wire),
                shared,
                ReadFlags { clipboard: false, poll: true },
                None,
                sink,
            )
            .await;

            let expected = if continuous {
                enable_continuous_updates(true, (2, 2)).to_vec()
            } else {
                update_request(true, (2, 2)).to_vec()
            };
            assert_eq!(written(&sent), expected, "continuous = {continuous}");
        }
    }

    /// A second `EndOfContinuousUpdates` is the acknowledgement of a disable, and
    /// this client never asks for one — so the server has stopped on its own, and
    /// the polling loop it replaced has to start again or the screen freezes.
    #[tokio::test]
    async fn a_server_that_stops_pushing_is_polled_again() {
        let mut wire = end_of_continuous_updates();
        wire.extend_from_slice(&end_of_continuous_updates());
        wire.extend_from_slice(&raw_update());

        let (uplink, sent) = test_uplink();
        let (sink, _rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        let _ = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: false, poll: true },
            None,
            sink,
        )
        .await;

        let mut expected = enable_continuous_updates(true, (2, 2)).to_vec();
        expected.extend_from_slice(&update_request(true, (2, 2)));
        expected.extend_from_slice(&update_request(true, (2, 2)));
        assert_eq!(
            written(&sent),
            expected,
            "the disable acknowledgement restarts the cycle, and the update after it is polled"
        );
    }

    /// The enabled region is part of the request, so a desktop that changed size
    /// invalidates it — and a server left holding the old rectangle would go on
    /// pushing updates for pixels that are no longer there.
    #[tokio::test]
    async fn a_resize_re_enables_continuous_updates_for_the_new_desktop() {
        let mut wire = end_of_continuous_updates();
        wire.extend_from_slice(&update(&[geometry(0, 0, 8, 4, ENCODING_DESKTOP_SIZE)]));

        let (uplink, sent) = test_uplink();
        let (sink, _rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        let _ = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: false, poll: true },
            None,
            sink,
        )
        .await;

        let mut expected = enable_continuous_updates(true, (2, 2)).to_vec();
        expected.extend_from_slice(&enable_continuous_updates(true, (8, 4)));
        // A resize still costs one full request: the client has just cleared a
        // framebuffer of a different size and nothing it holds is worth keeping.
        expected.extend_from_slice(&update_request(false, (8, 4)));
        assert_eq!(written(&sent), expected);
    }

    /// The server's marker, handed straight back. This is the only thing telling a
    /// pushing server how fast this end is keeping up, so it has to leave the read
    /// task rather than wait behind anything.
    #[tokio::test]
    async fn a_requested_fence_is_echoed_without_the_flags_this_client_does_not_honour() {
        // Request, BlockBefore, and SyncNext — which is not implemented and must not
        // be claimed back.
        let flags = FENCE_REQUEST | FENCE_BLOCK_BEFORE | (1 << 2);
        let (uplink, sent) = test_uplink();
        let (sink, _rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        let _ = read_loop(
            std::io::Cursor::new(server_fence(flags, b"marker")),
            shared,
            ReadFlags { clipboard: false, poll: true },
            None,
            sink,
        )
        .await;

        assert_eq!(written(&sent), client_fence(FENCE_BLOCK_BEFORE, b"marker"));
    }

    /// A payload past what the extension defines is echoed back cut to length, and
    /// read in full regardless. The two are separate obligations: the echo is bounded
    /// because the specification bounds it, and the *read* is not, because a message
    /// stepped over by the wrong number of bytes desyncs everything behind it — which
    /// is what the update after the fence is here to catch.
    #[tokio::test]
    async fn an_oversized_fence_payload_is_echoed_cut_to_length_and_read_whole() {
        let payload: Vec<u8> = (0..=u8::try_from(MAX_FENCE_PAYLOAD).unwrap()).collect();
        assert!(payload.len() > MAX_FENCE_PAYLOAD, "the payload has to be over the cap");
        let mut wire = server_fence(FENCE_REQUEST, &payload);
        wire.extend_from_slice(&raw_update());

        let (uplink, sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        let _ = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: false, poll: true },
            None,
            sink.clone(),
        )
        .await;

        // Request is not echoed and nothing else was set, so the flags go back empty.
        let mut expected = client_fence(0, &payload[..MAX_FENCE_PAYLOAD]);
        expected.extend_from_slice(&update_request(true, (2, 2)));
        assert_eq!(written(&sent), expected);
        sink.flush().await;
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok()).any(|m| matches!(m, ServerMsg::Tile(_))),
            "the rectangle behind the oversized fence has to survive it"
        );
    }

    /// A fence with no Request bit is an answer to something this side never asked,
    /// and answering it would be a fence of this client's own. Its payload is still
    /// consumed: the RFB stream has no framing above the record layer, so a message
    /// stepped over by the wrong number of bytes desyncs everything behind it.
    #[tokio::test]
    async fn an_unrequested_fence_is_stepped_over_rather_than_answered() {
        let mut wire = server_fence(FENCE_BLOCK_AFTER, b"unasked");
        wire.extend_from_slice(&raw_update());

        let (uplink, sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        let _ = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: false, poll: true },
            None,
            sink.clone(),
        )
        .await;

        assert_eq!(
            written(&sent),
            update_request(true, (2, 2)),
            "the update behind the fence was read, and nothing answered the fence"
        );
        sink.flush().await;
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok()).any(|m| matches!(m, ServerMsg::Tile(_))),
            "the rectangle behind the fence has to survive it"
        );
    }

    #[test]
    fn repaint_coverage_counts_overlaps_once() {
        let a = Rect::from_size(0, 0, 10, 10).unwrap();
        let b = Rect::from_size(5, 0, 10, 10).unwrap();
        assert_eq!(union_pixels(&[a, b]), 150);
    }

    #[test]
    fn full_repaint_drops_contained_and_post_completion_regions() {
        let mut repaint = FullRepaint::new(200);
        let first = Rect::from_size(0, 0, 10, 10).unwrap();
        repaint.accept(first);
        repaint.accept(Rect::from_size(2, 2, 2, 2).unwrap());
        assert_eq!(repaint.regions, vec![first]);

        repaint.accept(Rect::from_size(10, 0, 10, 10).unwrap());
        assert!(repaint.complete());
        repaint.accept(Rect::from_size(20, 0, 10, 10).unwrap());
        assert_eq!(repaint.regions.len(), 2);
    }

    #[test]
    fn full_repaint_zero_and_exhausted_budget_are_complete() {
        let zero = FullRepaint::new(0);
        assert!(zero.complete());

        let mut incomplete = FullRepaint::new(1);
        for _ in 1..FULL_REPAINT_UPDATE_BUDGET {
            incomplete.finish_update();
            assert!(!incomplete.complete());
        }
        incomplete.finish_update();
        assert!(incomplete.complete());
    }

    /// The Mac can answer a display selection with its layout, then empty
    /// metadata updates and small damage before the non-incremental pixels. None
    /// of those may earn the normal incremental poll: on macOS it replaces the
    /// pending full request and leaves the resized framebuffer black.
    #[tokio::test]
    async fn apple_poll_waits_until_the_full_repaint_arrives() {
        let wire = framed(&[
            apple_layout_update(Some(11), (2, 2)),
            vec![0, 0, 0, 0],
            raw_rect_update(0, 0, 1, 1, 0x20),
            raw_rect_update(0, 0, 2, 2, 0x40),
        ]);
        let (uplink, sent) = test_uplink();
        let (sink, _rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        let apple = Apple { asked_for_zlib: true, ..Apple::default() };

        let _ = read_loop(
            RecordReader::new(std::io::Cursor::new(wire), apple_keys()),
            shared,
            ReadFlags { clipboard: false, poll: true },
            Some(apple),
            sink,
        )
        .await;

        let mut expected = vnc_apple::auto_framebuffer_update((2, 2));
        expected.extend_from_slice(&update_request(false, (2, 2)));
        expected.extend_from_slice(&update_request(false, (2, 2)));
        expected.extend_from_slice(&update_request(false, (2, 2)));
        expected.extend_from_slice(&update_request(true, (2, 2)));
        assert_eq!(written(&sent), expected);
    }

    #[tokio::test]
    async fn apple_poll_settles_after_bounded_incomplete_updates() {
        let mut updates = vec![apple_layout_update(Some(11), (2, 2))];
        for _ in 0..FULL_REPAINT_UPDATE_BUDGET {
            updates.push(vec![0, 0, 0, 0]);
        }
        let wire = framed(&updates);
        let (uplink, sent) = test_uplink();
        let (sink, _rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        let apple = Apple { asked_for_zlib: true, ..Apple::default() };

        let _ = read_loop(
            RecordReader::new(std::io::Cursor::new(wire), apple_keys()),
            shared,
            ReadFlags { clipboard: false, poll: true },
            Some(apple),
            sink,
        )
        .await;

        let mut expected = vnc_apple::auto_framebuffer_update((2, 2));
        for _ in 0..FULL_REPAINT_UPDATE_BUDGET {
            expected.extend_from_slice(&update_request(false, (2, 2)));
        }
        expected.extend_from_slice(&update_request(true, (2, 2)));
        assert_eq!(written(&sent), expected);
    }

    /// The layout payload builder, shared with `vnc_apple`'s own tests rather than
    /// copied: it encodes the measured record offsets, and a second copy of those
    /// would have to be kept in step with the parser by hand. `vnc_apple` is also
    /// where it is cross-checked against a captured payload.
    use crate::vnc_apple::{TestScreen, test_layout as layout_payload};

    /// A layout does three things, and the third is the one that is easy to miss:
    /// it resizes, it reports the screens, and it re-arms the server. Without the
    /// re-arm the desktop keeps painting and only the pointer silently freezes, so
    /// nothing else here would catch its absence.
    #[tokio::test]
    async fn a_display_layout_resizes_reports_and_re_arms() {
        let (uplink, sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let desktop = shared_desktop((100, 100), None, None);
        let shared = test_shared(uplink, Arc::clone(&desktop), test_shadow((100, 100)));

        // The Retina screen selected, which is the case the density matters in.
        let payload = layout_payload(
            Some(11),
            &[(11, (1920, 1080), (3840, 2160), 0x01), (22, (1600, 1000), (1600, 1000), 0x00)],
        );
        let resized = read_display_layout(&mut payload.as_slice(), &shared, true, false, false, &sink)
            .await
            .unwrap();
        assert!(resized);

        // The framebuffer is the *backing* pixels, shown at that screen's own
        // density — 100% of the logical desktop, not a canvas scaled to fit
        // anything.
        assert_eq!(desktop.lock().unwrap().size, (3840, 2160));
        assert_eq!(desktop.lock().unwrap().scale, 2.0);
        sink.flush().await;
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerMsg::Resize { w: 3840, h: 2160, scale }) if scale == 2.0
        ));

        // The screens, with the checkmark where the Mac put it, and a way back to
        // the combined view listed ahead of them.
        match rx.try_recv().expect("a display list") {
            ServerMsg::Displays { active, displays } => {
                assert_eq!(active, 11);
                assert_eq!(displays.len(), 3);
                assert_eq!(displays[0].id, DisplayState::COMBINED);
                assert_eq!(displays[0].label, "All Displays");
                assert_eq!(displays[1].detail, "1920×1080 at 2x");
                assert_eq!(displays[2].label, "Display 2");
            }
            other => panic!("expected a display list, got {other:?}"),
        }

        // What went back, in order: the second `SetEncodings` — the one that finally
        // asks for zlib, which cannot be in the first without costing this whole
        // layout — and then the re-arm for the display the Mac confirmed. The
        // enclosing update loop sends the paired full request after it has consumed
        // every rectangle in this FramebufferUpdate.
        let mut expected = set_encodings(vnc_apple::ENCODINGS_WITH_ZLIB);
        expected.extend_from_slice(&vnc_apple::auto_framebuffer_update((3840, 2160)));
        assert_eq!(written(&sent), expected);
        assert!(
            vnc_apple::ENCODINGS_WITH_ZLIB.contains(&ENCODING_ZLIB)
                && !vnc_apple::ENCODINGS.contains(&ENCODING_ZLIB),
            "the first list must not carry zlib and the second must"
        );
    }

    /// The checkmark follows the Mac and nothing else. It is placed from the
    /// `current_display` a layout carries, so a selection the Mac declines leaves the
    /// menu agreeing with what is on the canvas rather than with what was clicked.
    #[tokio::test]
    async fn the_checkmark_comes_from_the_mac_not_from_the_request() {
        let (uplink, _sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let shared = test_shared(
            uplink,
            shared_desktop((1600, 1000), None, None),
            test_shadow((1600, 1000)),
        );
        let screens: [TestScreen; 2] = [
            (11, (1920, 1080), (1920, 1080), 0x01),
            (22, (1600, 1000), (1600, 1000), 0x00),
        ];
        let layout = |current| layout_payload(current, &screens);

        // A session opens on the combined view, which is what the Mac sends when
        // nothing has asked otherwise.
        read_display_layout(&mut layout(None).as_slice(), &shared, false, false, false, &sink)
            .await
            .unwrap();
        assert_eq!(shared.display.lock().unwrap().active, DisplayState::COMBINED);

        // Then a screen, then back again. Each move is a layout, never a request.
        read_display_layout(&mut layout(Some(22)).as_slice(), &shared, false, false, false, &sink)
            .await
            .unwrap();
        assert_eq!(shared.display.lock().unwrap().active, 22);
        read_display_layout(&mut layout(Some(22)).as_slice(), &shared, false, false, false, &sink)
            .await
            .unwrap();
        read_display_layout(&mut layout(None).as_slice(), &shared, false, false, false, &sink)
            .await
            .unwrap();
        assert_eq!(shared.display.lock().unwrap().active, DisplayState::COMBINED);

        sink.flush().await;
        let mut actives = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let ServerMsg::Displays { active, .. } = msg {
                actives.push(active);
            }
        }
        // Four layouts, three messages: the repeated one says nothing new. A client
        // holds no display state of its own, so a message it cannot act on is one it
        // would have to ignore.
        assert_eq!(actives, vec![DisplayState::COMBINED, 22, DisplayState::COMBINED]);
    }
}
