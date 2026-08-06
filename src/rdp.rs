//! Server-side RDP session driven by [FreeRDP](https://www.freerdp.com), through
//! the safe wrapper in the [`freerdp`] crate.
//!
//! The web server never speaks RDP to the browser: [`crate::ws`] bridges a
//! browser WebSocket to [`run`] here over a pair of channels. `run` starts a
//! session — FreeRDP's own thread does TCP, TLS, CredSSP and activation — then
//! drives it, turning damage into [`ServerMsg::Tile`] updates and [`ClientMsg`]
//! input into RDP input events.
//!
//! ## Threads
//!
//! There are three, and only the middle one is new here:
//!
//! ```text
//!   session thread (current-thread tokio)  ──  the select! loop below
//!         │  Input/Clipboard commands                    ▲  Event
//!         ▼                                              │
//!   the wrapper's queue ──> FreeRDP's thread ──> a forwarding thread ──> tokio
//! ```
//!
//! FreeRDP's event loop is a blocking `WaitForMultipleObjects`, so it owns an OS
//! thread and hands events out through a `std::sync::mpsc::Receiver` — which
//! cannot be awaited. [`bridge_events`] is the one-line thread that turns it into
//! something `select!` can take. Everything downstream of that is unchanged from
//! the IronRDP engine this replaced: the same damage coalescing, the same shadow,
//! the same tiles.
//!
//! ## Sound is not on that diagram
//!
//! Deliberately. The remote's audio leaves FreeRDP through a sink called in
//! place on its thread and goes straight into [`crate::audio`]'s queue — it never
//! enters the event channel above, and so can never queue behind a backlog of
//! damage rectangles. [`crate::rdp_audio`] is the whole of the adapter.
//!
//! See docs/architecture.md for the design.

use std::sync::Arc;

use freerdp::{Clipboard, ClipboardEvent, ClipboardFormat, Connect, Event, Frame, Framebuffer,
    Input, MouseButton as RdpButton, Session};
use log::{debug, info, warn};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::audio::AudioBridge;
use crate::config::{Security, TargetConfig};
use crate::copies;
use crate::encode::TileSink;
use crate::engine::{self, clamp_u16};
use crate::keymap;
use crate::protocol::{
    ClientMsg, ClipboardSnapshot, CopyRect, CursorShape, MAX_CURSOR_DIM, MouseButton, ServerMsg,
    UNSCALED,
};
use crate::rdp_audio;
use crate::rdp_clipboard::{self, CF_UNICODETEXT};
use crate::tiles::{self, Rect, Shadow};

// A Windows peer can advertise Unicode text, fail the first FormatDataRequest,
// then satisfy a retry shortly afterward. Retrying only after that explicit
// failure keeps the normal path fast and stays entirely separate from a remote
// Paste, which arrives as ClipboardEvent::LocalDataRequest instead.
const CLIPBOARD_READ_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(150),
    Duration::from_millis(400),
];

struct PendingClipboardRead {
    format: u32,
    failures: usize,
}

impl PendingClipboardRead {
    fn new(format: u32) -> Self {
        Self { format, failures: 0 }
    }

    fn retry_after_failure(&mut self) -> Option<Duration> {
        let delay = CLIPBOARD_READ_RETRY_DELAYS.get(self.failures).copied();
        if delay.is_some() {
            self.failures += 1;
        }
        delay
    }
}

// A layout — a size, a density, or both — the remote has been asked for and has
// not answered.
//
// Two things are waited out here, and a single schedule covers both. The first is
// the Display Control channel not yet being up: a layout sent then cannot go out
// at all, which is exactly what a resize reported from `connected` hits. The
// second is Windows applying a monitor layout only once the session it is
// starting has settled — one sent seconds after connect is discarded in silence
// *even after* the channel is ready. Nothing in the protocol names what is still
// missing, and nothing acknowledges a layout either, so the only way to tell a
// refusal from a delay is that the resize never comes.
//
// That second half was re-measured against the same Windows host through FreeRDP
// while this engine was being written, and it is not a quirk of either library:
// a byte-identical 800x600 layout was discarded 400 ms after the server's own
// DisplayControl capabilities PDU and honoured 6.7 s into the same session. The
// engine crate documents it and deliberately does not retry — a ladder needs a
// clock and a policy, and both are here.
//
// Hence a schedule rather than a single attempt, and one retry rather than two:
// FreeRDP's own Display Control client (client/X11/xf_disp.c) holds a single
// desired layout and re-sends *that* — size and scale factors ride the same PDU —
// so a size and a density can never race into two reactivations that desync
// `applied` from the desktop actually negotiated. The total is bounded because a
// server that will never honour this — anything not Windows, most likely — must
// not be asked forever.
const LAYOUT_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(750),
    Duration::from_millis(1500),
    Duration::from_secs(3),
    Duration::from_secs(6),
];

struct PendingLayout {
    layout: Layout,
    attempts: usize,
}

impl PendingLayout {
    fn new(layout: Layout) -> Self {
        Self { layout, attempts: 0 }
    }

    fn wait_again(&mut self) -> Option<Duration> {
        let delay = LAYOUT_RETRY_DELAYS.get(self.attempts).copied();
        if delay.is_some() {
            self.attempts += 1;
        }
        delay
    }
}

/// What came of asking for a layout.
///
/// Three outcomes and not a bool, because one of them is worth another attempt
/// and two are not, and a caller that cannot tell them apart either gives up on a
/// channel that was merely still opening or asks forever for a desktop it already
/// has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// Queued for the remote. Not a confirmation: only a resize is that, so this
    /// is still worth repeating if none arrives.
    Sent,
    /// The desktop already has this layout, so there was nothing to ask.
    Redundant,
    /// The channel cannot carry it yet. Worth asking again unchanged.
    NotReady,
}

/// How long a session may take to report its first desktop.
///
/// FreeRDP owns the socket now, so this covers what used to be two budgets: the
/// TCP connect (bounded inside FreeRDP by [`Connect::connect_timeout`], which is
/// set from [`engine::TCP_CONNECT_TIMEOUT`] so a switched-off host is still
/// reported as a connect failure rather than as a stall) and everything after it
/// — TLS, CredSSP, licensing, capability exchange. Their sum, so neither can eat
/// the other's time.
fn connect_budget() -> Duration {
    engine::TCP_CONNECT_TIMEOUT + engine::HANDSHAKE_TIMEOUT
}

/// Connect to the RDP host, then drive the session until it ends.
///
/// `input_rx` carries browser input; `frame_tx` carries screen updates back.
/// Both closing (browser gone / RDP ended) tears the session down.
///
/// A thin wrapper so the shutdown cannot be missed. Everything this engine sends the
/// client goes through a [`TileSink`], which forwards from a task of its own — and
/// the engine thread's runtime dies with this function, so anything the sink still
/// held would be lost. That includes the session's final `Error`, whose absence
/// would put the browser back on the picker with nothing to explain why. The body has
/// several early returns; this has one exit, and [`TileSink::finish`] is on it.
///
/// `audio` is `Some` exactly for a target that opted in, and it goes no further
/// than [`rdp_audio::connect`]: sound leaves this engine by the sink FreeRDP
/// calls on its own thread, never through the `select!` below. That is the whole
/// separation — see [`crate::rdp_audio`].
pub async fn run(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
    audio: Option<Arc<AudioBridge>>,
    feedback: Arc<crate::feedback::LinkFeedback>,
) {
    let sink = TileSink::new("rdp", frame_tx, config.render_plan(), feedback);
    session(config, input_rx, &sink, audio).await;
    sink.finish().await;
}

/// Turn the wrapper's blocking event receiver into one `select!` can await.
///
/// One thread, doing nothing but forwarding. It exists because the two halves
/// disagree about blocking, not because anything here needs concurrency: FreeRDP's
/// loop is a blocking wait on handles it owns, so its events come out of a
/// `std::sync::mpsc::Receiver`, and awaiting one of those inside a tokio task would
/// park the whole runtime.
///
/// Unbounded, and that is safe for the same reason the damage path is: an `Event`
/// carries a *rectangle*, never pixels — the pixels are in the shared framebuffer —
/// and `stage_damage` folds overlapping rectangles and collapses past a cap. A slow
/// consumer makes the rectangles coarser rather than the queue longer.
///
/// The thread ends when either end goes: FreeRDP's session finishing closes the
/// sender, and this loop dropping the receiver ends the session.
fn bridge_events(events: std::sync::mpsc::Receiver<Event>) -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    if let Err(e) = std::thread::Builder::new()
        .name("rdp-events".into())
        .spawn(move || {
            for event in events {
                if tx.send(event).is_err() {
                    break; // the session loop has gone
                }
            }
        })
    {
        // The closure was never spawned, so its `tx` died with it and `rx` is
        // already closed — which the caller reads as a session that ended before
        // it connected, with this line as the reason.
        warn!("rdp: could not spawn the event forwarding thread: {e}");
    }
    rx
}

async fn session(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    sink: &TileSink,
    audio: Option<Arc<AudioBridge>>,
) {
    let (session, events) = Session::start(connect_config(&config, audio));
    let mut events = bridge_events(events);

    let Some((width, height)) = await_desktop(&mut events, &config, sink).await else {
        return;
    };
    info!("rdp: connected, desktop {width}x{height}");

    // 1x, always: the density this session ends up at is the attached client's to
    // state, and it has not spoken yet — the connect above happens before
    // `ServerMsg::Connected` reaches it. A Retina client is therefore one
    // reactivation away from where it wants to be, which is the price of learning
    // the density from whoever attaches rather than from the config file.
    if sink
        .msg(ServerMsg::Resize { w: width, h: height, scale: Density::One.scale() })
        .await
        .is_err()
    {
        return; // browser already gone
    }
    // No RDP server ships for macOS, so a Mac never answers here.
    if sink.msg(ServerMsg::RemoteOs { macos: false }).await.is_err() {
        return; // browser already gone
    }

    if let Err(e) = active_loop(
        &session,
        events,
        Flags {
            resize: config.resize,
            clipboard: config.clipboard,
            default_size: (config.width, config.height),
        },
        (width, height),
        input_rx,
        sink,
    )
    .await
    {
        warn!("rdp: session error: {e:#}");
        let _ = sink.msg(ServerMsg::Error { message: format!("RDP session ended: {e}") }).await;
    }
    info!("rdp: session terminated");
}

