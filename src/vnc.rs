//! VNC client, in two dialects that share everything below the handshake.
//!
//! **RFB 3.8**, for every server a target names no subtype for: classic VNC,
//! RSA-AES or no authentication, and the extensions a generic server announces.
//!
//! **RFB 003.889**, Apple's own revision, which both Apple subtypes speak, as
//! Apple's viewer does: Apple's DH authentication, then the same RFB messages
//! carried inside an AES-128-CBC record layer ([`crate::vnc_record`]), alongside
//! Apple's control messages ([`crate::vnc_apple`]) and its pasteboard protocol.
//! The mode follows ServerInit, as it does there. `subtype = "ard"` is Standard
//! mode: the Mac's physical displays, with their list, selection and density, in
//! ZRLE rectangles. `subtype = "ard-high-performance"` is High Performance mode:
//! one virtual display at the target's pinned `width` and `height`, or at the
//! connecting client's screen resolution when no size is pinned, with the picture
//! and sound from the Mac's media stream ([`crate::vnc_apple_media`]) and ZRLE
//! rectangles carrying the picture until it is up. See docs/apple-vnc-889.md.
//!
//! The transport difference is contained in three places and nowhere else:
//! `Dialect` (which banner and ClientInit byte), the two preface functions after
//! ServerInit, and the optional record wrapper. One read loop, one input path, one
//! Apple metadata path and one tile path serve both.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

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

use crate::config::{Chroma, RenderPlan, Subtype, TargetConfig};
use crate::encode::{TileSupport, VideoSink};
use crate::engine::{self, clamp_u16, host_port};
use crate::keymap;
use crate::protocol::{
    ClientMsg, ClipboardSnapshot, CursorShape, CursorUnit, DisplayInfo, HostDisplay,
    MAX_CLIPBOARD_BYTES, MAX_CURSOR_DIM, MouseButton, ServerMsg, UNSCALED, WheelUnit,
    clipboard_fits,
};
use crate::shadow::{self, Rect, Shadow};
use crate::vnc_apple::{self, CursorCache};
use crate::vnc_apple_media::{self, MediaStream, PassedUnit, Pictures};
use crate::vnc_audio::{self, FrameDecoder, ServerAudio};
use crate::vnc_encodings::{Decoded, Decoders, Payload};
use crate::vnc_apple_clipboard;
use crate::camera::CameraSignal;
use crate::vnc_camera::{self, ServerCamera};
use crate::mic::MicSignal;
use crate::vnc_mic::{self, ServerMicrophone};
use crate::vnc_clipboard;
use crate::vnc_record::{self, Keys, RecordReader, RecordWriter};
use crate::vnc_rsa_aes::{self, FrameReader, Sealer, Strength};

const SECURITY_NONE: u8 = 1;
const SECURITY_VNC_AUTH: u8 = 2;
/// Apple's Diffie-Hellman authentication, the RFB security type that carries a
/// *user name* to a Mac — see [`ard_authenticate`] for why a Mac target needs
/// it and what happens without it. RealVNC's RSA-AES types carry one to
/// everything else; see [`crate::vnc_rsa_aes`].
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
/// Once polling has paused behind a pasteboard fetch, this much silence means the
/// fetch is not going to answer. Resume screen polling; an explicit browser read
/// was already answered from the cache.
const APPLE_CLIPBOARD_IDLE_GAP: Duration = Duration::from_secs(1);
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
/// job. Also the one codec a Mac is offered — see [`vnc_apple::ENCODINGS`].
pub(crate) const ENCODING_ZRLE: i32 = 16;
/// Standard RFB zlib: `u32 length` then that many bytes of one deflate stream
/// shared by every rectangle on the connection.
const ENCODING_ZLIB: i32 = 6;
/// Cursor pseudo-encoding: the server hands over the pointer shape (pixels +
/// a 1-bit mask, the rect's x/y being the hotspot) instead of drawing it into
/// the framebuffer.
const ENCODING_CURSOR: i32 = -239;
/// Cursor With Alpha pseudo-encoding: the same handover with the shape's alpha
/// intact — a `S32` encoding, then premultiplied RGBA in it — so a shadow and
/// antialiased edges survive that the 1-bit mask would cut away. A server that
/// speaks it prefers it; one that does not goes on sending [`ENCODING_CURSOR`].
const ENCODING_CURSOR_WITH_ALPHA: i32 = -314;
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

/// The wlshare density extension's pseudo-encoding, the ASCII bytes `WLSH`. Listed
/// in `SetEncodings` on every plain `vnc` target, the way ContinuousUpdates and
/// Fence are: wlshare answers it with an [`MSG_WLSHARE_DENSITY`] report before
/// its first update, and any other server ignores it like any encoding it does
/// not know, which is how the extension is discovered. See
/// docs/wlshare-density.md.
const ENCODING_WLSHARE_DENSITY: i32 = 0x574c_5348;
/// The extension's one message type, used in both directions: the server's
/// `OutputScale` report and the client's `ClientDensity` declaration. Outside
/// every registered RFB message type.
const MSG_WLSHARE_DENSITY: u8 = 0xE0;

/// The wlshare outputs extension's pseudo-encoding, the ASCII bytes `WLSO`.
/// Listed beside the density request on every plain `vnc` target and discovered
/// the same way: wlshare answers it with an [`MSG_WLSHARE_OUTPUTS`] list of the
/// compositor's outputs, and every other server ignores an encoding it does not
/// know. It is what fills the display picker on a generic target — one
/// framebuffer is one output, so a two-monitor desktop has to be asked which one
/// to send. See docs/wlshare-outputs.md.
const ENCODING_WLSHARE_OUTPUTS: i32 = 0x574c_534f;
/// wlshare's VP9 encoding, `WLSV`: every update one rectangle over the whole
/// desktop, a `u32` length and one frame of a single 4:4:4 VP9 stream — the stream
/// this gateway would encode from the same pixels for a browser that decodes profile
/// 1, which it then passes through untouched ([`VideoSink::pass`]). Listed only for
/// such a browser. wlshare sends it in place of ZRLE wherever it is listed and
/// announces nothing; any other server ignores it and sends what it always did.
const ENCODING_WLSHARE_VP9: i32 = 0x574c_5356;
/// The largest `WLSV` frame read rather than refused. A 4:4:4 keyframe of the largest
/// desktop the ceiling admits at the finest quantizer is a few megabytes; a length
/// past this is a server that has lost its framing.
const MAX_WLSHARE_VP9_FRAME: u32 = 64 << 20;
/// The longest a passed stream's fence echo waits for the browser to take what came
/// before it ([`VideoSink::drained`]). A client that is not drawing acknowledges
/// nothing, and its batches' budget comes back only with a pong, so an unbounded
/// wait would stop wlshare, which sends nothing until the echo, at one frame a
/// heartbeat. The paint window's own grace for such a window, and the one wlshare's
/// desktop client gives its own: past it the echo goes, and a client that is only
/// slow still holds the engine where it always did, at the budget.
const FENCE_HOLD_LIMIT: Duration = Duration::from_millis(500);
/// The extension's one message type, used in both directions: the server's
/// `OutputList` and the client's `SelectOutput`. Outside every registered RFB
/// message type.
const MSG_WLSHARE_OUTPUTS: u8 = 0xE1;
/// A fence the server wants echoed. Nothing else in the flags word obliges a
/// client, and the two it may keep are [`FENCE_BLOCK_BEFORE`] and
/// [`FENCE_BLOCK_AFTER`].
const FENCE_REQUEST: u32 = 1 << 31;
const FENCE_BLOCK_BEFORE: u32 = 1 << 0;
const FENCE_BLOCK_AFTER: u32 = 1 << 1;
/// Longest fence payload the extension defines. A server sending more is malformed;
/// the excess is consumed to keep the stream in step and left out of the echo.
const MAX_FENCE_PAYLOAD: usize = 64;
/// Longest FLAC frame one audio message may carry. A frame is 20 ms of the
/// format this client asks for, 3840 bytes before compression, and FLAC never
/// grows it by more than a few header bytes, so anything past 64 KiB is a
/// server that has lost its framing rather than one with a lot to say: its
/// bytes are stepped over instead of allocated.
const MAX_AUDIO_FRAME: u32 = 1 << 16;
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
    /// RFB 3.8, used by generic VNC.
    Rfb38,
    /// Apple's RFB 003.889 and the record layer that goes up after ServerInit,
    /// used by both Apple subtypes. Apple's viewer answers every Mac with this
    /// revision and chooses between Standard and High Performance after
    /// ServerInit, which is where [`apple_preface`] makes the same choice.
    Apple889,
}

impl Dialect {
    fn of(subtype: Option<Subtype>) -> Self {
        match subtype {
            Some(Subtype::Ard | Subtype::ArdHighPerformance) => Dialect::Apple889,
            None => Dialect::Rfb38,
        }
    }

    /// The version this client answers the server's greeting with.
    fn banner(self) -> &'static [u8; 12] {
        match self {
            Dialect::Rfb38 => b"RFB 003.008\n",
            Dialect::Apple889 => b"RFB 003.889\n",
        }
    }

    /// The ClientInit byte. Nominally RFB's shared-session flag. On Apple's
    /// revision `0x80` asks for the enhanced ServerInit, as Apple's viewer always
    /// does; `0x40`, which it sets only with a session picker to offer, would ask a
    /// Mac whose console user is not the one authenticated to choose a login session
    /// first — an exchange this client does not implement.
    fn client_init(self) -> u8 {
        match self {
            // Share the session: don't kick other clients. The single-session
            // policy lives in this program, not on the VNC server.
            Dialect::Rfb38 => 1,
            Dialect::Apple889 => 0x81,
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
    /// The same socket with RSA-AES's frames peeled off, boxed for the same
    /// reason.
    Frames(Box<FrameReader<Reader>>),
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
            Downlink::Frames(r) => std::pin::Pin::new(r).poll_read(cx, buf),
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
    out: Out,
    framing: Framing,
}

/// Where a framed message goes.
enum Out {
    /// The handshake's: written before [`Uplink::send`] returns, because every
    /// step of it waits on the server's answer to the last.
    ///
    /// Boxed rather than a type parameter: a vtable hop costs nothing measurable,
    /// and it keeps [`Shared`] and every rect handler free of a `W`.
    Socket(Box<dyn AsyncWrite + Send + Unpin>),
    /// The running session's: handed to [`write_queued`], so that nothing which
    /// sends ever waits on the socket — see [`Uplink::queued`].
    Queue(mpsc::UnboundedSender<Vec<u8>>, Arc<Backlog>),
}

/// How long a session that is shutting down waits for its writer to deliver what
/// is already queued — see the input loop's close.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(1);

/// What [`write_queued`] has been handed and has not yet written.
#[derive(Default)]
struct Backlog {
    bytes: std::sync::atomic::AtomicUsize,
    /// Notified after every write, which is the only time the count falls.
    written: tokio::sync::Notify,
}

impl Backlog {
    /// The most that may be waiting before the browser's camera and microphone
    /// stop being fed to it. Their queues upstream are bounded and shed what a
    /// stalled server cannot take; input and the read loop's own messages are
    /// small, owed to the server whatever its pace, and never held to this.
    const MEDIA_LIMIT: usize = 256 * 1024;

    /// The most that may be waiting before pointer motion and scrolling are held
    /// in [`HeldMotion`] instead of queued: about one capped wheel event on the
    /// Apple wire, the largest thing a single input becomes. Anything queued past
    /// it is motion the server acts out after the hand that made it has stopped.
    const MOTION_LIMIT: usize = 16 * 1024;

    fn behind(&self, limit: usize) -> bool {
        self.bytes.load(std::sync::atomic::Ordering::Relaxed) >= limit
    }

    /// Wait until less than `limit` is waiting to be written.
    async fn room(&self, limit: usize) {
        loop {
            // Registered before the check, so a write landing in between is a
            // wakeup this still sees.
            let written = self.written.notified();
            if !self.behind(limit) {
                return;
            }
            written.await;
        }
    }

    /// Wait until the socket has caught up enough to be worth a media sample.
    async fn room_for_media(&self) {
        self.room(Self::MEDIA_LIMIT).await;
    }
}

/// Pointer motion and scrolling held back while the uplink is behind, in the one
/// form in which each can wait: a position a newer one replaces, and a distance a
/// later one adds to.
///
/// Both are worthless late. A server that reads slowly — a Mac reads hardly at all
/// while it is pushing pixels — would otherwise be sent every position the pointer
/// crossed and every pulse of a scroll (up to 512 messages an event, on the Apple
/// wire) long after the hand had stopped, and would act all of it out. Held here,
/// what goes out when the writer catches up is where the pointer is *now* and at
/// most one event's worth of scroll; the rest is shed, which is what a scroll that
/// outran the link can afford to lose. Nothing else is ever held: a click or a key
/// is content, not motion, and goes out at once behind whatever was held, because
/// it means what it means only where the pointer was.
#[derive(Default)]
struct HeldMotion {
    pointer: Option<(i32, i32)>,
    /// Pixels of scroll intent, whatever unit they were reported in.
    wheel: (f32, f32),
}

impl HeldMotion {
    fn is_empty(&self) -> bool {
        self.pointer.is_none() && self.wheel == (0.0, 0.0)
    }

    /// Take `input` if it is motion, or hand it back.
    fn hold(&mut self, input: ClientMsg) -> Option<ClientMsg> {
        match input {
            ClientMsg::MouseMove { x, y } => self.pointer = Some((x, y)),
            ClientMsg::Wheel { dx, dy, unit } => {
                // Held under the cap a single event is spent under, so what is
                // shed is shed here rather than carried as a number that only grows.
                let add = |held: f32, delta: f32| {
                    let sum = held + Wheel::pixels(delta, unit);
                    if sum.is_finite() { sum.clamp(-Wheel::MAX_PX, Wheel::MAX_PX) } else { held }
                };
                self.wheel = (add(self.wheel.0, dx), add(self.wheel.1, dy));
            }
            other => return Some(other),
        }
        None
    }

    /// What was held, as the inputs it stands for: the position first, because a
    /// scroll lands where the pointer is.
    fn take(&mut self) -> impl Iterator<Item = ClientMsg> + use<> {
        let Self { pointer, wheel: (dx, dy) } = std::mem::take(self);
        let pointer = pointer.map(|(x, y)| ClientMsg::MouseMove { x, y });
        let wheel = ((dx, dy) != (0.0, 0.0))
            .then_some(ClientMsg::Wheel { dx, dy, unit: WheelUnit::Pixel });
        pointer.into_iter().chain(wheel)
    }
}

/// The running session's only writer: everything [`Uplink::send`] queued, in
/// order, until the queue closes with the session or the socket fails.
async fn write_queued(
    mut sock: Box<dyn AsyncWrite + Send + Unpin>,
    mut queue: mpsc::UnboundedReceiver<Vec<u8>>,
    backlog: Arc<Backlog>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    while let Some(bytes) = queue.recv().await {
        sock.write_all(&bytes).await.context("write to the VNC server")?;
        backlog.bytes.fetch_sub(bytes.len(), std::sync::atomic::Ordering::Relaxed);
        backlog.written.notify_waiters();
    }
    Ok(())
}

/// What wraps a message on its way out.
enum Framing {
    /// Bare RFB, as it has always been written.
    Plain,
    /// Apple's record layer, one record per message. Boxed: two AES key
    /// schedules and a staging buffer, an order of magnitude more than the
    /// other two, that every plain session would otherwise carry.
    Records(Box<RecordWriter>),
    /// RSA-AES's frames, as many per message as its length needs.
    Frames(Sealer),
}

impl Uplink {
    fn plain(sock: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self {
            out: Out::Socket(Box::new(sock)),
            framing: Framing::Plain,
        }
    }

    fn records(sock: impl AsyncWrite + Send + Unpin + 'static, keys: Keys) -> Self {
        Self {
            out: Out::Socket(Box::new(sock)),
            framing: Framing::Records(Box::new(RecordWriter::new(keys))),
        }
    }

    fn frames(sock: impl AsyncWrite + Send + Unpin + 'static, sealer: Sealer) -> Self {
        Self {
            out: Out::Socket(Box::new(sock)),
            framing: Framing::Frames(sealer),
        }
    }

    /// Hand the socket to a writer of its own, for the running session.
    ///
    /// A server that is busy writing pixels reads slowly, and a Mac that cannot
    /// write does not read at all. A send that waited on the socket therefore
    /// waited, with the uplink held, on a server that was itself waiting for this
    /// end to read — and the read loop, queued behind that send for its next
    /// update request, was not reading. Nothing in that circle times out. Framing
    /// still happens here, under the uplink's lock, so the wire's order is still
    /// the order of the sends; only the write moves, to the future this returns,
    /// and with it the one wait that could close the circle.
    ///
    /// Returns the uplink to share, what is queued behind it, and the writer to run
    /// for as long as the session does.
    fn queued(self) -> (Self, Arc<Backlog>, impl Future<Output = anyhow::Result<()>> + Send + 'static) {
        let Self { out, framing } = self;
        let Out::Socket(sock) = out else {
            unreachable!("an uplink is handed to its writer once, by the session that built it");
        };
        let (tx, queue) = mpsc::unbounded_channel();
        let backlog = Arc::new(Backlog::default());
        let writer = write_queued(sock, queue, Arc::clone(&backlog));
        (Self { out: Out::Queue(tx, Arc::clone(&backlog)), framing }, backlog, writer)
    }

    async fn send(&mut self, msg: &[u8]) -> anyhow::Result<()> {
        let Self { out, framing } = self;
        let framed = match framing {
            Framing::Plain => std::borrow::Cow::Borrowed(msg),
            Framing::Records(records) => std::borrow::Cow::Borrowed(records.frame(msg)?),
            Framing::Frames(sealer) => std::borrow::Cow::Owned(sealer.frame(msg)),
        };
        match out {
            Out::Socket(sock) => sock.write_all(&framed).await?,
            Out::Queue(queue, backlog) => {
                let framed = framed.into_owned();
                backlog.bytes.fetch_add(framed.len(), std::sync::atomic::Ordering::Relaxed);
                // Closed only once the writer has returned, and its error is the
                // one the session reports.
                queue
                    .send(framed)
                    .map_err(|_| anyhow::anyhow!("the VNC server's connection is closed for writing"))?;
            }
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
    /// [`ClientMsg::HostDisplay`], seeded from the session-open's screen.
    ///
    /// High Performance resize spends it when constructing a virtual-display
    /// mode. Standard Screen Sharing spends it in `SetServerScaling`, asking the
    /// Mac to return physical-display pixels at the browser's density. `scale` is
    /// what the *remote* granted; the two disagree exactly while a density change
    /// is in flight.
    host_density: f32,
    /// First screen of the server's layout. `Some` only once the server has
    /// sent an ExtendedDesktopSize rect — its declaration that SetDesktopSize
    /// is supported; nothing is requested before that.
    screen: Option<Screen>,
    /// A desktop size, in points, that could not be asked for yet — no support
    /// declared, or the density report still awaited — replayed on the first
    /// ExtendedDesktopSize rect or the report. A browser viewport report while
    /// the session runs, and at session-open the operator's pinned size
    /// ([`Flags::pinned`]).
    pending: Option<(u16, u16)>,
    /// The window's last requested size in points, kept so a scale report can
    /// ask for the same window again in the new pixels. `None` until the first
    /// generic resize request.
    viewport: Option<(u16, u16)>,
    /// Where the wlshare density extension stands on this connection.
    density: Density,
    /// The scale the server reported for its framebuffer, which labels every
    /// generic rect from then on. `None` on every other server and until the
    /// first report: generic RFB is [`UNSCALED`] by default.
    wire_scale: Option<f32>,
    /// Whether the window drives the desktop size ([`Flags::resize`]). The
    /// browser's density is declared to a reporting server only then: the server
    /// sets its output's scale to what is declared, and a client that could not
    /// then re-ask the pixels would be left with half a desktop.
    resize: bool,
    /// A `ClientDensity` is out, and the server has not answered it yet. The
    /// declaration carries the window in pixels at the declared density, and the
    /// server sets the output's mode and scale to it in one configuration — one
    /// redraw, not two — or refuses; either way it answers with a report. Resize
    /// requests are held until that report, which re-asks the window only when
    /// the server settled on pixels other than the ones declared.
    following: bool,
    /// The density last declared to the server, followed or not; `None` before
    /// the first declaration, and from a switch of output until the next — the
    /// output now shared was declared nothing. The report answering a
    /// declaration is compared with it: a browser whose density moved on while
    /// that declaration was out, or an output switched under it, is declared
    /// again then, so one transition is in flight at a time and the server's
    /// last word is the browser's newest.
    declared: Option<f32>,
    /// A scale report relabelled the current pixels, which cleared the browser's
    /// canvas, and nothing since has asked the server for them again. Cleared
    /// by the full update a resize's rect earns, or by the one requested in a
    /// resize's place — see [`read_output_scale`].
    repaint_owed: bool,
    /// A High Performance session's window-driven resizes — see [`HpResize`].
    hp: HpResize,
    /// A High Performance layout has arrived: the virtual display the session
    /// asked for exists, and a media-stream offer can name its size.
    laid_out: bool,
    /// The picture comes from the media stream ([`vnc_apple_media`]): a decoded
    /// picture of the current size has been shown since the last display change.
    /// Pixel polling then holds to [`HP_HOLD_REQUEST`], which still brings the
    /// cursor shapes and layouts, and ZRLE pixels are decoded but not shown.
    media_live: bool,
}

/// How long a High Performance viewport has to hold still before the Mac is
/// asked for it. A window drag reports sizes faster than `SetDisplayConfiguration`
/// can be served, and each one the Mac acts on reconfigures the virtual display:
/// only the size the window came to rest at goes out.
const HP_RESIZE_SETTLE: Duration = Duration::from_secs(1);

/// How long after the Mac's last layout the resize counts as settled and the
/// browser's cover comes down. The Mac follows one change with duplicate layouts
/// and a repaint; this is what keeps them behind the cover.
const HP_LAYOUT_QUIET: Duration = Duration::from_millis(500);

/// How long a resize may go unanswered before it is given up on. Not a pacing
/// timeout: the Mac reads no client message while it is still writing an update,
/// so a gateway that drains a 2x repaint slowly leaves a request unread for tens
/// of seconds, and a second request overlapping the first is what crashes its
/// agent. This only keeps a lost answer from pinning
/// the cover and every later resize for the rest of the session.
const HP_RESIZE_STUCK: Duration = Duration::from_secs(30);

/// The pixel region a High Performance session asks for while its display is
/// being reconfigured: one pixel at the origin, which every mode has.
///
/// The Mac's agent reads the screen for a region out of the capture surface
/// without checking it against the new, smaller one. A region of the old size
/// served just after a shrinking change is a `memcpy` past the end of the surface
/// in the agent's screen-read call, and the agent dies with the
/// session's display, audio and input. Two regions are live: a pixel request's,
/// and the one `AutoFrameBufferUpdate` armed, which the Mac serves on any captured
/// frame once its interval has passed, the first after the change included. Both
/// are narrowed to this before a change goes out, and polling holds to it until
/// the answering layout, which arrives inside an update.
const HP_HOLD_REQUEST: (u16, u16) = (1, 1);


/// Where a High Performance resize is in its exchange with the Mac.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum HpPhase {
    /// Nothing asked.
    #[default]
    Idle,
    /// These points are due, and wait for the read loop's next update boundary —
    /// the one point at which no full-size pixel request is outstanding — to go
    /// out ([`HpResize::take_due`]).
    Draining((u16, u16)),
    /// A `SetDisplayConfiguration` for these points is out and its layout has
    /// not arrived.
    InFlight((u16, u16)),
}

/// A High Performance session's window-driven resizes: debounced, one at a time,
/// and covered in the browser until the Mac has settled.
///
/// The Mac cannot serve overlapping `SetDisplayConfiguration`s — it answers some
/// with its old layout, and a second one arriving mid-change has crashed its
/// agent. So a reported size waits for [`HP_RESIZE_SETTLE`] of quiet and for any
/// request already out to be answered, and goes out at an update boundary with
/// pixel polling held to [`HP_HOLD_REQUEST`] until the answering layout. From the
/// first report until [`HP_LAYOUT_QUIET`] after that layout, the browser is told a
/// resize is in progress ([`ServerMsg::Resizing`]) and covers the desktop, as
/// Apple's client does. A session opens covered, through the resize from the
/// Mac's opening display to the window's.
///
/// Pure state with the clock passed in; [`hp_resize_step`] and the read loop act
/// on it, and the input loop wakes at [`HpResize::deadline`].
#[derive(Debug, Default)]
struct HpResize {
    /// The newest window size, in points, not yet asked for.
    want: Option<(u16, u16)>,
    /// When [`Self::want`] may go out: [`HP_RESIZE_SETTLE`] after its report.
    send_at: Option<tokio::time::Instant>,
    phase: HpPhase,
    /// When [`Self::phase`] left [`HpPhase::Idle`], for [`HP_RESIZE_STUCK`].
    since: Option<tokio::time::Instant>,
    /// When the cover may come down: [`HP_LAYOUT_QUIET`] after the last layout.
    quiet_until: Option<tokio::time::Instant>,
    /// The browser has been told a resize is in progress.
    shown: bool,
    /// The session's first layout has not arrived. A resizing session opens
    /// covered ([`Self::opening`]): the display it connects to is the Mac's own,
    /// not the window's, and the virtual display replacing it is still to come.
    awaiting_layout: bool,
}

/// What [`HpResize::step`] says to do next.
#[derive(Debug, PartialEq, Eq)]
enum HpStep {
    /// Tell the browser a resize is in progress.
    Show,
    /// A size is due: prompt the Mac for an update, at whose end the read loop
    /// sends it.
    Drain,
    /// Tell the browser the resize has settled.
    Hide,
    /// The Mac never answered: re-arm the full `AutoFrameBufferUpdate` region
    /// and ask for a full repaint, which the answering layout would have done.
    GiveUp,
}

impl HpResize {
    /// A resizing session's state at connect: covered until its first layout and
    /// whatever resize the window asks of it have settled.
    fn opening() -> Self {
        Self { awaiting_layout: true, shown: true, ..Self::default() }
    }

    /// The window reported `want` points. `noop` is whether that is the desktop
    /// already showing: with nothing in flight it cancels any earlier report, and
    /// with a request out it still goes, since the answer may be some other size.
    /// A size due but not yet sent is replaced: it waits on an update boundary
    /// that can be seconds away, and the window may have moved on since.
    fn report(&mut self, want: (u16, u16), noop: bool, now: tokio::time::Instant) {
        if matches!(self.phase, HpPhase::Draining(_)) {
            self.phase = HpPhase::Idle;
            self.since = None;
        }
        if noop && self.phase == HpPhase::Idle {
            self.want = None;
            self.send_at = None;
        } else {
            self.want = Some(want);
            self.send_at = Some(now + HP_RESIZE_SETTLE);
        }
    }

    /// The Mac sent a layout. One that `changed` the desktop answers whatever was
    /// out; the Mac also repeats a layout unchanged — its opening one arrives twice,
    /// the second after a request may already have gone — and that answers nothing.
    fn layout(&mut self, changed: bool, now: tokio::time::Instant) {
        self.awaiting_layout = false;
        if changed && matches!(self.phase, HpPhase::InFlight(_)) {
            self.phase = HpPhase::Idle;
            self.since = None;
        }
        if self.shown {
            self.quiet_until = Some(now + HP_LAYOUT_QUIET);
        }
    }

    /// The newest points this resize is headed for: a size still settling, else
    /// the one due or out. `None` with nothing asked.
    fn newest_points(&self) -> Option<(u16, u16)> {
        self.want.or(match self.phase {
            HpPhase::Draining(points) | HpPhase::InFlight(points) => Some(points),
            HpPhase::Idle => None,
        })
    }

    /// Whether pixel polling is held to [`HP_HOLD_REQUEST`].
    fn holds_pixels(&self) -> bool {
        self.phase != HpPhase::Idle
    }

    /// Whether nothing is pending, out or covered — the Mac is not about to change
    /// the display, so a media-stream offer made now is not torn down by one.
    fn settled(&self) -> bool {
        !self.shown && !self.awaiting_layout && self.phase == HpPhase::Idle && self.want.is_none()
    }

    /// The points due to go out, taken at an update boundary; the request is in
    /// flight from here.
    fn take_due(&mut self, now: tokio::time::Instant) -> Option<(u16, u16)> {
        let HpPhase::Draining(want) = self.phase else {
            return None;
        };
        self.phase = HpPhase::InFlight(want);
        self.since = Some(now);
        Some(want)
    }

    /// A due size turned out to be the desktop already showing: nothing goes out.
    fn drop_due(&mut self) {
        self.phase = HpPhase::Idle;
        self.since = None;
    }

    /// The next thing due at `now`, if anything is.
    fn step(&mut self, now: tokio::time::Instant) -> Option<HpStep> {
        if self.want.is_some() && !self.shown {
            self.shown = true;
            return Some(HpStep::Show);
        }
        if self.phase != HpPhase::Idle {
            if self.since.is_some_and(|since| now < since + HP_RESIZE_STUCK) {
                return None;
            }
            warn!(
                "vnc: the Mac has not answered a virtual-display resize in {}s; giving up on it",
                HP_RESIZE_STUCK.as_secs()
            );
            self.phase = HpPhase::Idle;
            self.since = None;
            return Some(HpStep::GiveUp);
        }
        // The opening configuration is itself unanswered until then.
        if self.awaiting_layout {
            return None;
        }
        if let Some(want) = self.want {
            if self.send_at.is_some_and(|at| now < at) {
                return None;
            }
            self.want = None;
            self.send_at = None;
            self.phase = HpPhase::Draining(want);
            self.since = Some(now);
            return Some(HpStep::Drain);
        }
        if self.shown && self.quiet_until.is_none_or(|at| now >= at) {
            self.shown = false;
            self.quiet_until = None;
            return Some(HpStep::Hide);
        }
        None
    }

    /// When [`Self::step`] next has something to do, or `None` until an event.
    fn deadline(&self, now: tokio::time::Instant) -> Option<tokio::time::Instant> {
        if self.want.is_some() && !self.shown {
            return Some(now);
        }
        if self.phase != HpPhase::Idle {
            return self.since.map(|since| since + HP_RESIZE_STUCK);
        }
        if self.awaiting_layout {
            return None;
        }
        if self.want.is_some() {
            return self.send_at;
        }
        self.shown.then(|| self.quiet_until.unwrap_or(now))
    }
}

/// The wlshare density extension's state on one connection — see
/// [`ENCODING_WLSHARE_DENSITY`] and docs/wlshare-density.md. The extension is
/// discovered, not configured: every plain `vnc` target asks, and the server's
/// first update decides whether it was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Density {
    /// An Apple dialect: the pseudo-encoding was never sent, because Apple's
    /// display layout carries the density its own way.
    Off,
    /// Sent, unanswered so far. Resize requests wait here, because a request in
    /// the wrong pixels is a desktop redrawn twice. Pixels arriving in this
    /// state settle it: see [`DesktopState::first_update`].
    Asked,
    /// The server sent pixels before any report, so it does not speak the
    /// extension: generic RFB, presented at [`UNSCALED`]. A report arriving
    /// after all is still taken, since the label is the wire's word.
    Unanswered,
    /// The server answered at least once: [`DesktopState::wire_scale`] is set.
    Reported,
}

/// wlshare's audio extension's state on one connection — see
/// [`crate::vnc_audio`]. Discovered exactly as [`Density`] is: a target
/// that asked for sound lists the pseudo-encoding, and what the server does
/// with it decides the rest. Kept by the read loop, which is the only side that
/// speaks the extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Audio {
    /// The target asked for no sound, or this is an Apple dialect: `ard` leaves
    /// the Mac's sound alone, and `ard-high-performance`'s bridge is fed by the
    /// media stream's sound leg.
    Off,
    /// Listed in `SetEncodings`, with nothing announced yet.
    Asked,
    /// The server announced the extension; the format was set and the stream
    /// turned on.
    Announced,
    /// Pixels arrived with no announcement before them, so this server has no
    /// sound to give. Said once — a server that announces late is still taken.
    Unanswered,
}

impl DesktopState {
    /// The density request's deadline, checked on every framebuffer update.
    /// wlshare answers `SetEncodings` before it sends a single update, so
    /// pixels with no report before them mean a server that does not speak the
    /// extension: the desktop is generic RFB at 1x from here, and the resize a
    /// report would have released goes out with the rect that declares
    /// SetDesktopSize support instead.
    fn first_update(&mut self) {
        if self.density == Density::Asked {
            debug!("vnc: the server does not report pixel density; presenting it at 1x");
            self.density = Density::Unanswered;
        }
    }
    /// The size and scale, as a client is told them.
    fn resize_msg(&self) -> ServerMsg {
        ServerMsg::Resize {
            w: self.size.0,
            h: self.size.1,
            scale: self.scale,
        }
    }

    /// The scale a generic rect is labelled with: the server's reported one
    /// where it has reported, [`UNSCALED`] everywhere else.
    fn generic_scale(&self) -> f32 {
        self.wire_scale.unwrap_or(UNSCALED)
    }

    /// The pixels a generic `SetDesktopSize` asks for a window of `points`:
    /// points × the reported scale, so the logical desktop is the window, and
    /// under the video stream's picture ceiling.
    fn generic_pixels(&self, points: (u16, u16)) -> (u16, u16) {
        self.pixels_at(points, self.generic_scale())
    }

    /// The pixels a window of `points` is at `scale`, under the video stream's
    /// picture ceiling.
    fn pixels_at(&self, points: (u16, u16), scale: f32) -> (u16, u16) {
        let px = |v: u16| (f32::from(v) * scale).round().clamp(1.0, f32::from(u16::MAX)) as u16;
        held_under_ceiling((px(points.0), px(points.1)))
    }

    /// The generic resize request for a window of `points`, or `None` when
    /// nothing should go out yet: the request is held in `pending` until the
    /// server has declared SetDesktopSize support and either answered the
    /// density request or sent pixels without it — a request in the wrong
    /// pixels is a desktop redrawn twice. `None` also when the desktop already has the
    /// size. Both that and a request sent clear any older hold: a replay must
    /// never ask for a window the browser has since left.
    ///
    /// Called with the uplink held — see [`send_decided`] — so the wire
    /// carries requests in the order they were decided.
    /// Whether asking a High Performance Mac for `points` would change nothing.
    /// The density has to agree too: a 3840×2160 desktop moving from 1x to 2x
    /// keeps every pixel and still needs the new mode sent.
    fn hp_noop(&self, points: (u16, u16)) -> bool {
        let mode = vnc_apple::virtual_display_mode(points, self.host_density);
        mode.pixels == self.size && (self.scale - self.host_density).abs() < 0.005
    }

