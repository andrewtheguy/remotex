//! VNC client, in two dialects that share everything below the handshake.
//!
//! **RFB 3.8**, which is every VNC server including a Mac's under `subtype =
//! "ard"`: Raw framebuffer updates, the cursor and resize pseudo-encodings,
//! classic or Apple DH authentication, baseline or Extended Clipboard.
//!
//! **RFB 003.889**, Apple's own revision, under `subtype =
//! "ard-high-performance"`: the same RFB messages carried inside an AES-128-CBC
//! record layer ([`crate::vnc_record`]), alongside Apple's control messages
//! ([`crate::vnc_apple`]). What that buys is **zlib instead of raw pixels**, the
//! Mac's screens listed so one of them can be picked, and each screen's **pixel
//! density**, which is what lets a Retina desktop be drawn at 100% instead of twice
//! its size. See docs/apple-vnc-889.md, which records the several places the
//! protocol reference is wrong — including the message this gateway used to send
//! that made the Mac hide its real screens.
//!
//! The difference is contained in three places and nowhere else: [`Dialect`]
//! (which banner, which ClientInit byte), the two preface functions after
//! ServerInit, and the encodings [`read_rect`] then has to handle. One read loop,
//! one input path, one tile path — decoded damage joins the common ordered tile
//! path either way.

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
    ClientMsg, ClipboardSnapshot, CursorShape, DisplayInfo, MAX_CLIPBOARD_BYTES, MouseButton,
    ServerMsg, UNSCALED, clipboard_fits,
};
use crate::tiles::{self, Rect, Shadow};
use crate::vnc_apple::{self, CursorCache, ZlibStream};
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
/// Bytes per pixel of the format we force with SetPixelFormat.
const BPP: usize = 4;
/// Cap on server-sent reason/name strings, so a bogus length can't OOM us.
const MAX_STRING: u32 = 1024;
/// Largest cursor edge accepted. Real pointers are 32x32 or 64x64; anything
/// beyond this is drained and ignored rather than drawn.
const MAX_CURSOR_DIM: u16 = 256;
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
    /// RFB 3.8, as every VNC server speaks it — including a Mac under plain
    /// `subtype = "ard"`, which changes only the authentication.
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
    /// Always [`UNSCALED`] on plain RFB, where a framebuffer is just its pixels
    /// and no server says otherwise. Apple's display layout does say otherwise —
    /// a Retina screen renders at twice its logical size — and reporting only the
    /// pixel count there would give the browser a canvas at half the size the Mac
    /// thinks it is.
    scale: f32,
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