/// Wait for the first desktop, reporting a failure to the client exactly as
/// [`engine::connect_and_handshake`] does for the engines that still open their
/// own socket.
///
/// That symmetry is the point: the picker shows this sentence, and a user
/// switching between an RDP and a VNC target should not be able to tell which
/// library produced it. `None` means the caller has nothing left to do.
async fn await_desktop(
    events: &mut mpsc::UnboundedReceiver<Event>,
    config: &TargetConfig,
    sink: &TileSink,
) -> Option<(u16, u16)> {
    let dest = engine::host_port(&config.host, config.port);
    let report = async |message: String| {
        warn!("rdp: connect failed: {message}");
        let _ = sink.msg(ServerMsg::Error { message: format!("RDP connect failed: {message}") }).await;
    };

    // One deadline for the whole wait rather than one per event. The budget is
    // how long a session may take to report its *first desktop*, and a timeout
    // restarted on every event that is not `Connected` bounds the gap between
    // two events instead — which is not a bound at all for a session that keeps
    // saying something other than the thing being waited for.
    let budget = connect_budget();
    let deadline = Instant::now() + budget;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(Event::Connected { width, height })) => {
                // Narrowed rather than trusted: everything downstream — the
                // shadow, the tile grid, the pointer clamp — is `u16`, and a
                // desktop larger than that is a server saying something this
                // gateway cannot represent.
                return Some((narrow(width), narrow(height)));
            }
            // A failure before the desktop. `Ok(())` here is the odd one: an
            // *orderly* end with nothing having connected, which is what a
            // server that accepts and then drops the session looks like.
            Ok(Some(Event::Ended(result))) => {
                let cause = match result {
                    // The hint is *mentioned*, never concluded — the same posture
                    // `engine::tcp_connect` takes for the same permission, and for
                    // the same reason: on macOS 15 a denied local-network
                    // permission is refused indistinguishably from an address with
                    // no route, and nothing on this side can tell them apart.
                    Err(e) if e.is_unreachable() => {
                        format!("{e}{}", engine::LOCAL_NETWORK_HINT)
                    }
                    Err(e) => e.to_string(),
                    Ok(()) => format!("{dest} closed the session before it opened a desktop"),
                };
                report(cause).await;
                return None;
            }
            // Anything else before `Connected` is FreeRDP being busy, not an
            // answer — keep waiting rather than treating a cursor as a desktop.
            Ok(Some(_)) => continue,
            Ok(None) => {
                report(format!("the {dest} session ended before it reported a desktop")).await;
                return None;
            }
            Err(_) => {
                report(format!(
                    "{dest} did not open a desktop within {}s",
                    budget.as_secs()
                ))
                .await;
                return None;
            }
        }
    }
}

/// Everything the engine crate needs to open this target's session.
///
/// The keepalive is restated rather than applied: FreeRDP owns the socket, and it
/// applies `TCP_KEEPIDLE`/`TCP_KEEPINTVL`/`TCP_KEEPCNT` — and Linux's
/// `TCP_USER_TIMEOUT` — itself in `libfreerdp/core/tcp.c`. The numbers come from
/// [`crate::engine`] so that a silent host is noticed on the same schedule
/// whichever protocol is carrying it, and so
/// [`engine::keepalive_budget`](crate::engine::keepalive_budget) — which an error
/// message quotes — cannot drift from what the RDP path actually asks for.
fn connect_config(config: &TargetConfig, audio: Option<Arc<AudioBridge>>) -> Connect {
    Connect {
        host: config.host.clone(),
        port: config.port,
        username: config.username.clone(),
        password: config.password.clone(),
        domain: config.domain.clone(),
        width: u32::from(config.width),
        height: u32::from(config.height),
        security: match config.security {
            Security::Auto => freerdp::Security::Auto,
            Security::Nla => freerdp::Security::Nla,
            Security::Tls => freerdp::Security::Tls,
        },
        clipboard: config.clipboard,
        audio: rdp_audio::connect(audio),
        resize: config.resize,
        connect_timeout: engine::TCP_CONNECT_TIMEOUT,
        keepalive: engine::keepalive(),
    }
}

/// What [`active_loop`] needs off the target profile, grouped the way
/// [`crate::vnc`]'s own `Flags` is: three values that always travel together and
/// are only ever read from the same place.
struct Flags {
    resize: bool,
    clipboard: bool,
    /// The target's configured `width`/`height`, in *points*: the size this
    /// session asked the server for at connect, and so what
    /// [`ClientMsg::DefaultSize`] means here. Points rather than pixels because
    /// the density can move underneath it — see [`Density::pixels`].
    default_size: (u16, u16),
}

/// How dense a desktop this session has asked the RDP server to render.
///
/// Two steps, because that is what the far end can usefully be told: a display
/// is 1x or 2x, and the midpoint decides. Two steps also keep the `scale` on
/// [`ServerMsg::Resize`] integral, so
/// the pixels a client asks for and the points it presents them at are exact
/// inverses rather than a rounding of each other.
///
/// This is a density we *declare*, never one we read back: RDP has no PDU that
/// reports the scale factor a server settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Density {
    One,
    Two,
}

impl Density {
    /// What a [`ClientMsg::HostScale`] means here.
    ///
    /// Through `scale_ratio` rather than dividing by hand: that is the guard which
    /// turns a value no screen could have into 1x and centralizes the 1.5 midpoint
    /// used for RDP's two supported densities.
    fn from_host(scale: u16) -> Self {
        if crate::protocol::scale_ratio(scale) >= 1.5 {
            Self::Two
        } else {
            Self::One
        }
    }

    /// Percent, for the monitor layout's `DesktopScaleFactor`.
    ///
    /// Both values sit inside MS-RDPEDISP's legal 100 to 500, which is load-bearing
    /// rather than incidental: a server MUST ignore *both* scale factors when
    /// either is out of range, so an out-of-spec density would quietly cost the
    /// whole feature rather than part of it. The engine crate clamps to the same
    /// window, so this is belt and braces — but it is the end that *invents* the
    /// number, which is the end that has to be right.
    fn percent(self) -> u32 {
        match self {
            Self::One => 100,
            Self::Two => 200,
        }
    }

    /// The `scale` on [`ServerMsg::Resize`]: how many framebuffer pixels the
    /// remote draws per point of its own desktop.
    fn scale(self) -> f32 {
        match self {
            Self::One => UNSCALED,
            Self::Two => 2.0,
        }
    }

    /// The pixels `points` covers at this density — the answer to
    /// [`ClientMsg::DefaultSize`], which is the one size here that is configured
    /// rather than measured, and so the one that has to be converted.
    fn pixels(self, points: (u16, u16)) -> (u32, u32) {
        let px = |points: u16| u32::from(points) * self.percent() / 100;
        (px(points.0), px(points.1))
    }
}

/// A desktop the server is being asked for: the size in pixels, and the density
/// to declare with it.
///
/// The two travel together because sending one without the other is a bug in each
/// direction. A size with no density tells the server to ignore the scale factor,
/// which on a live 2x session means dropping back to 1x UI in a 2x framebuffer;
/// a density with no size leaves the desktop the same number of pixels and merely
/// shrinks everything drawn in them. MS-RDPEDISP puts both on one PDU, and
/// [`Input::resize`] takes both for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    w: u32,
    h: u32,
    density: Density,
}

impl Layout {
    /// The desktop as it currently stands, at the density it was last asked for.
    fn current(desktop: (u16, u16), density: Density) -> Self {
        Self { w: u32::from(desktop.0), h: u32::from(desktop.1), density }
    }

    /// The same request at the size the protocol would actually accept: an even
    /// width, and 200 to 8192 per axis.
    ///
    /// Through the engine crate's own rule rather than a copy of it, because this
    /// is used to decide *whether to ask at all* — and a comparison against a
    /// number different from the one that will be sent asks forever.
    fn adjusted(self) -> Self {
        let (w, h) = freerdp::sanitise_size(self.w, self.h);
        Self { w, h, ..self }
    }

    /// The same desktop — the same number of *points* — re-expressed at another
    /// density.
    ///
    /// The pixels scale with the density and the points stay put, which is the
    /// whole meaning of a density change on this protocol — the remote stays the
    /// same size on screen and merely sharper. This is how a size and a density
    /// that were requested separately merge into one layout: a
    /// viewport reported in the announced density's pixels is carried up to a
    /// denser one still waiting for the channel, so both reach the server as a
    /// single monitor layout rather than two.
    fn at_density(self, density: Density) -> Self {
        if density == self.density {
            return self;
        }
        let px = |v: u32| v * density.percent() / self.density.percent();
        Self { w: px(self.w), h: px(self.h), density }
    }
}

impl std::fmt::Display for Layout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{} at {}x", self.w, self.h, self.density.percent() / 100)
    }
}

/// The pointer, as this engine hands it to the browser.
///
/// RDP servers do not draw the cursor into the screen bitmap — they send its
/// shape and leave the drawing to whoever presents the framebuffer. Forwarding
/// the shape puts it on the browser's own hardware pointer, which moves with the
/// mouse and costs the session nothing per move — the same arrangement VNC's
/// Cursor pseudo-encoding already has, through the same [`ServerMsg::Cursor`].
/// The alternative, compositing it into the framebuffer, made every mouse move a
/// screen update: damage, the [`DAMAGE_INTERVAL`] flush, an encode, the socket, a
/// decode and a paint before the pointer had visibly moved.
#[derive(Default)]
struct Pointer {
    /// What the browser should draw, `None` being its own arrow — see
    /// [`ServerMsg::Cursor`].
    shape: Option<CursorShape>,
    /// The image `shape` was encoded from — `None` for the hidden and default
    /// pointers, which have no image of their own. Kept so a repeat is
    /// recognised before [`shape_of`] rather than after it.
    source: Option<freerdp::CursorImage>,
    /// `shape` has moved since the browser was last told.
    changed: bool,
}

impl Pointer {
    /// The server named a shape, hid the pointer, or asked for the client's
    /// default one.
    ///
    /// The last two are both the browser's own arrow here: this end has no
    /// default shape of its own to send, and on a remote desktop a pointer you
    /// cannot see at all is worse than a generic one.
    fn set(&mut self, cursor: freerdp::Cursor) {
        let image = match cursor {
            freerdp::Cursor::Image(image) => Some(image),
            freerdp::Cursor::Hidden | freerdp::Cursor::Default => None,
        };
        // Compared rather than assumed different, and compared **before**
        // [`shape_of`]: a server re-selecting a pointer out of its own cache
        // sends the same image again, and `shape_of` PNG-encodes it. Comparing
        // the encoded shapes instead still paid for the encode every time the
        // mouse crossed a window edge and saved only the bytes on the wire.
        if image == self.source {
            return;
        }
        let shape = image.as_ref().and_then(shape_of);
        self.source = image;
        // A new image whose shape is not new: an oversized or malformed pointer
        // encodes to `None`, and so may the one before it. Nothing to say then,
        // but the image is still worth remembering — it is what stops the next
        // copy of it reaching the encoder.
        if shape == self.shape {
            return;
        }
        self.shape = shape;
        self.changed = true;
    }

    /// The change to send, taken once per batch of events rather than per
    /// event: selecting a cached pointer produces a hide *and* a shape, and the
    /// browser should see the shape rather than flicker through its own arrow on
    /// the way to it.
    fn change(&mut self) -> Option<ServerMsg> {
        std::mem::take(&mut self.changed).then(|| ServerMsg::Cursor(self.shape.clone()))
    }