    /// The `SetDisplayConfiguration` for a High Performance resize that is due,
    /// taken by the read loop at an update boundary — see [`HpResize`]. `None`
    /// when nothing is due, or when what is due is the desktop already showing.
    fn hp_take_request(&mut self, now: tokio::time::Instant) -> Option<Vec<u8>> {
        let want = self.hp.take_due(now)?;
        if self.hp_noop(want) {
            debug!("vnc: the window settled on the current desktop; nothing to ask for");
            self.hp.drop_due();
            return None;
        }
        debug!(
            "vnc: requesting Apple virtual-display resize to {}x{} points at {}x",
            want.0, want.1, self.host_density,
        );
        let mode = vnc_apple::virtual_display_mode(want, self.host_density);
        Some(vnc_apple::set_display_configuration(mode))
    }

    /// The region a pixel request asks for: the desktop, or while a High
    /// Performance display change is out or the media stream carries the
    /// picture, [`HP_HOLD_REQUEST`].
    fn poll_size(&self) -> (u16, u16) {
        if self.hp.holds_pixels() || self.media_live { HP_HOLD_REQUEST } else { self.size }
    }

    /// Whether the media stream may be offered for the current display: one
    /// exists, and nothing is pending, out or covered.
    fn media_offerable(&self) -> bool {
        self.laid_out && self.hp.settled()
    }

    fn generic_resize(&mut self, points: (u16, u16)) -> Option<[u8; 24]> {
        self.viewport = Some(points);
        if self.density == Density::Asked || self.following {
            debug!(
                "vnc: holding a {}x{} point desktop resize until the server {}",
                points.0,
                points.1,
                if self.following { "has answered the declared density" } else { "reports its scale" }
            );
            self.pending = Some(points);
            return None;
        }
        let pixels = self.generic_pixels(points);
        if pixels == self.size {
            // The browser is back at the current size; drop any stale stash so
            // a later support declaration doesn't replay it.
            self.pending = None;
            return None;
        }
        let Some(screen) = self.screen else {
            // Visible on stderr because from a browser this is indistinguishable
            // from a server that refused: the window asked, and the desktop did
            // not follow.
            debug!(
                "vnc: holding a {}x{} point desktop resize until the server declares \
                 SetDesktopSize support (no ExtendedDesktopSize rect yet)",
                points.0, points.1
            );
            self.pending = Some(points);
            return None;
        };
        debug!(
            "vnc: requesting desktop resize to {}x{} pixels for {}x{} points at {}x",
            pixels.0,
            pixels.1,
            points.0,
            points.1,
            self.generic_scale()
        );
        self.pending = None;
        Some(set_desktop_size(pixels, screen))
    }

    /// The `ClientDensity` declaring `declared` to a reporting server, or `None`
    /// where the window does not drive the desktop size — see
    /// [`Self::resize`]. It carries the window in pixels at that density — the
    /// newest one the browser asked for, or the desktop's own points before it
    /// has asked — so the server changes mode and scale together. Every
    /// declaration opens a follow ([`Self::following`]): the server answers it
    /// with a report, and until it does no resize goes out. The window it
    /// carries is no longer held: the answer asks again only if it was not
    /// granted.
    fn declare_density(&mut self, declared: f32) -> Option<[u8; 10]> {
        if !self.resize {
            return None;
        }
        // The reported scale, not the canvas's label: a first report is declared
        // back before it relabels the canvas.
        let points = self.pending.take().or(self.viewport).unwrap_or_else(|| {
            let scale = self.generic_scale();
            let point = |v: u16| (f32::from(v) / scale).round().max(1.0) as u16;
            (point(self.size.0), point(self.size.1))
        });
        self.viewport = Some(points);
        let pixels = self.pixels_at(points, declared);
        debug!(
            "vnc: declaring {declared}x for {}x{} pixels, a {}x{} point window",
            pixels.0, pixels.1, points.0, points.1
        );
        self.following = true;
        self.declared = Some(declared);
        Some(client_density(pixels, declared))
    }

    /// The `ClientDensity` for a `HostDisplay` report mid-session, or `None`.
    /// The density is recorded whatever happens; it is declared only once the
    /// server has reported, only when it changed, and not while a declaration
    /// is out — the report answering that one declares the newest density
    /// then, so a browser that changes twice while the server is busy ends up
    /// followed to where it is, not to where it passed through.
    fn host_density_changed(&mut self, declared: f32) -> Option<[u8; 10]> {
        let changed = (self.host_density - declared).abs() > 0.005;
        self.host_density = declared;
        (changed && self.density == Density::Reported && !self.following)
            .then(|| self.declare_density(declared))
            .flatten()
    }

    /// The `ClientDensity` a switch of shared output needs, or `None`. The
    /// browser's density was declared to the output left behind, and the
    /// browser's own report of it is unchanged by the switch, so nothing else
    /// would tell the new one. Declared on the terms a density change is: once
    /// the server has reported, and not while a declaration is out — the report
    /// answering that one declares instead, finding [`Self::declared`] cleared.
    /// The declaration carries the window, so the new output, whose size is its
    /// own and not the window's, takes it in the same configuration.
    fn output_switched(&mut self) -> Option<[u8; 10]> {
        self.declared = None;
        (self.density == Density::Reported && !self.following)
            .then(|| self.declare_density(self.host_density))
            .flatten()
    }
}

/// Decide a message from the desktop's state and send it, with the uplink held
/// from the decision to the write. Every generic resize request and every
/// density declaration leaves through here or [`request_resize`], which takes
/// the same lock first, so the wire's order is the decisions' order: a message
/// decided from newer state is never followed by one decided from older. The
/// read loop's replays go through this after their awaits rather than before,
/// so they read the state as it is when they send, not as it was when the rect
/// arrived. Returns whether anything went out.
async fn send_decided<M: AsRef<[u8]>>(
    uplink: &SharedUplink,
    desktop: &SharedDesktop,
    decide: impl FnOnce(&mut DesktopState) -> Option<M>,
) -> anyhow::Result<bool> {
    let mut up = uplink.lock().await;
    let msg = decide(&mut desktop.lock().unwrap());
    match msg {
        Some(msg) => up.send(msg.as_ref()).await.map(|()| true),
        None => Ok(false),
    }
}

/// [`send_decided`] for the camera: the uplink first, then the device, so a plug
/// decided from newer state never trails an unplug decided from older — the
/// server's announcement, read on the read loop, can race the browser's plug.
async fn send_camera_decided(
    uplink: &SharedUplink,
    link: &vnc_camera::Link,
    decide: impl FnOnce(&mut vnc_camera::Device) -> Option<Vec<u8>>,
) -> anyhow::Result<()> {
    let mut up = uplink.lock().await;
    let msg = decide(&mut link.device.lock().unwrap());
    match msg {
        Some(msg) => up.send(&msg).await,
        None => Ok(()),
    }
}

/// [`send_camera_decided`] for the microphone. A decision that ended a recording is
/// told to the bridge under the device's lock, so a start the read loop hears for the
/// same device can never be told after it.
async fn send_microphone_decided(
    uplink: &SharedUplink,
    link: &vnc_mic::Link,
    decide: impl FnOnce(&mut vnc_mic::Device) -> vnc_mic::Decision,
) -> anyhow::Result<()> {
    let mut up = uplink.lock().await;
    let message = {
        let mut device = link.device.lock().unwrap();
        let decision = decide(&mut device);
        if decision.closed {
            link.bridge.signal(MicSignal::Close);
        }
        decision.message
    };
    match message {
        Some(msg) => up.send(&msg).await,
        None => Ok(()),
    }
}

/// The mic socket's next command for the loop to send; never, on a session without a
/// microphone. Held while the uplink is behind, like [`camera_input`].
async fn microphone_input(queues: &mut Option<vnc_mic::Queues>, backlog: &Backlog) -> vnc_mic::Input {
    let next = match queues {
        Some(queues) => {
            backlog.room_for_media().await;
            queues.next().await
        }
        None => None,
    };
    match next {
        Some(input) => input,
        // The bridge keeps the control, and with it the sender, for as long as the
        // engine runs, so the queues only end with the engine.
        None => std::future::pending().await,
    }
}

/// The camera socket's next command for the loop to send; never, on a session
/// without a camera. Held while the uplink is behind: a send no longer waits on the
/// socket, so this is where a server that is not reading stops being fed samples,
/// and the socket's own queue sheds what it cannot take.
async fn camera_input(queues: &mut Option<vnc_camera::Queues>, backlog: &Backlog) -> vnc_camera::Input {
    let next = match queues {
        Some(queues) => {
            backlog.room_for_media().await;
            queues.next().await
        }
        None => None,
    };
    match next {
        Some(input) => input,
        // The bridge keeps the control, and with it the senders, for as long as
        // the engine runs, so the queues only end with the engine.
        None => std::future::pending().await,
    }
}

type SharedDesktop = Arc<std::sync::Mutex<DesktopState>>;

/// The remote's screens and which one is being shared: a Mac's, from its display
/// layout, or a wlroots compositor's outputs, from wlshare's `OutputList`. Empty
/// on every other server, and until the first list arrives.
#[derive(Debug, Default)]
struct DisplayState {
    displays: Vec<DisplayInfo>,
    /// The last physical-display layout a Standard Apple session returned.
    /// Kept so a browser density report or display selection can derive the
    /// connection-wide `SetServerScaling` factor from the chosen screen's native
    /// density. High Performance manages density in its virtual-display mode and
    /// leaves this empty.
    apple_layout: Option<vnc_apple::Layout>,
    /// A Standard Apple server scale sent but not yet confirmed by a layout.
    /// Duplicate layouts are common around login and lock; they must not turn one
    /// density change into an unbounded stream of identical requests. Layouts
    /// that arrive before the confirming one answer messages sent earlier — a
    /// selection's, or an older factor's — and are not reconciled against.
    ///
    /// With when it was sent: a request the Mac never answers — one it ignores
    /// at the login window, say — must not stand in for the factor in force
    /// forever. Past [`APPLE_SCALE_ANSWER`] the last layout's factor counts again.
    apple_scale_pending: Option<(f32, std::time::Instant)>,
    /// The composition last sent, `None` while a framebuffer is presented whole.
    /// Kept to send only a change, and to replay it to a browser that attaches.
    mosaic: Option<Vec<crate::protocol::MosaicRegion>>,
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
    /// Whether the server has listed its screens at all. A list that empties is
    /// still a list, and one a browser has to be told: see
    /// [`DisplayState::displays_msg`].
    listed: bool,
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
    /// the server has listed nothing — a generic server without the outputs
    /// extension, or one that has not answered yet. A list that emptied is sent
    /// empty: wlshare lists nothing once the compositor has no output left, and a
    /// browser told nothing would keep offering the outputs it last saw.
    fn displays_msg(&self) -> Option<ServerMsg> {
        self.listed.then(|| ServerMsg::Displays {
            active: self.active,
            displays: self.displays.clone(),
        })
    }

    /// Ask Standard Screen Sharing for the scale appropriate to `selection`, or
    /// return nothing when that is already the factor in force. `None` is All
    /// Displays; `Some` is a physical display id.
    ///
    /// A request still in flight is the factor in force: the Mac applies
    /// messages in order, so a selection made before it answers is compared
    /// with what it will answer at, not with the layout it is replacing.
    fn request_apple_scale(&mut self, selection: Option<u32>, host_density: f32) -> Option<f32> {
        let pending = self.pending_scale();
        let layout = self.apple_layout.as_ref()?;
        let want = layout.server_scale_for(selection, host_density);
        let in_force = pending.unwrap_or_else(|| layout.viewer_scale());
        if (in_force - want).abs() < 0.005 {
            return None;
        }
        self.apple_scale_pending = Some((want, std::time::Instant::now()));
        Some(want)
    }

    /// A browser pointer position, in the framebuffer it is looking at, as
    /// Standard Screen Sharing reads it: in the display's native pixels.
    ///
    /// `SetServerScaling` shrinks only what the Mac sends. Its pointer events
    /// still address the unscaled framebuffer — measured on macOS 26 at 0.5, the
    /// centre of a 1440x900 Retina screen's scaled 1440x900 framebuffer sent as
    /// is landed a quarter of the way in — so a position divides by the factor
    /// of the layout the browser was last resized to. High Performance keeps no
    /// Standard layout and passes through unchanged.
    fn apple_pointer(&self, x: i32, y: i32) -> (i32, i32) {
        let Some(scale) = self.apple_layout.as_ref().map(vnc_apple::Layout::viewer_scale) else {
            return (x, y);
        };
        let native = |v: i32| (f64::from(v) / f64::from(scale)).round() as i32;
        (native(x), native(y))
    }

    /// The factor sent and still awaiting its layout, unless it has waited past
    /// [`APPLE_SCALE_ANSWER`].
    fn pending_scale(&mut self) -> Option<f32> {
        match self.apple_scale_pending {
            Some((scale, sent)) if sent.elapsed() < APPLE_SCALE_ANSWER => Some(scale),
            _ => {
                self.apple_scale_pending = None;
                None
            }
        }
    }

    /// Record Apple's answer and decide whether its returned scale needs one new
    /// request for the browser display the session is currently on.
    ///
    /// Only a layout at the pending factor has caught up with everything sent.
    /// One before it — the answer to a selection sent just ahead of the factor,
    /// say — still shows the old scale, and asking again from it would undo the
    /// request already on its way.
    fn accept_apple_layout(
        &mut self,
        layout: &vnc_apple::Layout,
        host_density: f32,
    ) -> Option<f32> {
        self.apple_layout = Some(layout.clone());
        if let Some(pending) = self.pending_scale() {
            if (pending - layout.viewer_scale()).abs() >= 0.005 {
                return None;
            }
            self.apple_scale_pending = None;
        }
        self.request_apple_scale(layout.current, host_density)
    }
}

/// How long a `SetServerScaling` is taken to be on its way. `screensharingd`
/// answers one alone in the same millisecond (its log's `set scaling to` and
/// `encode display info2` lines), but one queued behind a display switch waits
/// for the switch: 3 s measured, switching to a virtual display just created.
/// One unanswered this long was ignored, and a request repeated early is only
/// a duplicate.
const APPLE_SCALE_ANSWER: std::time::Duration = std::time::Duration::from_secs(10);

type SharedDisplay = Arc<std::sync::Mutex<DisplayState>>;

/// Apple's display/cursor decoding state, owned by the read loop.
///
/// Not in [`Shared`]: the cursor cache is touched by nothing else, and a lock on
/// the pixel path to say so would be a lock that never contends. The ZRLE stream
/// is not here either: it is [`vnc_encodings::Decoders`]', as for any server.
#[derive(Default)]
struct Apple {
    cursors: CursorCache,
    /// True for High Performance mode, whose setup requested a virtual display.
    /// Layout records do not carry this fact themselves.
    virtual_display: bool,
    /// The media stream's decoded pictures, on `ard-high-performance`.
    pictures: Option<Pictures>,
}

impl Apple {
    /// The read loop's starting state for either Apple subtype.
    fn new(high_performance: bool, pictures: Option<Pictures>) -> Self {
        Self {
            virtual_display: high_performance,
            pictures,
            ..Self::default()
        }
    }
}

/// High Performance's media stream, shared by the engine's two loops, which both
/// offer it. Locked after [`SharedDesktop`] where both are held.
type SharedMedia = Arc<std::sync::Mutex<MediaStream>>;

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
    /// The id Apple's pasteboard messages carry. The Mac echoes a fetch's id in
    /// its reply and ignores the one on a send, so this stays at the zero the
    /// first fetch uses; Apple's viewer treats a zero reply as an unrequested one.
    apple_session_id: u32,
    /// Browser reads waiting for the next native Apple pasteboard response.
    /// The panel issues only one at a time, but count them so the wire remains
    /// correct if another client does not make that UI guarantee.
    apple_requests: usize,
    /// A native Apple pasteboard fetch has been sent and has not answered yet.
    /// While this is set, the read loop leaves a gap in the framebuffer polling
    /// cycle so the reply cannot remain queued forever behind pixel updates.
    apple_fetch_pending: bool,
    /// Another remote change arrived while the current native fetch was in
    /// flight. One reply cannot prove it includes that later change, so it earns
    /// exactly one follow-up fetch.
    apple_fetch_again: bool,
}

impl ClipboardState {
    /// Start one Apple pasteboard fetch, or coalesce with the one already in
    /// flight. A server change observed after that fetch began is remembered so
    /// its reply earns one final refresh.
    fn begin_apple_fetch(&mut self, remote_changed: bool) -> Option<u32> {
        if self.apple_fetch_pending {
            self.apple_fetch_again |= remote_changed;
            return None;
        }
        self.apple_fetch_pending = true;
        Some(self.apple_session_id)
    }

    /// Complete one Apple pasteboard fetch. Browser reads were already answered
    /// from the cache, so one native reply refreshes every read coalesced behind
    /// it. A remote change that raced the fetch starts one more fetch with the
    /// newly learned session id.
    fn finish_apple_fetch(&mut self) -> (bool, Option<u32>) {
        let requested = self.apple_requests > 0;
        self.apple_requests = 0;
        if std::mem::take(&mut self.apple_fetch_again) {
            (requested, Some(self.apple_session_id))
        } else {
            self.apple_fetch_pending = false;
            (requested, None)
        }
    }
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
// One argument per thing the session hands an engine, as `rdp::run` takes them; a
// parameter struct would exist only to be destructured here.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: TargetConfig,
    plan: RenderPlan,
    display: Option<HostDisplay>,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
    audio: Option<Arc<crate::audio::AudioBridge>>,
    camera: Option<Arc<crate::camera::CameraBridge>>,
    microphone: Option<Arc<crate::mic::MicBridge>>,
    feedback: Arc<crate::feedback::LinkFeedback>,
) {
    // An RFB update is rectangles, so a desktop past the video ceiling can go as
    // those — on a session the window does not size. With `resize` the gateway asks
    // for every size and holds each under the ceiling, and a remote that answers
    // past it is refused. Never High Performance's, whose picture is the media
    // stream's, decoded or passed, with ZRLE's encoded here in its gaps.
    let tiles = if config.resize || config.subtype == Some(Subtype::ArdHighPerformance) {
        TileSupport::None
    } else {
        TileSupport::Rects
    };
    // A browser whose decoder takes 4:4:4 is sent wlshare's own VP9 as it comes, when
    // the server is wlshare; every other browser is sent the stream encoded here. A
    // browser that takes the Mac's HEVC is sent that, on a target that passes it.
    let passing = Passing { wlshare_vp9: plan.chroma == Chroma::Full, apple_hevc: plan.apple_hevc };
    let sink = VideoSink::new("vnc", frame_tx, plan, feedback, tiles);
    session(config, display, passing, input_rx, audio, camera, microphone, &sink).await;
    sink.finish().await;
}

/// The streams a remote codes itself that this session passes to the browser as they
/// come, from the resolved plan.
#[derive(Clone, Copy)]
struct Passing {
    /// wlshare's VP9 encoding, listed for a browser that takes 4:4:4.
    wlshare_vp9: bool,
    /// A High Performance Mac's HEVC ([`crate::config::RenderPlan::apple_hevc`]).
    apple_hevc: bool,
}