/// The Mac's screens and which one is being shared. Empty on plain RFB, where
/// there is nothing to choose between, and until the first layout arrives.
#[derive(Debug, Default)]
struct DisplayState {
    displays: Vec<DisplayInfo>,
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

/// The 003.889 dialect's decoding state, owned by the read loop.
///
/// Not in [`Shared`]: the zlib stream and the cursor cache are touched by nothing
/// else, and a lock on the pixel path to say so would be a lock that never
/// contends.
#[derive(Default)]
struct Apple {
    /// One inflate stream for the connection's zlib rectangles. Created on the
    /// first one, never reset — see [`ZlibStream`].
    zlib: Option<ZlibStream>,
    cursors: CursorCache,
    /// Whether zlib has been asked for yet.
    ///
    /// It cannot be in the first `SetEncodings` — see
    /// [`vnc_apple::ENCODINGS_WITH_ZLIB`] — so it is asked for in a second one, once
    /// the Mac has reported its displays and there is nothing left to lose by it.
    /// Once, hence the flag: a layout arrives at every login and lock.
    asked_for_zlib: bool,
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
/// RFB has no "read the clipboard" request — the server pushes whenever the
/// remote clipboard changes — so `remote` keeps the latest text to answer
/// [`ClientMsg::ClipboardRequest`]. Forwarding the push live is not enough on
/// its own: a browser that attaches mid-session, or reattaches after a drop,
/// has missed every push so far and would see an empty panel with no way to
/// ask.
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
) {
    let sink = TileSink::new("vnc", frame_tx, config.tile_codec());
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

    let Connected { downlink, uplink, width, height, macos, poll } = connected;
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
            apple: Dialect::of(config.subtype) == Dialect::Apple889,
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
    /// A VNC target's size was ignored before this: the server's own size was
    /// the connect-time size and nothing here could name another. Honouring it
    /// costs nothing at connect — it is only ever consulted for a client that
    /// asks — and it is what lets an operator say what a phone should get
    /// without a second constant existing anywhere.
    default_size: (u16, u16),
    /// Whether this is the RFB 003.889 dialect, which is what gives the read loop
    /// its zlib stream, its cursor cache and a display list to report.
    apple: bool,
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
        Dialect::Apple889 => apple_preface(reader, sock, server, macos, wrap_key).await,
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
    uplink.send(&set_pixel_format()).await?;
    // Cursor is unconditional (the browser can always draw a pointer). The
    // resize pseudo-encodings are advertised only when the target opts in
    // (`resize = true`); without them the server never announces support and
    // the desktop keeps its connect-time size.
    let mut encodings = vec![ENCODING_RAW, ENCODING_CURSOR];
    if config.resize {
        encodings.push(ENCODING_EXTENDED_DESKTOP_SIZE);
        encodings.push(ENCODING_DESKTOP_SIZE);
    }
    if config.clipboard {
        // Extended Clipboard, which is the only way RFB carries anything
        // outside latin-1. Advertised on opt-in only; a server that ignores it
        // simply never sends caps and the latin-1 path stays in use.
        encodings.push(vnc_clipboard::ENCODING);
    }
    uplink.send(&set_encodings(&encodings)).await?;

    Ok(Connected {
        downlink: Downlink::Plain(reader),
        uplink,
        width: server.width,
        height: server.height,
        macos,
        poll: true,
    })
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
) -> anyhow::Result<Connected> {
    let wrap_key = wrap_key.ok_or_else(|| {
        anyhow::anyhow!(
            "Apple's protocol revision needs its DH authentication, which this server did not offer"
        )
    })?;

    // Both written back to back and before anything is read. The server emits the
    // rekey the moment it sees the first of them, so anything that waited for a
    // reply in between would be writing cleartext into a server that had already
    // switched.
    //
    // `ViewerInfo` is deliberately absent, and its absence was measured: macOS 26
    // reads *more* bytes for it than its own length field declares — the
    // "version strings" the reference names but never frames — so sending one
    // swallows whatever follows and the server then waits forever for the rest of
    // a message that has already been sent. With it the rekey never arrives; with
    // only these two it arrives immediately. Nothing is lost: the one bit the
    // server is known to read out of it gates observe-only mode, which this
    // client does not use.
    sock.write_all(&vnc_apple::set_encryption_start()).await?;
    sock.write_all(&vnc_apple::set_encryption_stop()).await?;

    let keys = await_rekey(&mut reader, &wrap_key).await?;
    info!("vnc: Apple record layer active");

    let mut uplink = Uplink::records(sock, keys);
    // No `SetDisplayConfiguration` (`0x1d`), and its absence is the load-bearing
    // part of this preface. Sending one — the bare static descriptor included —
    // makes macOS 26 create a virtual display spanning the real screens, turn them
    // off for the session's duration, and report a single-screen layout at a flat
    // density of 1. That is what made display picking and the Retina density both
    // look impossible on this wire. Omitting it is what gets the Mac's own screens,
    // their ids, and their individual scale factors. See [`crate::vnc_apple`].
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
        poll,
    } = flags;
    // The uplink is shared: the read loop answers the server (update requests,
    // re-arming), the input side sends pointer/key/display messages.
    let uplink: SharedUplink = Arc::new(Mutex::new(uplink));
    let desktop: SharedDesktop = Arc::new(std::sync::Mutex::new(DesktopState {
        size,
        scale: UNSCALED,
        screen: None,
        pending: None,
    }));
    let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
    let clipboard: SharedClipboard = Arc::new(std::sync::Mutex::new(ClipboardState::default()));
    let shadow: SharedShadow = Arc::new(std::sync::Mutex::new(Shadow::new("vnc", size.0, size.1)));
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
        apple.then(Apple::default),
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
                // behaviour `request_resize` already has.
                let wanted_size = match input {
                    ClientMsg::Viewport { w, h } => Some((w, h)),
                    ClientMsg::DefaultSize => Some(default_size),
                    _ => None,
                };
                let sent = if let Some(size) = wanted_size {
                    if resize {
                        request_resize(&uplink, &desktop, size).await
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
                    // Answered from the buffer the read loop fills: RFB has no
                    // way to *ask* the server for its clipboard. Empty until
                    // the remote copies something, which reads in the panel as
                    // "nothing has been copied over there yet".
                    if clipboard_enabled {
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
                    }
                    Ok(())
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
                    // asked for. Only the Apple dialect can act on it: standard RFB
                    // exposes one framebuffer and has no message for this.
                    if apple {
                        let known =
                            display.lock().unwrap().displays.iter().any(|d| d.id == id);
                        if known {
                            // `COMBINED` is this gateway's own list entry, not a
                            // screen the Mac named, so it maps back to the
                            // `combine_all_displays` byte rather than to an id.
                            let pick = (id != DisplayState::COMBINED).then_some(id);
                            debug!("vnc: asking the Mac for display {pick:?}");
                            send(&uplink, &vnc_apple::set_display_message(pick)).await
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
                    let msgs =
                        translate_input(input, &mut button_mask, &mut last_pos, &mut pressed_keys);
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

/// Handle a browser viewport report (dynamic resize): send
/// SetDesktopSize once the server has declared support via an
/// ExtendedDesktopSize rect; until then, stash the report for replay.
async fn request_resize(
    uplink: &SharedUplink,
    desktop: &SharedDesktop,
    want: (u16, u16),
) -> anyhow::Result<()> {
    let msg = {
        let mut d = desktop.lock().unwrap();
        if want.0 == 0 || want.1 == 0 {
            return Ok(());
        }
        if want == d.size {
            // The browser is back at the current size; drop any stale stash
            // so a later support declaration doesn't replay it.
            d.pending = None;
            return Ok(());
        }
        match d.screen {
            Some(screen) => set_desktop_size(want, screen),
            None => {
                d.pending = Some(want);
                return Ok(());
            }
        }
    };
    debug!("vnc: requesting desktop resize to {}x{}", want.0, want.1);
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
/// `apple` is `Some` on the RFB 003.889 dialect and carries the decoding state
/// only that dialect's encodings need.
async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    shared: Shared,
    flags: ReadFlags,
    mut apple: Option<Apple>,
    sink: TileSink,
) -> anyhow::Result<()> {
    let ReadFlags { clipboard: clipboard_enabled, poll } = flags;
    let Shared { uplink, desktop, clipboard, .. } = &shared;
    loop {
        let msg_type = match reader.read_u8().await {
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
                for _ in 0..rects {
                    let effect = read_rect(&mut reader, &shared, &mut apple, &sink).await?;
                    resized |= effect.resized;
                    if effect.last {
                        break;
                    }
                }
                // Complete the cycle — but only where there is a cycle to
                // complete. On the 003.889 wire `AutoFrameBufferUpdate` made the
                // server the driver, and a request per update would be a second
                // client racing the first. A resize still invalidates the old
                // contents, so it is asked for again there and only there.
                if poll || resized {
                    let size = desktop.lock().unwrap().size;
                    send(uplink, &update_request(poll && !resized, size)).await?;
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
                debug!("vnc: Apple message type {msg_type:#02x}, {len} bytes");
                discard(&mut reader, u64::from(len)).await?;
            }
            other => anyhow::bail!("unknown server message type {other}"),
        }
    }
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

/// What reading one rectangle did, beyond whatever it painted.
#[derive(Debug, Default, Clone, Copy)]
struct RectEffect {
    /// The desktop changed size, so what the browser holds is stale.
    resized: bool,
    /// A `LastRect`: this update ends here, whatever its header's count claimed.
    last: bool,
}

impl RectEffect {
    const NOTHING: Self = Self { resized: false, last: false };
    const LAST: Self = Self { resized: false, last: true };

    const fn resized(resized: bool) -> Self {
        Self { resized, last: false }
    }
}

/// Read one FramebufferUpdate rectangle — pixels compared against what the
/// browser holds and forwarded as tiles, or one of the pseudo-encodings that
/// carry a cursor, a size or a display layout instead.
async fn read_rect<R: AsyncRead + Unpin>(
    reader: &mut R,
    shared: &Shared,
    apple: &mut Option<Apple>,
    sink: &TileSink,
) -> anyhow::Result<RectEffect> {
    let Shared { uplink, desktop, cursor, shadow, .. } = shared;
    let x = reader.read_u16().await?;
    let y = reader.read_u16().await?;
    let w = reader.read_u16().await?;
    let h = reader.read_u16().await?;
    let encoding = reader.read_i32().await?;
    // Whether the pixels below arrive deflated. Decided here so the bounds check
    // and the tile path stay one path for both.
    let mut deflated = false;
    match encoding {
        ENCODING_RAW => {}
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
        // Plain-RFB only, and the guard is not decoration: it carries no density, so
        // applying one on the Apple dialect would overwrite a scale learned from a
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
        vnc_apple::ENCODING_ZLIB if apple.is_some() => deflated = true,
        vnc_apple::ENCODING_CURSOR_IMAGE if apple.is_some() => {
            read_cursor_image(reader, apple, cursor, (x, y), (w, h), sink).await?;
            return Ok(RectEffect::NOTHING);
        }
        vnc_apple::ENCODING_DISPLAY_LAYOUT if apple.is_some() => {
            let first = apple.as_ref().is_some_and(|a| !a.asked_for_zlib);
            if let Some(a) = apple.as_mut() {
                a.asked_for_zlib = true;
            }
            read_display_layout(reader, shared, first, sink).await?;
            // Deliberately *not* reported as a resize, even when it was one.
            // [`read_display_layout`] issues its own non-incremental request as part
            // of re-arming the server, so telling the caller the desktop resized
            // would have it ask for the same full frame a second time — which on a
            // 4480x1800 desktop is a wasted 400 KB every login and lock.
            return Ok(RectEffect::NOTHING);
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
    if w == 0 || h == 0 {
        return Ok(RectEffect::NOTHING);
    }

    let expect = usize::from(w) * usize::from(h) * BPP;
    let pixels = if deflated {
        // One `u32` of length, then that much of the connection's single deflate
        // stream. The stream is the connection's, not the rectangle's — see
        // [`ZlibStream`].
        let len = reader.read_u32().await?;
        // Deflate can *expand*, and on a small rectangle it always does: the
        // stream header and one sync flush cost more than a 1x1 rectangle's four
        // pixels (measured: nine bytes for four). So bounding this at `expect`
        // would refuse legitimate rectangles. A generous multiple still bounds the
        // read, which is all this check is for — the inflated size is checked
        // exactly, by `ZlibStream::inflate`.
        let ceiling = expect + expect / 64 + 1024;
        anyhow::ensure!(
            u64::from(len) <= ceiling as u64,
            "a zlib rect claims {len} compressed bytes for {expect} of pixels, past the \
             {ceiling} that even an incompressible one would take"
        );
        let mut chunk = vec![0u8; len as usize];
        reader.read_exact(&mut chunk).await?;
        let apple = apple.as_mut().expect("zlib is the Apple dialect's alone");
        apple
            .zlib
            .get_or_insert_with(|| ZlibStream::new("zlib"))
            .inflate(&chunk, expect)?
    } else {
        let mut pixels = vec![0u8; expect];
        reader.read_exact(&mut pixels).await?;
        pixels
    };
    let Some(rect) = Rect::from_size(x, y, w, h) else {
        return Ok(RectEffect::NOTHING);
    };
    let rgb = bgrx_to_rgb(&pixels);

    // What of this rect the browser does not already have. A server that
    // re-sends unchanged pixels — and they do, on a cursor crossing a window
    // boundary or a client asking for a full update — stops costing the browser
    // link anything here.
    let Some(changed) = shadow.lock().unwrap().accept(rect, &rgb) else {
        return Ok(RectEffect::NOTHING);
    };

    for band in changed.bands() {
        // Cropped out of the rect just read rather than out of the shadow: the
        // bytes are the same and this needs no lock. Its own buffer per band, since
        // the encoder reads it after this function has returned and `rgb` is gone.
        let mut pixels = Vec::new();
        tiles::crop(&rgb, rect, band, &mut pixels);
        sink.tile(band.left, band.top, band.w(), band.h(), pixels)
            .await?;
    }
    Ok(RectEffect::NOTHING)
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
            let shape =
                CursorShape::from_rgba(w, h, hx, hy, &masked_bgrx_to_rgba(&pixels, &mask, w))?;
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
/// `scale` is how large those pixels should look — [`UNSCALED`] on plain RFB,
/// which has no way to say otherwise, and the Mac's own ratio on the Apple
/// dialect. A scale change with no size change still counts: the same pixels shown
/// at a different size is a different canvas.
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
    let resize_msg = {
        let mut d = desktop.lock().unwrap();
        if d.size == new && d.scale == scale {
            return Ok(false);
        }
        d.size = new;
        d.scale = scale;
        d.resize_msg()
    };
    // The old pixels describe a framebuffer that no longer exists, and the
    // browser is about to reallocate its canvas.
    shadow.lock().unwrap().resize(new.0, new.1);
    info!("vnc: desktop resized to {}x{} at {scale}x", new.0, new.1);
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
    let shape = match apple.cursors.accept(id, hotspot, size, &deflated)? {
        vnc_apple::Cursor::Shape(shape) => shape,
        // Nothing to draw and nothing to say: the pointer keeps the shape it has,
        // which is closer to the truth than blanking it.
        vnc_apple::Cursor::Unchanged => return Ok(()),
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
    let layout = vnc_apple::parse_layout(&payload)?;

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
    // before the re-arm so the update that follows is the compressed one.
    if ask_for_zlib {
        debug!("vnc: display layout received, asking for zlib");
        uplink.send(&set_encodings(vnc_apple::ENCODINGS_WITH_ZLIB)).await?;
    }
    // Re-arm, on every layout and not only on a change of geometry.
    uplink.send(&vnc_apple::auto_framebuffer_update(size)).await?;
    uplink.send(&update_request(false, size)).await?;
    Ok(resized)
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
    button_mask: &mut u8,
    last_pos: &mut (u16, u16),
    pressed_keys: &mut HashMap<String, u32>,
) -> Vec<Vec<u8>> {
    match input {
        ClientMsg::MouseMove { x, y } => {
            *last_pos = (clamp_u16(x), clamp_u16(y));
            vec![pointer_event(*button_mask, *last_pos).to_vec()]
        }
        // `clicks` goes nowhere: RFB carries a button mask alone, and the guest
        // counts the clicks itself from the events it receives.
        ClientMsg::MouseButton { button, pressed, .. } => {
            let bit = match button {
                MouseButton::Left => 0x01,
                MouseButton::Middle => 0x02,
                MouseButton::Right => 0x04,
                // RFB's mask has bits for buttons 8 and 9, but no server agrees
                // on what they mean and the ones remotex talks to ignore them.
                // Dropped rather than sent as a scroll notch, which is what
                // those bits are on every server that does read them.
                MouseButton::Back | MouseButton::Forward => return Vec::new(),
            };
            if pressed {
                *button_mask |= bit;
            } else {
                *button_mask &= !bit;
            }
            vec![pointer_event(*button_mask, *last_pos).to_vec()]
        }
        // The unit is dropped: RFB has one notch and no way to say how big it is.
        ClientMsg::Wheel { dx, dy, .. } => {
            // A wheel notch is a press+release of buttons 4-7 (mask bits 3-6):
            // 4 = up, 5 = down, 6 = left, 7 = right. One notch per event,
            // like the RDP engine.
            let mut out = Vec::new();
            for (delta, negative_bit, positive_bit) in [(dy, 0x08, 0x10), (dx, 0x20, 0x40)] {
                if delta != 0.0 {
                    let bit = if delta > 0.0 { positive_bit } else { negative_bit };
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
        // `Audio` is another: it subscribes that attachment to the queue this
        // engine already fills, so there is nothing to translate for the remote.
        ClientMsg::Connect { .. }
        | ClientMsg::Disconnect
        | ClientMsg::CacheReset
        | ClientMsg::Audio { .. } => Vec::new(),
        // Intercepted by the input loop, which is where the requested screen is
        // recorded — see the `SelectDisplay` branch there. Standard RFB has nothing
        // for it in any case: one framebuffer spans every screen, and the
        // ExtendedDesktopSize list describes how they are laid out inside it rather
        // than offering a set to choose between.
        ClientMsg::SelectDisplay { .. } => Vec::new(),
        // Nothing to act on: RFB has no backing scale, and a VNC server's
        // framebuffer is already the pixels it has. Clients send this
        // unconditionally rather than asking what the engine is, so it is
        // ignored here rather than treated as a client error.
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
/// and is reported as not-macOS. What that costs is the native viewer's
/// keyboard convention, not correctness, which is why guessing from a desktop
/// name is not worth it.
fn is_macos_server(minor: u32, security_types: &[u8]) -> bool {
    minor == 889 || security_types.iter().any(|t| matches!(t, 30 | 35))
}

/// Repack BGRX pixels (our forced format on the wire) into packed RGB888.
fn bgrx_to_rgb(bgrx: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(bgrx.len() / BPP * 3);
    for px in bgrx.chunks_exact(BPP) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    rgb
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
    fn bgrx_repacks_to_rgb() {
        // Two pixels: pure red and pure blue in BGRX order.
        let bgrx = [0, 0, 255, 0, 255, 0, 0, 0];
        assert_eq!(bgrx_to_rgb(&bgrx), vec![255, 0, 0, 0, 0, 255]);
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

        let bytes = translate_input(
            ClientMsg::MouseButton {
                button: MouseButton::Left,
                pressed: true,
                clicks: 1,
            },
            &mut mask,
            &mut pos,
            &mut keys,
        );
        assert_eq!(bytes, vec![pointer_event(0x01, (10, 20)).to_vec()]);

        // A move while the button is held keeps it in the mask (drag).
        let bytes = translate_input(
            ClientMsg::MouseMove { x: 30, y: 40 },
            &mut mask,
            &mut pos,
            &mut keys,
        );
        assert_eq!(bytes, vec![pointer_event(0x01, (30, 40)).to_vec()]);

        // Scroll down = button 5 (0x10) press + release, on top of the held mask.
        let bytes = translate_input(
            ClientMsg::Wheel { dx: 0.0, dy: 3.0, unit: WheelUnit::Pixel },
            &mut mask,
            &mut pos,
            &mut keys,
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
            &mut mask,
            &mut pos,
            &mut keys,
        );
        assert_eq!(bytes, vec![pointer_event(0x00, (30, 40)).to_vec()]);
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
        (TileSink::new("vnc", frame_tx, crate::config::TileCodec::Png), frame_rx)
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
        request_resize(&uplink, &desktop, (1024, 768)).await.unwrap();
        request_resize(&uplink, &desktop, (0, 600)).await.unwrap();
        assert!(desktop.lock().unwrap().pending.is_none());
        assert!(written(&wire).is_empty());

        // Support not declared yet: stashed, nothing on the wire.
        request_resize(&uplink, &desktop, (800, 600)).await.unwrap();
        assert_eq!(desktop.lock().unwrap().pending, Some((800, 600)));
        assert!(written(&wire).is_empty());

        // Browser back at the current size: the stale stash is dropped.
        request_resize(&uplink, &desktop, (1024, 768)).await.unwrap();
        assert!(desktop.lock().unwrap().pending.is_none());

        // Support declared: SetDesktopSize goes out immediately.
        let screen = Screen { id: 7, flags: 0 };
        desktop.lock().unwrap().screen = Some(screen);
        request_resize(&uplink, &desktop, (800, 600)).await.unwrap();
        assert_eq!(written(&wire), set_desktop_size((800, 600), screen));
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
            &mut mask,
            &mut pos,
            keys,
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

    /// The whole read-side design in one test: a rectangle whose bytes are split
    /// across two records reaches the tile path as one rectangle, and nothing above
    /// the record layer knows the records were there.
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

    /// Under `AutoFrameBufferUpdate` the server drives, so a request per update
    /// would be a second client racing the first.
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
        let resized = read_display_layout(&mut payload.as_slice(), &shared, true, &sink)
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
        // layout — and then the re-arm pair.
        let mut expected = set_encodings(vnc_apple::ENCODINGS_WITH_ZLIB);
        expected.extend_from_slice(&vnc_apple::auto_framebuffer_update((3840, 2160)));
        expected.extend_from_slice(&update_request(false, (3840, 2160)));
        assert_eq!(written(&sent), expected);
        assert!(
            vnc_apple::ENCODINGS_WITH_ZLIB.contains(&vnc_apple::ENCODING_ZLIB)
                && !vnc_apple::ENCODINGS.contains(&vnc_apple::ENCODING_ZLIB),
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
        read_display_layout(&mut layout(None).as_slice(), &shared, false, &sink).await.unwrap();
        assert_eq!(shared.display.lock().unwrap().active, DisplayState::COMBINED);

        // Then a screen, then back again. Each move is a layout, never a request.
        read_display_layout(&mut layout(Some(22)).as_slice(), &shared, false, &sink).await.unwrap();
        assert_eq!(shared.display.lock().unwrap().active, 22);
        read_display_layout(&mut layout(Some(22)).as_slice(), &shared, false, &sink).await.unwrap();
        read_display_layout(&mut layout(None).as_slice(), &shared, false, &sink).await.unwrap();
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