    /// The pointer a freshly attached browser cannot otherwise learn: the server
    /// sends a shape only when it changes, which may have been long before this
    /// browser arrived. Sent even when nothing has arrived yet, because it is
    /// also this engine's statement that the browser owns the pointer from here
    /// — without it a client would hide its own and draw nothing until the first
    /// pointer PDU.
    fn attached(&mut self) -> ServerMsg {
        self.changed = false;
        ServerMsg::Cursor(self.shape.clone())
    }
}

/// A decoded pointer as the shape the browser draws, or `None` for one it should
/// not draw: anything this client will not render.
///
/// The engine crate converts FreeRDP's xor/and masks to straight-alpha RGBA on
/// its side, which is what PNG wants.
fn shape_of(image: &freerdp::CursorImage) -> Option<CursorShape> {
    let (w, h) = (narrow(image.width), narrow(image.height));
    if w == 0 || h == 0 {
        return None;
    }
    if w > MAX_CURSOR_DIM || h > MAX_CURSOR_DIM {
        warn!("rdp: ignoring an oversized {w}x{h} pointer");
        return None;
    }
    let (hx, hy) = (narrow(image.hotspot_x), narrow(image.hotspot_y));
    match CursorShape::from_rgba(w, h, hx, hy, &image.rgba) {
        Ok(shape) => Some(shape),
        Err(e) => {
            warn!("rdp: ignoring a {w}x{h} pointer: {e}");
            None
        }
    }
}