#[allow(clippy::too_many_arguments)]
async fn session(
    config: TargetConfig,
    display: Option<HostDisplay>,
    passing: Passing,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    audio: Option<Arc<crate::audio::AudioBridge>>,
    camera: Option<Arc<crate::camera::CameraBridge>>,
    microphone: Option<Arc<crate::mic::MicBridge>>,
    sink: &VideoSink,
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
        |stream| connect(&config, display, passing, stream),
    )
    .await
    else {
        return;
    };

    let Connected { downlink, uplink, width, height, macos, apple, poll, media, passthrough } = connected;
    info!("vnc: connected, desktop {width}x{height} px (macos={macos})");
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

    let high_performance = config.subtype == Some(Subtype::ArdHighPerformance);
    // A generic server is asked for wlshare's audio extension on the connection
    // itself ([`vnc_audio`]). High Performance's media stream carries the Mac's
    // sound beside its picture ([`vnc_apple_media`]). Standard mode never touches
    // the Mac's sound.
    let (media, wlshare_audio) = match media {
        Some((stream, pictures)) => (Some((stream.with_sound(audio), pictures)), None),
        None => (None, audio.filter(|_| !apple)),
    };
    if let Err(e) = active_loop(
        downlink,
        uplink,
        (width, height),
        Flags {
            macos,
            resize: config.resize,
            clipboard: config.clipboard,
            default_size: config.default_size(),
            pinned: (!apple).then(|| config.pinned_size()).flatten(),
            apple,
            high_performance,
            wlshare_audio,
            camera,
            microphone,
            host_density: display.map_or(UNSCALED, |d| crate::protocol::render_density(d.scale)),
            poll,
            media,
            passthrough,
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
    /// What [`ClientMsg::DefaultSize`] resolves to here —
    /// [`TargetConfig::default_size`], the pinned size or the built-in default.
    /// Carried rather than read from the config at the point of use because
    /// [`active_loop`] is given the handshaken link and these switches, not the
    /// profile behind them.
    default_size: (u16, u16),
    /// The operator's pinned size ([`TargetConfig::pinned_size`]), in points,
    /// on a generic target. The desktop is asked for it once, as soon as the
    /// server declares SetDesktopSize support — a pin is the operator's opening
    /// size and not the window's, so it is offered whether or not `resize` is
    /// granted.
    ///
    /// Under `resize` it commonly never goes out, and that is the intended
    /// outcome rather than a race lost: the browser reports its window the
    /// moment `connected` reaches it, which is well before the handshake can
    /// declare support, and a held request is superseded by the newest size the
    /// window wants — see [`DesktopState::generic_resize`]. Asking for the pin
    /// first would redraw the whole desktop for a size the browser had already
    /// left. A client that reports no window at all (a phone) leaves the pin to
    /// go out on the declaration.
    ///
    /// `None` on both Apple subtypes, where a pin is either spent by
    /// [`opening_mode`] at connect (High Performance) or refused by the config
    /// file (Standard `ard` exposes physical displays).
    pinned: Option<(u16, u16)>,
    /// Whether this is Apple's revision, 003.889, with the metadata encodings both
    /// Apple subtypes negotiate: the read loop's ZRLE stream, cursor cache and
    /// display list, and the Mac's reading of the pointer mask.
    apple: bool,
    /// Whether this is Apple's High Performance mode. It requests a virtual display
    /// during setup; plain `ard` does not.
    high_performance: bool,
    /// The desktop's sound over wlshare's audio extension, when a generic target
    /// asked for it: the queue the read loop feeds the samples a server that
    /// announces the extension then sends ([`vnc_audio`]). `None` on every
    /// Apple target and wherever `audio` was not asked for.
    wlshare_audio: Option<Arc<crate::audio::AudioBridge>>,
    /// The browser's camera, on a generic target that carries one: the bridge the
    /// camera socket drives, lent to a server that announces the wlshare camera
    /// extension ([`vnc_camera`]). `None` on every Apple target, which the config
    /// file refuses `camera` on, and wherever the key is absent.
    camera: Option<Arc<crate::camera::CameraBridge>>,
    /// The browser's microphone, on the same terms: the bridge the mic socket drives,
    /// lent to a server that announces the wlshare microphone extension
    /// ([`vnc_mic`]). `None` on every Apple target and wherever the key is absent.
    microphone: Option<Arc<crate::mic::MicBridge>>,
    /// The browser display's density at session-open. High Performance uses it
    /// for the opening virtual-display mode; Standard uses it when its first
    /// physical-display layout chooses Apple's server-side scale. Seeding
    /// [`DesktopState::host_density`] also keeps the client's first
    /// `hostDisplay` — an echo of the same screen — from reading as a change.
    host_density: f32,
    /// Whether the client drives the update cycle — see [`Connected::poll`].
    poll: bool,
    /// High Performance's media stream — see [`Connected::media`].
    media: Option<(MediaStream, Pictures)>,
    /// See [`Connected::passthrough`].
    passthrough: Option<Arc<[i32]>>,
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
    /// True on both Apple subtypes. A pending pasteboard fetch pauses the next
    /// request so it cannot be buried behind another framebuffer response.
    poll: bool,
    /// High Performance's media stream, which the picture comes from once it is
    /// up ([`vnc_apple_media`]), and the pictures it decodes: every High
    /// Performance session has one, and every other session `None`.
    media: Option<(MediaStream, Pictures)>,
    /// The encodings listed beside [`ENCODING_WLSHARE_VP9`] when the preface listed it,
    /// for a browser that decodes 4:4:4 on a generic server: what the read loop lists
    /// on its own while the desktop is past the video ceiling, and with it again once
    /// it is back. `None` where it was not listed.
    passthrough: Option<Arc<[i32]>>,
}

/// ServerInit, as much of it as anything here uses.
struct ServerInit {
    width: u16,
    height: u16,
    /// The client messages a Mac's enhanced ServerInit says it accepts, or `None`
    /// from any other server. See [`apple_commands`].
    apple_commands: Option<[u8; 16]>,
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
/// that point is common to both dialects.
///
/// The TCP connect happens in [`run`] (see [`engine::connect_and_handshake`]) so
/// its deadline and this handshake's are sequential rather than nested. The
/// 003.889 preface waits for the server's rekey, which puts *that* wait inside the
/// same budget — a Mac that authenticates and then says nothing is reported as a
/// handshake that ran long, not as a live session with a blank canvas.
async fn connect(
    config: &TargetConfig,
    display: Option<HostDisplay>,
    passing: Passing,
    stream: tokio::net::TcpStream,
) -> anyhow::Result<Connected> {
    let dialect = Dialect::of(config.subtype);
    // Where the connection runs, for High Performance's media stream: the Mac
    // sends it from its own address to this side's, on UDP.
    let addresses = (stream.peer_addr()?, stream.local_addr()?);
    let (read_half, mut sock) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let minor = read_version(&mut reader).await?;
    sock.write_all(dialect.banner()).await?;

    let types = read_security_types(&mut reader).await?;
    let macos = is_macos_server(minor, &types);
    let chosen = choose_security(&types, config.subtype, &config.password, &config.vnc_password)?;
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

    let secured = authenticate(&mut reader, &mut sock, config, chosen).await?;

    match dialect {
        Dialect::Rfb38 => {
            // SecurityResult is the first thing an RSA-AES server says inside its
            // frames, so the transport goes up before it is read.
            let (mut downlink, mut uplink) = match secured {
                Secured::Frames(session) => (
                    Downlink::Frames(Box::new(FrameReader::new(reader, session.opener))),
                    Uplink::frames(sock, session.sealer),
                ),
                Secured::Plain | Secured::Apple(_) => {
                    (Downlink::Plain(reader), Uplink::plain(sock))
                }
            };
            read_security_result(&mut downlink).await?;
            uplink.send(&[dialect.client_init()]).await?;
            let server = read_server_init(&mut downlink).await?;
            rfb38_preface(downlink, uplink, server, macos, config, passing.wlshare_vp9).await
        }
        Dialect::Apple889 => {
            let Secured::Apple(wrap_key) = secured else {
                anyhow::bail!(
                    "Apple's protocol revision needs its DH authentication, which this server \
                     did not offer"
                );
            };
            read_security_result(&mut reader).await?;
            sock.write_all(&[dialect.client_init()]).await?;
            let server = read_server_init(&mut reader).await?;
            let pass_hevc = passing.apple_hevc;
            apple_preface(reader, sock, server, macos, wrap_key, config, display, addresses, pass_hevc).await
        }
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

/// What a security type's exchange leaves behind for the session.
enum Secured {
    /// Nothing: the wire stays as it was.
    Plain,
    /// The record layer's initial wrap key, which Apple's DH branch produces for
    /// the 003.889 dialect: `MD5(shared)`, the very digest that encrypted the
    /// credentials.
    Apple([u8; 16]),
    /// RSA-AES's two ciphers: every byte from here on, SecurityResult included,
    /// rides inside their frames.
    Frames(vnc_rsa_aes::Session),
}

/// Run the chosen security type's exchange.
async fn authenticate<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    sock: &mut W,
    config: &TargetConfig,
    chosen: u8,
) -> anyhow::Result<Secured> {
    match chosen {
        SECURITY_ARD => Ok(Secured::Apple(
            ard_authenticate(reader, sock, &config.username, &config.password).await?,
        )),
        SECURITY_VNC_AUTH => {
            let mut challenge = [0u8; 16];
            reader.read_exact(&mut challenge).await?;
            sock.write_all(&auth_response(&config.vnc_password, &challenge))
                .await?;
            Ok(Secured::Plain)
        }
        rsa_aes if Strength::of(rsa_aes).is_some() => {
            let strength = Strength::of(rsa_aes).expect("guarded");
            let session =
                vnc_rsa_aes::authenticate(reader, sock, strength, &config.username, &config.password)
                    .await?;
            Ok(Secured::Frames(session))
        }
        _ => Ok(Secured::Plain),
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
    Ok(ServerInit { width, height, apple_commands: apple_commands(&name) })
}

/// Describe ServerInit's name field, which on Apple's revision is not a name.
///
/// A Mac prefixes it with 22 bytes: a `u16` zero, a `u32` of session flags, and a
/// 16-byte capability bitmap, with the UTF-8 name after all of it. Printing the lot
/// as a string gave a log line of mojibake with the real name buried in it, and
/// hid the flags. Bits 5 and up are the most virtual displays the Mac will create.
/// See docs/apple-vnc-889.md, "ServerInit's name field is not a name".
///
/// Anything that is not shaped like that is a name, which is what every other
/// server sends.
fn describe_desktop(field: &[u8]) -> String {
    if !is_enhanced_desktop(field) {
        return format!("{:?}", String::from_utf8_lossy(field));
    }
    let flags = u32::from_be_bytes(field[2..6].try_into().expect("four bytes of flags"));
    let name = String::from_utf8_lossy(&field[22..]);
    let named: Vec<&str> = [
        (0x01, "observe-only"),
        (0x02, "may-control"),
        (0x04, "session-select"),
        (0x08, "no-screen-capture"),
    ]
    .into_iter()
    .filter(|(bit, _)| flags & bit != 0)
    .map(|(_, name)| name)
    .collect();
    format!(
        "{name:?} (Apple flags {flags:#010x}: {}, up to {} virtual displays)",
        named.join(", "),
        flags >> 5
    )
}

/// Whether a ServerInit name field has the 22 bytes of structure a Mac's enhanced
/// ServerInit puts before the name. See [`describe_desktop`].
fn is_enhanced_desktop(field: &[u8]) -> bool {
    field.len() >= 22 && field[0] == 0
}

/// The capability bitmap of a Mac's enhanced ServerInit: the client messages it
/// accepts, one bit each ([`vnc_apple::holds_high_performance`] reads it).
fn apple_commands(field: &[u8]) -> Option<[u8; 16]> {
    is_enhanced_desktop(field).then(|| field[6..22].try_into().expect("sixteen bytes"))
}

/// Name an encoding in the log the way the documentation names it. Apple's own
/// encodings are written in hex there (`0x451`) while the wire and RFB's registry
/// count in decimal, so a positive number is given both ways; a pseudo-encoding is
/// negative, is only ever written in decimal, and would read as two's complement in
/// hex.
fn encoding_label(encoding: i32) -> String {
    if encoding > 0 { format!("{encoding} ({encoding:#x})") } else { encoding.to_string() }
}

/// The RFB 3.8 tail: force our pixel format and the encoding set.
async fn rfb38_preface(
    downlink: Downlink,
    mut uplink: Uplink,
    server: ServerInit,
    macos: bool,
    config: &TargetConfig,
    pass_444: bool,
) -> anyhow::Result<Connected> {
    uplink.send(&set_pixel_format()).await?;
    let encodings = rfb38_encoding_list(config.clipboard, config.audio, config.camera, config.microphone);
    let listed = if pass_444 && lists_wlshare_vp9((server.width, server.height)) {
        with_wlshare_vp9(&encodings)
    } else {
        encodings.clone()
    };
    uplink.send(&set_encodings(&listed)).await?;

    Ok(Connected {
        downlink,
        uplink,
        width: server.width,
        height: server.height,
        macos,
        apple: false,
        poll: true,
        media: None,
        passthrough: pass_444.then(|| encodings.into()),
    })
}

/// Whether a session that may be sent wlshare's VP9 lists it for a desktop of this
/// size: only within the video ceiling, since the stream is a picture of the whole
/// desktop and one past the ceiling is not video. Past it the desktop comes as ZRLE,
/// for tiles or for the ceiling's refusal, and the read loop lists the encoding again
/// when a resize brings the desktop back within.
fn lists_wlshare_vp9((w, h): (u16, u16)) -> bool {
    crate::video::within_ceiling((u32::from(w), u32::from(h)))
}

/// `encodings` with wlshare's VP9 encoding ahead of them, where a list read as a
/// preference puts what it would rather have. wlshare takes it wherever it is.
fn with_wlshare_vp9(encodings: &[i32]) -> Vec<i32> {
    std::iter::once(ENCODING_WLSHARE_VP9).chain(encodings.iter().copied()).collect()
}

fn rfb38_encoding_list(clipboard: bool, audio: bool, camera: bool, microphone: bool) -> Vec<i32> {
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
    // Cursor is unconditional (the browser can always draw a pointer), and so is
    // Cursor With Alpha, which only improves on it, and so are the two size
    // pseudo-encodings. They are how a server *tells* this end its
    // framebuffer changed size, which is not the same as being asked to change it:
    // that is asked for where a SetDesktopSize is decided, by the window under
    // `resize` and once at session-open under a pinned size. A server whose size
    // changes under a client that listed neither has no way to say so and hangs
    // up, which is what a wlshare output switch to a differently sized monitor
    // would do.
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
        ENCODING_CURSOR_WITH_ALPHA,
        ENCODING_CONTINUOUS_UPDATES,
        ENCODING_FENCE,
        ENCODING_EXTENDED_DESKTOP_SIZE,
        ENCODING_DESKTOP_SIZE,
    ];
    if clipboard {
        // Extended Clipboard is the only way generic RFB carries anything outside
        // latin-1. A server that ignores it never sends caps and the fallback stays
        // in use.
        encodings.push(vnc_clipboard::ENCODING);
    }
    if audio {
        // wlshare's audio extension, on a target that asked for sound. Discovery
        // again, and by the same shape as the density request: a server that
        // speaks it announces so with a rectangle of this encoding, and one that
        // does not says nothing and the session runs in silence. See
        // [`crate::vnc_audio`].
        encodings.push(vnc_audio::ENCODING);
    }
    if camera {
        // The wlshare camera extension, on a target that carries a camera, asked
        // the way audio is: wlshare answers that it takes one, and any other
        // server says nothing and the browser's camera is never plugged. See
        // [`crate::vnc_camera`].
        encodings.push(vnc_camera::ENCODING);
    }
    if microphone {
        // And the wlshare microphone extension, on a target that carries a
        // microphone, the same way. See [`crate::vnc_mic`].
        encodings.push(vnc_mic::ENCODING);
    }
    // The density request, asked of every generic server and last so it never
    // weighs on encoding preference. Its answer, when it comes, is the scale every
    // framebuffer from then on is labelled with; a Mac reports its densities in
    // its display layout and is not asked.
    encodings.push(ENCODING_WLSHARE_DENSITY);
    // The output list, on the same terms and for the same reason: a Mac sends its
    // screens in that layout, and this is the one way a generic server says it has
    // more than one to offer.
    encodings.push(ENCODING_WLSHARE_OUTPUTS);
    encodings
}

/// The virtual display a High Performance session opens with.
///
/// The points come from [`TargetConfig::opening_size`] — the pinned config
/// size, else the full resolution of the client's own screen, which is how
/// Apple's client opens: at the display it is on, never at a smaller target
/// size. The opening size matters more than any later one, because macOS lays
/// every remote window out on the virtual display at that size and windows
/// squeezed together onto a small opening display do not spread back out when
/// it grows. The density is the client screen's whatever named the points, so
/// a Retina client gets a sharp desktop even at a pinned size — quantized to
/// the 1x or 2x a virtual display can be backed at
/// ([`crate::protocol::render_density`]): a Mac asked for 1.25x or 1.5x
/// answers with a small 2x display rather than a rounded one.
fn opening_mode(config: &TargetConfig, display: Option<HostDisplay>) -> vnc_apple::VirtualMode {
    vnc_apple::virtual_display_mode(
        config.opening_size(display),
        display.map_or(UNSCALED, |d| crate::protocol::render_density(d.scale)),
    )
}

/// The RFB 003.889 tail, in either of Apple's modes: the cleartext prelude, the
/// wait for the rekey, then the encrypted preface of the mode the subtype names —
/// a virtual display and the arming sent beside the measured polling cycle for
/// High Performance, the Mac's own displays for Standard.
///
/// Both modes encrypt. Apple's viewer asks for the record layer only when its
/// `encryptionLevel` preference is 2 and leaves the session in cleartext by
/// default; remotex always asks, so neither the account's keystrokes nor the
/// media stream's keys cross the network in the clear.
///
/// The one function in this file that knows the record layer is switched on here,
/// which is deliberate: it runs before [`Connected`] exists, so there is no input
/// task and no second holder of the writer. The alternative — noticing the rekey
/// inside [`read_loop`] — has a race with no fix, because the server rotates
/// *both* its own ciphers the instant it sends the rekey: a mouse move delivered
/// in the window before this side catches up goes out in cleartext to a server
/// that is already decrypting, and the session is unrecoverable. Doing it here
/// makes that structurally impossible rather than unlikely.
#[allow(clippy::too_many_arguments)]
async fn apple_preface(
    mut reader: Reader,
    mut sock: OwnedWriteHalf,
    server: ServerInit,
    macos: bool,
    wrap_key: [u8; 16],
    config: &TargetConfig,
    display: Option<HostDisplay>,
    (peer, local): (std::net::SocketAddr, std::net::SocketAddr),
    pass_hevc: bool,
) -> anyhow::Result<Connected> {
    let high_performance = config.subtype == Some(Subtype::ArdHighPerformance);
    // Apple's viewer checks this before it sends a byte of the session, and turns a
    // Mac without it into a Standard session after asking. With no one to ask, it is
    // refused here rather than run on the physical display over ZRLE, a combination
    // Apple's viewer never makes.
    anyhow::ensure!(
        !high_performance
            || server.apple_commands.as_ref().is_some_and(vnc_apple::holds_high_performance),
        "this Mac does not offer High Performance Screen Sharing: its ServerInit does not \
         list SetDisplayConfiguration, without which there is no virtual display. Apple's \
         viewer connects it in Standard mode; use subtype = \"ard\""
    );
    // The native control prelude, written back to back before encryption. The
    // server emits the rekey as soon as encryption starts, so anything that waited
    // for a reply in between would risk writing cleartext to a server that had
    // already switched. ViewerInfo's body is the measured fixed numeric form, not
    // the mis-sized string form in the reverse-engineered reference; Apple's viewer
    // sends it and SetMode to every Mac.
    //
    // Both are also required for automatic pasteboard notifications, in either
    // mode. The AutoPasteboard enable itself must be cleartext: sending it as the
    // first encrypted record is accepted without error but produces no status or
    // data.
    sock.write_all(&vnc_apple::viewer_info()).await?;
    sock.write_all(&vnc_apple::set_mode_control()).await?;
    if config.clipboard {
        sock.write_all(&vnc_apple_clipboard::auto_pasteboard(true)).await?;
    }
    sock.write_all(&vnc_apple::set_encryption_start()).await?;
    sock.write_all(&vnc_apple::enable_inbound_record_decryption()).await?;

    let keys = await_rekey(&mut reader, &wrap_key).await?;
    info!("vnc: Apple record layer active");

    let mut uplink = Uplink::records(sock, keys);
    if high_performance {
        // High Performance mode is a virtual-display session. Request its mode
        // before the pixel format and encoding list. The same message is resent for
        // later viewport reports and screen changes; its dynamic-resolution flag is
        // set here regardless, so every fresh session restores the Mac's checkbox
        // to on.
        uplink
            .send(&vnc_apple::set_display_configuration(opening_mode(config, display)))
            .await?;
    }
    uplink.send(&set_pixel_format()).await?;
    // The same list in both modes: the display layout that names the Mac's screens,
    // or the one virtual display, and ZRLE for their pixels.
    uplink.send(&set_encodings(vnc_apple::ENCODINGS)).await?;
    if high_performance {
        // Arm the server's sender. Cursor shapes above all depend on it across a
        // login or lock, which is why the full region is re-sent on every layout
        // too, and which is when Standard first arms it.
        uplink
            .send(&vnc_apple::auto_framebuffer_update(server.size()))
            .await?;
    }

    Ok(Connected {
        downlink: Downlink::Records(Box::new(RecordReader::new(reader, keys))),
        uplink,
        width: server.width,
        height: server.height,
        macos,
        apple: true,
        poll: true,
        media: high_performance.then(|| MediaStream::new(peer, local, pass_hevc)),
        passthrough: None,
    })
}

/// Read cleartext server messages until the rekey arrives, and return the key and
/// IV it carried.
///
/// Nothing the client *asked* for can precede it: no pixel format, no encodings and
/// no update request have been sent, so the server has nothing to answer. What the
/// Mac does send unbidden in this window is Bell and `MiscStatus` (`0x14`), the
/// pasteboard status after a server restart, and those are stepped over by their
/// own framing, which is the only way to stay in step with the bytes after them.
///
/// Anything outside that list is named in the error rather than skipped. The
/// metadata burst that follows a rekey is already inside the record layer, so a
/// rectangle here is not a burst arriving early, it is a stream that has gone
/// somewhere unexpected.
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
                    "the server sent encoding {} before the record layer was up",
                    encoding_label(encoding)
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
            // Apple's MiscStatus (0x14): the Mac can send a pasteboard status
            // notification in the cleartext window after AutoPasteboard(start)
            // but before the rekey arrives. This happens after a server restart
            // when the Mac has stale clipboard state from the previous session.
            // Read and discard the body (u8 padding + u16 len + body).
            0x14 => {
                reader.read_u8().await?; // padding
                let len = reader.read_u16().await?;
                discard(reader, u64::from(len)).await?;
                debug!("vnc: skipped a MiscStatus before the record layer");
            }
            other => anyhow::bail!(
                "the server sent message type {other:#04x} before the record layer was up"
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
    sink: VideoSink,
) -> anyhow::Result<()> {
    let Flags {
        macos,
        resize,
        clipboard: clipboard_enabled,
        default_size,
        pinned,
        apple,
        high_performance,
        wlshare_audio,
        camera,
        microphone,
        host_density,
        poll,
        media,
        passthrough,
    } = flags;
    let (media, pictures) = match media {
        Some((media, pictures)) => (Some(Arc::new(std::sync::Mutex::new(media))), Some(pictures)),
        None => (None, None),
    };
    // The uplink is shared: the read loop answers the server (update requests,
    // re-arming), the input side sends pointer/key/display messages. Neither
    // writes to the socket itself from here on — see [`Uplink::queued`].
    let (uplink, backlog, writer) = uplink.queued();
    let mut write_task = tokio::spawn(writer);
    let uplink: SharedUplink = Arc::new(Mutex::new(uplink));
    let hp = if high_performance && resize { HpResize::opening() } else { HpResize::default() };
    let desktop: SharedDesktop = Arc::new(std::sync::Mutex::new(DesktopState {
        size,
        scale: UNSCALED,
        host_density,
        screen: None,
        // A pinned size is seeded as a held request: nothing can be asked for
        // before the server declares SetDesktopSize support, and the hold is
        // already replayed on that declaration — see [`Flags::pinned`].
        pending: pinned,
        viewport: None,
        density: if apple { Density::Off } else { Density::Asked },
        wire_scale: None,
        resize,
        following: false,
        declared: None,
        repaint_owed: false,
        hp,
        laid_out: false,
        media_live: false,
    }));
    let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
    let clipboard: SharedClipboard = Arc::new(std::sync::Mutex::new(ClipboardState::default()));
    let shadow: SharedShadow = Arc::new(std::sync::Mutex::new({
        Shadow::new("vnc", size.0, size.1)
    }));
    let display: SharedDisplay = Arc::new(std::sync::Mutex::new(DisplayState::default()));
    let hp_wake = Arc::new(tokio::sync::Notify::new());
    // The camera socket's traffic comes to this loop through the queues, and the
    // server's decisions go to the bridge from the read loop: both share the link.
    let (camera, mut camera_queues) = match camera {
        Some(bridge) => {
            let (link, queues) = vnc_camera::attach(&bridge);
            (Some(Arc::new(link)), Some(queues))
        }
        None => (None, None),
    };
    // The mic socket's the same way.
    let (microphone, mut microphone_queues) = match microphone {
        Some(bridge) => {
            let (link, queues) = vnc_mic::attach(&bridge);
            (Some(Arc::new(link)), Some(queues))
        }
        None => (None, None),
    };
    let shared = Shared {
        uplink: Arc::clone(&uplink),
        desktop: Arc::clone(&desktop),
        cursor: Arc::clone(&cursor),
        clipboard: Arc::clone(&clipboard),
        shadow: Arc::clone(&shadow),
        display: Arc::clone(&display),
        hp_wake: Arc::clone(&hp_wake),
        audio: wlshare_audio,
        camera: camera.clone(),
        microphone: microphone.clone(),
        media: media.clone(),
        passthrough,
    };

    // A resizing High Performance session opens covered — see [`HpResize::opening`].
    if desktop.lock().unwrap().hp.shown {
        sink.msg(ServerMsg::Resizing { active: true }).await?;
    }

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
        apple.then(|| Apple::new(high_performance, pictures)),
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
    let buttons = Buttons::new(apple);
    let mut held = HeldMotion::default();

    let result = loop {
        tokio::select! {
            res = &mut read_task => {
                write_task.abort();
                return res.map_err(|e| anyhow::anyhow!("read task failed: {e}"))?;
            }
            // Only ever an error: the queue it drains outlives this loop.
            res = &mut write_task => {
                break res.map_err(|e| anyhow::anyhow!("write task failed: {e}")).and_then(|r| r);
            }
            // The camera socket's plug, unplug and samples, written in the order the
            // browser sent them; what reaches the wire is the device's decision.
            input = camera_input(&mut camera_queues, &backlog) => {
                if let Some(link) = &camera {
                    let sent = send_camera_decided(&uplink, link, |device| match input {
                        vnc_camera::Input::Plug(format) => device.plug(format),
                        vnc_camera::Input::Unplug => device.unplug(),
                        vnc_camera::Input::Sample { unit, keyframe } => device.sample(&unit, keyframe),
                    })
                    .await;
                    if let Err(e) = sent {
                        break Err(e);
                    }
                }
            }
            // The mic socket's plug, unplug and PCM, the same way.
            input = microphone_input(&mut microphone_queues, &backlog) => {
                if let Some(link) = &microphone {
                    let sent = send_microphone_decided(&uplink, link, |device| match input {
                        vnc_mic::Input::Plug => device.plug(),
                        vnc_mic::Input::Unplug => device.unplug(),
                        vnc_mic::Input::Sample(pcm) => vnc_mic::Decision { message: device.sample(&pcm), closed: false },
                    })
                    .await;
                    if let Err(e) = sent {
                        break Err(e);
                    }
                }
            }
            // A High Performance resize has something due — see [`HpResize`].
            () = hp_resize_due(&desktop, &hp_wake), if high_performance && resize => {
                if let Err(e) = hp_resize_step(&uplink, &desktop, media.as_ref(), &sink).await {
                    break Err(e);
                }
            }
            // The writer has caught up: what was held goes out, as it now stands.
            () = backlog.room(Backlog::MOTION_LIMIT), if !held.is_empty() => {
                let msgs = held_messages(
                    &mut held,
                    &buttons,
                    &mut button_mask,
                    &mut last_pos,
                    &mut pressed_keys,
                    &mut wheel,
                    macos,
                );
                if let Err(e) = send_all(&uplink, &msgs).await {
                    break Err(e);
                }
            }
            input = input_rx.recv() => {
                let Some(input) = input else {
                    // The session layer's last word is the releases for what the
                    // browser left held, queued just ahead of the close, and
                    // aborting the writer below would drop whatever of them it has
                    // not written. Bounded, because a server that has stopped
                    // reading must not hold up the session that replaces this one.
                    if tokio::time::timeout(SHUTDOWN_DRAIN, backlog.room(1)).await.is_err() {
                        warn!("vnc: the server did not take the last input before shutdown");
                    }
                    info!("vnc: input channel closed; session shut down");
                    break Ok(());
                };
                // Into the Mac's pointer space before anything is held, while
                // the framebuffer the position was taken on is still the one
                // the layout names — see [`DisplayState::apple_pointer`].
                let input = match input {
                    ClientMsg::MouseMove { x, y } => {
                        let (x, y) = display.lock().unwrap().apple_pointer(x, y);
                        ClientMsg::MouseMove { x, y }
                    }
                    other => other,
                };
                // Motion waits while the uplink is behind — see [`HeldMotion`].
                // Everything else goes out now, behind whatever was held.
                let input = if backlog.behind(Backlog::MOTION_LIMIT) {
                    match held.hold(input) {
                        Some(input) => input,
                        None => continue,
                    }
                } else {
                    input
                };
                let msgs = held_messages(
                    &mut held,
                    &buttons,
                    &mut button_mask,
                    &mut last_pos,
                    &mut pressed_keys,
                    &mut wheel,
                    macos,
                );
                if let Err(e) = send_all(&uplink, &msgs).await {
                    break Err(e);
                }
                // Viewport reports drive dynamic resize, not an input event;
                // `DefaultSize` is the same request with the size supplied from
                // here instead of by the client — see [`ClientMsg::DefaultSize`]
                // — so the two resolve to a size first and share the one call,
                // which is also how the second inherits the stash-until-supported
                // and drop-the-no-op behaviour `request_resize` already has.
                //
                // `HostDisplay` is that request with no size of its own:
                // mid-session it is a *density* report. High Performance can
                // render its virtual display at the new density when resize is
                // granted. Standard cannot reconfigure a physical display, but
                // does ask the Mac to scale the framebuffer before encoding it.
                // The size it carries mattered only at session-open.
                let ask = match input {
                    ClientMsg::Viewport { w, h } => Some(ResizeAsk::Viewport((w, h))),
                    ClientMsg::DefaultSize => Some(ResizeAsk::Points(default_size)),
                    // On a generic target the report is forwarded as the client's
                    // declared density, once the server has shown it listens
                    // and not while an earlier declaration is unanswered — see
                    // [`DesktopState::host_density_changed`]. Decided and
                    // written under the uplink, like a resize, so a declaration
                    // decided from newer state never trails one from older.
                    // Nothing is resized on it here: the server sets its
                    // output's scale to the declaration and reports, and that
                    // report re-asks the window in the new pixels — see
                    // [`DesktopState::declare_density`].
                    ClientMsg::HostDisplay(screen) if !apple => {
                        let declared = crate::protocol::render_density(screen.scale);
                        if send_decided(&uplink, &desktop, |d| d.host_density_changed(declared)).await? {
                            debug!("vnc: declared a client density of {declared}x");
                        }
                        None
                    }
                    ClientMsg::HostDisplay(screen) if high_performance && resize => {
                        let density = crate::protocol::render_density(screen.scale);
                        let mut d = desktop.lock().unwrap();
                        let changed = (d.host_density - density).abs() > 0.005;
                        d.host_density = density;
                        changed.then_some(ResizeAsk::Density)
                    }
                    ClientMsg::HostDisplay(screen) if apple && !high_performance => {
                        let density = crate::protocol::render_density(screen.scale);
                        desktop.lock().unwrap().host_density = density;
                        // Decided and sent under the uplink lock, as every scale
                        // request is, so the Mac receives them in the order they
                        // were decided in — see [`DisplayState::request_apple_scale`].
                        let mut out = uplink.lock().await;
                        let scaling = {
                            let mut state = display.lock().unwrap();
                            let selection =
                                state.apple_layout.as_ref().and_then(|layout| layout.current);
                            state.request_apple_scale(selection, density)
                        };
                        if let Some(scale) = scaling {
                            debug!("vnc: asking the Mac for {scale}x server scaling");
                            // Break, not `?`: the tasks must be aborted on the way out.
                            if let Err(e) = out.send(&vnc_apple::set_server_scaling(scale)).await {
                                break Err(e);
                            }
                        }
                        None
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
                        (d.poll_size(), d.resize_msg())
                    };
                    // Ahead of the resize it describes, as when it was first sent.
                    let mosaic = display.lock().unwrap().mosaic.clone();
                    if let Some(regions) = mosaic
                        && let Err(e) = sink.msg(ServerMsg::Mosaic { regions, resize: true }).await
                    {
                        break Err(e);
                    }
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
                    // A resize in progress stays covered for the new browser too.
                    let resizing = desktop.lock().unwrap().hp.shown;
                    if resizing
                        && let Err(e) = sink.msg(ServerMsg::Resizing { active: true }).await
                    {
                        break Err(e);
                    }
                    // While the media stream carries the picture, the request below
                    // is for one pixel, and the repaint is its newest picture: the
                    // Mac sends one whenever its screen changes, so the newest is
                    // the screen as it is. A passed stream's repaint is an IDR
                    // from the Mac, which the browser's decoder starts over at.
                    let latest = {
                        let d = desktop.lock().unwrap();
                        let live = media.as_ref().filter(|_| d.media_live).map(|m| m.lock().unwrap());
                        if let Some(media) = &live {
                            media.want_keyframe();
                        }
                        live.and_then(|m| m.latest()).filter(|picture| picture.size == d.size)
                    };
                    if let Some(picture) = latest
                        && let Err(e) = blit_picture(&shadow, &picture, &sink).await
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
                    // asked for. Two engines can act on it: a Mac, which accepts
                    // the extension on both supported transports, and wlshare,
                    // whose outputs extension is the one way a generic server says
                    // it has more than one screen to send.
                    let known = display.lock().unwrap().displays.iter().any(|d| d.id == id);
                    if !known {
                        // A screen unplugged since the list was sent — or a target
                        // that never sent one. Dropped rather than forwarded, so
                        // the remote is not asked to bind to something that is
                        // gone and the checkmark stays where it is.
                        debug!("vnc: ignoring a selection of unknown display {id}");
                        Ok(())
                    } else if apple {
                        // `COMBINED` is this gateway's own list entry, not a
                        // screen the Mac named, so it maps back to the
                        // `combine_all_displays` byte rather than to an id.
                        let pick = (id != DisplayState::COMBINED).then_some(id);
                        debug!("vnc: asking the Mac for display {pick:?}");
                        let host_density = desktop.lock().unwrap().host_density;
                        // Held from the decision to the send; see `HostDisplay`.
                        let mut out = uplink.lock().await;
                        let scaling = display.lock().unwrap().request_apple_scale(pick, host_density);
                        // Queue the repaint while the selection is still the
                        // message in front of the Mac. Asking only after its
                        // answering layout is too late on macOS 26: the layout
                        // and resize arrive, but the request then earns only
                        // later damage. A client has just cleared its resized
                        // framebuffer, so a quiet screen stays black. The Mac
                        // accepts the old framebuffer bounds here and applies
                        // the request to the screen it is switching to.
                        let size = desktop.lock().unwrap().size;
                        let mut messages = vec![vnc_apple::set_display_message(pick)];
                        if let Some(scale) = scaling {
                            debug!("vnc: asking the Mac for {scale}x server scaling");
                            messages.push(vnc_apple::set_server_scaling(scale));
                        }
                        messages.push(update_request(false, size).to_vec());
                        async {
                            for msg in &messages {
                                out.send(msg).await?;
                            }
                            anyhow::Ok(())
                        }
                        .await
                    } else {
                        // wlshare: the list it sent is itself the proof it speaks
                        // the extension, since nothing else fills `displays` on a
                        // generic target. The request is answered with a list
                        // whatever becomes of it, and the repaint rides along for
                        // the same reason as above — a switch to a same-sized
                        // output carries no resize rectangle to redraw through,
                        // and the old screen's pixels are not this one's.
                        debug!("vnc: asking the server for output {id}");
                        let size = desktop.lock().unwrap().size;
                        send_all(
                            &uplink,
                            &[select_output(id).to_vec(), update_request(false, size).to_vec()],
                        )
                        .await
                    }
                } else {
                    let msgs = translate_input(
                        input,
                        &buttons,
                        &mut button_mask,
                        &mut last_pos,
                        &mut pressed_keys,
                        &mut wheel,
                        macos,
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
    write_task.abort();
    result
}

/// The wire messages for what [`HeldMotion`] held, which it no longer holds.
fn held_messages(
    held: &mut HeldMotion,
    buttons: &Buttons,
    button_mask: &mut u8,
    last_pos: &mut (u16, u16),
    pressed_keys: &mut HashMap<String, u32>,
    wheel: &mut Wheel,
    macos: bool,
) -> Vec<Vec<u8>> {
    held.take()
        .flat_map(|motion| {
            translate_input(motion, buttons, button_mask, last_pos, pressed_keys, wheel, macos)
        })
        .collect()
}

/// The unit a resize request states its size in. Everything resolves to logical
/// points first, because that is the one unit all three speak: a viewport report
/// and the configured default are points outright, and a density change carries
/// no size at all.
enum ResizeAsk {
    /// A browser viewport report: points.
    Viewport((u16, u16)),
    /// The target-defined default size: logical points.
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
/// apply, so its points are its pixels — and they are held under the video
/// stream's picture ceiling ([`crate::video::fit_ceiling`])
/// before anything is sent or stashed, so the desktop asked for is one the encoder
/// takes. A High Performance display needs no such hold: the Mac's own 3840×2160
/// backing ceiling in [`vnc_apple::virtual_display_mode`] is already inside it.
async fn request_resize(
    uplink: &SharedUplink,
    desktop: &SharedDesktop,
    ask: ResizeAsk,
    high_performance: bool,
) -> anyhow::Result<()> {
    // The uplink first, then the decision — see [`send_decided`].
    let mut up = uplink.lock().await;
    let msg = {
        let mut d = desktop.lock().unwrap();
        let want = match ask {
            ResizeAsk::Viewport((0, _) | (_, 0)) => return Ok(()),
            ResizeAsk::Viewport(points) | ResizeAsk::Points(points) => points,
            // The current size, in the points it is rendered from: the one
            // request that starts from pixels, and from this end's own. A size
            // still settling is the newer word on the points: a window dragged
            // to another screen reports both, and the density must not undo it.
            ResizeAsk::Density => d.hp.newest_points().unwrap_or_else(|| {
                let point = |v: u16| (f32::from(v) / d.scale).round().max(1.0) as u16;
                (point(d.size.0), point(d.size.1))
            }),
        };
        if high_performance {
            // Recorded, not sent: [`hp_resize_step`] asks the Mac once the window
            // has held still — see [`HpResize`].
            let noop = d.hp_noop(want);
            d.hp.report(want, noop, tokio::time::Instant::now());
            return Ok(());
        }
        // Points × the server's reported scale, or held — see
        // [`DesktopState::generic_resize`], which also logs the request.
        match d.generic_resize(want) {
            Some(msg) => msg.to_vec(),
            None => return Ok(()),
        }
    };
    up.send(&msg).await
}

/// Do whatever a High Performance resize has due: cover or uncover the browser's
/// desktop, and prompt the Mac for the update at whose end the read loop sends a
/// size the window has settled on — see [`HpResize`]. Run by the input loop at
/// [`HpResize::deadline`].
async fn hp_resize_step(
    uplink: &SharedUplink,
    desktop: &SharedDesktop,
    media: Option<&SharedMedia>,
    sink: &VideoSink,
) -> anyhow::Result<()> {
    loop {
        let step = desktop.lock().unwrap().hp.step(tokio::time::Instant::now());
        match step {
            None => return Ok(()),
            Some(HpStep::Show) => sink.msg(ServerMsg::Resizing { active: true }).await?,
            // The display has settled, which is what the media stream waits for:
            // offered mid-change, it is torn down by the change anyway.
            Some(HpStep::Hide) => {
                sink.msg(ServerMsg::Resizing { active: false }).await?;
                offer_media(uplink, desktop, media).await?;
            }
            // A full request answers at once even on a still desktop, so the
            // boundary the read loop waits for comes now rather than at the next
            // change on screen; the one pixel it asks for is in every mode.
            Some(HpStep::Drain) => {
                debug!("vnc: a virtual-display resize is due; prompting the update it goes out after");
                send(uplink, &update_request(false, HP_HOLD_REQUEST)).await?;
            }
            // Polling holds to one pixel only while a request is out, but the
            // armed region stays narrowed until a layout re-arms it.
            Some(HpStep::GiveUp) => {
                let size = desktop.lock().unwrap().size;
                send_all(
                    uplink,
                    &[vnc_apple::auto_framebuffer_update(size), update_request(false, size).to_vec()],
                )
                .await?;
            }
        }
    }
}

/// Offer High Performance's media stream for the current display, when there is
/// one to offer it for and nothing is about to change it — see
/// [`DesktopState::media_offerable`] and [`MediaStream::offer`]. The session's
/// first offer goes out behind the `SetEncodings` that names the stream.
async fn offer_media(
    uplink: &SharedUplink,
    desktop: &SharedDesktop,
    media: Option<&SharedMedia>,
) -> anyhow::Result<()> {
    let Some(media) = media else {
        return Ok(());
    };
    let (size, offer) = {
        let d = desktop.lock().unwrap();
        if !d.media_offerable() {
            return Ok(());
        }
        (d.size, media.lock().unwrap().offer(d.size))
    };
    let Some((first, configuration)) = offer else {
        return Ok(());
    };
    info!("vnc: offering the Mac's media stream for its {}x{} display", size.0, size.1);
    let mut msgs = Vec::with_capacity(2);
    if first {
        msgs.push(set_encodings(&vnc_apple_media::encodings_with_media_stream()));
    }
    msgs.push(configuration);
    send_all(uplink, &msgs).await
}

/// Show a picture the media stream decoded: the whole display, through the shadow
/// like any rectangle, so only what changed reaches the browser. A picture of
/// another size is the old display's last or the new one's before its layout, and
/// is dropped. The first one of a display takes the picture over from ZRLE —
/// see [`DesktopState::media_live`].
async fn show_picture(
    shared: &Shared,
    picture: &vnc_apple_media::Picture,
    sink: &VideoSink,
) -> anyhow::Result<()> {
    let first = {
        let mut d = shared.desktop.lock().unwrap();
        if picture.size != d.size || d.hp.holds_pixels() {
            return Ok(());
        }
        !std::mem::replace(&mut d.media_live, true)
    };
    if first {
        info!("vnc: the picture now comes from the Mac's HEVC media stream");
        // The armed region too, or the Mac would go on pushing ZRLE for every
        // change on screen — and it reads nothing from this side while it writes.
        send(&shared.uplink, &vnc_apple::auto_framebuffer_update(HP_HOLD_REQUEST)).await?;
    }
    blit_picture(&shared.shadow, picture, sink).await
}

/// A whole-display picture into the stream, as much of it as the browser lacks.
async fn blit_picture(
    shadow: &SharedShadow,
    picture: &vnc_apple_media::Picture,
    sink: &VideoSink,
) -> anyhow::Result<()> {
    let Some(rect) = Rect::from_size(0, 0, picture.size.0, picture.size.1) else {
        return Ok(());
    };
    let changed = shadow.lock().unwrap().accept(rect, &picture.rgb);
    if let Some(changed) = changed {
        if changed == rect {
            sink.damage(rect, &picture.rgb).await?;
        } else {
            let mut pixels = Vec::new();
            shadow::crop(&picture.rgb, rect, changed, &mut pixels);
            sink.damage(changed, &pixels).await?;
        }
    }
    sink.frame().await
}

/// Pass a unit of the Mac's HEVC to the browser, as [`show_picture`] shows a
/// decoded picture: one of another size, or one that comes while a resize holds
/// the display, is dropped, and the first one of a display takes the picture over
/// from ZRLE, whose rectangles went out as video encoded here. A dropped unit is one
/// the next ones predict from, so the browser starts over at a keyframe, which the
/// Mac is asked for as soon as a unit is held back waiting for one.
async fn pass_unit(shared: &Shared, unit: PassedUnit, sink: &VideoSink, media: &SharedMedia) -> anyhow::Result<()> {
    let first = {
        let mut d = shared.desktop.lock().unwrap();
        if unit.size != d.size || d.hp.holds_pixels() {
            sink.restart_pass();
            return Ok(());
        }
        !std::mem::replace(&mut d.media_live, true)
    };
    if first {
        info!("vnc: the picture is now the Mac's HEVC media stream, passed to the browser");
        // As in `show_picture`: the Mac would otherwise go on pushing ZRLE.
        send(&shared.uplink, &vnc_apple::auto_framebuffer_update(HP_HOLD_REQUEST)).await?;
    }
    let (w, h) = unit.size;
    let passed = crate::stream::Passed { decode: unit.decode, keyframe: unit.keyframe };
    if !sink.pass_hevc(w, h, unit.data, passed).await? {
        media.lock().unwrap().want_keyframe();
    }
    Ok(())
}

/// What the media stream handed the read loop next.
enum FromStream {
    Picture(Arc<vnc_apple_media::Picture>),
    Unit(PassedUnit),
    /// The stream's receiver has stopped.
    Stopped,
}

/// The next picture the media stream decoded, or unit it passes, on a session that
/// has one; otherwise never.
async fn next_picture(apple: &mut Option<Apple>) -> FromStream {
    let Some(pictures) = apple.as_mut().and_then(|a| a.pictures.as_mut()) else {
        return std::future::pending().await;
    };
    match pictures {
        Pictures::Decoded(pictures) => {
            if pictures.changed().await.is_err() {
                return std::future::pending().await;
            }
            pictures.borrow_and_update().clone().map_or(FromStream::Stopped, FromStream::Picture)
        }
        Pictures::Passed(units) => match units.recv().await {
            Some(Some(unit)) => FromStream::Unit(unit),
            Some(None) => FromStream::Stopped,
            None => std::future::pending().await,
        },
    }
}

/// Resolves when a High Performance resize has something due — at
/// [`HpResize::deadline`], re-read whenever `wake` says the read loop changed it.
async fn hp_resize_due(desktop: &SharedDesktop, wake: &tokio::sync::Notify) {
    loop {
        let at = desktop.lock().unwrap().hp.deadline(tokio::time::Instant::now());
        let due = async {
            match at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = due => return,
            () = wake.notified() => {}
        }
    }
}

/// A generic resize request held under the video stream's picture ceiling, in
/// the pixels a `SetDesktopSize` states. Logged when it bites, because the
/// desktop that arrives is then not the one the window asked for.
fn held_under_ceiling(want: (u16, u16)) -> (u16, u16) {
    let (w, h) = crate::video::fit_ceiling((u32::from(want.0), u32::from(want.1)));
    // Lossless: the ceiling only ever shrinks what a `u16` already held.
    let held = (u16::try_from(w).unwrap_or(u16::MAX), u16::try_from(h).unwrap_or(u16::MAX));
    if held != want {
        info!(
            "vnc: holding a {}x{} resize under the video stream's {}x{} picture ceiling",
            want.0, want.1, held.0, held.1
        );
    }
    held
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
    /// Wakes the input loop's High Performance resize timer when the read loop
    /// changes what it waits on — a layout arrived, or media setup finished.
    hp_wake: Arc<tokio::sync::Notify>,
    /// Where the desktop's sound goes on a generic target that asked for it —
    /// see [`Flags::wlshare_audio`]. `None` is a session with no sound to carry,
    /// and the extension is then neither advertised nor read.
    audio: Option<Arc<crate::audio::AudioBridge>>,
    /// The browser's camera — see [`Flags::camera`]. `None` is a session with no
    /// camera to lend, and the extension is then neither advertised nor read.
    camera: Option<Arc<vnc_camera::Link>>,
    /// The browser's microphone — see [`Flags::microphone`]. `None` is a session with no
    /// microphone to lend, and the extension is then neither advertised nor read.
    microphone: Option<Arc<vnc_mic::Link>>,
    /// High Performance's media stream — see [`Connected::media`]. Both loops
    /// offer it: the read loop at an update boundary, the input loop when a
    /// resize's cover comes down.
    media: Option<SharedMedia>,
    /// See [`Connected::passthrough`]. `Some` is a session that may be sent
    /// [`ENCODING_WLSHARE_VP9`], and the only one that reads it.
    passthrough: Option<Arc<[i32]>>,
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
    sink: VideoSink,
) -> anyhow::Result<()> {
    let ReadFlags { clipboard: clipboard_enabled, poll } = flags;
    let Shared {
        uplink, desktop, clipboard, display, hp_wake, audio, camera, microphone, media, passthrough, ..
    } = &shared;
    // Where the audio extension stands here. `Off` on a session with no bridge
    // to feed, which is also a session that never listed the encoding, so
    // neither the announcement nor a frame can arrive.
    let mut audio_state = if audio.is_some() { Audio::Asked } else { Audio::Off };
    // The running stream's FLAC decoder, from a begin to its end.
    let mut flac: Option<FrameDecoder> = None;
    let mut full_repaint: Option<FullRepaint> = None;
    let mut apple_poll_paused = false;
    let mut apple_poll_deadline: Option<tokio::time::Instant> = None;
    let apple_fetch_active = |apple: &Option<Apple>| {
        apple.is_some() && clipboard.lock().unwrap().apple_fetch_pending
    };
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
    // High Performance's signal that an offer went out — see [`MediaStream::offered`].
    let offers = media.as_ref().map(|m| m.lock().unwrap().offered());
    // Whether wlshare's VP9 is on the list the server holds: as the preface listed
    // it, and then by [`lists_wlshare_vp9`] at the end of each update.
    let mut vp9_listed = passthrough.is_some() && lists_wlshare_vp9(desktop.lock().unwrap().size);
    // The fences owed an echo, in order, while the picture is wlshare's VP9. Each
    // waits for the browser to have taken what came before it
    // ([`VideoSink::drained`]): wlshare holds one frame in flight and walks its
    // quality by the fence's round trip, which an immediate echo would make the round
    // trip to this gateway alone. Every fence waits in the one queue, so none
    // overtakes another, and each carries the deadline it was queued with,
    // [`FENCE_HOLD_LIMIT`] on: the loop turns on every server message, and a limit
    // measured afresh each turn would never run out on a server that keeps talking.
    // The flag is BlockAfter, which stops the reading until that fence is echoed.
    let mut held_fences: VecDeque<(tokio::time::Instant, bool, Vec<u8>)> = VecDeque::new();
    loop {
        // Raced against the next message rather than awaited on its own, so a paced
        // video stream still hands over pixels the mirror is holding when the remote
        // has gone quiet — see `VideoSink::due_at` for why that is correctness rather
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
        let clipboard_idle = async {
            match apple_poll_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        // When High Performance's media stream is next overdue — see
        // [`MediaStream::overdue`]. Read here, once a turn, like the deadline above,
        // and read again at an offer, which may come from the input loop while this
        // one waits behind a still screen.
        let media_deadline = media.as_ref().and_then(|m| m.lock().unwrap().deadline());
        let media_due = async {
            match media_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending().await,
            }
        };
        let media_offered = async {
            match &offers {
                Some(offers) => offers.notified().await,
                None => std::future::pending().await,
            }
        };
        let fence_deadline = held_fences.front().map(|(deadline, ..)| *deadline);
        // Only the last fence held can be BlockAfter: nothing is read behind one.
        let blocked = held_fences.back().is_some_and(|(_, block_after, _)| *block_after);
        let fence_due = async {
            if let Some(deadline) = fence_deadline.filter(|_| sink.passing()) {
                let _ = tokio::time::timeout_at(deadline, sink.drained()).await;
            }
        };
        let read = tokio::select! {
            byte = reader.read_u8(), if !blocked => byte,

            () = fence_due, if !held_fences.is_empty() => {
                let (.., echo) = held_fences.pop_front().expect("guarded");
                send(uplink, &echo).await?;
                continue;
            }

            picture = next_picture(&mut apple) => {
                match picture {
                    FromStream::Picture(picture) => {
                        if let Some(media) = media {
                            media.lock().unwrap().pictured(picture.size);
                        }
                        show_picture(&shared, &picture, &sink).await?;
                    }
                    FromStream::Unit(unit) => {
                        let media = media.as_ref().expect("a passed unit comes from the media stream");
                        media.lock().unwrap().pictured(unit.size);
                        pass_unit(&shared, unit, &sink, media).await?;
                    }
                    // The receiver failed. Apple's viewer has no way back to RFB
                    // pixels from a failed stream and ends the session, and so does
                    // this one.
                    FromStream::Stopped => {
                        let failure = media.as_ref().map_or_else(
                            || anyhow::anyhow!("its receiver stopped"),
                            |m| m.lock().unwrap().failure(),
                        );
                        return Err(failure.context("the Mac's media stream stopped"));
                    }
                }
                continue;
            }

            () = media_due => {
                let overdue = media
                    .as_ref()
                    .and_then(|m| m.lock().unwrap().overdue(std::time::Instant::now()));
                if let Some(overdue) = overdue {
                    return Err(overdue);
                }
                continue;
            }
            () = media_offered => continue,

            () = video_flush => {
                sink.frame().await?;
                continue;
            }
            () = clipboard_idle => {
                {
                    let mut state = clipboard.lock().unwrap();
                    state.apple_fetch_pending = false;
                    state.apple_fetch_again = false;
                    // The waiting browser reads go with it. A reply that arrives
                    // after this gap has been given up on is an unsolicited push,
                    // and marking it `requested` would let it answer a read the
                    // panel had not made yet.
                    state.apple_requests = 0;
                }
                apple_poll_paused = false;
                apple_poll_deadline = None;
                let size = desktop.lock().unwrap().poll_size();
                debug!("vnc: Apple pasteboard fetch left unanswered; resuming framebuffer polling");
                send(uplink, &update_request(full_repaint.is_none(), size)).await?;
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
                return Err(anyhow::Error::new(e));
            }
            Err(e) => return Err(anyhow::anyhow!("read server message: {e}")),
        };
        if apple_poll_paused {
            // Every complete server message proves the automatic stream is still
            // draining. Only a genuinely idle gap declares the native fetch
            // unanswered; a busy pixel queue may take arbitrarily long to empty.
            apple_poll_deadline = Some(tokio::time::Instant::now() + APPLE_CLIPBOARD_IDLE_GAP);
        }
        match msg_type {
            // FramebufferUpdate
            0 => {
                reader.read_u8().await?; // padding
                desktop.lock().unwrap().first_update();
                // `0xffff` here means "as many as it takes, ended by a LastRect" —
                // an update a server starts sending before it knows how long it
                // will be. macOS uses it for the metadata burst, so on the Apple
                // dialect this is the normal form rather than a curiosity. The count
                // still bounds the loop, so a server that promises a LastRect and
                // never sends one is stopped by the same code either way.
                let rects = reader.read_u16().await?;
                let mut resized = false;
                let mut full_repaint_owed = false;
                let mut audio_announced = false;
                let mut painted = false;
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
                    audio_announced |= effect.audio_announced;
                    painted |= effect.pixels.is_some();
                    if let (Some(repaint), Some(rect)) = (&mut full_repaint, effect.pixels) {
                        repaint.accept(rect);
                    }
                    if effect.last {
                        break;
                    }
                }
                // The extension's announcement and the answer to it: the format
                // this client wants and the switch that starts the stream. Sent
                // here rather than from the rectangle so it goes out once
                // whatever the update was made of, and after the update rather
                // than before, so a server sending pixels in the same one is not
                // answered mid-message.
                if audio_announced && audio_state != Audio::Announced {
                    audio_state = Audio::Announced;
                    info!(
                        "vnc: the server carries desktop audio; asking for {} Hz, {} channel, \
                         {}-bit sound",
                        vnc_audio::SOURCE_FORMAT.sample_rate,
                        vnc_audio::SOURCE_FORMAT.channels,
                        vnc_audio::SOURCE_FORMAT.bits_per_sample
                    );
                    send(uplink, &vnc_audio::set_format(vnc_audio::WANTED)).await?;
                    send(uplink, &vnc_audio::enable()).await?;
                } else if audio_state == Audio::Asked && painted {
                    // The announcement comes in an update of its own before any
                    // pixels, so pixels without one are a server that does not
                    // speak the extension — wayvnc, TigerVNC, x11vnc. The
                    // desktop is unaffected; only the sound is not there.
                    audio_state = Audio::Unanswered;
                    info!("vnc: the server carries no audio; the session runs without sound");
                }
                // One FramebufferUpdate is one frame's worth of damage, however many
                // rectangles it was described in — the cleanest frame boundary either
                // protocol offers, and where a video stream is told to encode what it
                // has. After the loop rather than inside it, so a `LastRect` breaking
                // out still reaches it.
                sink.frame().await?;
                // wlshare's VP9 is a picture of the whole desktop, which past the
                // ceiling is not video: off the list there, so wlshare sends the
                // desktop again as ZRLE for tiles, and back on the list within it,
                // where wlshare starts its stream again at a keyframe.
                if let Some(encodings) = passthrough {
                    let wanted = lists_wlshare_vp9(desktop.lock().unwrap().size);
                    if wanted != vp9_listed {
                        vp9_listed = wanted;
                        let listed = if wanted { with_wlshare_vp9(encodings) } else { encodings.to_vec() };
                        send(uplink, &set_encodings(&listed)).await?;
                    }
                }
                let size = {
                    let mut d = desktop.lock().unwrap();
                    if resized {
                        // The full update a resize earns below repaints whatever a
                        // scale report cleared — see [`read_output_scale`].
                        d.repaint_owed = false;
                    }
                    d.size
                };
                // The enabled region is part of the request, so a resize invalidates
                // it: without this the server would go on pushing updates for a
                // rectangle the desktop no longer has.
                if continuous && resized {
                    send(uplink, &enable_continuous_updates(true, size)).await?;
                }
                // A High Performance resize goes out here, at the end of an update,
                // because this is where no full-size pixel request is outstanding;
                // polling then holds to one pixel until the answering layout — see
                // [`HP_HOLD_REQUEST`].
                //
                // Nor while a media-stream offer is out: the Mac is starting a capture
                // of the display the change would replace. The answer ends an update
                // too, and the change goes out at that one.
                let hp_holding = if apple.as_ref().is_some_and(|a| a.virtual_display) {
                    let (request, drained, holding) = {
                        let mut d = desktop.lock().unwrap();
                        let draining = matches!(d.hp.phase, HpPhase::Draining(_));
                        let offer_out = media.as_ref().is_some_and(|m| m.lock().unwrap().pending());
                        let request =
                            if offer_out { None } else { d.hp_take_request(tokio::time::Instant::now()) };
                        if request.is_some() {
                            d.media_live = false;
                            if let Some(media) = media {
                                media.lock().unwrap().stopped();
                            }
                        }
                        let drained = draining && !matches!(d.hp.phase, HpPhase::Draining(_));
                        (request, drained, d.hp.holds_pixels() || d.media_live)
                    };
                    if let Some(msg) = request {
                        send_all(uplink, &[vnc_apple::auto_framebuffer_update(HP_HOLD_REQUEST), msg])
                            .await?;
                    }
                    if drained {
                        hp_wake.notify_one();
                    }
                    offer_media(uplink, desktop, media.as_ref()).await?;
                    holding
                } else {
                    false
                };
                // With the server pushing, asking for an *incremental* update is the
                // one thing that has to stop — it is the round trip per frame this
                // removes. Non-incremental requests are unaffected and still go where
                // they went: this gateway needs a full repaint that no amount of
                // waiting for damage will produce, on a reattach, a resize, or a
                // CopyRect whose source it never learned.
                let poll = poll && !continuous;
                if hp_holding {
                    send(uplink, &update_request(true, HP_HOLD_REQUEST)).await?;
                } else if full_repaint_owed {
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
                            if apple_fetch_active(&apple) {
                                apple_poll_paused = true;
                                apple_poll_deadline = Some(
                                    tokio::time::Instant::now() + APPLE_CLIPBOARD_IDLE_GAP,
                                );
                            } else {
                                send(uplink, &update_request(true, size)).await?;
                            }
                        }
                    } else if full_repaint.is_some() {
                        send(uplink, &update_request(false, size)).await?;
                    } else if poll || resized {
                        if poll && !resized && apple_fetch_active(&apple) {
                            apple_poll_paused = true;
                            apple_poll_deadline = Some(
                                tokio::time::Instant::now() + APPLE_CLIPBOARD_IDLE_GAP,
                            );
                        } else {
                            // A passed stream is owed no repaint by a resize: wlshare
                            // starts it again at the new size with a keyframe of the
                            // whole desktop, and a full request would only have it
                            // send a second one, which no shadow is there to skip.
                            // The Mac's passed HEVC is owed one: ZRLE carries the
                            // picture after a display change, as video encoded here.
                            let full = resized && !(passthrough.is_some() && sink.passing());
                            send(uplink, &update_request(!full, size)).await?;
                        }
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
                // of the first 512 KiB, which would look like the whole thing.
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
            // The wlshare OutputScale report: the framebuffer's density, from the one
            // generic server that can say. Only a session that asked reads it; on
            // the Apple dialects, 0xE0 is as unknown as it was.
            MSG_WLSHARE_DENSITY if desktop.lock().unwrap().density != Density::Off => {
                read_output_scale(&mut reader, uplink, desktop, &shared.shadow, &sink).await?;
            }
            // The wlshare OutputList: which outputs the compositor has and which
            // one it is sending. Read only on a generic target, the only kind
            // that listed the encoding; on the Apple dialects 0xE1 is as unknown
            // as it was, and a Mac's screens arrive in its display layout.
            MSG_WLSHARE_OUTPUTS if apple.is_none() => {
                read_output_list(&mut reader, uplink, desktop, display, &sink).await?;
            }
            // The QEMU message type, which wlshare's audio extension borrows for
            // a stream beginning and a stream ending ([`vnc_audio`]). Only a
            // session that asked for sound reads it — nothing else listed the
            // encoding — and on the Apple dialects 255 is as unknown as it was.
            vnc_audio::MSG_QEMU if audio_state != Audio::Off => {
                let mut header = [0u8; 3];
                reader.read_exact(&mut header).await?;
                // A submessage this client cannot measure is fatal rather than
                // skipped: the QEMU submessages share no length field, so one it
                // does not know leaves the stream at an offset nothing recovers
                // from.
                match vnc_audio::parse_server(header)? {
                    ServerAudio::Begin => {
                        info!("vnc: the server started the desktop's audio stream");
                        flac = Some(FrameDecoder::new()?);
                        if let Some(bridge) = audio {
                            bridge.publish_format(vnc_audio::SOURCE_FORMAT);
                        }
                    }
                    ServerAudio::End => {
                        info!("vnc: the server stopped the desktop's audio stream");
                        flac = None;
                        if let Some(bridge) = audio {
                            bridge.clear_format();
                        }
                    }
                }
            }
            // One FLAC frame of the running stream, decoded into the 16-bit
            // stereo this client asked for, which is what the queue takes.
            // Framed by its own length, so an implausible one is read past
            // rather than allocated, and one that does not decode costs its
            // twenty milliseconds rather than the session: the next frame
            // decodes on its own.
            vnc_audio::MSG_FRAME if audio_state != Audio::Off => {
                let mut header = [0u8; vnc_audio::FRAME_HEADER_LEN];
                reader.read_exact(&mut header).await?;
                let length = vnc_audio::frame_length(header);
                if length > MAX_AUDIO_FRAME {
                    discard(&mut reader, u64::from(length)).await?;
                    warn!("vnc: dropped a {length}-byte audio frame, over the {MAX_AUDIO_FRAME} byte limit");
                } else {
                    let mut frame = vec![0u8; length as usize];
                    reader.read_exact(&mut frame).await?;
                    match flac.as_mut() {
                        None => warn!("vnc: dropped an audio frame sent outside a stream"),
                        Some(decoder) => match decoder.decode(frame) {
                            Ok(samples) => {
                                if let Some(bridge) = audio {
                                    bridge.wave(samples);
                                }
                            }
                            Err(e) => warn!("vnc: dropped an audio frame: {e:#}"),
                        },
                    }
                }
            }
            // The wlshare camera extension's one message type: the server takes a
            // camera, or an application on the desktop opened or closed it, or a
            // keyframe is owed ([`vnc_camera`]). Only a session that carries a
            // camera listed the encoding, so only it reads the type; on every other
            // session 0xE2 is as unknown as it was.
            vnc_camera::MSG_CAMERA if let Some(link) = camera => {
                let mut header = [0u8; vnc_camera::SERVER_HEADER_LEN];
                reader.read_exact(&mut header).await?;
                let mut body = [0u8; vnc_camera::START_FORMAT_LEN];
                let body = &mut body[..vnc_camera::body_len(header)?];
                reader.read_exact(body).await?;
                match vnc_camera::parse_server(header, body)? {
                    ServerCamera::Available => {
                        send_camera_decided(uplink, link, vnc_camera::Device::announce).await?;
                    }
                    ServerCamera::Start(format) => {
                        info!(
                            "vnc: an application on the desktop opened the camera ({}x{})",
                            format.width, format.height
                        );
                        link.bridge.signal(CameraSignal::Start(format));
                    }
                    ServerCamera::Stop => {
                        info!("vnc: the camera is no longer open on the desktop");
                        link.bridge.signal(CameraSignal::Stop);
                    }
                    ServerCamera::Keyframe => link.bridge.signal(CameraSignal::Keyframe),
                }
            }
            // The wlshare microphone extension's one message type: the server takes a
            // microphone, or an application on the desktop started or stopped recording
            // ([`vnc_mic`]). Only a session that carries a microphone listed the
            // encoding, so only it reads the type.
            vnc_mic::MSG_MICROPHONE if let Some(link) = microphone => {
                let mut header = [0u8; vnc_mic::SERVER_HEADER_LEN];
                reader.read_exact(&mut header).await?;
                let mut body = [0u8; vnc_mic::START_FORMAT_LEN];
                let body = &mut body[..vnc_mic::body_len(header)?];
                reader.read_exact(body).await?;
                match vnc_mic::parse_server(header, body)? {
                    ServerMicrophone::Available => {
                        send_microphone_decided(uplink, link, |device| vnc_mic::Decision {
                            message: device.announce(),
                            closed: false,
                        })
                        .await?;
                    }
                    // Told to the bridge under the device's lock, for the reason
                    // [`send_microphone_decided`] gives.
                    ServerMicrophone::Start(format) => {
                        let mut device = link.device.lock().unwrap();
                        // The bridge sizes its resampler from the rate, so a format it
                        // cannot produce is refused before anything opens.
                        if let Err(e) = format.producible() {
                            warn!("vnc: the desktop records the microphone in a format the gateway cannot produce: {e:#}");
                        } else if device.start() {
                            info!(
                                "vnc: an application on the desktop is recording the microphone ({} ch, {} Hz)",
                                format.channels, format.sample_rate
                            );
                            link.bridge.signal(MicSignal::Open(format));
                        }
                    }
                    ServerMicrophone::Stop => {
                        let mut device = link.device.lock().unwrap();
                        if device.stop() {
                            info!("vnc: nothing on the desktop records the microphone any more");
                            link.bridge.signal(MicSignal::Close);
                        }
                    }
                }
            }
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
            // which is how it measures this end and paces itself. Echoed from the
            // read task, so the answer is not queued behind anything the input side is
            // doing — a fence that waited would report a link slower than it is.
            // While the picture is wlshare's VP9 the link it should report is the
            // browser's, so then it waits in `held_fences` for that.
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
                let echo = client_fence(flags, &payload);
                if sink.passing() {
                    let deadline = tokio::time::Instant::now() + FENCE_HOLD_LIMIT;
                    held_fences.push_back((deadline, flags & FENCE_BLOCK_AFTER != 0, echo));
                } else {
                    // The stream it was held for has stopped: what is held goes
                    // first, so this one overtakes none of them.
                    for (.., held) in held_fences.drain(..) {
                        send(uplink, &held).await?;
                    }
                    send(uplink, &echo).await?;
                }
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
                        let session_id = clipboard.lock().unwrap().begin_apple_fetch(true);
                        if let Some(session_id) = session_id {
                            send(uplink, &vnc_apple_clipboard::fetch(session_id)).await?;
                        }
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
                let receiver = match vnc_apple_clipboard::Receiver::new(header) {
                    Ok(receiver) => Some(receiver),
                    Err(e) => {
                        warn!("vnc: {e:#}");
                        None
                    }
                };
                let Some(mut receiver) = receiver else {
                    discard(&mut reader, compressed).await?;
                    if !clipboard_enabled {
                        continue;
                    }
                    let requested = finish_apple_clipboard_fetch(
                        clipboard,
                        desktop,
                        uplink,
                        &mut apple_poll_paused,
                        &mut apple_poll_deadline,
                    )
                    .await?;
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
                };
                // Streamed rather than held: the archive can be megabytes of other
                // flavors around a short text. A fault stops the inflating, never
                // the reading, which the stream's framing depends on.
                let mut received = Ok(());
                let mut left = compressed;
                let mut chunk = vec![0u8; 64 * 1024];
                while left > 0 {
                    let n = left.min(chunk.len() as u64) as usize;
                    reader.read_exact(&mut chunk[..n]).await?;
                    left -= n as u64;
                    if clipboard_enabled && received.is_ok() {
                        received = receiver.feed(&chunk[..n]);
                    }
                }
                if !clipboard_enabled {
                    continue;
                }
                let requested = finish_apple_clipboard_fetch(
                    clipboard,
                    desktop,
                    uplink,
                    &mut apple_poll_paused,
                    &mut apple_poll_deadline,
                )
                .await?;
                match received.and_then(|()| receiver.finish()) {
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
            other => anyhow::bail!("unknown server message type {other:#04x}"),
        }
    }
}

/// Settle one native Apple pasteboard fetch after its complete payload has been
/// consumed. A remote change that raced it is fetched once more before screen
/// polling resumes; otherwise the one-request-at-a-time pixel cycle can continue.
async fn finish_apple_clipboard_fetch(
    clipboard: &SharedClipboard,
    desktop: &SharedDesktop,
    uplink: &SharedUplink,
    poll_paused: &mut bool,
    poll_deadline: &mut Option<tokio::time::Instant>,
) -> anyhow::Result<bool> {
    let (requested, fetch_again) = clipboard.lock().unwrap().finish_apple_fetch();
    if let Some(session_id) = fetch_again {
        send(uplink, &vnc_apple_clipboard::fetch(session_id)).await?;
    } else if std::mem::take(poll_paused) {
        *poll_deadline = None;
        let size = desktop.lock().unwrap().poll_size();
        send(uplink, &update_request(true, size)).await?;
    }
    Ok(requested)
}

/// Forward one already-recorded remote clipboard snapshot. Returns whether the
/// browser link is gone, matching the read loop's other sink helpers.
async fn emit_clipboard(sink: &VideoSink, snapshot: ClipboardSnapshot, requested: bool) -> bool {
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
    sink: &VideoSink,
) -> anyhow::Result<()> {
    let (fetch, snapshot) = {
        let mut state = clipboard.lock().unwrap();
        state.apple_requests = state.apple_requests.saturating_add(1);
        (
            state.begin_apple_fetch(false),
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
    if let Some(session_id) = fetch {
        send(uplink, &vnc_apple_clipboard::fetch(session_id)).await
    } else {
        Ok(())
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
    sink: &VideoSink,
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
        // showing the first 512 KiB as though it were the whole clipboard.
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
    /// The audio extension's announcement rectangle — the server saying it
    /// can carry the desktop's sound. Acted on after the update rather than in
    /// the rect, so the enable goes out once however the announcement was
    /// framed. See [`crate::vnc_audio`].
    audio_announced: bool,
}

impl RectEffect {
    const NOTHING: Self = Self {
        resized: false,
        full_repaint_owed: false,
        pixels: None,
        last: false,
        audio_announced: false,
    };

    const AUDIO_ANNOUNCED: Self = Self { audio_announced: true, ..Self::NOTHING };
    const LAST: Self = Self { last: true, ..Self::NOTHING };

    const FULL_REPAINT: Self = Self {
        resized: false,
        full_repaint_owed: true,
        pixels: None,
        last: false,
        audio_announced: false,
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
    sink: &VideoSink,
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
        // The same, with the header's hotspot and size and an alpha channel.
        ENCODING_CURSOR_WITH_ALPHA => {
            read_alpha_cursor(reader, cursor, (x, y, w, h), sink).await?;
            return Ok(RectEffect::NOTHING);
        }
        // No payload at all: the rectangle's presence is the whole message.
        ENCODING_LAST_RECT => return Ok(RectEffect::LAST),
        // DesktopSize: the rect itself is the announcement; no payload.
        //
        // Non-Apple RFB only. A Mac sends it only to a viewer that did not list
        // `DisplayInfo`, which [`vnc_apple::ENCODINGS`] does, and it carries no
        // density: applied with Apple metadata it would overwrite a scale learned
        // from a display layout with `UNSCALED`. The layout carries the same size and
        // the density with it, and one arrives with every geometry change.
        ENCODING_DESKTOP_SIZE => {
            if apple.is_some() {
                debug!("vnc: ignoring a DesktopSize rect; the display layout is authoritative");
                return Ok(RectEffect::NOTHING);
            }
            let scale = desktop.lock().unwrap().generic_scale();
            return apply_resize(desktop, shadow, (w, h), scale, sink).await.map(RectEffect::resized);
        }
        ENCODING_EXTENDED_DESKTOP_SIZE if apple.is_none() => {
            return read_extended_desktop_size(reader, uplink, desktop, shadow, (x, y, w, h), sink)
                .await
                .map(RectEffect::resized);
        }
        // Ungated, like [`ENCODING_RAW`]: an Apple server cannot send what its own
        // list omits.
        ENCODING_ZLIB => payload = Payload::Zlib,
        vnc_apple::ENCODING_CURSOR_IMAGE if apple.is_some() => {
            read_cursor_image(reader, apple, cursor, (x, y), (w, h), sink).await?;
            return Ok(RectEffect::NOTHING);
        }
        vnc_apple::ENCODING_DISPLAY_LAYOUT if apple.is_some() => {
            let virtual_display = apple.as_ref().is_some_and(|a| a.virtual_display);
            read_display_layout(
                reader,
                shared,
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
        // Advertised, and ignored because a client draws the pointer where it last
        // put it.
        vnc_apple::ENCODING_CURSOR_POS if apple.is_some() => return Ok(RectEffect::NOTHING),
        // The Mac's keyboard, which this gateway does not act on. Both frame
        // themselves the same way — a `u16` saying how much follows — and reading
        // that length is the whole point: the RFB stream above the record layer has
        // no framing of its own, so walking past by the wrong number of bytes desyncs
        // everything after it.
        vnc_apple::ENCODING_VENDOR_KEYSYMS | vnc_apple::ENCODING_KEYBOARD_SOURCE
            if apple.is_some() =>
        {
            let len = reader.read_u16().await?;
            discard(reader, u64::from(len)).await?;
            return Ok(RectEffect::NOTHING);
        }
        // `DisplayInfo`, the older display list: `u16` width and height, a `u32` of
        // flags, a `u16` count, then 0x1c bytes per screen. It carries no density,
        // and a Mac sends it only to a viewer that did not list the layout, so it is
        // advertised — the layout needs it listed — and stepped over.
        vnc_apple::ENCODING_DISPLAY_INFO if apple.is_some() => {
            let mut head = [0u8; 10];
            reader.read_exact(&mut head).await?;
            let count = u64::from(u16::from_be_bytes([head[8], head[9]]));
            discard(reader, count * 0x1c).await?;
            return Ok(RectEffect::NOTHING);
        }
        // wlshare's audio announcement: an empty rectangle of the
        // pseudo-encoding this session listed, and the only way a generic server
        // ever says it can carry sound ([`vnc_audio`]). It has no body —
        // the announcement is the rectangle — so there is nothing to read past.
        // Only a session that asked can see one: the encoding was advertised
        // nowhere else, and an unadvertised encoding still falls through to the
        // refusal below.
        vnc_audio::ENCODING if shared.audio.is_some() => {
            return Ok(RectEffect::AUDIO_ANNOUNCED);
        }
        // A rekey after the preface. The Mac rotates only when the viewer asks
        // with `SetEncryption` command 1, which this client never sends after the
        // handshake, and it switches both of its ciphers the instant it sends the
        // rekey. Records this side has already framed under the old key — the
        // writer encrypts as it queues — would then fail the Mac's check, so an
        // unrequested rotation cannot be followed safely. Named and closed.
        vnc_apple::ENCODING_REKEY if apple.is_some() => {
            anyhow::bail!("the server re-keyed mid-session, which this client never requests")
        }
        // The Mac's replies to a media-stream offer ([`vnc_apple_media`]): a `u16`
        // saying how much follows, then the reply. A refusal ends the session, as it
        // ends Apple's viewer's. A stream the Mac took down with a display change of
        // its own hands the picture to ZRLE until the next offer, and ZRLE has sent
        // nothing while the stream ran, so the whole desktop is asked for.
        vnc_apple_media::ENCODING_MEDIA_STREAM if shared.media.is_some() => {
            let len = reader.read_u16().await?;
            let mut body = vec![0u8; usize::from(len)];
            reader.read_exact(&mut body).await?;
            let media = shared.media.as_ref().expect("guarded");
            let down = media.lock().unwrap().on_reply(&body)?;
            let was_live = down && std::mem::take(&mut desktop.lock().unwrap().media_live);
            return Ok(if was_live { RectEffect::FULL_REPAINT } else { RectEffect::NOTHING });
        }
        // A frame of wlshare's VP9, which is the whole desktop: passed to the browser
        // untouched, or dropped while the desktop is past the ceiling, until the list
        // the read loop sends without the encoding brings the desktop again as ZRLE.
        ENCODING_WLSHARE_VP9 if shared.passthrough.is_some() => {
            let len = reader.read_u32().await?;
            anyhow::ensure!(
                len <= MAX_WLSHARE_VP9_FRAME,
                "server sent a {len}-byte VP9 frame, past the {MAX_WLSHARE_VP9_FRAME} bytes one can be"
            );
            let mut frame = vec![0u8; len as usize];
            reader.read_exact(&mut frame).await?;
            let size = desktop.lock().unwrap().size;
            anyhow::ensure!(
                (x, y, w, h) == (0, 0, size.0, size.1),
                "server sent a VP9 frame of {w}x{h}+{x}+{y}, not the whole {}x{} desktop",
                size.0,
                size.1
            );
            if sink.tiling() {
                debug!("vnc: dropping a {w}x{h} VP9 frame past the video ceiling");
                return Ok(RectEffect::NOTHING);
            }
            sink.pass(w, h, frame).await?;
            return Ok(Rect::from_size(x, y, w, h).map_or(RectEffect::NOTHING, RectEffect::pixels));
        }
        other => {
            let label = encoding_label(other);
            anyhow::bail!("server sent encoding {label}, which was not advertised")
        }
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
        .decode(reader, payload, shadow, w, h)
        .await?;
    let Some(rect) = Rect::from_size(x, y, w, h) else {
        return Ok(RectEffect::NOTHING);
    };
    let rgb = match decoded {
        Decoded::Pixels(rgb) => rgb,
        // A CopyRect whose source this side never learned. Guessing would leave
        // wrong pixels on screen until something else happened to change that area;
        // one full request makes the source known instead.
        Decoded::Unavailable => return Ok(RectEffect::FULL_REPAINT),
    };
    // While the media stream carries the picture, ZRLE is decoded only to keep its
    // deflate stream in step: the Mac still answers the one-pixel polls, and pushes
    // a whole screen on its own at a login.
    if desktop.lock().unwrap().media_live {
        return Ok(RectEffect::pixels(rect));
    }

    // A rectangle carried as a tile goes out whole, as the server sent it. The
    // shadow still records it, for a later CopyRect to read its source from.
    if sink.tiling() {
        shadow.lock().unwrap().accept(rect, &rgb);
        sink.damage(rect, &rgb).await?;
        return Ok(RectEffect::pixels(rect));
    }

    // What of this rect the browser does not already have. A server that
    // re-sends unchanged pixels — and they do, on a cursor crossing a window
    // boundary or a client asking for a full update — stops costing the browser
    // link anything here.
    let Some(changed) = shadow.lock().unwrap().accept(rect, &rgb) else {
        return Ok(RectEffect::pixels(rect));
    };

    // Cropped out of the rect just read rather than out of the shadow: the bytes
    // are the same and this needs no lock.
    if changed == rect {
        sink.damage(rect, &rgb).await?;
    } else {
        let mut pixels = Vec::new();
        shadow::crop(&rgb, rect, changed, &mut pixels);
        sink.damage(changed, &pixels).await?;
    }
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
    sink: &VideoSink,
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
            let shape = CursorShape::from_rgba(
                w,
                h,
                hx,
                hy,
                CursorUnit::Pixels,
                &masked_bgrx_to_rgba(&pixels, &mask, w),
            )?;
            debug!("vnc: cursor {w}x{h} hotspot ({hx},{hy}), {} bytes", shape.png.len());
            (CursorState::Shape(shape.clone()), ServerMsg::Cursor(Some(shape)))
        }
    };
    *cursor.lock().unwrap() = state;
    sink.msg(msg).await
}

/// Read a Cursor With Alpha rect's payload — its own encoding, then the shape —
/// and forward it as [`read_cursor`] does.
///
/// Raw is the only encoding read. The extension lets a server pick any encoding
/// this end listed, but TigerVNC and QEMU, the servers that speak it, and
/// wlshare all send Raw, and ZRLE's CPIXEL would drop the very alpha the
/// extension exists for. Anything else cannot be skipped — its length is not in
/// the header — so it ends the session with the encoding named.
///
/// The spec's pixels are `R, G, B, A` with the alpha premultiplied, divided back
/// out here for a PNG. QEMU sends its cursor's native `B, G, R, A` words
/// instead; a greyscale pointer, which is what a guest's usually is, reads the
/// same either way, and that is the extent of the allowance made for it.
async fn read_alpha_cursor<R: AsyncRead + Unpin>(
    reader: &mut R,
    cursor: &SharedCursor,
    (hx, hy, w, h): (u16, u16, u16, u16),
    sink: &VideoSink,
) -> anyhow::Result<()> {
    let encoding = reader.read_i32().await?;
    anyhow::ensure!(
        encoding == ENCODING_RAW,
        "a Cursor With Alpha rect in encoding {}; only Raw is read",
        encoding_label(encoding)
    );
    let pixels_len = usize::from(w) * usize::from(h) * BPP;
    let (state, msg) = if w == 0 || h == 0 {
        debug!("vnc: server hid the pointer (alpha cursor)");
        (CursorState::Hidden, ServerMsg::Cursor(None))
    } else if w > MAX_CURSOR_DIM || h > MAX_CURSOR_DIM {
        // As for the masked cursor: the server still is not drawing it.
        warn!("vnc: ignoring an oversized {w}x{h} cursor");
        discard(reader, pixels_len as u64).await?;
        (CursorState::Hidden, ServerMsg::Cursor(None))
    } else {
        let mut rgba = vec![0u8; pixels_len];
        reader.read_exact(&mut rgba).await?;
        unpremultiply(&mut rgba);
        let shape = CursorShape::from_rgba(w, h, hx, hy, CursorUnit::Pixels, &rgba)?;
        debug!("vnc: alpha cursor {w}x{h} hotspot ({hx},{hy}), {} bytes", shape.png.len());
        (CursorState::Shape(shape.clone()), ServerMsg::Cursor(Some(shape)))
    };
    *cursor.lock().unwrap() = state;
    sink.msg(msg).await
}

/// Divide premultiplied RGBA's alpha back out of its colour, in place, for a
/// PNG, whose alpha is straight. A fully transparent pixel is cleared to black,
/// as [`masked_bgrx_to_rgba`] clears one outside the mask.
fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<BPP>().0 {
        let a = u32::from(px[3]);
        for c in &mut px[..3] {
            *c = (u32::from(*c) * 255 + a / 2).checked_div(a).map_or(0, |v| v.min(255) as u8);
        }
    }
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
    sink: &VideoSink,
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

    debug!(
        "vnc: ExtendedDesktopSize reason={reason} status={status} {w}x{h}, {screens} screen(s)"
    );
    if first.is_some() {
        let mut d = desktop.lock().unwrap();
        if d.screen.is_none() {
            info!("vnc: server declared SetDesktopSize support");
        }
        d.screen = first;
    }

    let resized = if reason == 1 && status != 0 {
        // Our SetDesktopSize did not take effect here, so the size stays what it
        // was — the rect's own dimensions are not trusted for it.
        //
        // The statuses are the extension's — 1 prohibited, 2 out of resources, 3
        // invalid layout — plus neatvnc's own 4, "request forwarded": wayvnc has
        // handed the size to the compositor and the resize arrives as a
        // server-initiated rect a moment later, so that one is not a refusal.
        // Its 1 has a cause worth naming: wayvnc lets only the first client that
        // resized change the layout while that client stays connected — a browser
        // on another gateway against the same server, say — and every other
        // client's request is prohibited until it disconnects.
        match status {
            4 => debug!("vnc: server forwarded the SetDesktopSize; the new size arrives as its own rect"),
            1 => warn!(
                "vnc: server prohibited the SetDesktopSize; wayvnc grants the layout to the \
                 first client that resized and refuses every other while it stays connected"
            ),
            2 => warn!("vnc: server rejected SetDesktopSize: out of resources"),
            3 => warn!("vnc: server rejected SetDesktopSize: invalid layout"),
            _ => warn!("vnc: server rejected SetDesktopSize (status {status})"),
        }
        // A scale report may have cleared the browser's canvas on the strength
        // of this request's rect repainting it — see [`read_output_scale`].
        // Refused, it repaints nothing, so the pixels are asked for as they are.
        if status != 4 && std::mem::take(&mut desktop.lock().unwrap().repaint_owed) {
            let size = desktop.lock().unwrap().size;
            debug!("vnc: asking for the whole framebuffer; the relabelled canvas is still blank");
            send(uplink, &update_request(false, size)).await?;
        }
        false
    } else {
        let scale = desktop.lock().unwrap().generic_scale();
        apply_resize(desktop, shadow, (w, h), scale, sink).await?
    };

    // Replay a viewport report that arrived before support was declared. The
    // stash is read here, after the browser has been told the new size, so a
    // window that moved on meanwhile — a request sent from the input side while
    // that message was on its way — leaves nothing stale to replay.
    send_decided(uplink, desktop, |d| {
        let want = d.pending.take()?;
        let msg = d.generic_resize(want);
        if msg.is_some() {
            debug!("vnc: desktop resize to {}x{} points replayed", want.0, want.1);
        }
        msg
    })
    .await?;
    Ok(resized)
}

/// Handle the wlshare `OutputScale` report — see [`MSG_WLSHARE_DENSITY`] and
/// docs/wlshare-density.md.
///
/// The report names the framebuffer size it describes and the scale it is drawn
/// at. The scale labels every generic rect from here on; when the size is the
/// current framebuffer's, the label changes now, because the same pixels shown
/// at a new density are a new canvas. Otherwise the rect carrying the new size
/// is about to arrive and takes the label then. The first report is also the
/// server's announcement that it listens: it declares the browser's density
/// back, carrying the held window in pixels at that density, and the server
/// sets the output's mode and scale to it in one configuration and reports
/// again — the desktop is then drawn once, in the right pixels. Any
/// report that changes the scale, or answers a declaration, re-asks for the
/// window in the new pixels, so a desktop toggled to 2x on the host keeps
/// filling the window rather than shrinking to half of it. A report answering
/// a declaration the browser's density has since left behind declares the new
/// density instead, so one transition is in flight at a time. And a relabel
/// that no resize request follows asks for the whole framebuffer: the relabel
/// cleared the browser's canvas, and outside a `FramebufferUpdate` nothing
/// else would paint the parts of the desktop that never change.
async fn read_output_scale<R: AsyncRead + Unpin>(
    reader: &mut R,
    uplink: &SharedUplink,
    desktop: &SharedDesktop,
    shadow: &SharedShadow,
    sink: &VideoSink,
) -> anyhow::Result<()> {
    let mut body = [0u8; OUTPUT_SCALE_BODY];
    reader.read_exact(&mut body).await?;
    let report = OutputScale::parse(&body)?;
    debug!(
        "vnc: server reports its {}x{} framebuffer at {}x",
        report.size.0, report.size.1, report.scale
    );
    let (relabel, reask) = {
        // The uplink first, then the decision — see [`send_decided`] — so a
        // declaration the input side decides from this report's state cannot
        // reach the wire ahead of the one decided here.
        let mut up = uplink.lock().await;
        let (relabel, declare, reask) = {
            let mut d = desktop.lock().unwrap();
            let first = d.density != Density::Reported;
            d.density = Density::Reported;
            let changed = d.wire_scale != Some(report.scale);
            d.wire_scale = Some(report.scale);
            let relabel = report.size == d.size && d.scale != report.scale;
            // This report answers an outstanding declaration, if one was out.
            let answered = std::mem::take(&mut d.following);
            let declared = d.host_density;
            // Declared on the first report, and again when the declaration
            // just answered is no longer the browser's density: a change that
            // arrived while the server was busy waited its turn here, and so did
            // a switch of output, which leaves nothing declared.
            let moved_on =
                answered && d.declared.is_none_or(|was| (was - declared).abs() > 0.005);
            let declare = (first || moved_on).then(|| d.declare_density(declared)).flatten();
            // Not while a declaration is out: the report answering it re-asks in
            // the pixels the server settles on.
            let reask = (changed || first || answered) && !d.following;
            (relabel, declare.map(|msg| (declared, msg)), reask)
        };
        if let Some((declared, msg)) = declare {
            info!("vnc: the server reports pixel density; declaring the client's {declared}x");
            up.send(&msg).await?;
        }
        (relabel, reask)
    };
    if relabel {
        apply_resize(desktop, shadow, report.size, report.scale, sink).await?;
        desktop.lock().unwrap().repaint_owed = true;
    }
    let mut resized = false;
    if reask {
        // The window's size, asked for again in the new pixels — or for the
        // first time, if the request was held for this report. Decided under
        // the uplink, after the browser has its new canvas, from whatever the
        // window wants by then. Not when the report already names the pixels
        // the window wants: that rect is on its way, and asking again would
        // only redraw it.
        resized = send_decided(uplink, desktop, |d| {
            d.pending
                .take()
                .or(d.viewport)
                .filter(|&points| d.generic_pixels(points) != report.size)
                .and_then(|points| d.generic_resize(points))
        })
        .await?;
    }
    // A relabel emptied the browser's canvas, and this message is outside any
    // `FramebufferUpdate`, so no request follows it by itself. A resize request
    // repaints through its rect, and a declaration through the report that
    // answers it, which decides here again — and a report naming a size the
    // framebuffer does not have yet announces the rect that repaints. With none
    // of those, the whole framebuffer is asked for now, or the parts of the
    // desktop that never change would stay blank.
    let repaint = {
        let mut d = desktop.lock().unwrap();
        let repaint = d.repaint_owed && !resized && !d.following && report.size == d.size;
        if repaint {
            d.repaint_owed = false;
        }
        repaint
    };
    if repaint {
        debug!(
            "vnc: asking for the whole {}x{} framebuffer again at its new density",
            report.size.0, report.size.1
        );
        send(uplink, &update_request(false, report.size)).await?;
    }
    Ok(())
}

/// Handle the wlshare `OutputList` — see [`MSG_WLSHARE_OUTPUTS`] and
/// docs/wlshare-outputs.md.
///
/// The list is what the display picker shows on a generic target, and the id it
/// names as shared is where the checkmark goes. It arrives as the answer to the
/// `SetEncodings` that asked for it, whenever the compositor's outputs change,
/// and as the answer to every [`ClientMsg::SelectDisplay`] this engine forwards —
/// including one the server would not or could not honour, which is answered with
/// the list as it stands. Nothing here is optimistic for the same reason the Apple
/// path has none: the browser holds no display state, so a selection that did not
/// happen leaves the menu agreeing with what is on the canvas.
///
/// The pixels of the switch itself need nothing from here. wlshare takes the new
/// output's size into its framebuffer blank and reports the geometry, so a
/// different size arrives as an `ExtendedDesktopSize` rectangle and a same-sized
/// output as the repaint the selection asked for.
async fn read_output_list<R: AsyncRead + Unpin>(
    reader: &mut R,
    uplink: &SharedUplink,
    desktop: &SharedDesktop,
    display: &SharedDisplay,
    sink: &VideoSink,
) -> anyhow::Result<()> {
    let mut header = [0u8; OUTPUT_LIST_HEADER];
    reader.read_exact(&mut header).await?;
    let count = u16::from_be_bytes([header[1], header[2]]);
    let active = u32::from_be_bytes([header[3], header[4], header[5], header[6]]);
    // A count no desk reaches. The entries are self-describing, so an
    // implausible one is a server that has lost its place in the stream rather
    // than a session to keep reading.
    anyhow::ensure!(
        count <= MAX_OUTPUTS,
        "the server listed {count} outputs, over the {MAX_OUTPUTS} this client reads"
    );
    let mut displays = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let mut entry = [0u8; OUTPUT_ENTRY];
        reader.read_exact(&mut entry).await?;
        let mut name = vec![0u8; usize::from(entry[13])];
        reader.read_exact(&mut name).await?;
        displays.push(output_info(&entry, &name));
    }
    debug!(
        "vnc: the server lists {} output(s), sharing id {active}",
        displays.len()
    );
    let (msg, switched) = {
        let mut state = display.lock().unwrap();
        let changed = state.displays != displays || state.active != active;
        // The shared output moved to another: not the session's first list, and
        // not to nothing at all.
        let switched = state.listed && state.active != active && active != 0;
        state.displays = displays;
        state.active = active;
        state.listed = true;
        // Sent only on a change, as the Apple path sends its own: every
        // `SetEncodings` is answered with a list, and a reconnecting browser is
        // told the current one by the reattach path.
        (changed.then(|| state.displays_msg()).flatten(), switched)
    };
    if let Some(msg) = msg {
        sink.msg(msg).await?;
    }
    if switched {
        send_decided(uplink, desktop, DesktopState::output_switched).await?;
    }
    Ok(())
}

/// One `OutputList` entry as the picker lists it.
///
/// The label is the compositor's own name for the output — `DP-2`, `HEADLESS-1` —
/// which is what the person at that desk sees in their own display settings, and
/// the only name anything here has for it. The detail is the Apple path's, and
/// says the same thing: the points the desktop occupies, then the density that
/// earns it more pixels, stated only when there is one to state.
fn output_info(entry: &[u8; OUTPUT_ENTRY], name: &[u8]) -> DisplayInfo {
    let id = u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]]);
    let pixels = (
        u16::from_be_bytes([entry[4], entry[5]]),
        u16::from_be_bytes([entry[6], entry[7]]),
    );
    let fixed = u32::from_be_bytes([entry[8], entry[9], entry[10], entry[11]]);
    // A zero scale would divide the size away; the label falls back to the
    // pixels, which is what an unscaled output's label is anyway.
    let scale = if fixed > 0 { fixed as f32 / 65536.0 } else { UNSCALED };
    let logical = (
        (f32::from(pixels.0) / scale).round() as u32,
        (f32::from(pixels.1) / scale).round() as u32,
    );
    let suffix = if scale > 1.005 {
        format!(" at {scale}x")
    } else {
        String::new()
    };
    DisplayInfo {
        id,
        label: String::from_utf8_lossy(name).into_owned(),
        detail: format!("{}×{}{suffix}", logical.0, logical.1),
        // wlroots names no primary output, and a list of two monitors has none
        // to mark.
        main: false,
        virtual_display: entry[12] & OUTPUT_HEADLESS != 0,
    }
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
    sink: &VideoSink,
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
    sink.reset_render();
    info!(
        "vnc: desktop resized from {}x{} px at {}x to {}x{} px at {scale}x ({}x{} pt)",
        was.0.0,
        was.0.1,
        was.1,
        new.0,
        new.1,
        f32::from(new.0) / scale,
        f32::from(new.1) / scale
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
    sink: &VideoSink,
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
    virtual_display: bool,
    rearm_pasteboard: bool,
    sink: &VideoSink,
) -> anyhow::Result<bool> {
    let Shared { uplink, desktop, shadow, display, hp_wake, .. } = shared;
    // The length counts the bytes after itself — see [`vnc_apple::parse_layout`].
    let declared = reader.read_u16().await?;
    let mut payload = vec![0u8; usize::from(declared)];
    reader.read_exact(&mut payload).await?;
    let layout = if virtual_display {
        vnc_apple::parse_virtual_display_layout(&payload)?
    } else {
        vnc_apple::parse_layout(&payload)?
    };

    // The composition goes first: it says how the framebuffer the resize names
    // is presented, and a browser must not present one layout's pixels through
    // another's regions. So it says whether that resize is coming, and the
    // browser holds it until then rather than laying it over the old pixels.
    let mosaic = if virtual_display { None } else { layout.mosaic() };
    let resize = {
        let d = desktop.lock().unwrap();
        d.size != layout.backing || d.scale != layout.scale()
    };
    let mosaic_msg = {
        let mut state = display.lock().unwrap();
        (state.mosaic != mosaic).then(|| {
            state.mosaic.clone_from(&mosaic);
            ServerMsg::Mosaic { regions: mosaic.unwrap_or_default(), resize }
        })
    };
    if let Some(msg) = mosaic_msg {
        sink.msg(msg).await?;
    }
    let resized = apply_resize(desktop, shadow, layout.backing, layout.scale(), sink).await?;
    if virtual_display {
        let mut d = desktop.lock().unwrap();
        d.hp.layout(resized, tokio::time::Instant::now());
        d.laid_out = true;
        // A new display stopped the media stream, whoever asked for it: the
        // picture is ZRLE's until the stream is offered for it and delivers.
        if resized {
            d.media_live = false;
            if let Some(media) = &shared.media {
                media.lock().unwrap().stopped();
            }
        }
        drop(d);
        hp_wake.notify_one();
    }

    // The Mac says which screen it is sending, so nothing here has to be inferred
    // from what was asked for. `current` is a screen id, or `None` for the combined
    // view of all of them — which is what a session starts on, and which
    // [`DisplayState::COMBINED`] is the client-facing name for.
    let host_density = desktop.lock().unwrap().host_density;
    // Held from the decision to the send, as every scale request is, so the Mac
    // receives them in the order they were decided in.
    let mut out = uplink.lock().await;
    let (msg, server_scaling) = {
        let mut state = display.lock().unwrap();
        let mut infos = layout.infos();
        // With more than one screen there is a combined view to go back to, and it
        // has to be listed or a client that picks a screen can never leave it. With
        // one screen there is nothing to combine, and the entry would be the same
        // picture under a second name.
        if infos.len() > 1 {
            // The points the screens span together, as Apple's viewer labels the
            // same entry — not the framebuffer, which is only the union of every
            // screen while this view is the one selected.
            let (w, h) = layout.points_spanned();
            let detail = format!("{w}×{h}");
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
        state.listed = true;
        let server_scaling = if virtual_display {
            None
        } else {
            state.accept_apple_layout(&layout, host_density)
        };
        // Sent only on a change, since a client holds no display state of its own
        // and the checkmark is the only thing telling it what it is looking at. Most
        // layouts change neither half — one arrives at every login and lock — and
        // say nothing new.
        (changed.then(|| state.displays_msg()).flatten(), server_scaling)
    };
    if let Some(scale) = server_scaling {
        debug!("vnc: asking the Mac for {scale}x server scaling");
        out.send(&vnc_apple::set_server_scaling(scale)).await?;
    }
    drop(out);
    if let Some(msg) = msg {
        sink.msg(msg).await?;
    }

    // A layout that answered nothing leaves a High Performance change out, and
    // the region stays narrowed until the one that answers it — see
    // [`HP_HOLD_REQUEST`].
    let armed = desktop.lock().unwrap().poll_size();
    let mut uplink = uplink.lock().await;
    if rearm_pasteboard {
        uplink.send(&vnc_apple_clipboard::auto_pasteboard(true)).await?;
    }
    debug!(
        "vnc: arming auto framebuffer updates for {}x{}",
        armed.0, armed.1
    );
    uplink.send(&vnc_apple::auto_framebuffer_update(armed)).await?;
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

    /// A delta in the pixels it stands for, whatever unit it was reported in.
    fn pixels(delta: f32, unit: WheelUnit) -> f32 {
        match unit {
            WheelUnit::Pixel => delta,
            WheelUnit::Line => delta * Self::LINE_PX,
            WheelUnit::Page => delta * Self::PAGE_LINES * Self::LINE_PX,
        }
    }

    /// Whole pulses to send for one wheel event, as (horizontal, vertical).
    fn pulses(&mut self, dx: f32, dy: f32, unit: WheelUnit) -> (i32, i32) {
        let px = |delta: f32| Self::pixels(delta, unit);
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
/// generic server honours it. A Mac's agent reads the same mask positionally
/// instead, as CGMouseButton numbers: bit 2 = *right*, bit 3 = *middle*.
/// `screensharingd` swaps the two bits back for every viewer except one that
/// answered 3.888 or 3.889, so on Apple's revision a right-click sent by the book
/// lands as a middle-click — the button macOS does nothing with — which is what a
/// dead right button in High Performance was. Measured on macOS 26.6 by holding
/// each button through a live session and reading `CGEventSource.buttonState` on
/// the Mac. See docs/apple-vnc-889.md.
enum Buttons {
    /// The RFB convention: bit 2 = middle, bit 3 = right.
    Rfb,
    /// A Mac's positional reading on 003.889: bit 2 = right, bit 3 = middle.
    Apple,
}

impl Buttons {
    fn new(apple: bool) -> Self {
        if apple {
            Self::Apple
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
            (Self::Apple, MouseButton::Middle) => Some(0x04),
            (Self::Apple, MouseButton::Right) => Some(0x02),
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
    macos: bool,
) -> Vec<Vec<u8>> {
    // A Mac reads the modifier keysyms by its own table (see
    // [`keymap::apple_keysym`]); every other server takes the X11 ones.
    let keysym = if macos { keymap::apple_keysym } else { keymap::keysym };
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
            // Screen Sharing scrolls only on a mask of exactly 0x08 or 0x10 and
            // posts any other mask as buttons by bit position, so a pulse there goes
            // without the held buttons — which the release restores — and the
            // horizontal bits, clicks on buttons 5 and 6, are not sent at all. See
            // docs/apple-vnc-889.md, "A Mac scrolls only on a lone wheel bit".
            let (axes, held): (&[_], u8) = match wheel {
                Wheel::Apple { .. } => (&[(py, 0x08, 0x10)], 0),
                Wheel::Notch => (&[(py, 0x08, 0x10), (px, 0x20, 0x40)], *button_mask),
            };
            let mut out = Vec::new();
            for &(pulses, negative_bit, positive_bit) in axes {
                let bit = if pulses > 0 { positive_bit } else { negative_bit };
                for _ in 0..pulses.abs() {
                    out.push(pointer_event(held | bit, *last_pos).to_vec());
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
                let held = |keys: [&str; 2]| keys.iter().any(|k| pressed_keys.contains_key(*k));
                let shift_down = held(["ShiftLeft", "ShiftRight"]);
                // A Mac adds Shift for an uppercase keysym, and Caps Lock leaves its
                // shortcuts alone: Command-Z under Caps Lock is still Undo.
                let shortcut =
                    macos && (held(["MetaLeft", "MetaRight"]) || held(["ControlLeft", "ControlRight"]));
                let is_letter = matches!(code.as_bytes(), [b'K', b'e', b'y', b'A'..=b'Z']);
                let shift = if is_letter && !shortcut { shift_down ^ caps } else { shift_down };
                match keysym(&code, shift) {
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
                    .or_else(|| keysym(&code, false))
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
        // bridge handles them and they never reach here. `CameraFormat` is the
        // camera socket's opening message, never forwarded as input, and a VNC
        // target carries no camera anyway.
        ClientMsg::Connect { .. }
        | ClientMsg::Disconnect
        | ClientMsg::PaintAck { .. }
        | ClientMsg::CameraFormat { .. } => Vec::new(),
        // Intercepted by the input loop, which is where the requested screen is
        // checked — see the `SelectDisplay` branch there. The Apple extension
        // supplies the selectable list on either transport, and wlshare's outputs
        // extension supplies it on a generic one; a server with neither never
        // sends a list, so no id ever names anything.
        ClientMsg::SelectDisplay { .. } => Vec::new(),
        // RFB has no touch: a contact is a thing only MS-RDPEI carries, and the
        // client offers the mode only after the RDP engine's `touchReady`,
        // which this engine never sends. Anything arriving here is dropped
        // rather than faked into a mouse — the trackpad gestures are that.
        ClientMsg::Touch { .. } => Vec::new(),
        // Intercepted by the input loop where it means something — a High
        // Performance virtual display follows the client's screen, by density
        // or by full resolution depending on `resize`. Everywhere else there is
        // nothing to act on: RFB has no backing scale, and a VNC server's
        // framebuffer is already the pixels it has. Clients send this
        // unconditionally rather than asking what the engine is, so it is
        // ignored here rather than treated as a client error.
        ClientMsg::HostDisplay { .. } => Vec::new(),
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
/// BlockBefore needs nothing done to honour it — the read loop reads and acts on one
/// message at a time, in order — and neither does BlockAfter on an echo sent at once.
/// One the loop holds stops its reading until it goes. So the whole of the obligation
/// is the echo, and the flags this end does not implement are dropped from it rather
/// than claimed.
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

/// The bytes after the type of a wlshare `OutputScale` report: padding, width,
/// height, then the scale.
const OUTPUT_SCALE_BODY: usize = 9;

/// The bytes after the type of a wlshare `OutputList`: padding, the entry count,
/// then the shared output's id.
const OUTPUT_LIST_HEADER: usize = 7;
/// The fixed part of one `OutputList` entry: id, width, height, scale, flags and
/// the length of the name that follows it.
const OUTPUT_ENTRY: usize = 14;
/// An entry's flags, bit 0: an output the compositor made rather than a monitor
/// somebody is sitting at.
const OUTPUT_HEADLESS: u8 = 1;
/// More outputs than a desk has. The list is read, not trusted.
const MAX_OUTPUTS: u16 = 64;

/// A wlshare `OutputScale` report: the server's framebuffer in pixels and the
/// scale it is drawn at — see docs/wlshare-density.md.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OutputScale {
    size: (u16, u16),
    scale: f32,
}

impl OutputScale {
    /// Decode the report's body. The scale is 16.16 unsigned fixed point; a zero
    /// is a server bug rather than a density, and is refused as such.
    fn parse(body: &[u8; OUTPUT_SCALE_BODY]) -> anyhow::Result<Self> {
        let size = (
            u16::from_be_bytes([body[1], body[2]]),
            u16::from_be_bytes([body[3], body[4]]),
        );
        let fixed = u32::from_be_bytes([body[5], body[6], body[7], body[8]]);
        anyhow::ensure!(fixed > 0, "the server reported a scale of zero for its {}x{} framebuffer", size.0, size.1);
        Ok(Self {
            size,
            scale: fixed as f32 / 65536.0,
        })
    }
}

/// The wlshare `ClientDensity` declaration, in `OutputScale`'s layout: padding,
/// the window's width and height in pixels at the declared density, then the
/// browser's density as 16.16 unsigned fixed point. Sent once the server has
/// reported, whenever the client's screen changes density, and on a switch of
/// output.
fn client_density(pixels: (u16, u16), scale: f32) -> [u8; 10] {
    let fixed = (f64::from(scale) * 65536.0).round().clamp(1.0, f64::from(u32::MAX)) as u32;
    let mut msg = [0u8; 10];
    msg[0] = MSG_WLSHARE_DENSITY;
    // msg[1]: padding
    msg[2..4].copy_from_slice(&pixels.0.to_be_bytes());
    msg[4..6].copy_from_slice(&pixels.1.to_be_bytes());
    msg[6..10].copy_from_slice(&fixed.to_be_bytes());
    msg
}

/// The wlshare `SelectOutput` request: the id of the output to share, after three
/// bytes of padding. Sent for a [`ClientMsg::SelectDisplay`] naming an id from
/// the last `OutputList`, and answered by the server with another list.
fn select_output(id: u32) -> [u8; 8] {
    let mut msg = [0u8; 8];
    msg[0] = MSG_WLSHARE_OUTPUTS;
    // msg[1..4]: padding
    msg[4..8].copy_from_slice(&id.to_be_bytes());
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
    for block in response.as_chunks_mut::<8>().0 {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    response
}

/// Pick the RFB security type to answer with.
///
/// The target's subtype decides first, not which credential fields happen to
/// be filled: `Ard` is a declaration that the far end is a Mac and that the
/// credentials name an account there. On a Mac that difference decides which
/// screen you get — see [`ard_authenticate`] — so a subtype the server cannot
/// answer is an error rather than a silent fall back to the anonymous path.
///
/// A plain `vnc` target has two credentials for two kinds of server, and takes
/// whichever the server can answer: `username` and `password` are an account
/// for RSA-AES, the encrypted type and so the one preferred when both are
/// possible; `vnc_password` is a secret belonging to the *machine* for
/// `VncAuth`, which tells the server nothing about who is connecting.
fn choose_security(
    types: &[u8],
    subtype: Option<Subtype>,
    password: &str,
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
    let rsa_aes = [
        vnc_rsa_aes::SECURITY_RSA_AES_256,
        vnc_rsa_aes::SECURITY_RSA_AES_128,
    ]
    .into_iter()
    .find(|t| types.contains(t));
    if !password.is_empty()
        && let Some(rsa_aes) = rsa_aes
    {
        return Ok(rsa_aes);
    }
    if !vnc_password.is_empty() && types.contains(&SECURITY_VNC_AUTH) {
        return Ok(SECURITY_VNC_AUTH);
    }
    if types.contains(&SECURITY_NONE) {
        return Ok(SECURITY_NONE);
    }
    let offers_vnc_auth = types.contains(&SECURITY_VNC_AUTH);
    let offers_rsa_aes = types.iter().any(|&t| Strength::of(t).is_some());
    anyhow::ensure!(
        !(offers_vnc_auth || offers_rsa_aes),
        "VNC server requires {} but the target has no {} configured",
        match (offers_rsa_aes, offers_vnc_auth) {
            (true, true) => "an account (RSA-AES) or a password (VncAuth)",
            (true, false) => "an account (RSA-AES)",
            _ => "a password",
        },
        match (offers_rsa_aes, offers_vnc_auth) {
            (true, true) => "username and password, nor vnc_password,",
            (true, false) => "username and password",
            _ => "vnc_password",
        }
    );
    anyhow::bail!(
        "no supported VNC security type (server offers {types:?}; \
         this client speaks None, VncAuth, RSA-AES and Apple's DH authentication)"
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
    for block in out.as_chunks_mut::<16>().0 {
        cipher.encrypt_block(block.into());
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
    for (i, px) in bgrx.as_chunks::<BPP>().0.iter().enumerate() {
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
        assert_eq!(
            choose_security(&MACOS_TYPES, Some(Subtype::Ard), "pw", "").unwrap(),
            SECURITY_ARD
        );
        // The very same server, answered anonymously, because that is what a
        // target without the subtype asked for — and it is what costs you the
        // Mac's own screen.
        assert_eq!(choose_security(&MACOS_TYPES, None, "", "pw").unwrap(), SECURITY_VNC_AUTH);
        // A subtype the server cannot answer is a configuration error, not a
        // reason to authenticate as nobody.
        let err = choose_security(&[SECURITY_VNC_AUTH, SECURITY_NONE], Some(Subtype::Ard), "pw", "")
            .unwrap_err();
        assert!(format!("{err:#}").contains("not macOS Screen Sharing"), "{err:#}");

        // The high-performance subtype authenticates identically — the dialect
        // above it differs, the security type does not — and names itself when the
        // server cannot answer.
        assert_eq!(
            choose_security(&MACOS_TYPES, Some(Subtype::ArdHighPerformance), "pw", "").unwrap(),
            SECURITY_ARD
        );
        let err = choose_security(&[SECURITY_NONE], Some(Subtype::ArdHighPerformance), "pw", "")
            .unwrap_err();
        assert!(format!("{err:#}").contains("\"ard-high-performance\""), "{err:#}");
    }

    /// Both Apple subtypes speak Apple's revision, as Apple's viewer does to every
    /// Mac whichever mode it then opens; only a generic target speaks RFB 3.8.
    #[test]
    fn the_dialect_follows_the_subtype() {
        assert_eq!(Dialect::of(None), Dialect::Rfb38);
        assert_eq!(Dialect::of(Some(Subtype::Ard)), Dialect::Apple889);
        assert_eq!(
            Dialect::of(Some(Subtype::ArdHighPerformance)),
            Dialect::Apple889
        );
        // The two bytes that are the whole visible difference on the wire.
        assert_eq!(Dialect::Rfb38.banner(), b"RFB 003.008\n");
        assert_eq!(Dialect::Apple889.banner(), b"RFB 003.889\n");
        assert_eq!(Dialect::Rfb38.client_init(), 1);
        assert_eq!(Dialect::Apple889.client_init(), 0x81);
    }

    /// macvm's enhanced name field (macOS 26.6.2), read as a bitmap, and a plain
    /// name, which has none.
    #[test]
    fn the_command_bitmap_comes_from_the_enhanced_name_field() {
        let mut field = vec![0, 0, 0, 0, 0, 0x52];
        field.extend_from_slice(&[0xbf, 0xf6, 0xe7, 0x2f, 0xec]);
        field.extend_from_slice(&[0; 11]);
        field.extend_from_slice(b"mac");
        let commands = apple_commands(&field).expect("an enhanced field");
        assert_eq!(commands[..5], [0xbf, 0xf6, 0xe7, 0x2f, 0xec]);
        assert!(vnc_apple::holds_high_performance(&commands));
        assert!(describe_desktop(&field).contains("up to 2 virtual displays"));

        assert_eq!(apple_commands(b"a desktop named at length"), None);
        assert_eq!(apple_commands(&field[..21]), None);
    }

    #[test]
    fn security_falls_back_the_way_it_always_did() {
        assert_eq!(choose_security(&[SECURITY_NONE], None, "", "").unwrap(), SECURITY_NONE);
        // A password with no VncAuth on offer is not a failure: an open server
        // is still an open server.
        assert_eq!(choose_security(&[SECURITY_NONE], None, "", "pw").unwrap(), SECURITY_NONE);
        let err = choose_security(&[SECURITY_VNC_AUTH], None, "", "").unwrap_err();
        assert!(format!("{err:#}").contains("requires a password"), "{err:#}");
        let err = choose_security(&[19], None, "", "pw").unwrap_err();
        assert!(format!("{err:#}").contains("no supported VNC security type"), "{err:#}");
    }

    /// wayvnc's offer with `enable_auth`: VeNCrypt, RSA-AES-256, RSA-AES-128.
    const WAYVNC_TYPES: [u8; 3] = [19, 129, 5];

    #[test]
    fn an_account_takes_rsa_aes_at_its_widest() {
        use crate::vnc_rsa_aes::{SECURITY_RSA_AES_128, SECURITY_RSA_AES_256};
        assert_eq!(
            choose_security(&WAYVNC_TYPES, None, "pw", "").unwrap(),
            SECURITY_RSA_AES_256
        );
        assert_eq!(
            choose_security(&[SECURITY_RSA_AES_128, SECURITY_VNC_AUTH], None, "pw", "").unwrap(),
            SECURITY_RSA_AES_128
        );
        // Encrypted beats the classic challenge when the target could do either;
        // the classic one is still what a vnc_password-only target gets.
        assert_eq!(
            choose_security(&[SECURITY_VNC_AUTH, SECURITY_RSA_AES_128], None, "pw", "vncpw").unwrap(),
            SECURITY_RSA_AES_128
        );
        assert_eq!(
            choose_security(&[SECURITY_VNC_AUTH, SECURITY_RSA_AES_128], None, "", "vncpw").unwrap(),
            SECURITY_VNC_AUTH
        );
        // The Apple subtype still wants Apple's type, whatever else is offered.
        let err = choose_security(&WAYVNC_TYPES, Some(Subtype::Ard), "pw", "").unwrap_err();
        assert!(format!("{err:#}").contains("not macOS Screen Sharing"), "{err:#}");
        // wlshare offers RSA-AES at both widths and nothing else; an account
        // takes the widest.
        assert_eq!(
            choose_security(&[SECURITY_RSA_AES_256, SECURITY_RSA_AES_128], None, "pw", "").unwrap(),
            SECURITY_RSA_AES_256
        );
        // And the refusal says which credential is missing.
        let err = choose_security(&WAYVNC_TYPES, None, "", "vncpw").unwrap_err();
        assert!(format!("{err:#}").contains("username and password"), "{err:#}");
        let err = choose_security(&[SECURITY_VNC_AUTH, SECURITY_RSA_AES_128], None, "", "").unwrap_err();
        assert!(format!("{err:#}").contains("nor vnc_password"), "{err:#}");
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
        for block in plain.as_chunks_mut::<16>().0 {
            cipher.decrypt_block(block.into());
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
            rfb38_encoding_list(false, false, false, false),
            vec![
                ENCODING_COPY_RECT,
                ENCODING_ZRLE,
                ENCODING_ZLIB,
                ENCODING_HEXTILE,
                ENCODING_RRE,
                ENCODING_RAW,
                ENCODING_CURSOR,
                ENCODING_CURSOR_WITH_ALPHA,
                ENCODING_CONTINUOUS_UPDATES,
                ENCODING_FENCE,
                ENCODING_EXTENDED_DESKTOP_SIZE,
                ENCODING_DESKTOP_SIZE,
                ENCODING_WLSHARE_DENSITY,
                ENCODING_WLSHARE_OUTPUTS,
            ]
        );

        // Every pixel encoding advertised is one this side can be handed. A rect
        // header alone is enough to prove it: an unrecognised encoding bails with
        // "not advertised" before any payload is read, and the promised ones do not.
        //
        // Pixel encodings are the non-negative ones, the wlshare requests
        // aside: spelt in ASCII they are positive, and pseudo-encodings all the
        // same. The pseudo-encodings are excluded because a server never sends
        // one as a rectangle at all — the clipboard's arrives as a
        // ServerCutText, the density report and the output list as their own
        // messages, and the audio announcement is an empty rectangle with no
        // pixels behind it.
        let pixel_encodings = rfb38_encoding_list(true, true, true, true)
            .into_iter()
            .filter(|encoding| {
                *encoding >= 0
                    && ![ENCODING_WLSHARE_DENSITY, ENCODING_WLSHARE_OUTPUTS, vnc_camera::ENCODING, vnc_mic::ENCODING, vnc_audio::ENCODING]
                        .contains(encoding)
            });
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

    /// Both Apple modes ask for the display layout and ZRLE, and for none of the
    /// generic extensions: the pasteboard is Apple's own protocol, the only sound
    /// is the media stream's, and a Mac reports its densities and screens in the
    /// layout.
    #[test]
    fn a_mac_is_asked_for_its_layout_and_zrle_and_no_generic_extension() {
        let encodings = vnc_apple::ENCODINGS;
        assert!(encodings.contains(&vnc_apple::ENCODING_DISPLAY_LAYOUT));
        assert!(encodings.contains(&ENCODING_ZRLE));
        assert!(!encodings.contains(&ENCODING_ZLIB));
        for generic in [
            vnc_clipboard::ENCODING,
            vnc_audio::ENCODING,
            vnc_camera::ENCODING,
            vnc_mic::ENCODING,
            ENCODING_WLSHARE_DENSITY,
            ENCODING_WLSHARE_OUTPUTS,
        ] {
            assert!(!encodings.contains(&generic), "{generic:#x}");
        }
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

    // ── Cursor With Alpha pseudo-encoding ───────────────────────────────────

    #[tokio::test]
    async fn an_alpha_cursor_is_unpremultiplied_into_an_rgba_png() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (sink, mut rx) = test_sink();
        // 3x1, premultiplied: opaque red, a half-transparent white, and a
        // transparent pixel whose colour must not survive.
        let mut payload = ENCODING_RAW.to_be_bytes().to_vec();
        payload.extend_from_slice(&[255, 0, 0, 255, 128, 128, 128, 128, 7, 7, 7, 0]);
        let mut reader = payload.as_slice();

        read_alpha_cursor(&mut reader, &cursor, (2, 0, 3, 1), &sink).await.unwrap();

        let shape = match forwarded(&sink, &mut rx).await.unwrap() {
            ServerMsg::Cursor(Some(shape)) => shape,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!((shape.w, shape.h, shape.hx, shape.hy), (3, 1, 2, 0));
        assert!(!shape.point_sized, "RFB cursors are framebuffer pixels");
        assert_eq!(
            decode_rgba(&shape.png),
            (3, 1, vec![255, 0, 0, 255, 255, 255, 255, 128, 0, 0, 0, 0])
        );
        match cursor_msg(&cursor) {
            Some(ServerMsg::Cursor(Some(cached))) => assert_eq!(cached, shape),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_alpha_cursor_hides_the_pointer_after_its_encoding() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (sink, mut rx) = test_sink();
        // The encoding is there even with no pixels after it; a trailing byte
        // stands in for the next rect.
        let mut payload = ENCODING_RAW.to_be_bytes().to_vec();
        payload.push(0xAB);
        let mut reader = payload.as_slice();
        read_alpha_cursor(&mut reader, &cursor, (0, 0, 0, 0), &sink).await.unwrap();
        assert_eq!(reader, &[0xAB]);
        assert!(matches!(forwarded(&sink, &mut rx).await, Some(ServerMsg::Cursor(None))));
        assert!(matches!(cursor_msg(&cursor), Some(ServerMsg::Cursor(None))));
    }

    #[tokio::test]
    async fn an_oversized_alpha_cursor_is_drained_and_hides_the_pointer() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (sink, mut rx) = test_sink();
        let (w, h) = (MAX_CURSOR_DIM + 1, 1);
        let mut payload = ENCODING_RAW.to_be_bytes().to_vec();
        payload.extend(std::iter::repeat_n(0u8, usize::from(w) * BPP));
        payload.push(0xAB);
        let mut reader = payload.as_slice();
        read_alpha_cursor(&mut reader, &cursor, (0, 0, w, h), &sink).await.unwrap();
        assert_eq!(reader, &[0xAB]);
        assert!(matches!(forwarded(&sink, &mut rx).await, Some(ServerMsg::Cursor(None))));
    }

    /// An encoding other than Raw has no length this end can skip by, so it is
    /// an error naming it rather than a guess at where the next rect starts.
    #[tokio::test]
    async fn an_alpha_cursor_in_another_encoding_ends_the_session() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (sink, _rx) = test_sink();
        let payload = ENCODING_ZRLE.to_be_bytes();
        let err = read_alpha_cursor(&mut payload.as_slice(), &cursor, (0, 0, 2, 2), &sink)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("encoding 16"), "{err:#}");
        assert_eq!(*cursor.lock().unwrap(), CursorState::ServerDrawn);
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

    /// The two wlshare requests, checked byte by byte against
    /// docs/wlshare-density.md and docs/wlshare-outputs.md rather than through
    /// the encoder's own eyes. Both are asked of every generic server, after
    /// every encoding that decides pixels, and of no Mac (see
    /// `a_mac_is_asked_for_its_layout_and_zrle_and_no_generic_extension`).
    #[test]
    fn the_wlshare_extensions_are_asked_of_every_generic_server() {
        assert_eq!(ENCODING_WLSHARE_DENSITY, i32::from_be_bytes(*b"WLSH"));
        assert_eq!(ENCODING_WLSHARE_OUTPUTS, i32::from_be_bytes(*b"WLSO"));
        for clipboard in [false, true] {
            let generic = rfb38_encoding_list(clipboard, false, false, false);
            assert_eq!(&generic[generic.len() - 2..], &[ENCODING_WLSHARE_DENSITY, ENCODING_WLSHARE_OUTPUTS]);
        }
    }

    // ── The outputs extension ───────────────────────────────────────────────

    /// One output as these tests spell it, before it is bytes.
    struct Listed {
        id: u32,
        name: &'static str,
        size: (u16, u16),
        scale: f32,
        headless: bool,
    }

    /// The body of an `OutputList` after its message type, built from
    /// docs/wlshare-outputs.md rather than from [`output_info`]'s own reading.
    fn output_list_body(active: u32, outputs: &[Listed]) -> Vec<u8> {
        let mut body = vec![0u8]; // padding
        body.extend_from_slice(&(outputs.len() as u16).to_be_bytes());
        body.extend_from_slice(&active.to_be_bytes());
        for output in outputs {
            body.extend_from_slice(&output.id.to_be_bytes());
            body.extend_from_slice(&output.size.0.to_be_bytes());
            body.extend_from_slice(&output.size.1.to_be_bytes());
            body.extend_from_slice(&((output.scale * 65536.0) as u32).to_be_bytes());
            body.push(u8::from(output.headless));
            body.push(output.name.len() as u8);
            body.extend_from_slice(output.name.as_bytes());
        }
        body
    }

    /// A wlshare output list becomes the display picker: the compositor's own
    /// names, the points each screen occupies, and the checkmark on the one being
    /// sent. A list that says nothing new is not forwarded again, since the
    /// browser holds no display state to correct.
    #[tokio::test]
    async fn an_output_list_becomes_the_display_picker() {
        let (uplink, _wire) = test_uplink();
        let desktop = shared_desktop((1920, 1080), None, None);
        let (sink, mut rx) = test_sink();
        let display: SharedDisplay = Arc::new(std::sync::Mutex::new(DisplayState::default()));
        let outputs = [
            Listed { id: 3, name: "DP-2", size: (1920, 1080), scale: 1.0, headless: false },
            Listed { id: 7, name: "HEADLESS-1", size: (3456, 1766), scale: 2.0, headless: true },
        ];

        let mut wire = output_list_body(7, &outputs);
        // A trailing byte stands in for the next message: it must survive.
        wire.push(0xAB);
        let mut reader = wire.as_slice();
        read_output_list(&mut reader, &uplink, &desktop, &display, &sink).await.unwrap();
        assert_eq!(reader, &[0xAB]);

        let Some(ServerMsg::Displays { active, displays }) = forwarded(&sink, &mut rx).await else {
            panic!("the list was not forwarded");
        };
        assert_eq!(active, 7);
        assert_eq!(displays.len(), 2);
        assert_eq!(displays[0].label, "DP-2");
        assert_eq!(displays[0].detail, "1920×1080", "1x states no density");
        assert!(!displays[0].virtual_display);
        assert_eq!(displays[1].label, "HEADLESS-1");
        assert_eq!(displays[1].detail, "1728×883 at 2x", "points, then the density");
        assert!(displays[1].virtual_display, "the headless flag is what marks it");
        assert_eq!(display.lock().unwrap().active, 7);

        // The same list again: nothing new to say.
        read_output_list(&mut output_list_body(7, &outputs).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();
        assert!(forwarded(&sink, &mut rx).await.is_none());

        // The server switched: the same list, a new checkmark.
        read_output_list(&mut output_list_body(3, &outputs).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();
        assert!(matches!(
            forwarded(&sink, &mut rx).await,
            Some(ServerMsg::Displays { active: 3, .. })
        ));
    }

    /// A list that empties is forwarded empty, which is what hides the picker: a
    /// browser left holding the last non-empty list would keep offering outputs
    /// the compositor no longer has. An empty list that says nothing new is not
    /// forwarded again.
    #[tokio::test]
    async fn an_output_list_that_empties_clears_the_picker() {
        let (uplink, _wire) = test_uplink();
        let desktop = shared_desktop((1920, 1080), None, None);
        let (sink, mut rx) = test_sink();
        let display: SharedDisplay = Arc::new(std::sync::Mutex::new(DisplayState::default()));
        let outputs = [
            Listed { id: 3, name: "DP-2", size: (1920, 1080), scale: 1.0, headless: false },
            Listed { id: 7, name: "HDMI-A-1", size: (1280, 800), scale: 1.0, headless: false },
        ];
        read_output_list(&mut output_list_body(3, &outputs).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();
        assert!(matches!(
            forwarded(&sink, &mut rx).await,
            Some(ServerMsg::Displays { active: 3, ref displays }) if displays.len() == 2
        ));

        // Every output went away: nothing listed, nothing shared.
        read_output_list(&mut output_list_body(0, &[]).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();
        let Some(ServerMsg::Displays { active, displays }) = forwarded(&sink, &mut rx).await else {
            panic!("the emptied list was not forwarded");
        };
        assert_eq!(active, 0);
        assert!(displays.is_empty());
        assert!(display.lock().unwrap().displays.is_empty());

        // Empty again: nothing new to say.
        read_output_list(&mut output_list_body(0, &[]).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();
        assert!(forwarded(&sink, &mut rx).await.is_none());

        // A browser attaching now — or reattaching with the old list still in
        // its menu — is told the list is empty, not left with what it had.
        assert!(matches!(
            display.lock().unwrap().displays_msg(),
            Some(ServerMsg::Displays { active: 0, ref displays }) if displays.is_empty()
        ));
    }

    /// A server that has listed nothing — one without the extension, or one that
    /// has not answered yet — gives a reattaching browser nothing to be told.
    #[test]
    fn no_list_received_is_nothing_to_replay() {
        assert!(DisplayState::default().displays_msg().is_none());
    }

    /// A switch of shared output declares the browser's density to the output
    /// now shared: it was declared to the one left behind, and the browser's own
    /// report is unchanged by the switch. The session's first list and a list
    /// that empties declare nothing.
    #[tokio::test]
    async fn a_switch_of_output_declares_the_density_again() {
        let (uplink, wire) = test_uplink();
        let desktop = shared_desktop((3456, 1766), None, None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Reported;
            d.wire_scale = Some(2.0);
            d.host_density = 2.0;
            d.declared = Some(2.0);
            d.viewport = Some((1728, 883));
        }
        let (sink, _rx) = test_sink();
        let display: SharedDisplay = Arc::new(std::sync::Mutex::new(DisplayState::default()));
        let outputs = [
            Listed { id: 3, name: "HEADLESS-1", size: (3456, 1766), scale: 2.0, headless: true },
            Listed { id: 7, name: "HEADLESS-2", size: (1920, 1080), scale: 1.0, headless: true },
        ];

        read_output_list(&mut output_list_body(3, &outputs).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();
        assert!(written(&wire).is_empty(), "the first list is no switch");

        // wlshare reports the new output's 1x before the list that moves the
        // checkmark; the browser is still 2x.
        desktop.lock().unwrap().wire_scale = Some(1.0);
        read_output_list(&mut output_list_body(7, &outputs).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();
        assert_eq!(written(&wire), client_density((3456, 1766), 2.0), "the window at the browser's density");
        {
            let d = desktop.lock().unwrap();
            assert!(d.following, "the new output is asked to follow");
            assert_eq!(d.declared, Some(2.0));
        }

        // What the run loop does with the browser's re-sent, unchanged report:
        // already declared, so nothing more.
        assert!(!send_decided(&uplink, &desktop, |d| d.host_density_changed(2.0)).await.unwrap());

        desktop.lock().unwrap().following = false;
        read_output_list(&mut output_list_body(0, &[]).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();
        assert_eq!(written(&wire), client_density((3456, 1766), 2.0), "no output left to declare to");
    }

    /// A switch while a declaration is out waits its turn, as a density change
    /// does: the report answering that declaration finds nothing declared to the
    /// output now shared and declares the browser's density to it.
    #[tokio::test]
    async fn a_switch_while_a_declaration_is_out_is_declared_by_the_answer() {
        let (uplink, wire) = test_uplink();
        let (sink, _rx) = test_sink();
        let desktop = shared_desktop((1920, 1080), None, None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Reported;
            d.wire_scale = Some(1.0);
            d.host_density = 1.0;
            d.viewport = Some((960, 540));
        }
        let display: SharedDisplay = Arc::new(std::sync::Mutex::new(DisplayState::default()));
        let outputs = [
            Listed { id: 3, name: "HEADLESS-1", size: (1920, 1080), scale: 1.0, headless: true },
            Listed { id: 7, name: "HEADLESS-2", size: (1920, 1080), scale: 1.0, headless: true },
        ];
        read_output_list(&mut output_list_body(3, &outputs).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();

        // The browser moved to a 2x screen: declared, and the server is busy.
        assert!(send_decided(&uplink, &desktop, |d| d.host_density_changed(2.0)).await.unwrap());
        let declared = client_density((1920, 1080), 2.0).to_vec();
        assert_eq!(written(&wire), declared);

        // The switch lands before the answer: nothing more goes out yet.
        read_output_list(&mut output_list_body(7, &outputs).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();
        assert_eq!(written(&wire), declared, "one declaration in flight at a time");

        // The answer is the new output at its own 1x: the browser's 2x goes to it.
        let body = output_scale_body((1920, 1080), 1.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1920, 1080)), &sink)
            .await
            .unwrap();
        assert_eq!(written(&wire), [declared.clone(), declared].concat());
        let d = desktop.lock().unwrap();
        assert!(d.following);
        assert_eq!(d.declared, Some(2.0));
    }

    /// A switch to an output already at the browser's density still declares,
    /// and the declaration carries the window: the new output's size is its own,
    /// not the window's, and it takes the window's in the same configuration.
    /// Measured against wlshare on a headless sway, where nothing else would
    /// ever ask.
    #[tokio::test]
    async fn a_switch_at_the_same_density_carries_the_window_to_the_new_output() {
        let (uplink, wire) = test_uplink();
        let (sink, _rx) = test_sink();
        let screen = Screen { id: 1, flags: 0 };
        let desktop = shared_desktop((1600, 1200), Some(screen), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Reported;
            d.wire_scale = Some(2.0);
            d.scale = 2.0;
            d.host_density = 2.0;
            d.declared = Some(2.0);
            d.viewport = Some((800, 600));
        }
        let display: SharedDisplay = Arc::new(std::sync::Mutex::new(DisplayState::default()));
        let outputs = [
            Listed { id: 3, name: "HEADLESS-1", size: (1600, 1200), scale: 2.0, headless: true },
            Listed { id: 7, name: "HEADLESS-2", size: (1280, 800), scale: 2.0, headless: true },
        ];
        read_output_list(&mut output_list_body(3, &outputs).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();

        read_output_list(&mut output_list_body(7, &outputs).as_slice(), &uplink, &desktop, &display, &sink)
            .await
            .unwrap();
        let declared = client_density((1600, 1200), 2.0);
        assert_eq!(written(&wire), declared);
        assert!(desktop.lock().unwrap().following, "the declaration is out whatever its scale");

        // The new output's own size arrives, then the answer: the window's
        // pixels at the same 2x, so nothing more is asked.
        desktop.lock().unwrap().size = (1280, 800);
        let body = output_scale_body((1600, 1200), 2.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1280, 800)), &sink)
            .await
            .unwrap();
        assert_eq!(written(&wire), declared);
        assert!(!desktop.lock().unwrap().following);
    }

    /// A count no compositor sends ends the session: the entries are
    /// self-describing, so a number like that is a stream this end has lost its
    /// place in rather than a desk with that many monitors.
    #[tokio::test]
    async fn an_implausible_output_count_is_refused() {
        let (uplink, _wire) = test_uplink();
        let desktop = shared_desktop((1920, 1080), None, None);
        let (sink, _rx) = test_sink();
        let display: SharedDisplay = Arc::new(std::sync::Mutex::new(DisplayState::default()));
        let mut body = vec![0u8];
        body.extend_from_slice(&(MAX_OUTPUTS + 1).to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        assert!(read_output_list(&mut body.as_slice(), &uplink, &desktop, &display, &sink).await.is_err());
    }

    /// The selection's wire: the id, big-endian, after three bytes of padding.
    #[test]
    fn a_selected_output_is_asked_for_by_id() {
        assert_eq!(select_output(7), [0xE1, 0, 0, 0, 0, 0, 0, 7]);
        assert_eq!(select_output(u32::MAX), [0xE1, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    // ── wlshare's audio extension ───────────────────────────────────────────

    /// A `FramebufferUpdate` whose whole content is the extension's
    /// announcement: one empty rectangle of encoding `WLSF`.
    fn audio_announcement() -> Vec<u8> {
        let mut wire = vec![0u8, 0]; // FramebufferUpdate, padding
        wire.extend_from_slice(&1u16.to_be_bytes());
        wire.extend_from_slice(&[0u8; 8]); // a 0x0 rect at the origin
        wire.extend_from_slice(b"WLSF");
        wire
    }

    /// A `FramebufferUpdate` of one Raw rectangle covering the 2x2 desktop these
    /// tests use: real pixels, and no announcement in front of them. The size
    /// matters — a 0x0 rect carries no pixels and would leave the extension
    /// still merely `Asked`.
    fn pixels_without_announcement() -> Vec<u8> {
        let mut wire = vec![0u8, 0];
        wire.extend_from_slice(&1u16.to_be_bytes());
        wire.extend_from_slice(&0u16.to_be_bytes()); // x
        wire.extend_from_slice(&0u16.to_be_bytes()); // y
        wire.extend_from_slice(&2u16.to_be_bytes()); // w
        wire.extend_from_slice(&2u16.to_be_bytes()); // h
        wire.extend_from_slice(&0i32.to_be_bytes()); // Raw
        wire.extend_from_slice(&[0x20u8; 2 * 2 * 4]); // BGRX
        wire
    }

    /// Begin and end spelt from the extension's text rather than built by this
    /// module: type 255, submessage 1, a big-endian operation.
    fn server_audio(operation: u16) -> Vec<u8> {
        let mut msg = vec![255u8, 1];
        msg.extend_from_slice(&operation.to_be_bytes());
        msg
    }

    /// 20 ms of 48 kHz 16-bit stereo, every sample `value`, as the frame
    /// message wlshare sends for it: type 0xE4, three bytes of padding, a
    /// big-endian length and the FLAC frame flacenc makes of it. flacenc shares
    /// nothing with the decoder the read loop uses.
    fn server_frame(value: i16) -> Vec<u8> {
        use flacenc::component::BitRepr as _;
        use flacenc::error::Verify as _;
        use flacenc::source::Fill as _;
        let mut info = flacenc::component::StreamInfo::new(48_000, 2, 16).unwrap();
        info.set_block_sizes(960, 960).unwrap();
        let config = flacenc::config::Encoder::default().into_verified().unwrap();
        let mut framebuf = flacenc::source::FrameBuf::with_size(2, 960).unwrap();
        framebuf.fill_le_bytes(&frame_samples(value), 2).unwrap();
        let frame = flacenc::encode_fixed_size_frame(&config, &framebuf, 0, &info).unwrap();
        let mut sink = flacenc::bitsink::ByteSink::new();
        frame.write(&mut sink).unwrap();
        let frame = sink.into_inner();
        let mut msg = vec![0xE4u8, 0, 0, 0];
        msg.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        msg.extend_from_slice(&frame);
        msg
    }

    /// The samples [`server_frame`] carries, as the queue should receive them.
    fn frame_samples(value: i16) -> Vec<u8> {
        value.to_le_bytes().repeat(960 * 2)
    }

    /// Run one wire through the read loop with a bridge attached, and hand back
    /// what was sent, the bridge, and a listener taken *before* the loop ran —
    /// the queue is live-only, so a listener taken afterwards would see nothing
    /// whatever arrived.
    async fn run_audio_wire(
        wire: Vec<u8>,
    ) -> (Vec<u8>, Arc<crate::audio::AudioBridge>, crate::audio::AudioListener) {
        let (uplink, sent) = test_uplink();
        let (sink, _rx) = test_sink();
        let bridge = Arc::new(crate::audio::AudioBridge::new());
        let listener = bridge.take_listener();
        let shared = test_shared_with_audio(
            test_shared(uplink, shared_desktop((2, 2), None, None), test_shadow((2, 2))),
            &bridge,
        );
        // The wire always ends, and the loop reports that as the server hanging
        // up; what it did before then is what these tests are about.
        let _ = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: true, poll: false },
            None,
            sink,
        )
        .await;
        (written(&sent), bridge, listener)
    }

    /// The pseudo-encoding is asked of a generic server only where the target
    /// asked for sound. QEMU's own, which promises raw samples, is never asked.
    #[test]
    fn the_audio_extension_is_asked_only_where_sound_was() {
        assert_eq!(vnc_audio::ENCODING.to_be_bytes(), *b"WLSF");
        for clipboard in [false, true] {
            let asked = rfb38_encoding_list(clipboard, true, false, false);
            assert!(asked.contains(&vnc_audio::ENCODING));
            assert!(!asked.contains(&-259), "QEMU's raw samples are not taken");
            assert_eq!(
                &asked[asked.len() - 2..],
                &[ENCODING_WLSHARE_DENSITY, ENCODING_WLSHARE_OUTPUTS],
                "the wlshare requests stay last, so audio never weighs on encoding preference"
            );
            assert!(
                !rfb38_encoding_list(clipboard, false, false, false)
                    .contains(&vnc_audio::ENCODING),
                "a target without audio does not ask"
            );
        }
    }

    /// The camera extension is asked of a generic server exactly where the target
    /// carries a camera, and — like every wlshare request — without weighing on
    /// encoding preference: the density and outputs requests stay last.
    #[test]
    fn the_camera_extension_is_asked_only_where_a_camera_is_carried() {
        for clipboard in [false, true] {
            let asked = rfb38_encoding_list(clipboard, true, true, false);
            assert!(asked.contains(&vnc_camera::ENCODING));
            assert_eq!(&asked[asked.len() - 2..], &[ENCODING_WLSHARE_DENSITY, ENCODING_WLSHARE_OUTPUTS]);
            assert!(!rfb38_encoding_list(clipboard, true, false, false).contains(&vnc_camera::ENCODING));
        }
    }

    /// The microphone extension on the same terms: asked of a generic server exactly
    /// where the target carries a microphone, and never ahead of the density and
    /// outputs requests.
    #[test]
    fn the_microphone_extension_is_asked_only_where_a_microphone_is_carried() {
        for clipboard in [false, true] {
            let asked = rfb38_encoding_list(clipboard, true, true, true);
            assert!(asked.contains(&vnc_mic::ENCODING));
            assert_eq!(&asked[asked.len() - 2..], &[ENCODING_WLSHARE_DENSITY, ENCODING_WLSHARE_OUTPUTS]);
            assert!(!rfb38_encoding_list(clipboard, true, true, false).contains(&vnc_mic::ENCODING));
        }
    }

    /// The announcement rectangle is answered with the format this client wants
    /// and the switch that starts the stream, and the FLAC frames that follow
    /// reach the queue as the samples they encode, one buffer a frame.
    #[tokio::test]
    async fn an_announced_audio_stream_is_turned_on_and_its_frames_reach_the_queue() {
        let wire = [
            audio_announcement(),
            server_audio(1), // begin
            server_frame(1234),
            server_frame(-5),
        ]
        .concat();
        let (written, bridge, mut listener) = run_audio_wire(wire).await;
        // Set-format then enable, byte for byte: S16, two channels, 48 000 Hz.
        assert_eq!(written, vec![255, 1, 0, 2, 3, 2, 0, 0, 0xBB, 0x80, 255, 1, 0, 0]);
        assert_eq!(bridge.negotiated_format(), Some(vnc_audio::SOURCE_FORMAT));
        assert_eq!(listener.queued_wave().as_deref(), Some(frame_samples(1234).as_slice()));
        assert_eq!(listener.queued_wave().as_deref(), Some(frame_samples(-5).as_slice()));
        assert!(listener.queued_wave().is_none(), "one message, one buffer");
    }

    /// A frame length past the limit is a server that lost its framing, and a
    /// frame that does not decode is one frame's worth of sound: either is
    /// read past, and the stream keeps its place for the frame after it.
    #[tokio::test]
    async fn an_implausible_or_undecodable_frame_is_dropped() {
        let mut garbage = vec![0xE4u8, 0, 0, 0];
        garbage.extend_from_slice(&16u32.to_be_bytes());
        garbage.extend_from_slice(&[0xAB; 16]);
        let wire = [
            audio_announcement(),
            server_audio(1),
            vec![0xE4, 0, 0, 0, 0x00, 0x01, 0x00, 0x01],
            vec![0u8; 0x1_0001],
            garbage,
            server_frame(9),
        ]
        .concat();
        let (_, _bridge, mut listener) = run_audio_wire(wire).await;
        assert_eq!(listener.queued_wave().as_deref(), Some(frame_samples(9).as_slice()));
        assert!(listener.queued_wave().is_none());
    }

    /// QEMU's raw data operation is not something a server that announced this
    /// extension sends, and it cannot be measured as anything else: fatal.
    #[tokio::test]
    async fn raw_qemu_samples_end_the_session() {
        let (uplink, _sent) = test_uplink();
        let (sink, _rx) = test_sink();
        let bridge = Arc::new(crate::audio::AudioBridge::new());
        let shared = test_shared_with_audio(
            test_shared(uplink, shared_desktop((2, 2), None, None), test_shadow((2, 2))),
            &bridge,
        );
        let wire = [audio_announcement(), server_audio(1), vec![255, 1, 0, 2, 0, 0, 0, 4, 1, 2, 3, 4]].concat();
        let err = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: true, poll: false },
            None,
            sink,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("operation 2"), "{err:#}");
    }

    /// The end of a stream takes the negotiated format with it: an open
    /// response fills with silence rather than ending, which is what a desktop
    /// going quiet should sound like.
    #[tokio::test]
    async fn the_end_of_an_audio_stream_clears_the_negotiated_format() {
        let wire = [
            audio_announcement(),
            server_audio(1),
            server_frame(3),
            server_audio(0), // end
        ]
        .concat();
        let (_, bridge, _listener) = run_audio_wire(wire).await;
        assert_eq!(bridge.negotiated_format(), None);
    }

    /// wayvnc and every other generic server: pixels arrive with nothing
    /// announced in front of them, and the session runs on in silence. Nothing
    /// is sent, because there is nothing to enable.
    #[tokio::test]
    async fn a_server_that_announces_nothing_is_never_asked_to_start() {
        let (uplink, sent) = test_uplink();
        let (sink, mut rx) = test_sink();
        let bridge = Arc::new(crate::audio::AudioBridge::new());
        let shared = test_shared_with_audio(
            test_shared(uplink, shared_desktop((2, 2), None, None), test_shadow((2, 2))),
            &bridge,
        );
        let _ = read_loop(
            std::io::Cursor::new(pixels_without_announcement()),
            shared,
            ReadFlags { clipboard: true, poll: false },
            None,
            sink.clone(),
        )
        .await;
        // The update really carried pixels, which is what settles the extension
        // as unanswered; against a 0x0 rect this test would prove nothing.
        assert!(
            forwarded(&sink, &mut rx).await.is_some(),
            "the Raw rect should have reached the browser as pixels"
        );
        assert!(written(&sent).is_empty(), "nothing was sent to a server with no sound to give");
        assert_eq!(bridge.negotiated_format(), None);
    }

    /// And giving up on a server is not final: the pixels above settle the
    /// extension as unanswered, and an announcement after them is still taken —
    /// the stream is turned on and its samples reach the queue as ever.
    #[tokio::test]
    async fn an_announcement_after_the_pixels_is_still_taken() {
        let wire = [
            pixels_without_announcement(),
            audio_announcement(),
            server_audio(1),
            server_frame(7),
        ]
        .concat();
        let (written, bridge, mut listener) = run_audio_wire(wire).await;
        assert_eq!(written, vec![255, 1, 0, 2, 3, 2, 0, 0, 0xBB, 0x80, 255, 1, 0, 0]);
        assert_eq!(bridge.negotiated_format(), Some(vnc_audio::SOURCE_FORMAT));
        assert_eq!(listener.queued_wave().as_deref(), Some(frame_samples(7).as_slice()));
    }

    /// The QEMU submessages share no length field, so one this client cannot
    /// measure is fatal rather than stepped over — the alternative is a stream
    /// read at an offset nothing recovers from.
    #[tokio::test]
    async fn an_unmeasurable_qemu_submessage_ends_the_session() {
        let (uplink, _sent) = test_uplink();
        let (sink, _rx) = test_sink();
        let bridge = Arc::new(crate::audio::AudioBridge::new());
        let shared = test_shared_with_audio(
            test_shared(uplink, shared_desktop((2, 2), None, None), test_shadow((2, 2))),
            &bridge,
        );
        let mut wire = audio_announcement();
        wire.extend_from_slice(&[255, 2, 0, 1]); // submessage 2, which is not audio
        let err = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: true, poll: false },
            None,
            sink,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("submessage 2"), "{err:#}");
    }

    #[test]
    fn client_density_is_the_type_padding_the_pixels_and_a_16_16_scale() {
        assert_eq!(
            client_density((3456, 1766), 2.0),
            [0xE0, 0, 0x0D, 0x80, 0x06, 0xE6, 0x00, 0x02, 0x00, 0x00]
        );
        assert_eq!(&client_density((1728, 883), 1.0)[6..], &[0x00, 0x01, 0x00, 0x00]);
        assert_eq!(&client_density((2592, 1325), 1.5)[6..], &[0x00, 0x01, 0x80, 0x00]);
    }

    /// The declaration is the report's layout, so the report's own parser reads
    /// it back: an independent decoder for the encoder.
    #[test]
    fn client_density_reads_back_as_an_output_scale() {
        let msg = client_density((2592, 1325), 1.5);
        let body: [u8; OUTPUT_SCALE_BODY] = msg[1..].try_into().unwrap();
        assert_eq!(OutputScale::parse(&body).unwrap(), OutputScale { size: (2592, 1325), scale: 1.5 });
    }

    #[test]
    fn output_scale_parses_the_size_and_the_fixed_point_scale() {
        // padding, 3456, 1766, 2.0
        let body = [0, 0x0D, 0x80, 0x06, 0xE6, 0x00, 0x02, 0x00, 0x00];
        assert_eq!(
            OutputScale::parse(&body).unwrap(),
            OutputScale { size: (3456, 1766), scale: 2.0 }
        );
        let fractional = [0, 0x07, 0x80, 0x04, 0x38, 0x00, 0x01, 0x80, 0x00];
        assert_eq!(OutputScale::parse(&fractional).unwrap().scale, 1.5);
        // A zero scale is a server bug, not a density.
        let zero = [0, 0x07, 0x80, 0x04, 0x38, 0, 0, 0, 0];
        assert!(OutputScale::parse(&zero).is_err());
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
            false,
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
            false,
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
            false,
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
            false,
        );
        assert_eq!(bytes, vec![pointer_event(0x00, (30, 40)).to_vec()]);
    }

    /// A Mac on Apple's revision reads the mask as CGMouseButton numbers, so right
    /// and middle ride each other's RFB bits there — and only there. Measured on
    /// macOS 26.6: see [`Buttons`].
    #[test]
    fn apples_revision_swaps_the_middle_and_right_mask_bits() {
        for (buttons, right_bit, middle_bit) in [
            (Buttons::Rfb, 0x04u8, 0x02u8),
            (Buttons::Apple, 0x02, 0x04),
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
                    false,
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
                false,
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

    /// One wheel event with `held` buttons down, as the masks it sends.
    fn wheel_masks(wheel: &mut Wheel, held: u8, dx: f32, dy: f32) -> Vec<u8> {
        let (mut mask, mut pos) = (held, (5u16, 6u16));
        translate_input(
            ClientMsg::Wheel { dx, dy, unit: WheelUnit::Line },
            &Buttons::Rfb,
            &mut mask,
            &mut pos,
            &mut HashMap::new(),
            wheel,
            true,
        )
        .iter()
        .map(|event| event[1])
        .collect()
    }

    /// A Mac scrolls only on a mask of exactly 0x08 or 0x10 and posts anything
    /// else as buttons by bit position: a pulse sent with a held button would be
    /// Back or Forward, and a horizontal one a click on button 5 or 6.
    #[test]
    fn a_mac_gets_each_scroll_pulse_alone() {
        let mut apple = Wheel::new(true);
        let down = wheel_masks(&mut apple, 0x01, 0.0, 1.0);
        assert!(!down.is_empty());
        for pair in down.chunks(2) {
            assert_eq!(pair, [0x10, 0x01], "the pulse alone, then the held button again");
        }
        assert!(wheel_masks(&mut apple, 0x00, 3.0, 0.0).is_empty(), "no horizontal axis");

        // Every other server reads the mask by the RFB convention, held buttons and all.
        let mut generic = Wheel::new(false);
        assert_eq!(wheel_masks(&mut generic, 0x01, 1.0, 1.0), [0x11, 0x01, 0x41, 0x01]);
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
            viewport: None,
            density: Density::Off,
            wire_scale: None,
            resize: true,
            following: false,
            declared: None,
            repaint_owed: false,
            hp: HpResize::default(),
            laid_out: false,
            media_live: false,
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
            hp_wake: Arc::new(tokio::sync::Notify::new()),
            audio: None,
            camera: None,
            microphone: None,
            media: None,
            passthrough: None,
        }
    }

    /// The same, for a session that asked for the desktop's sound: the bridge
    /// the read loop feeds is what makes the extension readable at all.
    fn test_shared_with_audio(shared: Shared, audio: &Arc<crate::audio::AudioBridge>) -> Shared {
        Shared { audio: Some(Arc::clone(audio)), ..shared }
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
    fn test_sink() -> (VideoSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, frame_rx) = mpsc::channel(8);
        let plan = crate::config::RenderPlan {
            quality: 60,
            adaptive: None,
            chroma: crate::config::Chroma::Subsampled,
            apple_hevc: false,
        };
        let feedback = Arc::new(crate::feedback::LinkFeedback::new());
        let sink = VideoSink::new("vnc", frame_tx, plan, feedback, TileSupport::None);
        // Larger than any desktop these tests paint, so a rectangle lands in the
        // mirror without the `Resize` a live engine would have sent first.
        sink.presize(256, 256);
        (sink, frame_rx)
    }

    /// A sink that has been told the desktop is `size`, which its mirror needs before
    /// it takes a pixel — on a live session that is the engine's own `Resize`.
    async fn sized_sink(size: (u16, u16)) -> (VideoSink, mpsc::Receiver<ServerMsg>) {
        let (sink, mut rx) = test_sink();
        sink.msg(ServerMsg::Resize { w: size.0, h: size.1, scale: UNSCALED }).await.unwrap();
        sink.flush().await;
        assert!(matches!(rx.recv().await, Some(ServerMsg::Resize { .. })));
        (sink, rx)
    }

    /// The access units a flushed sink has put on its channel.
    fn units(rx: &mut mpsc::Receiver<ServerMsg>) -> Vec<crate::protocol::VideoUnit> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|msg| match msg {
                ServerMsg::Video(unit) => Some(unit),
                _ => None,
            })
            .collect()
    }

    /// What the sink has forwarded so far, or `None` for nothing.
    ///
    /// The flush is the point: a [`VideoSink`] forwards from a task of its own, so a
    /// bare `try_recv` would race it and read `None` for a message that is on its
    /// way. `None` here means the engine sent nothing, which is what these tests
    /// mean when they assert it.
    async fn forwarded(
        sink: &VideoSink,
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

    /// A request that goes out supersedes any older stash, so the rect that
    /// answers it finds nothing to replay: the desktop the browser left is
    /// never asked for again behind the one it wants.
    #[tokio::test]
    async fn a_sent_request_clears_the_stash_so_its_rect_replays_nothing() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let screen = Screen { id: 7, flags: 0 };
        // Stashed while support was still undeclared, then the window moved on.
        let desktop = shared_desktop((1024, 768), Some(screen), Some((800, 600)));

        request_resize(&uplink, &desktop, ResizeAsk::Viewport((640, 480)), false).await.unwrap();
        assert_eq!(written(&wire), set_desktop_size((640, 480), screen));
        assert_eq!(desktop.lock().unwrap().pending, None, "the sent request supersedes the stash");

        // The server answers with the new size: the browser is told, and the
        // stale 800x600 is not sent after the 640x480 the window asked for.
        let payload = eds_payload(screen);
        read_extended_desktop_size(
            &mut payload.as_slice(),
            &uplink,
            &desktop,
            &test_shadow((1024, 768)),
            (1, 0, 640, 480),
            &sink,
        )
        .await
        .unwrap();
        let resize = forwarded(&sink, &mut rx).await;
        assert!(matches!(resize, Some(ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED })));
        assert_eq!(written(&wire), set_desktop_size((640, 480), screen), "nothing replayed");
        assert_eq!(desktop.lock().unwrap().viewport, Some((640, 480)));
    }

    /// The operator's pinned size on a generic target is seeded as a held
    /// request, so the desktop is asked for it as soon as the server declares
    /// SetDesktopSize support — and asked for it on a target *without*
    /// `resize`, which is the whole point of a pin: `resize` decides whether
    /// the window drives the size afterwards, not whether the operator's
    /// opening size is spent at all.
    #[tokio::test]
    async fn a_pinned_size_is_asked_for_on_a_target_without_resize() {
        let (uplink, wire) = test_uplink();
        let (sink, _rx) = test_sink();
        let screen = Screen { id: 9, flags: 1 };
        // What `active_loop` builds for `width = 1440`, `height = 900`,
        // `resize = false` against a server whose desktop is 1920x1080.
        let desktop = shared_desktop((1920, 1080), None, Some((1440, 900)));
        desktop.lock().unwrap().resize = false;

        // wlshare's opening announcement: reason 0, the server's own size.
        let payload = eds_payload(screen);
        read_extended_desktop_size(
            &mut payload.as_slice(),
            &uplink,
            &desktop,
            &test_shadow((1920, 1080)),
            (0, 0, 1920, 1080),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(written(&wire), set_desktop_size((1440, 900), screen));
        let d = desktop.lock().unwrap();
        assert_eq!(d.pending, None, "the pin was spent, not held again");
        assert_eq!(d.viewport, Some((1440, 900)), "and it is what a scale report re-asks for");
    }

    /// The same declaration on an unpinned target asks for nothing: a session
    /// with no pin keeps the server's own size until the window says otherwise.
    #[tokio::test]
    async fn an_unpinned_target_asks_for_nothing_when_support_is_declared() {
        let (uplink, wire) = test_uplink();
        let (sink, _rx) = test_sink();
        let desktop = shared_desktop((1920, 1080), None, None);
        desktop.lock().unwrap().resize = false;

        let payload = eds_payload(Screen { id: 9, flags: 1 });
        read_extended_desktop_size(
            &mut payload.as_slice(),
            &uplink,
            &desktop,
            &test_shadow((1920, 1080)),
            (0, 0, 1920, 1080),
            &sink,
        )
        .await
        .unwrap();

        assert!(written(&wire).is_empty());
    }

    /// The same pin under `resize`, which is the ordering every desktop browser
    /// produces: the window is reported the moment `connected` reaches it, long
    /// before a rect can declare SetDesktopSize support, so the report supersedes
    /// the held pin exactly as it supersedes any older hold. One request goes out
    /// on the declaration and it is the window's — the pin is never asked for
    /// behind a size the browser has already left.
    #[tokio::test]
    async fn a_viewport_report_supersedes_a_pin_that_has_not_gone_out() {
        let (uplink, wire) = test_uplink();
        let (sink, _rx) = test_sink();
        let screen = Screen { id: 9, flags: 1 };
        let desktop = shared_desktop((1920, 1080), None, Some((1440, 900)));

        // The browser's first viewport, handled before any rect has arrived.
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1280, 800)), false).await.unwrap();
        assert!(written(&wire).is_empty(), "nothing goes out before support is declared");
        assert_eq!(desktop.lock().unwrap().pending, Some((1280, 800)), "the pin gave way");

        let payload = eds_payload(screen);
        read_extended_desktop_size(
            &mut payload.as_slice(),
            &uplink,
            &desktop,
            &test_shadow((1920, 1080)),
            (0, 0, 1920, 1080),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(written(&wire), set_desktop_size((1280, 800), screen));
        assert_eq!(desktop.lock().unwrap().pending, None);
    }

    /// The viewport changes while the read loop's replay is blocked behind the
    /// uplink: both requests go out, in the order they were decided, and the
    /// window's newest size is the last word on the wire.
    #[tokio::test]
    async fn a_viewport_change_while_the_replay_is_blocked_lands_last() {
        let (uplink, wire) = test_uplink();
        let (sink, _rx) = test_sink();
        let screen = Screen { id: 3, flags: 0 };
        let desktop = shared_desktop((1024, 768), None, Some((800, 600)));

        // Something else holds the uplink while the declaring rect arrives.
        let held = uplink.lock().await;
        let replay = tokio::spawn({
            let (uplink, desktop, shadow) = (Arc::clone(&uplink), Arc::clone(&desktop), test_shadow((1024, 768)));
            let payload = eds_payload(screen);
            async move {
                read_extended_desktop_size(
                    &mut payload.as_slice(),
                    &uplink,
                    &desktop,
                    &shadow,
                    (0, 0, 1024, 768),
                    &sink,
                )
                .await
            }
        });
        // Support is recorded before the replay waits for the uplink; with no
        // size change there is no other await between the two.
        while desktop.lock().unwrap().screen.is_none() {
            tokio::task::yield_now().await;
        }
        // The browser's window changes meanwhile; the input side queues behind
        // the replay for the same lock.
        let input = tokio::spawn({
            let (uplink, desktop) = (Arc::clone(&uplink), Arc::clone(&desktop));
            async move { request_resize(&uplink, &desktop, ResizeAsk::Viewport((640, 480)), false).await }
        });
        tokio::task::yield_now().await;
        assert!(written(&wire).is_empty(), "nothing goes out while the uplink is held");
        drop(held);
        replay.await.unwrap().unwrap();
        input.await.unwrap().unwrap();

        let mut expect = set_desktop_size((800, 600), screen).to_vec();
        expect.extend_from_slice(&set_desktop_size((640, 480), screen));
        assert_eq!(written(&wire), expect, "decided order, newest last");
        let d = desktop.lock().unwrap();
        assert_eq!(d.pending, None);
        assert_eq!(d.viewport, Some((640, 480)));
    }

    /// A generic server is never asked for a desktop the encoder refuses: the
    /// window's size is held under the picture ceiling before it is sent or stashed.
    #[tokio::test]
    async fn a_generic_resize_is_held_under_the_video_ceiling() {
        let (uplink, wire) = test_uplink();
        let screen = Screen { id: 7, flags: 0 };
        let desktop = shared_desktop((1024, 768), Some(screen), None);

        request_resize(&uplink, &desktop, ResizeAsk::Viewport((5120, 2880)), false).await.unwrap();
        assert_eq!(written(&wire), set_desktop_size((3840, 2400), screen));

        // Already the held size: a window still 5120×2880 asks for nothing more.
        desktop.lock().unwrap().size = (3840, 2400);
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((5120, 2880)), false).await.unwrap();
        assert_eq!(written(&wire), set_desktop_size((3840, 2400), screen), "nothing further went out");

        // Stashed before support is declared: the stash is the window in points,
        // and the replay holds it under the same ceiling the live request would.
        let stashed = shared_desktop((1024, 768), None, None);
        request_resize(&uplink, &stashed, ResizeAsk::Viewport((5120, 2880)), false).await.unwrap();
        assert_eq!(stashed.lock().unwrap().pending, Some((5120, 2880)));
        assert_eq!(stashed.lock().unwrap().generic_pixels((5120, 2880)), (3840, 2400));
    }

    /// Let a High Performance report settle, run what falls due, and play the
    /// read loop's part at the update boundary the drain prompts: the
    /// `SetDisplayConfiguration` that goes out there, if any.
    async fn hp_settle(
        uplink: &SharedUplink,
        desktop: &SharedDesktop,
        sink: &VideoSink,
    ) -> Option<Vec<u8>> {
        tokio::time::advance(HP_RESIZE_SETTLE).await;
        hp_resize_step(uplink, desktop, None, sink).await.unwrap();
        desktop.lock().unwrap().hp_take_request(tokio::time::Instant::now())
    }

    /// The configuration asked for `points` at `density`.
    fn hp_config(points: (u16, u16), density: f32) -> Vec<u8> {
        vnc_apple::set_display_configuration(vnc_apple::virtual_display_mode(points, density))
    }

    #[tokio::test(start_paused = true)]
    async fn high_performance_resize_sends_a_full_dynamic_configuration() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let desktop = shared_desktop((1024, 768), None, None);

        request_resize(&uplink, &desktop, ResizeAsk::Viewport((800, 600)), true).await.unwrap();
        hp_resize_step(&uplink, &desktop, None, &sink).await.unwrap();
        assert!(written(&wire).is_empty(), "nothing goes out before the window settles");
        assert!(matches!(
            forwarded(&sink, &mut rx).await,
            Some(ServerMsg::Resizing { active: true })
        ));

        assert_eq!(hp_settle(&uplink, &desktop, &sink).await, Some(hp_config((800, 600), 1.0)));
        // The drain is a one-pixel full request, which the Mac answers at once.
        assert_eq!(written(&wire), update_request(false, HP_HOLD_REQUEST));
        assert_eq!(desktop.lock().unwrap().poll_size(), HP_HOLD_REQUEST, "polling is held");
        assert!(desktop.lock().unwrap().pending.is_none());
    }
    #[test]
    fn a_session_opens_at_the_pinned_size_or_the_clients_own_screen() {
        let target = |size: &str| -> TargetConfig {
            toml::from_str(&format!(
                "name = \"t\"\nprotocol = \"vnc\"\nsubtype = \"ard-high-performance\"\n\
                 host = \"h\"\n{size}"
            ))
            .unwrap()
        };
        let screen = crate::protocol::HostDisplay { w: 1728, h: 1117, scale: 200, fit: false };

        // No pinned size: the client's screen, at the client's density — how
        // Apple's own client opens, and the layout every remote window gets.
        assert_eq!(
            opening_mode(&target(""), Some(screen)),
            vnc_apple::virtual_display_mode((1728, 1117), 2.0)
        );

        // A pinned size wins, but the density is still the client screen's.
        assert_eq!(
            opening_mode(&target("width = 1600\nheight = 1000"), Some(screen)),
            vnc_apple::virtual_display_mode((1600, 1000), 2.0)
        );

        // No screen named at all (a probe): the pinned size or the default, at 1x.
        assert_eq!(
            opening_mode(&target(""), None),
            vnc_apple::virtual_display_mode(crate::config::DEFAULT_SIZE, 1.0)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_density_change_re_renders_the_same_points_at_the_new_density() {
        let (uplink, _wire) = test_uplink();
        let (sink, _rx) = test_sink();
        // A 1600×1000 1x desktop whose client window just moved to a 2x screen.
        let desktop = shared_desktop((1600, 1000), None, None);
        desktop.lock().unwrap().host_density = 2.0;

        request_resize(&uplink, &desktop, ResizeAsk::Density, true).await.unwrap();
        assert_eq!(
            hp_settle(&uplink, &desktop, &sink).await,
            Some(hp_config((1600, 1000), 2.0)),
            "current points, twice the pixels"
        );
    }

    /// A window dragged to another screen reports its new size and then its new
    /// density; the density keeps the size that is still settling rather than
    /// re-asking for the desktop's current points.
    #[tokio::test(start_paused = true)]
    async fn a_density_change_keeps_a_settling_viewport() {
        let (uplink, _wire) = test_uplink();
        let (sink, _rx) = test_sink();
        let desktop = shared_desktop((1600, 1000), None, None);

        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1280, 800)), true).await.unwrap();
        desktop.lock().unwrap().host_density = 2.0;
        request_resize(&uplink, &desktop, ResizeAsk::Density, true).await.unwrap();
        assert_eq!(hp_settle(&uplink, &desktop, &sink).await, Some(hp_config((1280, 800), 2.0)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_viewport_report_is_read_as_points() {
        let (uplink, _wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        // Steady state on a Retina client: the desktop is 3200×2000 pixels shown
        // at 2x, and the browser reports its viewport in points — the 1600×1000
        // it has — whatever scale it was last told. The same size must not
        // re-request anything, nor cover the desktop.
        let desktop = shared_desktop((3200, 2000), None, None);
        {
            let mut d = desktop.lock().unwrap();
            d.scale = 2.0;
            d.host_density = 2.0;
        }

        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1600, 1000)), true).await.unwrap();
        assert_eq!(hp_settle(&uplink, &desktop, &sink).await, None, "the current size");
        assert!(forwarded(&sink, &mut rx).await.is_none(), "no cover for a no-op");

        // A genuinely new window size: 1600×1200 points, rendered at the
        // client's density.
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1600, 1200)), true).await.unwrap();
        assert_eq!(hp_settle(&uplink, &desktop, &sink).await, Some(hp_config((1600, 1200), 2.0)));

        // The same points reported while this end still announces 1x — a
        // browser right after a reconnect, before the layout has reached it —
        // ask for the same desktop again, not half of one, once the Mac has
        // answered the request already out.
        desktop.lock().unwrap().scale = UNSCALED;
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1600, 1200)), true).await.unwrap();
        assert_eq!(hp_settle(&uplink, &desktop, &sink).await, None, "one request in flight at a time");
        desktop.lock().unwrap().hp.layout(true, tokio::time::Instant::now());
        hp_resize_step(&uplink, &desktop, None, &sink).await.unwrap();
        assert_eq!(
            desktop.lock().unwrap().hp_take_request(tokio::time::Instant::now()),
            Some(hp_config((1600, 1200), 2.0))
        );
    }

    /// A resizing session opens covered. Nothing is asked of the Mac until its
    /// opening display has arrived, and the cover stays through the resize to
    /// the window's size.
    #[test]
    fn hp_opens_covered_until_the_first_resize_settles() {
        let t0 = tokio::time::Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut hp = HpResize::opening();
        assert!(hp.shown, "covered from the start");

        hp.report((1728, 902), false, t0);
        assert_eq!(hp.step(ms(5_000)), None, "the opening configuration is still unanswered");
        assert_eq!(hp.deadline(ms(5_000)), None);

        // The opening display, then the same layout again after the request
        // for the window's size has gone: the repeat answers nothing.
        hp.layout(true, ms(6_000));
        assert_eq!(hp.step(ms(6_000)), Some(HpStep::Drain));
        assert_eq!(hp.take_due(ms(6_100)), Some((1728, 902)));
        hp.layout(false, ms(6_200));
        assert!(hp.holds_pixels(), "a repeated layout is not the answer");

        hp.layout(true, ms(8_000));
        assert_eq!(hp.step(ms(8_000)), None, "the cover waits out the quiet");
        assert_eq!(hp.step(ms(8_000) + HP_LAYOUT_QUIET), Some(HpStep::Hide));
        assert!(!hp.shown);
    }

    /// A window already the size of the opening display asks for nothing; the
    /// cover comes down once that display has arrived.
    #[test]
    fn hp_opens_without_a_resize_when_the_window_fits() {
        let t0 = tokio::time::Instant::now();
        let mut hp = HpResize::opening();
        hp.report((1728, 1080), false, t0);
        hp.report((1728, 1080), true, t0);
        hp.layout(true, t0);
        assert_eq!(hp.step(t0 + HP_LAYOUT_QUIET), Some(HpStep::Hide));
        assert!(!hp.holds_pixels());
    }

    /// A drag's stream of sizes is one request, for the size it came to rest at,
    /// sent at an update boundary; the cover goes up at the first report and
    /// comes down only once the Mac's layout has held still.
    #[test]
    fn hp_resize_debounces_and_covers_until_the_layout_settles() {
        let t0 = tokio::time::Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut hp = HpResize::default();

        hp.report((1000, 700), false, t0);
        assert_eq!(hp.step(t0), Some(HpStep::Show));
        assert_eq!(hp.step(t0), None);
        hp.report((1100, 700), false, ms(400));
        hp.report((1200, 700), false, ms(800));
        assert_eq!(hp.deadline(ms(800)), Some(ms(800) + HP_RESIZE_SETTLE));
        assert_eq!(hp.step(ms(1500)), None, "the last report restarted the wait");
        assert!(!hp.holds_pixels());
        assert_eq!(hp.step(ms(1800)), Some(HpStep::Drain));
        assert!(hp.holds_pixels(), "nothing full-size is asked for once a size is due");
        assert_eq!(hp.take_due(ms(1850)), Some((1200, 700)));
        assert_eq!(hp.take_due(ms(1850)), None, "taken once");

        // A report while that request is out waits for its answer, however long
        // the Mac takes.
        hp.report((1300, 700), false, ms(1900));
        assert_eq!(hp.step(ms(20_000)), None, "one request in flight at a time");
        hp.layout(false, ms(10_000));
        assert!(hp.holds_pixels(), "a layout that changes nothing answers nothing");
        hp.layout(true, ms(20_100));
        assert!(!hp.holds_pixels(), "the answer releases polling");
        assert_eq!(hp.step(ms(20_100)), Some(HpStep::Drain));
        assert_eq!(hp.take_due(ms(20_100)), Some((1300, 700)));

        // Its answer, then a duplicate layout: the cover waits out the quiet.
        hp.layout(true, ms(20_300));
        hp.layout(false, ms(20_500));
        assert_eq!(hp.step(ms(20_900)), None);
        assert_eq!(hp.deadline(ms(20_900)), Some(ms(20_500) + HP_LAYOUT_QUIET));
        assert_eq!(hp.step(ms(21_000)), Some(HpStep::Hide));
        assert!(!hp.shown);
        assert_eq!(hp.deadline(ms(21_000)), None, "nothing more is due");
    }

    /// A report while a size waits for its update boundary replaces it, and one
    /// back to the desktop showing cancels it.
    #[test]
    fn hp_resize_report_replaces_a_size_not_yet_sent() {
        let t0 = tokio::time::Instant::now();
        let mut hp = HpResize::default();
        hp.report((1000, 700), false, t0);
        assert_eq!(hp.step(t0), Some(HpStep::Show));
        let due = t0 + HP_RESIZE_SETTLE;
        assert_eq!(hp.step(due), Some(HpStep::Drain));
        hp.report((1200, 800), false, due);
        assert_eq!(hp.take_due(due), None, "the stale size never goes out");
        let due = due + HP_RESIZE_SETTLE;
        assert_eq!(hp.step(due), Some(HpStep::Drain));
        hp.report((1728, 1080), true, due);
        assert_eq!(hp.take_due(due), None);
        assert_eq!(hp.newest_points(), None, "a return to the desktop showing cancels");
    }

    /// An answer that never comes does not leave the cover up for good.
    #[test]
    fn hp_resize_gives_up_on_an_unanswered_request() {
        let t0 = tokio::time::Instant::now();
        let mut hp = HpResize::default();
        hp.report((1000, 700), false, t0);
        assert_eq!(hp.step(t0), Some(HpStep::Show));
        let due = t0 + HP_RESIZE_SETTLE;
        assert_eq!(hp.step(due), Some(HpStep::Drain));
        assert_eq!(hp.take_due(due), Some((1000, 700)));
        assert_eq!(hp.newest_points(), Some((1000, 700)), "the points out are still the newest");
        let expiry = due + HP_RESIZE_STUCK;
        assert_eq!(hp.deadline(expiry), Some(expiry));
        assert_eq!(hp.step(expiry), Some(HpStep::GiveUp));
        assert!(!hp.holds_pixels());
        assert_eq!(hp.step(expiry), Some(HpStep::Hide));
    }

    /// The body of an OutputScale report for `size` at `scale`.
    fn output_scale_body(size: (u16, u16), scale: f32) -> [u8; OUTPUT_SCALE_BODY] {
        let mut body = [0u8; OUTPUT_SCALE_BODY];
        body[1..3].copy_from_slice(&size.0.to_be_bytes());
        body[3..5].copy_from_slice(&size.1.to_be_bytes());
        body[5..9].copy_from_slice(&((scale * 65536.0) as u32).to_be_bytes());
        body
    }

    /// A generic target holds its first resize until the server has said what
    /// scale it draws at. The report labels the framebuffer, and the held window
    /// goes out with the browser's density, in points × that density, as one
    /// declaration; its answer, naming those pixels, asks nothing more.
    #[tokio::test]
    async fn a_resize_waits_for_the_scale_report_and_goes_out_with_the_declaration() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let screen = Screen { id: 3, flags: 0 };
        let desktop = shared_desktop((1024, 768), Some(screen), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Asked;
            d.host_density = 2.0;
        }

        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1728, 883)), false).await.unwrap();
        assert!(written(&wire).is_empty(), "held until the server reports");
        assert_eq!(desktop.lock().unwrap().pending, Some((1728, 883)));

        // The report describes the current framebuffer at 2x: same pixels, a new
        // label, and the held request goes out at points × 2.
        let body = output_scale_body((1024, 768), 2.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1024, 768)), &sink)
            .await
            .unwrap();
        let declared = client_density((3456, 1766), 2.0);
        assert_eq!(written(&wire), declared);
        assert!(matches!(
            forwarded(&sink, &mut rx).await,
            Some(ServerMsg::Resize { w: 1024, h: 768, scale }) if scale == 2.0
        ));
        {
            let d = desktop.lock().unwrap();
            assert_eq!(d.density, Density::Reported);
            assert_eq!(d.wire_scale, Some(2.0));
            assert_eq!(d.pending, None);
            assert_eq!(d.viewport, Some((1728, 883)));
            assert!(d.following);
        }

        // The answer names the window's pixels: its rect is on the way.
        let body = output_scale_body((3456, 1766), 2.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1024, 768)), &sink)
            .await
            .unwrap();
        assert_eq!(written(&wire), declared);
        assert!(forwarded(&sink, &mut rx).await.is_none());
        assert!(!desktop.lock().unwrap().following);
    }

    /// With no window asked for yet, the declaration keeps the desktop's logical
    /// size, in the points the report says it is drawn at — not the canvas's
    /// label, which the report has not relabelled yet.
    #[tokio::test]
    async fn a_declaration_with_no_window_keeps_the_reported_logical_size() {
        let (uplink, wire) = test_uplink();
        let (sink, _rx) = test_sink();
        let desktop = shared_desktop((1280, 800), Some(Screen { id: 3, flags: 0 }), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Asked;
            d.host_density = 2.0;
        }
        let body = output_scale_body((1280, 800), 2.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1280, 800)), &sink)
            .await
            .unwrap();
        assert_eq!(written(&wire), client_density((1280, 800), 2.0));
        assert_eq!(desktop.lock().unwrap().viewport, Some((640, 400)));
    }

    /// A reported scale that is not the browser's is declared with the window in
    /// the browser's pixels. A window that changes while the server follows is
    /// held, and the report answering the declaration releases it in the pixels
    /// the server settled on.
    #[tokio::test]
    async fn a_declared_density_holds_the_resize_until_the_server_has_followed() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let screen = Screen { id: 3, flags: 0 };
        let desktop = shared_desktop((1920, 1080), Some(screen), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Asked;
            d.host_density = 2.0;
        }
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1728, 883)), false).await.unwrap();

        // Output at 1x, browser at 2x: declare, and ask for nothing yet.
        let body = output_scale_body((1920, 1080), 1.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1920, 1080)), &sink)
            .await
            .unwrap();
        let declared = client_density((3456, 1766), 2.0).to_vec();
        assert_eq!(written(&wire), declared, "the window declared at 2x");
        assert!(forwarded(&sink, &mut rx).await.is_none(), "1x is what the browser already has");
        {
            let d = desktop.lock().unwrap();
            assert!(d.following);
            assert_eq!(d.pending, None, "the declaration carried it");
        }

        // The window changes while the server is following: held.
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1600, 900)), false).await.unwrap();
        assert_eq!(written(&wire), declared, "nothing more on the wire");
        assert_eq!(desktop.lock().unwrap().pending, Some((1600, 900)));

        // The server set the output to the declared 3456×1766 at 2x: a label
        // for the rect on its way, and the newest window asked for in points × 2.
        let body = output_scale_body((3456, 1766), 2.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1920, 1080)), &sink)
            .await
            .unwrap();
        let expected = [declared, set_desktop_size((3200, 1800), screen).to_vec()].concat();
        assert_eq!(written(&wire), expected);
        assert!(forwarded(&sink, &mut rx).await.is_none(), "the rect carries the label");
        let d = desktop.lock().unwrap();
        assert!(!d.following);
        assert_eq!(d.pending, None);
        assert_eq!(d.wire_scale, Some(2.0));
    }

    /// A server that will not follow — resizing disabled, another client owning
    /// the layout — answers the declaration with the scale as it is, and the held
    /// resize goes out in those pixels rather than waiting forever.
    #[tokio::test]
    async fn a_refused_declaration_is_answered_and_the_resize_goes_out_as_is() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let screen = Screen { id: 3, flags: 0 };
        let desktop = shared_desktop((1920, 1080), Some(screen), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Asked;
            d.host_density = 2.0;
        }
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1728, 883)), false).await.unwrap();

        let body = output_scale_body((1920, 1080), 1.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1920, 1080)), &sink)
            .await
            .unwrap();
        assert_eq!(written(&wire), client_density((3456, 1766), 2.0));

        // Answered with the output as it was: the follow is over, the window is
        // asked for at 1x, and the browser is told nothing new.
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1920, 1080)), &sink)
            .await
            .unwrap();
        let expected = [client_density((3456, 1766), 2.0).to_vec(), set_desktop_size((1728, 883), screen).to_vec()].concat();
        assert_eq!(written(&wire), expected);
        assert!(forwarded(&sink, &mut rx).await.is_none());
        let d = desktop.lock().unwrap();
        assert!(!d.following);
        assert_eq!(d.pending, None);
    }

    /// A density change mid-session is declared with the window in the new
    /// pixels, and a server that grants it answers with those pixels: one
    /// reconfiguration, with nothing asked again.
    #[tokio::test]
    async fn a_density_change_mid_session_is_followed_by_the_answering_report() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let screen = Screen { id: 3, flags: 0 };
        let desktop = shared_desktop((3456, 1766), Some(screen), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Reported;
            d.wire_scale = Some(2.0);
            d.scale = 2.0;
            d.host_density = 2.0;
            d.viewport = Some((1728, 883));
        }

        // The browser moved to a 1x screen: what the run loop does on HostDisplay.
        assert!(send_decided(&uplink, &desktop, |d| d.host_density_changed(1.0)).await.unwrap());
        assert!(desktop.lock().unwrap().following);

        let body = output_scale_body((1728, 883), 1.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((3456, 1766)), &sink)
            .await
            .unwrap();
        assert_eq!(written(&wire), client_density((1728, 883), 1.0));
        assert!(forwarded(&sink, &mut rx).await.is_none(), "the rect carries the label");
        let d = desktop.lock().unwrap();
        assert!(!d.following);
        assert_eq!(d.wire_scale, Some(UNSCALED));
    }

    /// A density that changes while a declaration is unanswered waits: the
    /// answering report finds the browser elsewhere and declares that, so the
    /// server is asked for one transition at a time and ends where the browser
    /// is, not where it passed through.
    #[tokio::test]
    async fn a_density_changed_while_a_declaration_is_out_is_declared_by_the_answer() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let screen = Screen { id: 3, flags: 0 };
        let desktop = shared_desktop((1920, 1080), Some(screen), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Reported;
            d.wire_scale = Some(1.0);
            d.host_density = 1.0;
            d.viewport = Some((1728, 883));
        }

        // To a 2x screen: declared, and the server is now busy following.
        assert!(send_decided(&uplink, &desktop, |d| d.host_density_changed(2.0)).await.unwrap());
        let at_2x = client_density((3456, 1766), 2.0).to_vec();
        assert_eq!(written(&wire), at_2x);
        assert!(desktop.lock().unwrap().following);

        // Back to 1x before the answer: recorded, not declared.
        assert!(!send_decided(&uplink, &desktop, |d| d.host_density_changed(1.0)).await.unwrap());
        assert_eq!(written(&wire), at_2x, "nothing more while the first is out");
        assert_eq!(desktop.lock().unwrap().host_density, 1.0);

        // The server followed to 2x: the browser's current 1x is declared in the
        // answer's place, with the window at 1x.
        let body = output_scale_body((3456, 1766), 2.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1920, 1080)), &sink)
            .await
            .unwrap();
        let at_1x = client_density((1728, 883), 1.0).to_vec();
        assert_eq!(written(&wire), [at_2x.clone(), at_1x.clone()].concat());
        assert!(forwarded(&sink, &mut rx).await.is_none(), "the rect carries the label");
        {
            let d = desktop.lock().unwrap();
            assert!(d.following, "the second declaration is out");
            assert_eq!(d.declared, Some(1.0));
        }

        // The answer to that one names the window's pixels at 1x: nothing more.
        let body = output_scale_body((1728, 883), 1.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1920, 1080)), &sink)
            .await
            .unwrap();
        assert_eq!(written(&wire), [at_2x, at_1x].concat());
        assert!(forwarded(&sink, &mut rx).await.is_none());
        let d = desktop.lock().unwrap();
        assert!(!d.following);
        assert_eq!(d.wire_scale, Some(1.0));
    }

    /// A report that relabels the current pixels clears the browser's canvas.
    /// When the window already has the pixels the report names, no resize
    /// follows to repaint it, so the whole framebuffer is asked for instead.
    #[tokio::test]
    async fn a_relabel_with_no_resize_to_send_asks_for_the_whole_framebuffer() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let screen = Screen { id: 3, flags: 0 };
        let desktop = shared_desktop((3456, 1766), Some(screen), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Reported;
            d.wire_scale = Some(1.0);
            d.host_density = 2.0;
            d.declared = Some(2.0);
            d.viewport = Some((1728, 883));
        }

        // The host toggles the output to 2x: 3456×1766 is already points × 2.
        let body = output_scale_body((3456, 1766), 2.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((3456, 1766)), &sink)
            .await
            .unwrap();
        assert!(matches!(
            forwarded(&sink, &mut rx).await,
            Some(ServerMsg::Resize { w: 3456, h: 1766, scale }) if scale == 2.0
        ));
        assert_eq!(written(&wire), update_request(false, (3456, 1766)), "a full update, no resize");
        let d = desktop.lock().unwrap();
        assert!(!d.repaint_owed);
        assert!(!d.following);
    }

    /// The resize a relabel sends is what repaints the canvas. Refused, it
    /// repaints nothing, and the refusal asks for the pixels as they are.
    #[tokio::test]
    async fn a_refused_resize_after_a_relabel_asks_for_the_whole_framebuffer() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let screen = Screen { id: 3, flags: 0 };
        let desktop = shared_desktop((1920, 1080), Some(screen), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Reported;
            d.wire_scale = Some(1.0);
            d.host_density = 2.0;
            d.declared = Some(2.0);
            d.viewport = Some((1728, 883));
        }

        let body = output_scale_body((1920, 1080), 2.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1920, 1080)), &sink)
            .await
            .unwrap();
        assert!(forwarded(&sink, &mut rx).await.is_some(), "relabelled");
        let resize = set_desktop_size((3456, 1766), screen).to_vec();
        assert_eq!(written(&wire), resize, "the resize is what will repaint");
        assert!(desktop.lock().unwrap().repaint_owed);

        // Prohibited: another client owns the layout. The canvas is still blank.
        let payload = eds_payload(screen);
        read_extended_desktop_size(
            &mut payload.as_slice(),
            &uplink,
            &desktop,
            &test_shadow((1920, 1080)),
            (1, 1, 1920, 1080),
            &sink,
        )
        .await
        .unwrap();
        let expected = [resize, update_request(false, (1920, 1080)).to_vec()].concat();
        assert_eq!(written(&wire), expected);
        assert!(!desktop.lock().unwrap().repaint_owed);
    }

    /// Where the window does not drive the desktop size, nothing is declared: a
    /// server following the browser's density would leave the pixels unasked-for.
    #[tokio::test]
    async fn a_fixed_size_target_labels_by_the_report_and_declares_nothing() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let desktop = shared_desktop((1920, 1080), Some(Screen { id: 3, flags: 0 }), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Asked;
            d.host_density = 2.0;
            d.resize = false;
        }

        let body = output_scale_body((1920, 1080), 1.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1920, 1080)), &sink)
            .await
            .unwrap();
        assert!(written(&wire).is_empty(), "no declaration, no request");
        assert!(forwarded(&sink, &mut rx).await.is_none());
        let d = desktop.lock().unwrap();
        assert_eq!(d.density, Density::Reported);
        assert_eq!(d.wire_scale, Some(1.0));
        assert!(!d.following);
    }

    /// A report naming a size the framebuffer does not have yet is the label for
    /// the rect about to arrive, not a resize of the current pixels; and when it
    /// already names what the window wants, nothing is asked again.
    #[tokio::test]
    async fn a_scale_report_for_a_new_size_waits_for_its_rect() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let screen = Screen { id: 3, flags: 0 };
        let desktop = shared_desktop((1024, 768), Some(screen), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Reported;
            d.wire_scale = Some(1.0);
            d.viewport = Some((1024, 768));
        }

        let body = output_scale_body((2048, 1536), 2.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1024, 768)), &sink)
            .await
            .unwrap();
        assert!(written(&wire).is_empty(), "the rect is on its way; nothing to ask");
        assert!(forwarded(&sink, &mut rx).await.is_none(), "no relabel of pixels that are going away");
        assert_eq!(desktop.lock().unwrap().wire_scale, Some(2.0));

        // The rect carries the reported scale as its label.
        let payload = eds_payload(screen);
        read_extended_desktop_size(
            &mut payload.as_slice(),
            &uplink,
            &desktop,
            &test_shadow((1024, 768)),
            (0, 0, 2048, 1536),
            &sink,
        )
        .await
        .unwrap();
        assert!(matches!(
            forwarded(&sink, &mut rx).await,
            Some(ServerMsg::Resize { w: 2048, h: 1536, scale }) if scale == 2.0
        ));
    }

    /// A server that sends pixels before any report does not speak the
    /// extension: the request is settled as unanswered, a held resize is no
    /// longer held on it, and the desktop is generic RFB at 1x. A report that
    /// arrives after all is still the wire's word and is taken. Once settled
    /// either way, later updates change nothing.
    #[tokio::test]
    async fn pixels_before_the_first_report_settle_the_request_as_unanswered() {
        let (uplink, wire) = test_uplink();
        let (sink, mut rx) = test_sink();
        let screen = Screen { id: 3, flags: 0 };
        let desktop = shared_desktop((1024, 768), Some(screen), None);
        {
            let mut d = desktop.lock().unwrap();
            d.density = Density::Asked;
            d.host_density = 2.0;
        }
        request_resize(&uplink, &desktop, ResizeAsk::Viewport((1728, 883)), false).await.unwrap();
        assert!(written(&wire).is_empty(), "held until the server reports or sends pixels");

        desktop.lock().unwrap().first_update();
        assert_eq!(desktop.lock().unwrap().density, Density::Unanswered);
        assert_eq!(desktop.lock().unwrap().generic_scale(), UNSCALED);
        // The hold is off: the stashed window goes out in points, at 1x.
        let replayed = send_decided(&uplink, &desktop, |d| d.pending.take().and_then(|p| d.generic_resize(p)))
            .await
            .unwrap();
        assert!(replayed);
        assert_eq!(written(&wire), set_desktop_size((1728, 883), screen).to_vec());

        // A late report is still taken, as a first one: the current pixels are
        // relabelled, and the browser's density declared with the window in
        // points × that density.
        let before = written(&wire).len();
        let body = output_scale_body((1024, 768), 2.0);
        read_output_scale(&mut body.as_slice(), &uplink, &desktop, &test_shadow((1024, 768)), &sink)
            .await
            .unwrap();
        assert_eq!(desktop.lock().unwrap().density, Density::Reported);
        assert!(matches!(
            forwarded(&sink, &mut rx).await,
            Some(ServerMsg::Resize { w: 1024, h: 768, scale }) if scale == 2.0
        ));
        assert_eq!(written(&wire)[before..], client_density((3456, 1766), 2.0)[..]);

        let mut d = desktop.lock().unwrap();
        d.first_update();
        assert_eq!(d.density, Density::Reported);
        d.density = Density::Off;
        d.first_update();
        assert_eq!(d.density, Density::Off);
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
        key_on(false, keys, code, pressed, caps)
    }

    /// [`key`] against a server that is (or is not) a Mac.
    fn key_on(
        macos: bool,
        keys: &mut HashMap<String, u32>,
        code: &str,
        pressed: bool,
        caps: bool,
    ) -> Vec<u8> {
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
            macos,
        )
        .concat()
    }

    #[test]
    fn a_mac_gets_alt_as_meta_and_releases_what_it_pressed() {
        let mut keys = HashMap::new();
        // Alt goes out as Meta_L so Screen Sharing lands it on Option, and the
        // release is the keysym that was pressed, not a fresh X11 lookup.
        assert_eq!(key_on(true, &mut keys, "AltLeft", true, false), key_event(true, 0xFFE7));
        assert_eq!(key_on(true, &mut keys, "AltLeft", false, false), key_event(false, 0xFFE7));
        assert_eq!(key_on(true, &mut keys, "AltRight", true, false), key_event(true, 0xFFE8));
        // The Windows key already lands on Command as Super_L, so it is unchanged,
        // and a generic server still gets Alt_L for Alt.
        assert_eq!(key_on(true, &mut keys, "MetaLeft", true, false), key_event(true, 0xFFEB));
        assert_eq!(key_on(false, &mut keys, "AltLeft", true, false), key_event(true, 0xFFE9));
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

    /// Caps Lock leaves a Mac's shortcuts alone: Command-Z is Undo under it, not
    /// the Command-Shift-Z the uppercase keysym would post. Other servers, and
    /// letters typed without Command or Control, keep the case.
    #[test]
    fn caps_lock_does_not_shift_a_mac_shortcut() {
        for modifier in ["MetaLeft", "ControlRight"] {
            let mut keys = HashMap::new();
            key_on(true, &mut keys, modifier, true, true);
            assert_eq!(key_on(true, &mut keys, "KeyZ", true, true), key_event(true, 0x7a));
        }
        let mut keys = HashMap::new();
        key_on(false, &mut keys, "ControlLeft", true, true);
        assert_eq!(key_on(false, &mut keys, "KeyZ", true, true), key_event(true, 0x5a));
        let mut keys = HashMap::new();
        key_on(true, &mut keys, "AltLeft", true, true);
        assert_eq!(key_on(true, &mut keys, "KeyZ", true, true), key_event(true, 0x5a));
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
        assert!(format!("{err:#}").contains("message type 0x01"), "{err:#}");
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
        let (sink, mut rx) = sized_sink((2, 2)).await;
        let shadow = test_shadow((2, 2));
        let shared = test_shared(uplink, shared_desktop((2, 2), None, None), Arc::clone(&shadow));
        // Kept, not discarded: a decoder that bailed on the third encoding would
        // leave the first rectangle's pixels in the shadow and the check below would
        // still pass. Running out of stream is the only acceptable way to stop.
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
        assert_eq!(units(&mut rx).len(), 1, "one update, one access unit");
        // Every encoding decoded the same picture, so the shadow — which holds what
        // the last of them left — matches the first.
        let held = shadow.lock().unwrap().copy_out(Rect::from_size(0, 0, 2, 2).unwrap());
        let first: Vec<u8> = bgrx.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0]]).collect();
        assert_eq!(held, Some(first), "five encodings of one picture, one picture");
    }

    /// CopyRect saves the VNC link its pixels: the source is read back out of the
    /// shadow and lands at the destination, in the mirror the next unit encodes.
    #[tokio::test]
    async fn a_copy_rect_is_read_back_out_of_the_shadow() {
        let wire = update(&[
            raw_rect(0, 0, 2, 2, [0x30, 0x20, 0x10]),
            copy_rect((2, 0, 2, 2), (0, 0)),
        ]);

        let (uplink, _sent) = test_uplink();
        let (sink, mut rx) = sized_sink((4, 2)).await;
        let shadow = test_shadow((4, 2));
        let shared = test_shared(uplink, shared_desktop((4, 2), None, None), Arc::clone(&shadow));
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
        assert_eq!(units(&mut rx).len(), 1, "the paint and the copy are one update");
        let shadow = shadow.lock().unwrap();
        assert_eq!(
            shadow.copy_out(Rect::from_size(2, 0, 2, 2).unwrap()),
            shadow.copy_out(Rect::from_size(0, 0, 2, 2).unwrap()),
            "the copy landed at the destination"
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
        let (sink, mut rx) = sized_sink((2, 2)).await;
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
        assert_eq!(units(&mut rx).len(), 1, "the rectangle behind the empty one");
    }

    #[tokio::test]
    async fn a_rectangle_split_across_records_still_becomes_a_frame() {
        let update = raw_update();
        let (a, b) = update.split_at(update.len() - 6);
        let wire = framed(&[a.to_vec(), b.to_vec()]);

        let (uplink, _sent) = test_records_uplink(apple_keys());
        let (sink, mut rx) = sized_sink((2, 2)).await;
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

        // The rectangle arrived whole, which is the point: one 2x2 frame.
        sink.flush().await;
        assert_eq!(units(&mut rx).len(), 1, "one frame");
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
            Some(Apple::default()),
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

        {
            let state = clipboard.lock().unwrap();
            assert_eq!(state.apple_requests, 1);
            assert!(state.apple_fetch_pending);
        }
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
    async fn apple_clipboard_fetches_cut_a_gap_in_the_pixel_stream() {
        let status = [0x14, 0, 0, 4, 0, 1, 0, 2];
        let mut wire = status.repeat(2);
        wire.extend_from_slice(&[0, 0, 0, 0]); // one empty FramebufferUpdate
        wire.extend_from_slice(&vnc_apple_clipboard::send(7, "first").unwrap());
        wire.extend_from_slice(&vnc_apple_clipboard::send(7, "second").unwrap());

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
            ReadFlags { clipboard: true, poll: true },
            Some(Apple::default()),
            sink,
        )
        .await;

        let mut expected = vnc_apple_clipboard::fetch(0).to_vec();
        // The second change coalesces while the first fetch is outstanding. Its
        // follow-up must precede the next pixel request or the Mac can refill the
        // ordered stream with frames and strand the pasteboard again.
        expected.extend_from_slice(&vnc_apple_clipboard::fetch(7));
        expected.extend_from_slice(&update_request(true, (2, 2)));
        assert_eq!(written(&sent), expected);
    }

    #[tokio::test]
    async fn an_unanswered_apple_clipboard_fetch_resumes_polling_after_an_idle_gap() {
        tokio::time::pause();
        for repaint_pending in [false, true] {
            let status = [0x14, 0, 0, 4, 0, 1, 0, 2];
            let fence = server_fence(FENCE_REQUEST, b"idle");
            let (mut server, reader) = tokio::io::duplex(256);
            let (uplink_wire, mut mac) = tokio::io::duplex(256);
            let uplink = Arc::new(Mutex::new(Uplink::plain(uplink_wire)));
            let (sink, _rx) = test_sink();
            let shared = test_shared(
                uplink,
                shared_desktop((2, 2), None, None),
                test_shadow((2, 2)),
            );
            let clipboard = shared.clipboard.clone();
            let task = tokio::spawn(read_loop(
                reader,
                shared,
                ReadFlags { clipboard: true, poll: true },
                Some(Apple::default()),
                sink,
            ));

            server.write_all(&status).await.unwrap();
            server.write_all(&status).await.unwrap();
            server.write_all(&[0, 0, 0, 0]).await.unwrap();
            server.write_all(&fence).await.unwrap();

            let mut before_idle = vnc_apple_clipboard::fetch(0).to_vec();
            before_idle.extend_from_slice(&client_fence(0, b"idle"));
            if repaint_pending {
                server.write_all(&apple_layout_update(Some(11), (2, 2))).await.unwrap();
                before_idle.extend_from_slice(&vnc_apple::auto_framebuffer_update((2, 2)));
                before_idle.extend_from_slice(&update_request(false, (2, 2)));
            }
            let mut observed = vec![0; before_idle.len()];
            mac.read_exact(&mut observed).await.unwrap();
            assert_eq!(observed, before_idle);
            {
                let state = clipboard.lock().unwrap();
                assert!(state.apple_fetch_pending);
                assert!(state.apple_fetch_again);
            }

            tokio::time::advance(APPLE_CLIPBOARD_IDLE_GAP + Duration::from_millis(1)).await;
            let mut resumed = [0; 10];
            mac.read_exact(&mut resumed).await.unwrap();
            assert_eq!(resumed, update_request(!repaint_pending, (2, 2)));
            {
                let state = clipboard.lock().unwrap();
                assert!(!state.apple_fetch_pending);
                assert!(!state.apple_fetch_again);
            }

            task.abort();
        }
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
            Some(Apple::default()),
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
        // Declared past Apple's own limit, and so never inflated.
        let mut oversized = vec![0x1f, 0, 0, 0];
        oversized.extend_from_slice(&9u32.to_be_bytes());
        oversized.extend_from_slice(&(vnc_apple_clipboard::MAX_ARCHIVE_BYTES + 1).to_be_bytes());
        oversized.extend_from_slice(&4u32.to_be_bytes());
        oversized.extend_from_slice(&[0; 4]);

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

    /// When a server drives its own updates, a request per update would race that
    /// schedule. Apple's measured server leaves `poll` true despite being armed.
    #[tokio::test]
    async fn a_server_driven_session_does_not_poll_for_the_next_update() {
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

    /// The shutdown drain: what was queued behind a slow socket — the session
    /// layer's releases last — is on the wire before the writer is aborted.
    #[tokio::test]
    async fn the_shutdown_drain_waits_for_queued_input_to_be_written() {
        let (sock, mut server) = tokio::io::duplex(16);
        let (uplink, backlog, writer) = Uplink::plain(sock).queued();
        let writer = tokio::spawn(writer);
        let uplink: SharedUplink = Arc::new(Mutex::new(uplink));

        let mut queued = vec![vec![0u8; 64]; 8];
        queued.push(key_event(false, 0xffe3).to_vec());
        send_all(&uplink, &queued).await.unwrap();
        assert!(backlog.behind(1), "a 16-byte socket cannot have taken it all yet");

        let expected: Vec<u8> = queued.concat();
        let reader = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut wire = vec![0u8; expected.len()];
            server.read_exact(&mut wire).await.unwrap();
            assert_eq!(wire, expected);
        });
        tokio::time::timeout(SHUTDOWN_DRAIN, backlog.room(1))
            .await
            .expect("the writer did not drain inside the shutdown bound");
        writer.abort();
        reader.await.unwrap();
    }

    /// The circle a busy Mac closed, with the server's half of it held still: input
    /// the server has not read fills its socket, and the loop must go on reading
    /// pixels anyway — asking for the next update behind that input rather than
    /// waiting, with the uplink held, for a server that reads nothing until it has
    /// been read from.
    #[tokio::test]
    async fn a_server_that_is_not_reading_is_still_read_from() {
        // Sixteen bytes of socket, and nobody reading the other end of it.
        let (sock, mut unread) = tokio::io::duplex(16);
        let (uplink, backlog, writer) = Uplink::plain(sock).queued();
        let writer = tokio::spawn(writer);
        let uplink: SharedUplink = Arc::new(Mutex::new(uplink));

        // The input side, well past what that socket takes.
        let flood = vec![vec![0u8; 64]; 64];
        tokio::time::timeout(Duration::from_secs(5), send_all(&uplink, &flood))
            .await
            .expect("a send waited on a socket nobody is reading")
            .unwrap();

        // Every update earns a request for the next, which is a send of its own.
        let updates: Vec<u8> = (0..32).flat_map(|shade| raw_rect_update(0, 0, 2, 2, shade)).collect();
        let (sink, mut frames) = test_sink();
        tokio::spawn(async move { while frames.recv().await.is_some() {} });
        let shared = test_shared(
            Arc::clone(&uplink),
            shared_desktop((2, 2), None, None),
            test_shadow((2, 2)),
        );
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            read_loop(
                std::io::Cursor::new(updates),
                shared,
                ReadFlags { clipboard: false, poll: true },
                None,
                sink,
            ),
        )
        .await
        .expect("the read loop stopped reading behind a send")
        .unwrap_err();
        assert!(format!("{err:#}").contains("closed the connection"), "{err:#}");

        // Nothing was dropped to get there, and the requests went out behind the
        // input they were queued behind.
        let mut sent = vec![0u8; 64 * 64 + 1];
        unread.read_exact(&mut sent).await.unwrap();
        assert!(sent[..64 * 64].iter().all(|&b| b == 0));
        assert_eq!(sent[64 * 64], 3, "a FramebufferUpdateRequest follows the input");
        let mut rest = vec![0u8; 32 * 10 - 1];
        unread.read_exact(&mut rest).await.unwrap();
        backlog.room_for_media().await;
        assert!(!writer.is_finished(), "the writer outlives everything queued so far");
    }

    /// What waits behind a slow uplink is where the pointer is and how far the
    /// scroll got, not the history of either.
    #[test]
    fn held_motion_keeps_the_newest_position_and_the_sum_of_the_scroll() {
        let mut held = HeldMotion::default();
        assert!(held.is_empty());
        for x in 0..100 {
            assert!(held.hold(ClientMsg::MouseMove { x, y: 2 * x }).is_none());
        }
        assert!(held.hold(ClientMsg::Wheel { dx: 0.0, dy: 30.0, unit: WheelUnit::Pixel }).is_none());
        assert!(held.hold(ClientMsg::Wheel { dx: 0.0, dy: 2.0, unit: WheelUnit::Line }).is_none());
        assert!(held.hold(ClientMsg::Wheel { dx: -4.0, dy: -12.0, unit: WheelUnit::Pixel }).is_none());
        assert!(!held.is_empty());

        // The position first: the scroll lands where the pointer is.
        let out: Vec<ClientMsg> = held.take().collect();
        assert!(matches!(out[0], ClientMsg::MouseMove { x: 99, y: 198 }));
        let line = Wheel::LINE_PX;
        assert!(matches!(
            out[1],
            ClientMsg::Wheel { dx, dy, unit: WheelUnit::Pixel } if dx == -4.0 && dy == 18.0 + 2.0 * line
        ));
        assert_eq!(out.len(), 2);
        assert!(held.is_empty(), "taken is no longer held");
    }

    /// A scroll that outran the link is shed at one event's worth, and nothing that
    /// is not motion is ever held.
    #[test]
    fn held_scroll_is_capped_and_only_motion_is_held() {
        let mut held = HeldMotion::default();
        for _ in 0..50 {
            held.hold(ClientMsg::Wheel { dx: 0.0, dy: 400.0, unit: WheelUnit::Pixel });
        }
        held.hold(ClientMsg::Wheel { dx: f32::NAN, dy: 0.0, unit: WheelUnit::Pixel });
        let out: Vec<ClientMsg> = held.take().collect();
        assert!(matches!(
            out[..],
            [ClientMsg::Wheel { dx, dy, .. }] if dx == 0.0 && dy == Wheel::MAX_PX
        ));

        let key = ClientMsg::Key { code: "KeyA".into(), pressed: true, caps: false };
        assert!(matches!(held.hold(key), Some(ClientMsg::Key { .. })));
        let click = ClientMsg::MouseButton { button: MouseButton::Left, pressed: true, clicks: 1 };
        assert!(matches!(held.hold(click), Some(ClientMsg::MouseButton { .. })));
        assert!(held.is_empty());
    }

    /// Behind is a count of what the writer still holds, and room is its falling.
    #[tokio::test]
    async fn the_backlog_is_behind_until_the_writer_has_written() {
        let (sock, mut server) = tokio::io::duplex(16);
        let (mut uplink, backlog, writer) = Uplink::plain(sock).queued();
        let writer = tokio::spawn(writer);
        assert!(!backlog.behind(Backlog::MOTION_LIMIT));
        uplink.send(&vec![0u8; Backlog::MOTION_LIMIT + 16]).await.unwrap();
        assert!(backlog.behind(Backlog::MOTION_LIMIT));

        let mut sent = vec![0u8; Backlog::MOTION_LIMIT + 16];
        server.read_exact(&mut sent).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), backlog.room(Backlog::MOTION_LIMIT))
            .await
            .expect("room never came though everything was read");
        assert!(!writer.is_finished());
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

    /// One FramebufferUpdate of a `w`×`h` wlshare VP9 frame whose opening byte is
    /// `first`, then a fence asking for `marker` back.
    fn wlshare_vp9_update(w: u16, h: u16, first: u8, marker: &[u8]) -> Vec<u8> {
        let mut msg = vec![0u8, 0];
        msg.extend_from_slice(&1u16.to_be_bytes());
        for field in [0, 0, w, h] {
            msg.extend_from_slice(&field.to_be_bytes());
        }
        msg.extend_from_slice(&ENCODING_WLSHARE_VP9.to_be_bytes());
        let mut frame = vec![0u8; 100];
        frame[0] = first;
        msg.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        msg.extend_from_slice(&frame);
        msg.extend_from_slice(&server_fence(FENCE_REQUEST, marker));
        msg
    }

    /// A read loop over a connection that stays open, so a held echo is not cut off by
    /// the end of the stream.
    fn open_read_loop(wire: Vec<u8>, shared: Shared, sink: VideoSink) -> tokio::task::JoinHandle<()> {
        let (mut server, client) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            server.write_all(&wire).await.unwrap();
            let _ = read_loop(client, shared, ReadFlags { clipboard: false, poll: false }, None, sink).await;
            drop(server);
        })
    }

    /// wlshare's VP9 goes to the browser as it came, and the fence behind a frame is
    /// echoed only once the browser's side has let go of it: wlshare walks its
    /// quality by that round trip, and must see the browser's queue in it.
    #[tokio::test]
    async fn a_passed_frames_fence_waits_for_the_browser_to_take_it() {
        let (uplink, sent) = test_uplink();
        let (sink, mut rx) = sized_sink((64, 32)).await;
        let mut shared = test_shared(uplink, shared_desktop((64, 32), None, None), test_shadow((64, 32)));
        shared.passthrough = Some(rfb38_encoding_list(false, false, false, false).into());
        let task = open_read_loop(wlshare_vp9_update(64, 32, 0xa0, b"f1"), shared, sink);

        assert!(matches!(rx.recv().await, Some(ServerMsg::VideoFormat { .. })));
        let Some(ServerMsg::Video(unit)) = rx.recv().await else {
            panic!("the frame was not passed");
        };
        assert!(unit.keyframe && unit.data.len() == 100 && unit.data[0] == 0xa0);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(written(&sent).is_empty(), "the fence was echoed while the browser held the frame");

        drop(unit);
        let echo = client_fence(0, b"f1");
        tokio::time::timeout(Duration::from_secs(2), async {
            while written(&sent) != echo {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the fence was echoed once the frame was taken");
        task.abort();
    }

    /// A browser that never acknowledges holds a fence no longer than the grace a
    /// window that is not drawing gets, so wlshare, which sends nothing until the
    /// echo, is not stopped by it.
    #[tokio::test]
    async fn a_passed_frames_fence_is_held_no_longer_than_the_limit() {
        let (uplink, sent) = test_uplink();
        let (sink, mut rx) = sized_sink((64, 32)).await;
        let mut shared = test_shared(uplink, shared_desktop((64, 32), None, None), test_shadow((64, 32)));
        shared.passthrough = Some(rfb38_encoding_list(false, false, false, false).into());
        let started = tokio::time::Instant::now();
        let task = open_read_loop(wlshare_vp9_update(64, 32, 0xa0, b"f1"), shared, sink);

        assert!(matches!(rx.recv().await, Some(ServerMsg::VideoFormat { .. })));
        let _kept = rx.recv().await;
        let echo = client_fence(0, b"f1");
        tokio::time::timeout(FENCE_HOLD_LIMIT * 4, async {
            while written(&sent) != echo {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the fence was echoed with the frame still held");
        assert!(started.elapsed() >= FENCE_HOLD_LIMIT, "echoed before the limit");
        task.abort();
    }

    /// The limit runs from when the fence was queued, not from the loop's last turn:
    /// a server that keeps talking — here a Bell every 50 ms — turns the loop far more
    /// often than the limit, and must not hold the echo for as long as it talks.
    #[tokio::test]
    async fn a_held_fence_goes_at_its_deadline_while_the_server_keeps_talking() {
        let (uplink, sent) = test_uplink();
        let (sink, mut rx) = sized_sink((64, 32)).await;
        let mut shared = test_shared(uplink, shared_desktop((64, 32), None, None), test_shadow((64, 32)));
        shared.passthrough = Some(rfb38_encoding_list(false, false, false, false).into());
        let (mut server, client) = tokio::io::duplex(1 << 16);
        server.write_all(&wlshare_vp9_update(64, 32, 0xa0, b"f1")).await.unwrap();
        let talking = tokio::spawn(async move {
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                server.write_all(&[2]).await.unwrap(); // Bell
            }
            server
        });
        let task = tokio::spawn(async move {
            let _ = read_loop(client, shared, ReadFlags { clipboard: false, poll: false }, None, sink).await;
        });

        assert!(matches!(rx.recv().await, Some(ServerMsg::VideoFormat { .. })));
        let _kept = rx.recv().await;
        let echo = client_fence(0, b"f1");
        tokio::time::timeout(FENCE_HOLD_LIMIT * 3, async {
            while written(&sent) != echo {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the fence waited out the server's talking instead of its deadline");
        assert!(!talking.is_finished(), "the server had stopped talking before the echo");
        task.abort();
        talking.abort();
    }

    /// A resize past the ceiling takes the encoding off the list, so wlshare sends the
    /// desktop again for tiles; a frame already on its way is dropped and its fence
    /// goes back at once, and a resize back within the ceiling lists the encoding again.
    #[tokio::test]
    async fn a_desktop_past_the_ceiling_is_off_the_list_until_it_is_back_within() {
        let (small, big) = ((64, 32), (5376, 2288));
        let (uplink, sent) = test_uplink();
        let (frame_tx, mut rx) = mpsc::channel(64);
        let plan = crate::config::RenderPlan { quality: 60, adaptive: None, chroma: Chroma::Full, apple_hevc: false };
        let feedback = Arc::new(crate::feedback::LinkFeedback::new());
        let sink = VideoSink::new("vnc", frame_tx, plan, feedback, TileSupport::Rects);
        sink.msg(ServerMsg::Resize { w: small.0, h: small.1, scale: UNSCALED }).await.unwrap();
        let mut shared = test_shared(uplink, shared_desktop(small, None, None), test_shadow(small));
        let encodings = rfb38_encoding_list(false, false, false, false);
        shared.passthrough = Some(encodings.clone().into());
        let mut wire = update(&[geometry(0, 0, big.0, big.1, ENCODING_DESKTOP_SIZE)]);
        wire.extend_from_slice(&wlshare_vp9_update(big.0, big.1, 0xa0, b"f1"));
        wire.extend_from_slice(&update(&[geometry(0, 0, small.0, small.1, ENCODING_DESKTOP_SIZE)]));
        let _ = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: false, poll: false },
            None,
            sink,
        )
        .await;

        let wrote = written(&sent);
        let mut from = 0;
        for (what, expected) in [
            ("the list without it", set_encodings(&encodings)),
            ("the echo", client_fence(0, b"f1")),
            ("the list with it", set_encodings(&with_wlshare_vp9(&encodings))),
        ] {
            let at = wrote[from..]
                .windows(expected.len())
                .position(|window| window == expected)
                .unwrap_or_else(|| panic!("{what} did not follow"));
            from += at + expected.len();
        }
        while let Ok(msg) = rx.try_recv() {
            assert!(!matches!(msg, ServerMsg::Video(_) | ServerMsg::VideoFormat { .. }), "{msg:?}");
        }
    }

    /// Before anything is passed, a fence goes back at once and in order, as on any
    /// other session: there is no browser queue yet for it to report.
    #[tokio::test]
    async fn a_fence_goes_back_at_once_while_nothing_is_passed() {
        let mut wire = server_fence(FENCE_REQUEST | FENCE_BLOCK_AFTER, b"marker");
        wire.extend_from_slice(&raw_update());

        let (uplink, sent) = test_uplink();
        let (sink, _rx) = sized_sink((2, 2)).await;
        let mut shared = test_shared(uplink, shared_desktop((2, 2), None, None), test_shadow((2, 2)));
        shared.passthrough = Some(rfb38_encoding_list(false, false, false, false).into());
        let _ = read_loop(
            std::io::Cursor::new(wire),
            shared,
            ReadFlags { clipboard: false, poll: true },
            None,
            sink,
        )
        .await;

        let mut expected = client_fence(FENCE_BLOCK_AFTER, b"marker");
        expected.extend_from_slice(&update_request(true, (2, 2)));
        assert_eq!(written(&sent), expected);
    }

    /// A held fence that asks for BlockAfter is honoured by holding the reading too:
    /// nothing behind it is read until it has gone back.
    #[tokio::test]
    async fn a_held_block_after_fence_stops_the_reading_until_it_goes() {
        let (uplink, sent) = test_uplink();
        let (sink, mut rx) = sized_sink((64, 32)).await;
        let mut shared = test_shared(uplink, shared_desktop((64, 32), None, None), test_shadow((64, 32)));
        shared.passthrough = Some(rfb38_encoding_list(false, false, false, false).into());
        let mut wire = wlshare_vp9_update(64, 32, 0xa0, b"f1");
        wire.truncate(wire.len() - server_fence(FENCE_REQUEST, b"f1").len());
        wire.extend_from_slice(&server_fence(FENCE_REQUEST | FENCE_BLOCK_AFTER, b"f1"));
        wire.extend_from_slice(&wlshare_vp9_update(64, 32, 0xa4, b"f2"));
        let task = open_read_loop(wire, shared, sink);

        assert!(matches!(rx.recv().await, Some(ServerMsg::VideoFormat { .. })));
        let Some(ServerMsg::Video(unit)) = rx.recv().await else {
            panic!("the frame was not passed");
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv()).await.is_err(),
            "the frame behind a held BlockAfter fence was read"
        );
        assert!(written(&sent).is_empty(), "the fence was echoed while the browser held the frame");

        drop(unit);
        let next = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the frame behind the fence was read once it went back");
        assert!(matches!(next, Some(ServerMsg::Video(unit)) if !unit.keyframe && unit.data[0] == 0xa4));
        assert_eq!(written(&sent), client_fence(FENCE_BLOCK_AFTER, b"f1"));
        task.abort();
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
        let (sink, mut rx) = sized_sink((2, 2)).await;
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
            !units(&mut rx).is_empty(),
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
        let (sink, mut rx) = sized_sink((2, 2)).await;
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
            !units(&mut rx).is_empty(),
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
        let apple = Apple::default();

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
        let apple = Apple::default();

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
    /// copied: it encodes the record offsets, and a second copy of those
    /// would have to be kept in step with the parser by hand. `vnc_apple` is also
    /// where it is cross-checked against a captured payload.
    use crate::vnc_apple::{TestScreen, test_layout_wire as layout_payload};

    /// A selection sent while a server scale is still in flight is judged
    /// against that scale, and the layouts answering messages sent before the
    /// newest factor do not start a counter-request: the Mac applies messages in
    /// order, so only the layout at the pending factor has caught up.
    #[test]
    fn standard_server_scaling_follows_the_messages_in_flight() {
        use crate::vnc_apple::{parse_layout, test_layout, test_scale_layout};
        const SCREENS: [TestScreen; 2] =
            [(1, (1280, 800), (1280, 800), 0x01), (7, (1440, 900), (2880, 1800), 0x00)];
        let mut state = DisplayState::default();

        // A 1x browser on All Displays over mixed densities: composed from the
        // native pixels, so nothing to ask.
        let native = parse_layout(&test_layout(None, &SCREENS)).unwrap();
        assert_eq!(state.accept_apple_layout(&native, 1.0), None);

        // The Retina screen is picked, and asks for half.
        assert_eq!(state.request_apple_scale(Some(7), 1.0), Some(0.5));
        // The selection's own answer comes first, still at 1: it predates the
        // factor, so it asks for nothing — not 1 again.
        let picked = parse_layout(&test_layout(Some(7), &SCREENS)).unwrap();
        assert_eq!(state.accept_apple_layout(&picked, 1.0), None);
        let mut halved = test_layout(
            Some(7),
            &[(1, (1280, 800), (640, 400), 0x01), (7, (1440, 900), (1440, 900), 0x00)],
        );
        test_scale_layout(&mut halved, 0.5, &[1.0, 2.0]);
        let halved = parse_layout(&halved).unwrap();
        assert_eq!(state.accept_apple_layout(&halved, 1.0), None);
        assert!(state.apple_scale_pending.is_none());

        // Back to All Displays before the Mac answers a pick of the 1x screen:
        // both want 1, which is already on its way.
        assert_eq!(state.request_apple_scale(Some(1), 1.0), Some(1.0));
        assert_eq!(state.request_apple_scale(None, 1.0), None);
        // A 2x browser display needs no scaling on the 1x screen either.
        assert_eq!(state.request_apple_scale(Some(1), 2.0), None);

        // A request the Mac never answered stops standing in for the factor in
        // force. The 1 asked for the 1x screen is still on its way, so picking
        // that screen again asks for nothing; once it has gone unanswered too
        // long, the last layout's 0.5 counts again and the pick asks anew.
        assert_eq!(state.request_apple_scale(Some(1), 1.0), None, "still on its way");
        let sent = std::time::Instant::now().checked_sub(APPLE_SCALE_ANSWER).unwrap();
        state.apple_scale_pending = Some((1.0, sent));
        assert_eq!(state.request_apple_scale(Some(1), 1.0), Some(1.0));
    }

    /// Standard's pointer addresses the unscaled framebuffer, so a position on
    /// a framebuffer the Mac halved is doubled on the way out, and one on an
    /// unscaled framebuffer — or with no Standard layout at all — is not.
    #[test]
    fn a_standard_pointer_is_sent_in_native_pixels() {
        use crate::vnc_apple::{parse_layout, test_layout, test_scale_layout};
        let mut state = DisplayState::default();
        assert_eq!(state.apple_pointer(720, 450), (720, 450));

        let mut halved = test_layout(Some(7), &[(7, (1440, 900), (1440, 900), 0x00)]);
        test_scale_layout(&mut halved, 0.5, &[2.0]);
        state.apple_layout = Some(parse_layout(&halved).unwrap());
        assert_eq!(state.apple_pointer(720, 450), (1440, 900));

        state.apple_layout = Some(parse_layout(&test_layout(Some(7), &[(7, (1440, 900), (2880, 1800), 0x00)])).unwrap());
        assert_eq!(state.apple_pointer(1440, 900), (1440, 900));
    }

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
        let resized = read_display_layout(&mut payload.as_slice(), &shared, false, false, &sink)
            .await
            .unwrap();
        assert!(resized);

        // This first layout still carries Apple's opening 1.0 viewer scale, so
        // its returned framebuffer and effective density are reported exactly as
        // received while the server-side correction is requested.
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

        // What went back: ask Apple to return this 2x screen at the browser's 1x
        // density, then re-arm for the display the Mac confirmed. The enclosing
        // update loop sends the paired full request after it has consumed every
        // rectangle in this FramebufferUpdate.
        let mut expected = vnc_apple::set_server_scaling(0.5);
        expected.extend_from_slice(&vnc_apple::auto_framebuffer_update((3840, 2160)));
        assert_eq!(written(&sent), expected);
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
        read_display_layout(&mut layout(None).as_slice(), &shared, false, false, &sink)
            .await
            .unwrap();
        assert_eq!(shared.display.lock().unwrap().active, DisplayState::COMBINED);

        // Then a screen, then back again. Each move is a layout, never a request.
        read_display_layout(&mut layout(Some(22)).as_slice(), &shared, false, false, &sink)
            .await
            .unwrap();
        assert_eq!(shared.display.lock().unwrap().active, 22);
        read_display_layout(&mut layout(Some(22)).as_slice(), &shared, false, false, &sink)
            .await
            .unwrap();
        read_display_layout(&mut layout(None).as_slice(), &shared, false, false, &sink)
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