async fn active_loop(
    session: &Session,
    mut events: mpsc::UnboundedReceiver<Event>,
    flags: Flags,
    connected_at: (u16, u16),
    mut input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    sink: &TileSink,
) -> anyhow::Result<()> {
    let Flags { resize, clipboard, default_size } = flags;
    let input = session.input();
    let framebuffer = session.framebuffer();

    let mut desktop = connected_at;
    // The pixels the browser has already been sent. Lives beside the framebuffer
    // it shadows, and is forgotten on a repaint and on a resize.
    let mut shadow = Shadow::new("rdp", desktop.0, desktop.1);
    shadow.classify_cells(sink.wants_cells());

    // Last known pointer position, so button/wheel events (which the browser
    // sends without coordinates) land where the cursor actually is.
    let mut last_pos: (u16, u16) = (desktop.0 / 2, desktop.1 / 2);
    // The pointer shape, on its way to the browser that draws it.
    let mut pointer = Pointer::default();

    // The remote's clipboard as last fetched, which is what answers the panel's
    // Fetch — RDP, like RFB, has no way to *ask* for the current contents, only
    // to react to a change. `None` means nothing has been copied over there
    // since this session started.
    let mut remote_clipboard: Option<ClipboardSnapshot> = None;
    // What the browser last sent, held until the remote actually pastes and
    // asks for it. That deferral is MS-RDPECLIP's delayed rendering: we
    // advertise the format, the bytes travel only if they are wanted.
    let mut local_clipboard: Option<String> = None;
    // A remote Copy/Cut whose delayed-rendered text we are fetching. The retry
    // deadline exists only after the remote explicitly refuses a request; one
    // successful response or a newer FormatList cancels the old generation.
    let mut pending_clipboard_read: Option<PendingClipboardRead> = None;
    let mut clipboard_retry_at: Option<Instant> = None;

    // The density the desktop is *known* to be at — known, because this only moves
    // when a resize proves it.
    //
    // Believing a write instead was a bug with two faces. The desktop stayed 1x
    // while this end declared 2x, so every later resize went out with a density the
    // server had thrown away, and — the one a person actually notices — a reattach
    // announced `scale` 2.0 for a 1x framebuffer, which a client presents at half
    // size. Nothing acknowledges a layout on this protocol, so the absence of a
    // resize is the only evidence there is, and it has to be the evidence used.
    let mut applied = Density::One;
    // Whether the remote has offered DisplayControl. Until it has, a layout has
    // nowhere to go — the engine crate would hold it, but holding it there loses
    // the retry ladder below, which is what a Windows host actually needs.
    let mut resize_ready = false;
    // The layout in flight — a size, a density, or both — and when to repeat it.
    // The client states each *once per attach* and then dedupes it (a viewport,
    // and `sendHostScale` in frontend/src/useRemoteDesktop.ts), so nothing will ask
    // again from the other end: every attempt after the first has to come from
    // here. One slot, not one per kind, so a size
    // and a density can only ever be pending as a single merged layout — see
    // [`PendingLayout`] for why one retry rather than two.
    let mut pending_layout: Option<PendingLayout> = None;
    let mut layout_retry_at: Option<Instant> = None;

    // Damage accumulated toward the next tile flush, and its deadline. A busy RDP
    // server reports damage far faster than anything presents it — ~126 batches a
    // second measured against a 60 Hz screen, back when the pointer was composited
    // into the framebuffer and even a still desktop produced one per mouse event.
    // Each used to take the pack-and-compare walk on arrival. Now a batch on a
    // quiet screen still goes out on the spot, and everything inside one interval
    // after it coalesces — overlapping reports collapse to one rectangle — and is
    // packed once at the deadline, against the newest framebuffer.
    let mut pending_damage: Vec<Rect> = Vec::new();
    let mut damage_flushed = Instant::now() - DAMAGE_INTERVAL;
    let mut damage_due: Option<Instant> = None;
    // Whether this server has ever marked a frame boundary (`Event::Frame`). Once it
    // has, the marker is the flush signal and the interval above demotes to a safety
    // net — see the flush scheduling at the bottom of the loop. A property of the
    // server, so it survives resizes and never unlearns.
    let mut frame_marks = false;

    loop {
        let clipboard_retry = async {
            match clipboard_retry_at {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        let layout_retry = async {
            match layout_retry_at {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        // Video only, and `None` unless the mirror is holding pixels no access unit
        // has carried — see `TileSink::due_at` for why a paced stream cannot rely on
        // the next event to come and collect them. A clean mirror parks on
        // the round-returned signal instead of forever: while a round is away being
        // encoded the live table is empty and `due_at` cannot see the damage that
        // lands meanwhile, so the round's return is what re-arms this.
        let video_flush = async {
            match sink.due_at().await {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => sink.round_returned().await,
            }
        };
        // Damage waiting out its accumulation interval — see `pending_damage` above.
        let damage_flush = async {
            match damage_due {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    // The forwarding thread ended without an `Ended` event, which
                    // means its own channel broke rather than the session closing.
                    anyhow::bail!("the RDP engine stopped reporting");
                };
                match event {
                    Event::Paint(rect) => {
                        // Staged rather than sent: whether this goes out now or at
                        // the deadline is decided once, at the end of the loop.
                        stage_damage(&mut pending_damage, damaged(rect));
                    }
                    // The server says this is where its frame ends, which is the fact
                    // the damage interval was built to guess at: everything staged
                    // since the last one of these is one coherent picture, so it goes
                    // out now — not in up-to-16ms, and never cut in half.
                    Event::Frame => {
                        frame_marks = true;
                        flush_damage(framebuffer, &mut pending_damage, &mut shadow, sink)
                            .await?;
                        damage_flushed = Instant::now();
                        damage_due = None;
                    }
                    Event::Cursor(cursor) => pointer.set(cursor),
                    Event::ResizeReady { max_monitors, max_area } => {
                        debug!("rdp: the remote offers dynamic resize, up to {max_monitors} monitors and {max_area} pixels");
                        resize_ready = true;
                        // Whatever was asked for while the channel was still
                        // opening, asked for again now rather than waiting out
                        // the rest of its schedule.
                        if pending_layout.is_some() {
                            layout_retry_at = Some(Instant::now());
                        }
                    }
                    // The server renegotiated the desktop — its answer to a
                    // monitor layout, and the only confirmation a layout ever
                    // gets. It also arrives unprompted when a server resizes a
                    // session on its own, which is why `applied` follows the
                    // pending layout rather than assuming there was one.
                    Event::Resize { width, height } => {
                        desktop = (narrow(width), narrow(height));
                        // A resize is the only acknowledgment this protocol has
                        // and it is an ambiguous one, since a server also
                        // renegotiates the desktop unprompted. So a pending
                        // layout counts as confirmed only when the size that
                        // came back is the size that went out — `adjusted`,
                        // because that is the one `request_layout` sends — and
                        // an unsolicited resize leaves it pending with its
                        // ladder still running. Taking it either way was the
                        // fault `applied` is documented against: the density of
                        // a layout the server had just thrown away became the
                        // `scale` announced for the desktop it built instead.
                        if let Some(pending) =
                            pending_layout.take_if(|p| confirms(p, desktop))
                        {
                            applied = pending.layout.density;
                            layout_retry_at = None;
                        }
                        info!("rdp: resized, desktop {}x{}", desktop.0, desktop.1);
                        // Damage staged before the resize names rectangles of a
                        // framebuffer that no longer exists.
                        pending_damage.clear();
                        damage_due = None;
                        shadow.resize(desktop.0, desktop.1);
                        // The cell grid is anchored at (0,0) in framebuffer pixels, so
                        // a new size makes every key name somewhere else.
                        sink.reset_render();
                        last_pos = (
                            last_pos.0.min(desktop.0.saturating_sub(1)),
                            last_pos.1.min(desktop.1.saturating_sub(1)),
                        );
                        sink.msg(ServerMsg::Resize {
                            w: desktop.0,
                            h: desktop.1,
                            scale: applied.scale(),
                        }).await?;
                        // The framebuffer was cleared by the resize and a server
                        // is not obliged to repaint. Asked for from here rather
                        // than by the engine crate, which must not send it from
                        // inside the callback — that fires part-way through the
                        // reactivation, before the connection can carry client
                        // PDUs. Going through the queue puts it after.
                        input.refresh();
                    }
                    Event::Clipboard(event) => {
                        handle_clipboard(
                            session.clipboard(),
                            event,
                            clipboard,
                            &mut local_clipboard,
                            &mut remote_clipboard,
                            &mut pending_clipboard_read,
                            &mut clipboard_retry_at,
                            sink,
                        ).await?;
                    }
                    Event::Ended(result) => {
                        info!("rdp: session ended: {result:?}");
                        // Best effort, and only for the pixels still pending: the
                        // session is over either way, and the shadow's claim about them
                        // dies with it.
                        for rect in pending_damage.drain(..) {
                            if send_tiles(framebuffer, rect, &mut shadow, sink).await.is_err() {
                                break;
                            }
                        }
                        let _ = sink.frame().await;
                        return result.map_err(|e| anyhow::anyhow!("{e}"));
                    }
                    // The desktop this session already reported, restated when a
                    // reactivation re-runs the connection sequence. Nothing to do:
                    // `Event::Resize` is what carries a size change.
                    Event::Connected { .. } => {}
                }
            }
            msg = input_rx.recv() => {
                let Some(msg) = msg else {
                    info!("rdp: input channel closed; session shut down");
                    break;
                };
                // A (re)attached browser needs the desktop size and a full
                // repaint from the server-owned framebuffer.
                if matches!(msg, ClientMsg::Refresh) {
                    // The repaint below covers every pixel, so damage waiting out
                    // its interval is subsumed by it.
                    pending_damage.clear();
                    damage_due = None;
                    damage_flushed = Instant::now();
                    // A repaint means the client has nothing, so the shadow must
                    // not claim otherwise. This covers detach, reattach and
                    // takeover in one place, because `Refresh` is injected on
                    // every attach — and it is what keeps the session layer's
                    // frame dropping while nobody is attached from turning into
                    // a permanently blank region.
                    shadow.forget();
                    // The repaint that follows re-sends every pixel at the base
                    // encode, which settles every debt and makes every cell's
                    // history a single redraw rather than motion.
                    sink.reset_render();
                    sink.msg(ServerMsg::Resize {
                        w: desktop.0,
                        h: desktop.1,
                        scale: applied.scale(),
                    }).await?;
                    sink.msg(ServerMsg::RemoteOs { macos: false }).await?;
                    // Not part of the repaint: the pixels carry no pointer, and
                    // the server only names a shape when it changes.
                    sink.msg(pointer.attached()).await?;
                    send_tiles(
                        framebuffer,
                        Rect {
                            left: 0,
                            top: 0,
                            right: desktop.0.saturating_sub(1),
                            bottom: desktop.1.saturating_sub(1),
                        },
                        &mut shadow,
                        sink,
                    )
                    .await?;
                    // A repaint is a frame. Without this, the whole repaint would
                    // sit in the video mirror unsent, while the shadow already
                    // counts every pixel of it as delivered.
                    sink.frame().await?;
                    continue;
                }
                // The density of the screen this client's window is on. Ignored
                // outright without `resize`, whose Display Control channel is the
                // only way to restate a density on a live session.
                //
                // Recorded and scheduled rather than asked for here, so the retry
                // branch is the single place a layout is requested from — one
                // attempt and five look the same to this arm.
                if let ClientMsg::HostScale { scale } = msg {
                    if resize {
                        // Only the density changes; the size the desktop should be
                        // stays what it is. Re-express whatever size is already
                        // pending at the new density, or the live desktop when
                        // nothing is — so a size still waiting for the channel is
                        // carried along by a screen change rather than dropped.
                        let want = Density::from_host(scale);
                        let base = pending_layout
                            .as_ref()
                            .map_or_else(|| Layout::current(desktop, applied), |p| p.layout);
                        install_layout(
                            base.at_density(want),
                            Layout::current(desktop, applied),
                            &mut pending_layout,
                            &mut layout_retry_at,
                        );
                    }
                    continue;
                }
                // A viewport report is a client-initiated resize, applied whenever
                // one arrives. How often that is — on every window change, or only
                // when the user asks — is the client's own setting and not
                // something this end tells apart. Ignored unless negotiated.
                //
                // The size arrives in remote *pixels*, already multiplied by the
                // `scale` this end announced, so it needs no conversion — but it
                // does need the current density attached to it, or the request
                // would tell the server to forget one it is already applying.
                //
                // `DefaultSize` is the same request with the size supplied from
                // here: the target's configured `width`/`height`, read as points,
                // which is what this session connected at while it was still 1x.
                // See [`ClientMsg::DefaultSize`].
                let wanted_size = match msg {
                    ClientMsg::Viewport { w, h } => Some((u32::from(w), u32::from(h))),
                    ClientMsg::DefaultSize => Some(applied.pixels(default_size)),
                    _ => None,
                };
                if let Some((w, h)) = wanted_size {
                    if resize {
                        // Scheduled, not sent-and-forgotten, for the same reason a
                        // density is: a size that arrives before the Display Control
                        // channel is up cannot go out, and a client
                        // states its viewport once and dedupes it, so nothing would
                        // re-send it. This is the start of every session with
                        // auto-resize on by default — both reports come from
                        // `connected`, before the channel is up — so without the
                        // retry the desktop stayed at its connect size until a manual
                        // resize landed. The size arrives in the announced density's
                        // pixels; if a denser layout is already pending it is carried
                        // up to that density, so the two go out as one layout and
                        // never as two resizes racing to set `applied`.
                        let density = pending_layout.as_ref().map_or(applied, |p| p.layout.density);
                        install_layout(
                            Layout { w, h, density: applied }.at_density(density),
                            Layout::current(desktop, applied),
                            &mut pending_layout,
                            &mut layout_retry_at,
                        );
                    }
                    continue;
                }
                // The clipboard pair, intercepted here for the same reason as
                // the two above: they act on a virtual channel rather than
                // translating to input. Both are no-ops when the
                // target did not opt in — the browser hides the control then,
                // so this is the belt to that UI's braces.
                if let ClientMsg::Clipboard { text } = &msg {
                    if clipboard {
                        // We are taking ownership of the remote clipboard, so
                        // an older remote Copy/Cut can no longer be fetched.
                        pending_clipboard_read = None;
                        clipboard_retry_at = None;
                        // Only advertised, not sent. The remote asks for the
                        // bytes if and when someone pastes.
                        match rdp_clipboard::to_remote(text) {
                            Some(text) => {
                                debug!("rdp: advertising {} bytes to the remote clipboard", text.len());
                                local_clipboard = Some(text);
                                advertise_clipboard(session.clipboard(), local_clipboard.as_deref());
                            }
                            // Refused, so the remote keeps whatever it had:
                            // advertising a partial copy would hand out a paste
                            // that looks complete. The client refuses this and
                            // says why before it reaches the gateway.
                            None => warn!(
                                "rdp: refusing {} bytes to the remote clipboard, over the {} byte limit",
                                text.len(),
                                crate::protocol::MAX_CLIPBOARD_BYTES
                            ),
                        }
                    }
                    continue;
                }
                if matches!(msg, ClientMsg::ClipboardRequest) {
                    // Answered from the buffer the channel fills. Empty until
                    // the remote copies something, which reads in the panel as
                    // "nothing has been copied over there yet".
                    if clipboard {
                        let snapshot = remote_clipboard
                            .clone()
                            .unwrap_or_else(ClipboardSnapshot::unobserved);
                        sink.msg(ServerMsg::Clipboard {
                            text: snapshot.text,
                            changed_at_ms: snapshot.changed_at_ms,
                            requested: true,
                            oversized_bytes: snapshot.oversized_bytes,
                        }).await?;
                    }
                    continue;
                }
                for event in translate_input(msg, &mut last_pos) {
                    event.apply(input);
                }
                continue;
            }
            _ = clipboard_retry => {
                clipboard_retry_at = None;
                if let (Some(read), Some(cb)) = (pending_clipboard_read.as_ref(), session.clipboard()) {
                    cb.request(read.format);
                }
                continue;
            }
            _ = layout_retry => {
                layout_retry_at = None;
                let Some(pending) = pending_layout.as_mut() else {
                    continue; // a resize confirmed it first
                };
                let wanted = pending.layout;
                match request_layout(input, resize_ready, Layout::current(desktop, applied), wanted) {
                    // Sent, but nothing acknowledges a layout — so ask again
                    // until a resize proves it or the schedule runs out.
                    // Dropping the request is all that giving up takes: `applied`
                    // was never advanced, so the announced scale still describes
                    // the desktop that is actually there.
                    Asked::Sent => match pending.wait_again() {
                        Some(delay) => layout_retry_at = Some(Instant::now() + delay),
                        None => {
                            warn!(
                                "rdp: the remote never applied {}; leaving the desktop at {}",
                                wanted,
                                Layout::current(desktop, applied),
                            );
                            pending_layout = None;
                        }
                    },
                    // Not an attempt. The channel is not open, so no rung of the
                    // ladder could have worked, and spending one costs the
                    // request the attempts it needs once the channel *is* there
                    // — which is the whole failure `install_layout` describes,
                    // since both of a session's opening reports arrive before
                    // DisplayControl comes up. Nothing is re-armed here either:
                    // `Event::ResizeReady` is the event being waited for and it
                    // re-arms any layout still pending, so a server that never
                    // offers the channel is waited on rather than polled.
                    Asked::NotReady => {}
                    // Nothing more to try: the desktop already agrees.
                    Asked::Redundant => pending_layout = None,
                }
                continue;
            }
            _ = video_flush => {
                // The interval elapsed with pixels still sitting in the mirror. On the
                // same task as every other frame boundary, so the stream stays serial
                // by construction rather than by the lock.
                sink.frame().await?;
                continue;
            }
            _ = damage_flush => {
                flush_damage(framebuffer, &mut pending_damage, &mut shadow, sink).await?;
                damage_flushed = Instant::now();
                damage_due = None;
                // The flush is a frame boundary of its own: under a video plan those
                // blits just landed in the mirror, and nothing else may come to
                // collect them.
                sink.frame().await?;
                continue;
            }
        }

        if let Some(msg) = pointer.change() {
            sink.msg(msg).await?;
        }
        // Two flush regimes, chosen by whether the server marks its frames.
        //
        // Marking server: the marker is the flush signal, and the only deadline is a
        // safety net hung well past any real frame — never the leading-edge flush,
        // whose whole point was to guess that a lone report *was* the frame. Guessing
        // on top of a server that says would reintroduce the cut-in-half frames the
        // marker exists to end.
        //
        // Otherwise, the original guess: a quiet screen's damage leaves on the spot —
        // the first batch after an idle interval pays no added latency — and
        // everything after it within one interval waits for the deadline, coalesced.
        // Leading edge, trailing edge: the same shape the client's own motion
        // coalescing has.
        if !pending_damage.is_empty() {
            if frame_marks {
                if damage_due.is_none() {
                    damage_due = Some(Instant::now() + FRAME_NET);
                }
            } else if damage_flushed.elapsed() >= DAMAGE_INTERVAL {
                flush_damage(framebuffer, &mut pending_damage, &mut shadow, sink).await?;
                damage_flushed = Instant::now();
                damage_due = None;
            } else if damage_due.is_none() {
                damage_due = Some(damage_flushed + DAMAGE_INTERVAL);
            }
        }
        // The closest thing this engine has to the end of a frame: one event is
        // everything one PDU produced, and a video stream needs to be told when to
        // stop accumulating and encode. Most turns of this loop redraw nothing, which
        // is why this is a no-op when nothing was blitted rather than a frame per PDU.
        sink.frame().await?;
    }

    shadow.report();
    Ok(())
}

/// Act on one thing the remote clipboard did.
///
/// Its own function rather than an arm of the loop because there are six
/// outcomes and the loop is long enough; the state it moves is all passed in, so
/// nothing here is reachable from anywhere else.
#[allow(clippy::too_many_arguments)]
async fn handle_clipboard(
    clipboard: Option<&Clipboard>,
    event: ClipboardEvent,
    enabled: bool,
    local: &mut Option<String>,
    remote: &mut Option<ClipboardSnapshot>,
    pending: &mut Option<PendingClipboardRead>,
    retry_at: &mut Option<Instant>,
    sink: &TileSink,
) -> anyhow::Result<()> {
    if !enabled {
        return Ok(());
    }
    match event {
        // The capability exchange finished. Advertising even an empty clipboard
        // is what tells the remote there is a client on this end at all.
        ClipboardEvent::Ready => advertise_clipboard(clipboard, local.as_deref()),
        // Ask straight away rather than waiting for the panel's Fetch, so a copy
        // on the remote reaches the browser unprompted exactly as it does for VNC.
        ClipboardEvent::RemoteFormats(formats) => {
            match rdp_clipboard::pick_text_format(&formats) {
                Some(format) => {
                    *pending = Some(PendingClipboardRead::new(format));
                    *retry_at = None;
                    if let Some(cb) = clipboard {
                        cb.request(format);
                    }
                }
                None => {
                    *pending = None;
                    *retry_at = None;
                    debug!("rdp: the remote copied no text format we can carry");
                    *remote = Some(ClipboardSnapshot::changed(String::new(), remote.as_ref()));
                }
            }
        }
        ClipboardEvent::RemoteData { data, .. } => {
            *pending = None;
            *retry_at = None;
            // Invalid bytes cannot become valid by repeating the same request, so
            // a malformed payload keeps the last good clipboard value and
            // schedules nothing.
            let Some(text) = rdp_clipboard::decode_unicode(&data) else {
                warn!("rdp: undecodable clipboard text from the remote, {} bytes", data.len());
                return Ok(());
            };
            let snapshot = match rdp_clipboard::from_remote(&text) {
                Ok(text) => {
                    debug!("rdp: remote clipboard updated, {} bytes", text.len());
                    ClipboardSnapshot::changed(text, remote.as_ref())
                }
                // Reported as its size instead of the first 64 KiB
                // of it: the panel can say what happened, where a
                // truncated paste could not be told from a whole one.
                Err(bytes) => {
                    debug!(
                        "rdp: remote clipboard is {bytes} bytes, over the {} byte limit",
                        crate::protocol::MAX_CLIPBOARD_BYTES
                    );
                    ClipboardSnapshot::oversized(bytes, remote.as_ref())
                }
            };
            *remote = Some(snapshot.clone());
            sink.msg(ServerMsg::Clipboard {
                text: snapshot.text,
                changed_at_ms: snapshot.changed_at_ms,
                requested: false,
                oversized_bytes: snapshot.oversized_bytes,
            })
            .await?;
        }
        // Nothing to show, and deliberately not forwarded as empty
        // text: that would wipe the panel over a transient refusal.
        // MS-RDPECLIP's CB_RESPONSE_FAIL does not identify why the
        // peer could not process the request. A live Windows peer
        // recovered when the same advertised format was retried.
        ClipboardEvent::RemoteDataFailed { .. } => {
            if let Some(read) = pending.as_mut() {
                match read.retry_after_failure() {
                    Some(delay) => {
                        debug!(
                            "rdp: retrying refused remote clipboard read in {}ms",
                            delay.as_millis()
                        );
                        *retry_at = Some(Instant::now() + delay);
                    }
                    None => {
                        debug!("rdp: remote clipboard read exhausted its retries");
                        *pending = None;
                        *retry_at = None;
                    }
                }
            }
        }
        // The remote is pasting and **is waiting**. Every one of these must be
        // answered, including with `None` — a request left unanswered is a remote
        // application blocked in its paste handler, which on Windows is a frozen
        // window rather than an error.
        ClipboardEvent::LocalDataRequest { format } => {
            let Some(cb) = clipboard else { return Ok(()) };
            let data = match local.as_deref() {
                Some(text) if format == CF_UNICODETEXT => {
                    debug!("rdp: handing {} bytes to the remote's paste", text.len());
                    Some(rdp_clipboard::encode_unicode(text))
                }
                Some(_) => {
                    warn!("rdp: the remote asked for clipboard format {format}, which we never offered");
                    None
                }
                None => None,
            };
            cb.respond(format, data);
        }
    }
    Ok(())
}

/// Tell the remote what our clipboard now holds (MS-RDPECLIP Format List).
///
/// `text` of `None` advertises nothing, which is the honest answer before the
/// browser has sent anything and is still worth sending.
fn advertise_clipboard(clipboard: Option<&Clipboard>, text: Option<&str>) {
    let Some(clipboard) = clipboard else { return }; // the target did not opt in
    let formats = match text {
        Some(_) => vec![ClipboardFormat::new(CF_UNICODETEXT)],
        None => Vec::new(),
    };
    clipboard.advertise(formats);
}

/// Install `wanted` as the layout to ask the server for, or clear the schedule
/// when there is nothing to ask.
///
/// The first attempt belongs in the retry branch with the rest, so this only
/// records the want and dates the deadline now; it never asks. Two cases need no
/// schedule: the desktop is already `current` (so a screen change that moved back
/// before the remote caught up, or a viewport equal to the desktop, asks for
/// nothing), and the identical layout is already pending (so a deduped-but-repeated
/// report does not reset the attempt count). The comparison is against the adjusted
/// `wanted`, matching what [`request_layout`] will send.
fn install_layout(
    wanted: Layout,
    current: Layout,
    pending: &mut Option<PendingLayout>,
    retry_at: &mut Option<Instant>,
) {
    if wanted.adjusted() == current {
        *pending = None;
        *retry_at = None;
    } else if pending.as_ref().map(|p| p.layout) != Some(wanted) {
        *pending = Some(PendingLayout::new(wanted));
        *retry_at = Some(Instant::now());
    }
}

/// Ask the server for a different desktop — a size, a density, or both. The
/// server answers by renegotiating the session, which arrives as
/// [`Event::Resize`].
///
/// Returns which of the three [`Asked`] outcomes happened, rather than whether
/// anything went out, because the retry branch has to know which layouts are
/// worth another attempt. Note that even [`Asked::Sent`] is not a confirmation:
/// this protocol acknowledges nothing, so a layout the server silently discards
/// looks from here exactly like one it is about to act on.
///
/// The first thing checked is whether the remote has offered DisplayControl at
/// all. The engine crate would hold a layout sent before that and send it when
/// the channel opens — which is *worse* than not sending it, because the moment
/// the channel opens is precisely when a Windows host ignores layouts, and the
/// held request would be spent on the one attempt least likely to work.
///
/// Also a no-op when the desktop is already that layout, and that guard earns its
/// place here rather than at the callers: this is the one engine where asking for
/// what you already have is *expensive*, since the server answers any request with
/// a full renegotiation. VNC drops an unchanged request itself, so
/// this is what makes the client requests idempotent across both engines —
/// which matters most for the automatic [`ClientMsg::DefaultSize`] a
/// mobile client sends on every reattach.
///
/// Compared after the size adjustment, because that is the layout that would
/// actually be asked for: an odd width lands on the even one beside it, and
/// comparing before the adjustment would call that a change when it is not. The
/// density is part of that comparison, so a request that only changes the density
/// — which is what a client dragged between two screens of the same size sends —
/// is not mistaken for a repeat.
/// Whether a desktop the server has just reported is the answer to `pending`.
///
/// A pure function beside [`install_layout`] and [`request_layout`] for the same
/// reason they are: this is where the errors live, and it can be asserted without
/// a session.
///
/// Against `adjusted`, because that is the layout [`request_layout`] actually
/// sends — comparing against the unadjusted one would refuse to recognise the
/// server's answer to an odd viewport width, and the retry ladder would then run
/// to exhaustion against a desktop that was already right.
///
/// The density is not compared and cannot be: MS-RDPEDISP carries the scale
/// factor on the same PDU as the size but the server acknowledges neither, and no
/// PDU reports the scale a server settled on. The size is the only evidence there
/// is, which is why it has to be the evidence used.
fn confirms(pending: &PendingLayout, desktop: (u16, u16)) -> bool {
    let want = pending.layout.adjusted();
    (want.w, want.h) == (u32::from(desktop.0), u32::from(desktop.1))
}

fn request_layout(input: &Input, ready: bool, current: Layout, wanted: Layout) -> Asked {
    let wanted = wanted.adjusted();
    if !ready {
        debug!("rdp: {wanted} requested before the remote offered dynamic resize");
        return Asked::NotReady;
    }
    if wanted == current {
        debug!("rdp: the desktop is already {wanted}; not asking for a resize");
        return Asked::Redundant;
    }
    info!("rdp: requesting {wanted}");
    input.resize(wanted.w, wanted.h, wanted.density.percent());
    Asked::Sent
}

/// A `u32` from the engine crate as the `u16` everything downstream of here is.
///
/// Saturating rather than `as`, which would wrap: a 70000-pixel desktop is a server
/// saying something this gateway cannot represent, and 4464 pixels is a worse answer
/// to that than 65535. Nothing real reaches the ceiling — RDP's own limit is 8192 a
/// side — so this is about what happens when something is *not* real.
fn narrow(v: u32) -> u16 {
    v.min(u32::from(u16::MAX)) as u16
}

/// One damage rectangle, in the inclusive-edge form the tile path uses.
///
/// The two disagree on purpose and the conversion is the only place that knows: the
/// engine crate reports position-and-size, and [`Rect`] is inclusive on all four
/// edges because that is how RFB reports one and both engines share the tile path.
/// Saturating throughout, so an oversized rectangle becomes a clamped one rather
/// than an overflow — `send_tiles` intersects it with the framebuffer anyway.
fn damaged(rect: freerdp::Rect) -> Rect {
    Rect {
        left: narrow(rect.x),
        top: narrow(rect.y),
        right: narrow(rect.x.saturating_add(rect.width)).saturating_sub(1),
        bottom: narrow(rect.y.saturating_add(rect.height)).saturating_sub(1),
    }
}

/// One thing to do to the remote, translated out of a browser message.
///
/// An enum rather than a direct call on [`Input`], so [`translate_input`] stays a
/// pure function of its arguments and the mapping — which is where the errors in
/// an input path live — can be asserted without a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteInput {
    Move { x: u16, y: u16 },
    Button { button: RdpButton, down: bool, x: u16, y: u16 },
    Wheel { delta: i16, horizontal: bool, x: u16, y: u16 },
    Key { scancode: u8, extended: bool, down: bool },
}

impl RemoteInput {
    fn apply(self, input: &Input) {
        match self {
            Self::Move { x, y } => input.mouse_move(x, y),
            Self::Button { button, down, x, y } => input.mouse_button(button, down, x, y),
            Self::Wheel { delta, horizontal, x, y } => input.wheel(delta, horizontal, x, y),
            Self::Key { scancode, extended, down } => input.key(scancode, extended, down),
        }
    }
}

/// One notch of a conventional wheel, in the rotation units RDP counts.
const WHEEL_NOTCH: i16 = 120;

/// Translate one browser input message into what to do to the remote.
fn translate_input(input: ClientMsg, last_pos: &mut (u16, u16)) -> Vec<RemoteInput> {
    match input {
        ClientMsg::MouseMove { x, y } => {
            let (x, y) = (clamp_u16(x), clamp_u16(y));
            *last_pos = (x, y);
            vec![RemoteInput::Move { x, y }]
        }
        // `clicks` goes nowhere: RDP carries button state alone, and Windows
        // counts the clicks itself from the events it receives.
        ClientMsg::MouseButton { button, pressed, .. } => {
            let button = match button {
                MouseButton::Left => RdpButton::Left,
                MouseButton::Right => RdpButton::Right,
                MouseButton::Middle => RdpButton::Middle,
                // The side buttons travel in RDP's *extended* pointer PDU, which
                // the engine crate sends for these two. They used to be dropped,
                // because the fast-path event the old engine built could not
                // carry them.
                MouseButton::Back => RdpButton::X1,
                MouseButton::Forward => RdpButton::X2,
            };
            vec![RemoteInput::Button {
                button,
                down: pressed,
                x: last_pos.0,
                y: last_pos.1,
            }]
        }
        // The unit is dropped: RDP spends a notch as 120 rotation units whatever
        // the delta was measured in, and the guest applies its own scrolling.
        ClientMsg::Wheel { dx, dy, .. } => {
            let mut events = Vec::new();
            // RDP: positive rotation is up/forward. The DOM deltaY is positive
            // when scrolling down, so invert it.
            if dy != 0.0 {
                events.push(RemoteInput::Wheel {
                    delta: if dy > 0.0 { -WHEEL_NOTCH } else { WHEEL_NOTCH },
                    horizontal: false,
                    x: last_pos.0,
                    y: last_pos.1,
                });
            }
            if dx != 0.0 {
                events.push(RemoteInput::Wheel {
                    delta: if dx > 0.0 { WHEEL_NOTCH } else { -WHEEL_NOTCH },
                    horizontal: true,
                    x: last_pos.0,
                    y: last_pos.1,
                });
            }
            events
        }
        // `caps` is VNC-only: the RDP host tracks its own CapsLock from the
        // forwarded scancode.
        ClientMsg::Key { code, pressed, .. } => match keymap::scancode(&code) {
            Some((scancode, extended)) => {
                vec![RemoteInput::Key { scancode, extended, down: pressed }]
            }
            None => {
                debug!("rdp: unmapped key code {code}");
                Vec::new()
            }
        },
        // Handled by the active loop (client-initiated resize, and the density
        // that is a resize here) before translation, so these arms are
        // unreachable in practice.
        ClientMsg::Viewport { .. } | ClientMsg::DefaultSize | ClientMsg::HostScale { .. } => {
            Vec::new()
        }
        // Handled by the active loop (full repaint) before translation.
        ClientMsg::Refresh => Vec::new(),
        // Handled by the active loop (MS-RDPECLIP, a static virtual channel)
        // before translation.
        ClientMsg::Clipboard { .. } | ClientMsg::ClipboardRequest => Vec::new(),
        // Session-control messages act on the slot, not an engine — the ws
        // bridge handles them and they never reach here. `CacheReset` is one of
        // them: it empties that socket's tile cache and injects its own `Refresh`.
        ClientMsg::Connect { .. }
        | ClientMsg::Disconnect
        | ClientMsg::CacheReset
        | ClientMsg::PaintAck { .. } => Vec::new(),
        // An RDP session is one framebuffer spanning every monitor the server
        // composed into it, and its protocol has no way to ask for one of them.
        // So this engine never sends a display list, no client offers the
        // picker, and anything arriving here is a client that invented one.
        ClientMsg::SelectDisplay { .. } => Vec::new(),
    }
}

/// The shortest gap between two tile flushes — the still path's counterpart to
/// `VIDEO_FRAME_INTERVAL`, at the 60 Hz a screen actually presents rather than the
/// stream's 30.
///
/// The interval coalesces, it does not merely defer: a busy server's ~126 damage
/// batches a second overlap heavily and [`stage_damage`] folds an overlapping
/// report into the one
/// already waiting, so the deadline packs and compares each region once instead of
/// per report.
const DAMAGE_INTERVAL: Duration = Duration::from_millis(16);

/// The safety net under a frame-marking server: how long staged damage may wait for
/// the marker that normally flushes it. Anchored to when the damage was staged, not
/// to the last flush — a frame beginning after a long idle must not fire it on
/// arrival. The value is guacamole-server's render-thread fallback for the same
/// signal going missing; it is a net, so it should never decide latency on a healthy
/// session, only bound the damage an unmarked tail can strand.
const FRAME_NET: Duration = Duration::from_millis(100);

/// Most rectangles the pending-damage list holds before collapsing to one bounding
/// box. Slop from a collapse costs a pack and a `memcmp` on pixels that did not
/// change — exactly what the shadow exists to absorb — never wire bytes.
const DAMAGE_RECTS_CAP: usize = 32;

/// Fold `rect` into the damage accumulated toward the next flush.
///
/// A report overlapping one already staged is unioned into it — the common case,
/// since a busy server re-reports the same regions many times per interval — and a
/// disjoint one is kept apart, so a caret and a video at opposite corners do not
/// conspire to repack the whole desktop.
fn stage_damage(pending: &mut Vec<Rect>, rect: Rect) {
    let union = |a: &Rect, b: &Rect| Rect {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    };
    for staged in pending.iter_mut() {
        if staged.intersect(&rect).is_some() {
            *staged = union(staged, &rect);
            return;
        }
    }
    if pending.len() >= DAMAGE_RECTS_CAP {
        let whole = pending.drain(..).fold(rect, |acc, r| union(&acc, &r));
        pending.push(whole);
    } else {
        pending.push(rect);
    }
}

/// Send whatever part of `rect` the client does not already have, as tiles of at
/// most [`crate::protocol::CELL_H`] rows each. How that region is cut, and what
/// each piece is encoded as, is [`TileSink::damage`]'s business.
///
/// Comparing against `shadow` earns its keep on this engine in particular: it
/// repaints regions that did not change, which nothing upstream filters. They come
/// back as `None` here and cost nothing but a pack and a `memcmp`.
///
/// The framebuffer lock is held for the pack and released before the await, which
/// is what keeps a slow encoder from stalling FreeRDP's next paint: the engine
/// crate hands out its frame under a mutex the RDP thread also takes on every
/// `EndPaint`.
/// Drain the staged damage: copies first, tiles for the rest.
///
/// Under a plan that takes copies, the flush's damage is searched for regions the
/// client already holds elsewhere on its canvas — a scroll, mostly — and each find
/// goes out as a `COPY` record instead of image bytes ([`crate::copies`]). The
/// shadow applies every copy exactly as the client will, so the tile pass that
/// follows sees the copied pixels as delivered and pays nothing for them; whatever
/// a copy did not carry — the newly revealed strip of a scroll — travels as tiles
/// like any other damage. The copy records go through `msg`, as VNC's CopyRect
/// does, because that queue's order against the tiles is the contract a copy reads
/// the canvas under.
async fn flush_damage(
    framebuffer: &Framebuffer,
    pending: &mut Vec<Rect>,
    shadow: &mut Shadow,
    sink: &TileSink,
) -> anyhow::Result<()> {
    if sink.copies() && !pending.is_empty() {
        let plans = framebuffer.with(|frame| {
            copies::plan(
                &frame.pixels,
                frame.stride,
                narrow(frame.width),
                narrow(frame.height),
                pending,
                shadow,
            )
        });
        for copy in plans {
            // `Some(true)` is the only answer that owes the client a record: `None`
            // means the shadow cannot make the move (drop it — the tiles carry those
            // pixels instead), `Some(false)` that the destination already held them.
            if shadow.copy_within(copy.src, copy.dst) == Some(true) {
                sink.msg(ServerMsg::Copy(CopyRect {
                    sx: copy.src.left,
                    sy: copy.src.top,
                    x: copy.dst.left,
                    y: copy.dst.top,
                    w: copy.dst.w(),
                    h: copy.dst.h(),
                }))
                .await?;
            }
        }
    }
    for rect in pending.drain(..) {
        send_tiles(framebuffer, rect, shadow, sink).await?;
    }
    Ok(())
}

async fn send_tiles(
    framebuffer: &Framebuffer,
    rect: Rect,
    shadow: &mut Shadow,
    sink: &TileSink,
) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    let Some(rect) = framebuffer.with(|frame| {
        // Clamped to *both* sizes, because they can disagree for one turn of the
        // loop: the RDP thread resizes the framebuffer from inside its own
        // callback, and the shadow follows when `Event::Resize` is processed.
        let (fb_w, fb_h) = shadow.size();
        let w = narrow(frame.width).min(fb_w);
        let h = narrow(frame.height).min(fb_h);
        if rect.left >= w || rect.top >= h {
            return None;
        }
        let rect = Rect {
            left: rect.left,
            top: rect.top,
            right: rect.right.min(w - 1),
            bottom: rect.bottom.min(h - 1),
        };
        if rect.right < rect.left || rect.bottom < rect.top {
            return None;
        }
        pack_rgb(frame, rect, &mut buf);
        Some(rect)
    }) else {
        return Ok(());
    };

    let Some(changed) = shadow.accept(rect, &buf) else {
        return Ok(());
    };

    // Its own buffer per piece, not the one above: the encoder reads those pixels
    // after this call has returned, and the framebuffer is overwritten by the next
    // paint. Cropped out of the pack already made rather than repacked from the
    // frame — a piece's rows are not contiguous in `buf`, but they are row copies,
    // where a repack is another per-pixel swizzle over the same pixels.
    sink.damage(&changed, |piece| {
        let mut pixels = Vec::new();
        tiles::crop(&buf, rect, piece, &mut pixels);
        pixels
    })
    .await
}

/// Pack `rect` out of the framebuffer into `buf` as RGB888.
///
/// The frame is `RGBX32` — the engine crate's own choice, made at `gdi_init` time
/// precisely so that a consumer which encodes finds R,G,B in memory order — so
/// this drops every fourth byte and copies the rest. The fourth byte is not alpha
/// and carries nothing.
///
/// Through [`Frame::rows`] rather than stride arithmetic here: getting a stride
/// wrong by hand produces a picture that is *nearly* right, sheared by a few
/// pixels a row, which is a bug people stare at for an hour.
fn pack_rgb(frame: &Frame, rect: Rect, buf: &mut Vec<u8>) {
    let w = usize::from(rect.w());
    let h = usize::from(rect.h());
    buf.clear();
    buf.resize(w * h * 3, 0);
    let rows = frame.rows(freerdp::Rect {
        x: u32::from(rect.left),
        y: u32::from(rect.top),
        width: rect.w().into(),
        height: rect.h().into(),
    });
    for (dst, src) in buf.chunks_exact_mut(w * 3).zip(rows) {
        // The literal strides are what let the compiler vectorize the 4-in/3-out
        // shuffle; sized writes rather than per-pixel `extend` for the same reason.
        for (out, px) in dst.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
            out.copy_from_slice(&px[..3]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WheelUnit;

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> Rect {
        Rect { left, top, right, bottom }
    }

    /// A cursor image of `w`x`h` opaque red, as the engine crate hands one over.
    fn cursor(w: u32, h: u32) -> freerdp::Cursor {
        freerdp::Cursor::Image(freerdp::CursorImage {
            width: w,
            height: h,
            hotspot_x: 1,
            hotspot_y: 2,
            rgba: [255, 0, 0, 255].repeat((w * h) as usize),
        })
    }

    fn shape(msg: &ServerMsg) -> Option<&CursorShape> {
        match msg {
            ServerMsg::Cursor(shape) => shape.as_ref(),
            other => panic!("expected a cursor message, got {other:?}"),
        }
    }

    /// The whole point of forwarding the shape: it reaches the browser, hotspot
    /// and all, and does so once.
    #[test]
    fn a_shape_travels_once_and_carries_its_hotspot() {
        let mut pointer = Pointer::default();
        pointer.set(cursor(32, 32));
        let msg = pointer.change().expect("the first shape is a change");
        let shape = shape(&msg).expect("a shape, not the client's own arrow");
        assert_eq!((shape.w, shape.h, shape.hx, shape.hy), (32, 32, 1, 2));
        assert!(pointer.change().is_none(), "nothing changed since");
    }

    /// A pointer the server re-selects out of its own cache arrives again with
    /// identical pixels, and re-sending them every time the mouse crosses a
    /// window edge is exactly the traffic this design exists to remove.
    #[test]
    fn reselecting_the_same_pointer_says_nothing() {
        let mut pointer = Pointer::default();
        pointer.set(cursor(32, 32));
        assert!(pointer.change().is_some());
        pointer.set(cursor(32, 32));
        assert!(pointer.change().is_none());
        // A different shape is a real change.
        pointer.set(cursor(48, 48));
        assert!(pointer.change().is_some());
    }

    /// Selecting a cached pointer produces a hide *and* a shape in one batch. The
    /// browser should see the shape, not flicker through its own arrow on the way
    /// to it.
    #[test]
    fn a_hide_followed_by_a_shape_is_one_message() {
        let mut pointer = Pointer::default();
        pointer.set(cursor(32, 32));
        assert!(pointer.change().is_some());
        pointer.set(freerdp::Cursor::Hidden);
        pointer.set(cursor(48, 48));
        let msg = pointer.change().expect("the batch ended on a new shape");
        let shape = shape(&msg).expect("the shape, not the hide before it");
        assert_eq!((shape.w, shape.h), (48, 48));
    }

    #[test]
    fn hiding_an_already_hidden_pointer_says_nothing() {
        let mut pointer = Pointer::default();
        pointer.set(freerdp::Cursor::Hidden);
        assert!(pointer.change().is_none(), "it was already the client's arrow");
        pointer.set(cursor(16, 16));
        assert!(pointer.change().is_some());
        pointer.set(freerdp::Cursor::Default);
        let msg = pointer.change().expect("back to the client's own arrow");
        assert!(shape(&msg).is_none());
    }

    /// A pointer too large to draw becomes the client's own rather than being
    /// forwarded — and the browser is *told*, so it stops drawing the last one.
    #[test]
    fn a_pointer_too_large_to_draw_becomes_the_clients_own() {
        let mut pointer = Pointer::default();
        pointer.set(cursor(16, 16));
        assert!(pointer.change().is_some());
        pointer.set(cursor(u32::from(MAX_CURSOR_DIM) + 1, 16));
        let msg = pointer.change().expect("the oversized shape is still a change");
        assert!(shape(&msg).is_none(), "and it is the client's own arrow");
    }

    #[test]
    fn an_attaching_browser_is_told_the_pointer_either_way() {
        let mut pointer = Pointer::default();
        // Nothing has arrived yet, and the browser is still told it owns the
        // pointer.
        assert!(shape(&pointer.attached()).is_none());
        pointer.set(cursor(24, 24));
        // `attached` also settles the pending change, so the shape is not sent
        // twice.
        assert!(shape(&pointer.attached()).is_some());
        assert!(pointer.change().is_none());
    }

    /// The edge convention flips here, and getting it wrong is a one-pixel seam
    /// down the right and bottom of every tile — visible, and easy to stare past.
    #[test]
    fn a_damage_rectangle_becomes_inclusive_on_every_edge() {
        let r = damaged(freerdp::Rect { x: 10, y: 20, width: 4, height: 2 });
        assert_eq!(r, rect(10, 20, 13, 21));
        // The whole of a 1280x800 desktop, which is the repaint case.
        assert_eq!(
            damaged(freerdp::Rect { x: 0, y: 0, width: 1280, height: 800 }),
            rect(0, 0, 1279, 799)
        );
        // One pixel is one pixel, not zero and not two.
        assert_eq!(damaged(freerdp::Rect { x: 5, y: 5, width: 1, height: 1 }), rect(5, 5, 5, 5));
    }

    /// Nothing real reaches these, which is exactly why they are worth pinning: an
    /// `as u16` here would wrap a 70000-pixel desktop to 4464 and paint a plausible
    /// picture of the wrong size.
    #[test]
    fn an_impossible_size_saturates_rather_than_wrapping() {
        assert_eq!(narrow(0), 0);
        assert_eq!(narrow(1280), 1280);
        assert_eq!(narrow(65_535), u16::MAX);
        assert_eq!(narrow(70_000), u16::MAX);
        assert_eq!(narrow(u32::MAX), u16::MAX);
        // And a rectangle whose corner would overflow the addition.
        let r = damaged(freerdp::Rect { x: u32::MAX, y: 0, width: u32::MAX, height: 1 });
        assert_eq!((r.left, r.right), (u16::MAX, u16::MAX - 1));
    }

    #[test]
    fn overlapping_damage_folds_into_one_rectangle() {
        let mut pending = Vec::new();
        stage_damage(&mut pending, rect(0, 0, 10, 10));
        stage_damage(&mut pending, rect(5, 5, 20, 20));
        assert_eq!(pending, vec![rect(0, 0, 20, 20)]);
    }

    #[test]
    fn disjoint_damage_stays_apart() {
        let mut pending = Vec::new();
        stage_damage(&mut pending, rect(0, 0, 10, 10));
        stage_damage(&mut pending, rect(100, 100, 110, 110));
        assert_eq!(pending, vec![rect(0, 0, 10, 10), rect(100, 100, 110, 110)]);
    }

    #[test]
    fn past_the_cap_the_list_collapses_to_a_bounding_box() {
        let mut pending = Vec::new();
        for i in 0..DAMAGE_RECTS_CAP as u16 {
            stage_damage(&mut pending, rect(i * 20, 0, i * 20 + 5, 5));
        }
        assert_eq!(pending.len(), DAMAGE_RECTS_CAP);
        stage_damage(&mut pending, rect(0, 100, 5, 105));
        assert_eq!(pending.len(), 1, "the cap collapses the list");
        assert_eq!(pending[0].top, 0);
        assert_eq!(pending[0].bottom, 105);
    }

    #[test]
    fn mouse_move_sets_flags_and_updates_last_pos() {
        let mut last = (0, 0);
        let events = translate_input(ClientMsg::MouseMove { x: 100, y: 200 }, &mut last);
        assert_eq!(events, vec![RemoteInput::Move { x: 100, y: 200 }]);
        assert_eq!(last, (100, 200));
    }

    #[test]
    fn negative_and_huge_coords_are_clamped() {
        let mut last = (0, 0);
        let events = translate_input(ClientMsg::MouseMove { x: -5, y: 70000 }, &mut last);
        assert_eq!(events, vec![RemoteInput::Move { x: 0, y: u16::MAX }]);
        assert_eq!(last, (0, u16::MAX));
    }

    #[test]
    fn button_press_uses_last_pos_and_down_flag() {
        let mut last = (7, 9);
        let events = translate_input(
            ClientMsg::MouseButton { button: MouseButton::Right, pressed: true, clicks: 1 },
            &mut last,
        );
        assert_eq!(
            events,
            vec![RemoteInput::Button { button: RdpButton::Right, down: true, x: 7, y: 9 }]
        );

        let events = translate_input(
            ClientMsg::MouseButton { button: MouseButton::Right, pressed: false, clicks: 1 },
            &mut last,
        );
        assert_eq!(
            events,
            vec![RemoteInput::Button { button: RdpButton::Right, down: false, x: 7, y: 9 }]
        );
    }

    /// The side buttons used to be dropped here, because the fast-path event the
    /// old engine built had nowhere to put them. They travel now, on RDP's
    /// extended pointer PDU.
    #[test]
    fn the_side_buttons_reach_the_remote() {
        let mut last = (3, 4);
        for (button, expected) in
            [(MouseButton::Back, RdpButton::X1), (MouseButton::Forward, RdpButton::X2)]
        {
            let events = translate_input(
                ClientMsg::MouseButton { button, pressed: true, clicks: 1 },
                &mut last,
            );
            assert_eq!(
                events,
                vec![RemoteInput::Button { button: expected, down: true, x: 3, y: 4 }],
                "{button:?}"
            );
        }
    }

    /// The sign convention, which is the easiest thing here to get backwards: the
    /// DOM's deltaY is positive downward and RDP's rotation is positive upward.
    #[test]
    fn wheel_down_is_negative_vertical() {
        let mut last = (1, 2);
        let events =
            translate_input(ClientMsg::Wheel { dx: 0.0, dy: 3.0, unit: WheelUnit::Pixel }, &mut last);
        assert_eq!(
            events,
            vec![RemoteInput::Wheel { delta: -WHEEL_NOTCH, horizontal: false, x: 1, y: 2 }]
        );

        let events = translate_input(
            ClientMsg::Wheel { dx: 0.0, dy: -3.0, unit: WheelUnit::Pixel },
            &mut last,
        );
        assert_eq!(
            events,
            vec![RemoteInput::Wheel { delta: WHEEL_NOTCH, horizontal: false, x: 1, y: 2 }]
        );

        // Horizontal is its own event, and both axes at once are two.
        let events = translate_input(
            ClientMsg::Wheel { dx: 2.0, dy: 2.0, unit: WheelUnit::Pixel },
            &mut last,
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], RemoteInput::Wheel { horizontal: false, .. }));
        assert!(matches!(
            events[1],
            RemoteInput::Wheel { delta: WHEEL_NOTCH, horizontal: true, .. }
        ));

        // No movement, no event.
        assert!(
            translate_input(ClientMsg::Wheel { dx: 0.0, dy: 0.0, unit: WheelUnit::Pixel }, &mut last)
                .is_empty()
        );
    }

    #[test]
    fn key_maps_scancode_release_and_extended() {
        let mut last = (0, 0);
        let events = translate_input(
            ClientMsg::Key { code: "KeyA".into(), pressed: true, caps: false },
            &mut last,
        );
        assert_eq!(
            events,
            vec![RemoteInput::Key { scancode: 0x1E, extended: false, down: true }]
        );

        let events = translate_input(
            ClientMsg::Key { code: "KeyA".into(), pressed: false, caps: false },
            &mut last,
        );
        assert_eq!(
            events,
            vec![RemoteInput::Key { scancode: 0x1E, extended: false, down: false }]
        );

        // An extended key carries the E0 prefix, which the engine crate turns
        // into the KBDEXT bit.
        let events = translate_input(
            ClientMsg::Key { code: "ArrowUp".into(), pressed: true, caps: false },
            &mut last,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], RemoteInput::Key { extended: true, .. }));
    }

    #[test]
    fn unmapped_key_produces_no_events() {
        let mut last = (0, 0);
        assert!(
            translate_input(
                ClientMsg::Key { code: "NoSuchKey".into(), pressed: true, caps: false },
                &mut last,
            )
            .is_empty()
        );
    }

    #[test]
    fn refused_remote_clipboard_reads_retry_with_a_bound() {
        let mut read = PendingClipboardRead::new(CF_UNICODETEXT);
        let mut delays = Vec::new();
        while let Some(delay) = read.retry_after_failure() {
            delays.push(delay);
        }
        assert_eq!(delays, CLIPBOARD_READ_RETRY_DELAYS.to_vec());
        // And it stays exhausted rather than starting over.
        assert!(read.retry_after_failure().is_none());
    }

    #[test]
    fn a_hosts_density_quantizes_at_the_midpoint() {
        assert_eq!(Density::from_host(100), Density::One);
        assert_eq!(Density::from_host(125), Density::One);
        assert_eq!(Density::from_host(149), Density::One);
        assert_eq!(Density::from_host(150), Density::Two);
        assert_eq!(Density::from_host(200), Density::Two);
        assert_eq!(Density::from_host(300), Density::Two);
        // A value no screen could have is 1x rather than a panic or a huge scale.
        assert_eq!(Density::from_host(0), Density::One);
    }

    /// Out of range does not mean "the density is dropped" but "the desktop is
    /// not scaled at all" — a server that finds either scale factor illegal
    /// ignores both — so this end, which invents the number, must not.
    #[test]
    fn a_density_is_a_legal_scale_factor_and_an_integral_scale() {
        for density in [Density::One, Density::Two] {
            assert!((100..=500).contains(&density.percent()), "{density:?}");
            // And the engine crate agrees, so nothing is silently adjusted on
            // the way out.
            assert_eq!(freerdp::sanitise_scale(density.percent()), density.percent());
            // The scale and the percent are exact inverses, which is what keeps
            // a client's points and the remote's pixels from rounding apart.
            assert_eq!(density.scale(), density.percent() as f32 / 100.0);
        }
    }

    #[test]
    fn the_configured_size_is_points_once_a_density_is_in_play() {
        assert_eq!(Density::One.pixels((1280, 800)), (1280, 800));
        assert_eq!(Density::Two.pixels((1280, 800)), (2560, 1600));
    }

    #[test]
    fn a_layouts_density_is_part_of_what_makes_it_a_new_request() {
        let one = Layout { w: 1280, h: 800, density: Density::One };
        let two = Layout { w: 1280, h: 800, density: Density::Two };
        assert_ne!(one, two, "the same pixels at another density is a new request");

        let mut pending = None;
        let mut retry_at = None;
        install_layout(two, one, &mut pending, &mut retry_at);
        assert!(pending.is_some(), "a density change alone is worth asking for");
        assert!(retry_at.is_some());
    }

    /// The whole reason a layout is scheduled rather than sent once: the remote
    /// may ignore it in silence, so it is asked again — and not forever.
    #[test]
    fn a_layout_is_asked_for_more_than_once_and_not_forever() {
        let mut pending = PendingLayout::new(Layout { w: 1280, h: 800, density: Density::One });
        let mut delays = Vec::new();
        while let Some(delay) = pending.wait_again() {
            delays.push(delay);
        }
        assert_eq!(delays, LAYOUT_RETRY_DELAYS.to_vec());
        assert!(pending.wait_again().is_none(), "the schedule is exhausted, not restarted");
    }

    #[test]
    fn a_size_carried_to_another_density_keeps_its_points() {
        let one = Layout { w: 1280, h: 800, density: Density::One };
        let two = one.at_density(Density::Two);
        assert_eq!(two, Layout { w: 2560, h: 1600, density: Density::Two });
        // And back again, exactly — the two densities are integral multiples.
        assert_eq!(two.at_density(Density::One), one);
        // A no-op conversion is the identity, not a rounding of itself.
        assert_eq!(one.at_density(Density::One), one);
    }

    #[test]
    fn a_layout_is_scheduled_only_when_it_is_new() {
        let current = Layout { w: 1280, h: 800, density: Density::One };
        let mut pending = None;
        let mut retry_at = None;

        // The desktop already agrees — including after the size adjustment, which
        // is what stops an odd viewport width asking forever.
        install_layout(Layout { w: 1281, h: 800, density: Density::One }, current, &mut pending, &mut retry_at);
        assert!(pending.is_none(), "1281 adjusts to the 1280 already on screen");

        // A real change schedules.
        let wanted = Layout { w: 1600, h: 900, density: Density::One };
        install_layout(wanted, current, &mut pending, &mut retry_at);
        assert_eq!(pending.as_ref().map(|p| p.layout), Some(wanted));

        // Repeating it does not restart the schedule.
        pending.as_mut().unwrap().wait_again();
        let attempts = pending.as_ref().unwrap().attempts;
        install_layout(wanted, current, &mut pending, &mut retry_at);
        assert_eq!(pending.as_ref().unwrap().attempts, attempts, "the count survives a repeat");

        // And a request that matches the desktop again clears it.
        install_layout(current, current, &mut pending, &mut retry_at);
        assert!(pending.is_none());
        assert!(retry_at.is_none());
    }

    /// The engine crate's size rule is the one this end compares against. If they
    /// ever disagree, a viewport whose width the crate adjusts would look like a
    /// change on every report and ask forever.
    #[test]
    fn the_adjusted_layout_is_what_the_engine_crate_would_send() {
        for (w, h) in [(1281u32, 800u32), (1u32, 1u32), (10_000, 10_000), (1280, 800)] {
            let layout = Layout { w, h, density: Density::Two }.adjusted();
            assert_eq!((layout.w, layout.h), freerdp::sanitise_size(w, h));
            assert_eq!(layout.w % 2, 0, "the width must be even");
            assert!((200..=8192).contains(&layout.w) && (200..=8192).contains(&layout.h));
            // The density rides along untouched.
            assert_eq!(layout.density, Density::Two);
        }
    }

    /// Only one of the three outcomes is worth repeating, and telling them apart
    /// is the whole reason [`Asked`] is not a bool.
    #[test]
    fn only_a_transient_outcome_is_worth_repeating() {
        let session = || {
            // A session that never connects, purely for its `Input` handle: the
            // queue takes commands whether or not anything is draining it, which
            // is exactly the "a call after the session ended is dropped" contract.
            Session::start(Connect {
                host: "127.0.0.1".into(),
                port: 1,
                connect_timeout: Duration::from_millis(1),
                ..Connect::default()
            })
        };
        let (session, _events) = session();
        let input = session.input();
        let current = Layout { w: 1280, h: 800, density: Density::One };

        // Before the remote offers the channel, nothing can go out — and this is
        // the one outcome worth asking again on.
        assert_eq!(
            request_layout(input, false, current, Layout { w: 1600, h: 900, density: Density::One }),
            Asked::NotReady
        );
        // With the channel up, a real change is sent.
        assert_eq!(
            request_layout(input, true, current, Layout { w: 1600, h: 900, density: Density::One }),
            Asked::Sent
        );
        // And the desktop it already has is not asked for at all, because the
        // answer would be a full renegotiation of the session.
        assert_eq!(request_layout(input, true, current, current), Asked::Redundant);
        // Including through the size adjustment.
        assert_eq!(
            request_layout(input, true, current, Layout { w: 1281, h: 800, density: Density::One }),
            Asked::Redundant
        );
    }

    /// A resize is the only acknowledgment a layout gets, and the same event
    /// arrives when a server resizes a session on its own. Telling the two apart
    /// is what stops an unsolicited resize from claiming a density the server
    /// never applied — the fault `applied` carries a paragraph about.
    #[test]
    fn only_the_size_that_was_asked_for_confirms_a_layout() {
        let pending = PendingLayout::new(Layout { w: 1600, h: 900, density: Density::Two });
        assert!(confirms(&pending, (1600, 900)), "the size that went out came back");
        assert!(!confirms(&pending, (1280, 800)), "a desktop the server chose for itself");
        assert!(!confirms(&pending, (1600, 1000)), "one axis is not enough");

        // Through the adjustment, because an odd width is not what was sent: a
        // server answering 1600 has answered the request, and a comparison
        // against the 1601 asked for would retry until the ladder ran out.
        let odd = PendingLayout::new(Layout { w: 1601, h: 900, density: Density::One });
        assert!(confirms(&odd, (1600, 900)));
        assert!(!confirms(&odd, (1601, 900)), "1601 is not a size this can be sent as");
    }

    /// The pack drops the fourth byte and keeps the order. Its own test because
    /// the framebuffer's format is a decision made in the engine crate — RGBX32,
    /// chosen so a consumer that encodes finds R,G,B in memory order — and a
    /// change to it would otherwise show up as wrong colours on a screen.
    #[test]
    fn packing_drops_the_fourth_byte_and_keeps_rgb_order() {
        let frame = Frame {
            width: 2,
            height: 2,
            stride: 8,
            pixels: vec![
                1, 2, 3, 0xFF, 4, 5, 6, 0xFF, // row 0
                7, 8, 9, 0xFF, 10, 11, 12, 0xFF, // row 1
            ],
        };
        let mut buf = Vec::new();
        pack_rgb(&frame, rect(0, 0, 1, 1), &mut buf);
        assert_eq!(buf, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

        // And a sub-rectangle takes the right columns of the right rows, which is
        // where a stride mistake would shear the picture.
        pack_rgb(&frame, rect(1, 1, 1, 1), &mut buf);
        assert_eq!(buf, vec![10, 11, 12]);
    }
}
