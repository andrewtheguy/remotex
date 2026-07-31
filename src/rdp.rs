//! Server-side RDP session driven by [IronRDP](https://crates.io/crates/ironrdp).
//!
//! The web server never speaks RDP to the browser: [`crate::ws`] bridges a
//! browser WebSocket to [`run`] here over a pair of channels. `run` connects to
//! the configured RDP host (TCP → TLS → RDP activation), then drives the active
//! session — decoding the framebuffer into [`ServerMsg::Tile`] updates and
//! injecting [`ClientMsg`] input as RDP fast-path PDUs.
//!
//! See docs/architecture.md for the design.

use std::sync::Arc;

use ironrdp::cliprdr::pdu::{ClipboardFormat, ClipboardFormatId, FormatDataResponse};
use ironrdp::cliprdr::{Client, CliprdrClient, CliprdrSvcMessages};
use ironrdp::core::IntoOwned as _;
use ironrdp::pdu::PduResult;
use ironrdp::connector::connection_activation::{
    ConnectionActivationFactory, ConnectionActivationSequence, ConnectionActivationState,
};
use ironrdp::connector::{
    ClientConnector, ConnectionResult, Config, Credentials, DesktopSize, ServerName,
};
use ironrdp::core::WriteBuf;
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::input::MousePdu;
use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::input::mouse::PointerFlags;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::rdpdr::{NoopRdpdrBackend, Rdpdr};
use ironrdp::rdpsnd::client::Rdpsnd;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::reqwest::ReqwestNetworkClient;
use ironrdp_tokio::{FramedWrite as _, TokioFramed, single_sequence_step};
use log::{debug, info, warn};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::audio::AudioBridge;
use crate::config::TargetConfig;
use crate::encode::TileSink;
use crate::engine::{self, clamp_u16, host_port};
use crate::keymap;
use crate::protocol::{ClientMsg, ClipboardSnapshot, MouseButton, ServerMsg, UNSCALED};
use crate::rdp_audio;
use crate::rdp_clipboard::{self, ClipboardEvent};
use crate::tiles::{Rect, Shadow};

// A type-erased async stream, so the connect path (which upgrades TCP → TLS) can
// return a single concrete framed type.
trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite {}
type UpgradedFramed = TokioFramed<Box<dyn AsyncReadWrite + Unpin + Send + Sync>>;

// A Windows peer can advertise Unicode text, fail the first FormatDataRequest,
// then satisfy a retry shortly afterward. Retrying only after that explicit
// failure keeps the normal path fast and stays entirely separate from a remote
// Paste, which arrives as ClipboardEvent::DataRequested instead.
const CLIPBOARD_READ_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(150),
    Duration::from_millis(400),
];

struct PendingClipboardRead {
    format: ClipboardFormatId,
    failures: usize,
}

impl PendingClipboardRead {
    fn new(format: ClipboardFormatId) -> Self {
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
// the Display Control channel not yet having its capabilities: a layout sent then
// cannot go out at all, which is exactly what a resize reported from `connected`
// hits, before the channel is up. The second is Windows applying a monitor layout
// only once the session it is starting has settled — one sent seconds after
// connect is discarded in silence *even after* the channel is ready. Nothing in
// the protocol names what is still missing, and nothing acknowledges a layout
// either, so the only way to tell a refusal from a delay is that the reactivation
// never comes.
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
/// Four outcomes and not a bool, because two of them are worth another attempt and
/// two are not, and a caller that cannot tell them apart either gives up on a
/// channel that was merely still opening or retries a layout that will be refused
/// identically every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// Written to the channel. Not a confirmation: only a reactivation is that, so
    /// this is still worth repeating if none arrives.
    Sent,
    /// The desktop already has this layout, so there was nothing to ask.
    Redundant,
    /// The channel cannot carry it yet. Worth asking again unchanged.
    NotReady,
    /// The layout itself was refused, so asking again would fail the same way.
    Refused,
}

/// Connect to the RDP host, then drive the session until it ends.
///
/// `input_rx` carries browser input; `frame_tx` carries screen updates back.
/// Both closing (browser gone / RDP ended) tears the session down.
///
/// When present, `audio` receives redirected PCM. Each attachment subscribes
/// independently and sends encoded packets over the session WebSocket.
///
/// A thin wrapper so the shutdown cannot be missed. Everything this engine sends the
/// client goes through a [`TileSink`], which forwards from a task of its own — and
/// the engine thread's runtime dies with this function, so anything the sink still
/// held would be lost. That includes the session's final `Error`, whose absence
/// would put the browser back on the picker with nothing to explain why. The body has
/// several early returns; this has one exit, and [`TileSink::finish`] is on it.
pub async fn run(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
    audio: Option<Arc<AudioBridge>>,
) {
    let sink = TileSink::new("rdp", frame_tx, config.tile_codec());
    session(config, input_rx, &sink, audio).await;
    sink.finish().await;
}

async fn session(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    sink: &TileSink,
    audio: Option<Arc<AudioBridge>>,
) {
    // The clipboard channel processor runs inside `ActiveStage` and can only
    // report through a channel — see [`crate::rdp_clipboard`]. Created here so
    // it outlives `connect`, which is where the backend is handed over.
    let (clip_tx, clip_rx) = mpsc::unbounded_channel();

    // The budget covers negotiation, the TLS upgrade and CredSSP — each of which
    // can stall on a host that accepts the connection and then says nothing, which
    // no socket timeout catches. The TCP connect has its own deadline inside the
    // helper, so a slow one is reported as what it is.
    let dest = host_port(&config.host, config.port);
    let Some((connection_result, framed)) = engine::connect_and_handshake(
        "rdp",
        &dest,
        engine::HANDSHAKE_TIMEOUT,
        sink,
        |stream| connect(&config, stream, clip_tx, audio),
    )
    .await
    else {
        return;
    };

    let desktop = connection_result.desktop_size;
    info!("rdp: connected, desktop {}x{}", desktop.width, desktop.height);
    // 1x, always: the density this session ends up at is the attached client's to
    // state, and it has not spoken yet — the handshake above happens before
    // `ServerMsg::Connected` reaches it. A Retina client is therefore one
    // reactivation away from where it wants to be, which is the price of learning
    // the density from whoever attaches rather than from the config file.
    if sink
        .msg(ServerMsg::Resize {
            w: desktop.width,
            h: desktop.height,
            scale: Density::One.scale(),
        })
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
        connection_result,
        framed,
        Flags {
            resize: config.resize,
            clipboard: config.clipboard,
            default_size: (config.width, config.height),
        },
        clip_rx,
        input_rx,
        sink,
    )
    .await
    {
        warn!("rdp: session error: {e:#}");
        let _ = sink
            .msg(ServerMsg::Error {
                message: format!("RDP session ended: {e}"),
            })
            .await;
    }
    info!("rdp: session terminated");
}

/// RDP negotiation → TLS upgrade → CredSSP/finalize, on a connected socket.
///
/// The TCP connect happens in [`run`] (see [`engine::connect_and_handshake`]) so
/// its deadline and this handshake's are sequential rather than nested.
///
/// `clip_tx` is handed to the clipboard backend when the target opted in; it is
/// dropped unused otherwise, and the channel is then never registered at all.
/// `audio` works the same way — and *has* to happen here rather than later,
/// because RDPSND is negotiated when the connection is established: an HTTP
/// listener appearing afterwards cannot add the channel to a live connection.
async fn connect(
    config: &TargetConfig,
    stream: tokio::net::TcpStream,
    clip_tx: mpsc::UnboundedSender<ClipboardEvent>,
    audio: Option<Arc<AudioBridge>>,
) -> anyhow::Result<(ConnectionResult, UpgradedFramed)> {
    let server_name = config.host.clone();

    let client_addr = stream
        .local_addr()
        .map_err(|e| anyhow::anyhow!("get local address: {e}"))?;

    let mut framed = TokioFramed::new(stream);
    let mut connector = register_channels(
        ClientConnector::new(build_connector_config(config), client_addr),
        config,
        clip_tx,
        audio,
    );

    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .map_err(|e| anyhow::anyhow!("RDP negotiation (connect_begin): {}", describe(&e)))?;

    let (initial_stream, leftover) = framed.into_inner();

    let (tls_stream, tls_cert) = ironrdp_tls::upgrade(initial_stream, &server_name)
        .await
        .map_err(|e| anyhow::anyhow!("TLS upgrade: {e}"))?;

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);

    let erased: Box<dyn AsyncReadWrite + Unpin + Send + Sync> = Box::new(tls_stream);
    let mut upgraded_framed = TokioFramed::new_with_leftover(erased, leftover);

    let server_public_key = ironrdp_tls::extract_tls_server_public_key(&tls_cert)
        .ok_or_else(|| anyhow::anyhow!("could not extract TLS server public key"))?
        .to_owned();

    let connection_result = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut upgraded_framed,
        &mut ReqwestNetworkClient::new(),
        ServerName::new(&server_name),
        server_public_key,
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("RDP activation (connect_finalize): {}", describe(&e)))?;

    Ok((connection_result, upgraded_framed))
}

/// Registers the virtual channels a target's flags ask for.
///
/// Separate from [`connect`] so the registration can be asserted on without a
/// socket: `ClientConnector::static_channels` is public, so a test can name the
/// channels this puts on the wire. That matters most for `rdpdr`, which is
/// invisible in every other way — nothing is redirected through it — and whose
/// absence costs an audio target all of its sound.
fn register_channels(
    mut connector: ClientConnector,
    config: &TargetConfig,
    clip_tx: mpsc::UnboundedSender<ClipboardEvent>,
    audio: Option<Arc<AudioBridge>>,
) -> ClientConnector {
    // One `drdynvc` for every dynamic channel this session wants, because there is
    // only one to have: registering it twice would advertise the channel twice and
    // leave the second registration's listeners on a channel the server never
    // talks to. Resize wants the Display Control channel, audio wants
    // `AUDIO_PLAYBACK_DVC`, and either alone is reason enough to negotiate it.
    if config.resize || audio.is_some() {
        let mut drdynvc = DrdynvcClient::new();
        if config.resize {
            // The Display Control Virtual Channel, so the session can drive the
            // remote resolution from the browser viewport (client-initiated
            // resize). The capabilities callback is a no-op — `encode_resize`
            // reads the channel state directly once the server answers.
            drdynvc = drdynvc
                .with_dynamic_channel(DisplayControlClient::new(|_caps| Ok(Vec::new())));
        }
        if let Some(audio) = &audio {
            // The dynamic half of MS-RDPEA. The Windows host tested against picks
            // the static channel instead, but FreeRDP is offered this one on the
            // same host, so which half a server uses is not ours to predict —
            // see [`crate::rdp_audio`], which implements both.
            drdynvc = drdynvc
                .with_dynamic_channel(rdp_audio::AudioPlaybackDvc::new(Arc::clone(audio)));
        }
        connector = connector.with_static_channel(drdynvc);
    }
    if config.clipboard {
        // MS-RDPECLIP. Registered only on opt-in, so a target without the flag
        // never even negotiates the channel and the remote cannot advertise a
        // clipboard at us.
        connector = connector.with_static_channel(CliprdrClient::new(Box::new(
            rdp_clipboard::Backend::new(clip_tx),
        )));
    }
    if let Some(audio) = audio {
        // And the static half, on the same opt-in terms. Both are registered
        // because which one a server uses is the server's choice; the bridge
        // settles which one ends up feeding the queue.
        connector = connector
            .with_static_channel(Rdpsnd::new(Box::new(rdp_audio::Handler::new(audio))));
        // MS-RDPEFS, announced with no devices and an inert backend: nothing is
        // ever redirected through it, and it is not optional. Windows gates audio
        // redirection on device redirection being advertised — with `rdpsnd` alone
        // the tested host opened neither audio channel and sent no format PDU;
        // adding this made the same host start redirecting immediately, over the
        // static channel. FreeRDP encodes the rule as forcing device redirection
        // on whenever audio playback is on ("rdpsnd requires rdpdr to be
        // registered", client/common/cmdline.c). The published spec states only
        // the converse — [MS-RDPEFS] Appendix A<1>: without "RDPSND" advertised
        // the server issues nothing on "RDPDR" — so the pairing is observed
        // behaviour rather than a documented rule, but it is what makes the
        // difference between silence and sound.
        connector = connector.with_static_channel(Rdpdr::new(
            Box::new(NoopRdpdrBackend),
            // Only ever user-visible as the "on <computer>" suffix File Explorer
            // puts after a redirected drive, and we announce no drives.
            "remotex".to_owned(),
        ));
    }
    connector
}

/// Drive the active RDP session: server frames in, input out, tiles back.
///
/// `resize` mirrors the target's config flag: when set, the Display Control
/// channel was negotiated at connect and browser [`ClientMsg::Viewport`] reports
/// drive a client-initiated resolution change (see [`resize_desktop`]).
///
/// `clipboard` does the same for MS-RDPECLIP, and `clip_rx` carries what that
/// channel's processor noticed. Both clipboard buffers live here rather than in
/// the backend: the backend is called from inside `ActiveStage`, which this
/// function owns exclusively, so keeping the state on this side avoids sharing
/// it through a lock.
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
    /// turns a value no screen could have into 1x, and the 1.5 midpoint is the one
    /// the agent applies to this same message.
    fn from_host(scale: u16) -> Self {
        if crate::protocol::scale_ratio(scale) >= 1.5 {
            Self::Two
        } else {
            Self::One
        }
    }

    /// Percent, for the monitor layout's `DesktopScaleFactor`.
    ///
    /// Both values sit inside MS-RDPBCGR's legal 100 to 500, which is load-bearing
    /// rather than incidental: a server MUST ignore *both* scale factors when
    /// either is out of range, so an out-of-spec density would quietly cost the
    /// whole feature rather than part of it. Nothing in FreeRDP's core enforces
    /// that either — only its command line does — so the clamp has to live at
    /// whichever end invents the number.
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
/// shrinks everything drawn in them.
///
/// `u32` because that is what [`MonitorLayoutEntry::adjust_display_size`] and
/// `encode_resize` work in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    w: u32,
    h: u32,
    density: Density,
}

impl Layout {
    /// The desktop as it currently stands, at the density it was last asked for.
    fn current(desktop: DesktopSize, density: Density) -> Self {
        Self {
            w: u32::from(desktop.width),
            h: u32::from(desktop.height),
            density,
        }
    }

    /// The same request at the size the protocol would actually accept: an even
    /// width, and 200 to 8192 per axis.
    fn adjusted(self) -> Self {
        let (w, h) = MonitorLayoutEntry::adjust_display_size(self.w, self.h);
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

async fn active_loop(
    connection_result: ConnectionResult,
    mut framed: UpgradedFramed,
    flags: Flags,
    mut clip_rx: mpsc::UnboundedReceiver<ClipboardEvent>,
    mut input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    sink: &TileSink,
) -> anyhow::Result<()> {
    let Flags { resize, clipboard, default_size } = flags;
    // Retained so a DeactivateAll (the server-side half of a resize) can drive a
    // fresh Deactivation-Reactivation Sequence. The builder below only consumes
    // the channel/share fields, so pull this out first.
    let activation_factory = connection_result.activation_factory;

    let mut desktop = connection_result.desktop_size;
    let mut image = DecodedImage::new(PixelFormat::RgbA32, desktop.width, desktop.height);
    // The pixels the browser has already been sent. Lives beside the framebuffer
    // it shadows, and is forgotten on a repaint and on a resize.
    let mut shadow = Shadow::new("rdp", desktop.width, desktop.height);

    let mut active_stage = ActiveStageBuilder {
        static_channels: connection_result.static_channels,
        user_channel_id: connection_result.user_channel_id,
        io_channel_id: connection_result.io_channel_id,
        message_channel_id: connection_result.message_channel_id,
        share_id: connection_result.share_id,
        compression_type: connection_result.compression_type,
        enable_server_pointer: connection_result.enable_server_pointer,
        pointer_software_rendering: connection_result.pointer_software_rendering,
    }
    .build();

    // Last known pointer position, so button/wheel events (which the browser
    // sends without coordinates) land where the cursor actually is.
    let mut last_pos: (u16, u16) = (desktop.width / 2, desktop.height / 2);

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
    // when a reactivation proves it.
    //
    // Believing a write instead was a bug with two faces. The desktop stayed 1x
    // while this end declared 2x, so every later resize went out with a density the
    // server had thrown away, and — the one a person actually notices — a reattach
    // announced `scale` 2.0 for a 1x framebuffer, which a client presents at half
    // size. Nothing acknowledges a layout on this protocol, so the absence of a
    // reactivation is the only evidence there is, and it has to be the evidence
    // used.
    let mut applied = Density::One;
    // The layout in flight — a size, a density, or both — and when to repeat it.
    // A client states each *once per attach* and then dedupes it (a viewport, and
    // `sendHostScale` in frontend/src/useRemoteDesktop.ts and the viewer's own in
    // AppModel.swift), so nothing will ask again from the other end: every attempt
    // after the first has to come from here. One slot, not one per kind, so a size
    // and a density can only ever be pending as a single merged layout — see
    // [`PendingLayout`] for why one retry rather than two.
    let mut pending_layout: Option<PendingLayout> = None;
    let mut layout_retry_at: Option<Instant> = None;

    loop {
        // The clipboard sender lives inside `active_stage`, so it is only
        // closed when the target did not opt in and the backend was never
        // built. Parking on a future that never completes retires the branch;
        // returning `None` into the `select!` instead would spin the loop, and
        // treating it as end-of-session would end every non-clipboard session
        // before the first tile.
        let clipboard_event = async {
            match clip_rx.recv().await {
                Some(event) => event,
                None => std::future::pending().await,
            }
        };
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

        let outputs = tokio::select! {
            frame = framed.read_pdu() => {
                let (action, payload) = frame.map_err(|e| anyhow::anyhow!("read frame: {e}"))?;
                active_stage
                    .process(&mut image, action, &payload)
                    .map_err(|e| anyhow::anyhow!("process frame: {e}"))?
            }
            input = input_rx.recv() => {
                let Some(input) = input else {
                    info!("rdp: input channel closed; session shut down");
                    break;
                };
                // A (re)attached browser needs the desktop size and a full
                // repaint from the server-owned framebuffer.
                if matches!(input, ClientMsg::Refresh) {
                    // A repaint means the client has nothing, so the shadow must
                    // not claim otherwise. This covers detach, reattach and
                    // takeover in one place, because `Refresh` is injected on
                    // every attach — and it is what keeps the session layer's
                    // frame dropping while nobody is attached from turning into
                    // a permanently blank region.
                    shadow.forget();
                    sink
                        .msg(ServerMsg::Resize {
                            w: desktop.width,
                            h: desktop.height,
                            scale: applied.scale(),
                        })
                        .await?;
                    sink.msg(ServerMsg::RemoteOs { macos: false }).await?;
                    send_tiles(
                        &image,
                        Rect {
                            left: 0,
                            top: 0,
                            right: desktop.width.saturating_sub(1),
                            bottom: desktop.height.saturating_sub(1),
                        },
                        &mut shadow,
                        sink,
                    )
                    .await?;
                    continue;
                }
                // The density of the screen this client's window is on. Ignored
                // outright without `resize`, whose Display Control channel is the
                // only way to restate a density on a live session.
                //
                // Recorded and scheduled rather than asked for here, so the retry
                // branch is the single place a layout is requested from — one
                // attempt and five look the same to this arm.
                if let ClientMsg::HostScale { scale } = input {
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
                let wanted_size = match input {
                    ClientMsg::Viewport { w, h } => Some((u32::from(w), u32::from(h))),
                    ClientMsg::DefaultSize => Some(applied.pixels(default_size)),
                    _ => None,
                };
                if let Some((w, h)) = wanted_size {
                    if resize {
                        // Scheduled, not sent-and-forgotten, for the same reason a
                        // density is: a size that arrives before the Display Control
                        // channel has its capabilities cannot go out, and a client
                        // states its viewport once and dedupes it, so nothing would
                        // re-send it. This is the start of every session with
                        // auto-resize on by default — both reports come from
                        // `connected`, before the channel is up — so without the
                        // retry the desktop stayed at its connect size until a manual
                        // resize landed. The size arrives in the announced density's
                        // pixels; if a denser layout is already pending it is carried
                        // up to that density, so the two go out as one layout and
                        // never as two reactivations racing to set `applied`.
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
                // translating to fast-path input. Both are no-ops when the
                // target did not opt in — the browser hides the control then,
                // so this is the belt to that UI's braces.
                if let ClientMsg::Clipboard { text } = &input {
                    if clipboard {
                        // We are taking ownership of the remote clipboard, so
                        // an older remote Copy/Cut can no longer be fetched.
                        pending_clipboard_read = None;
                        clipboard_retry_at = None;
                        // Only advertised, not sent. The remote asks for the
                        // bytes if and when someone pastes.
                        match rdp_clipboard::to_remote(text) {
                            Some(text) => {
                                debug!(
                                    "rdp: advertising {} bytes to the remote clipboard",
                                    text.len()
                                );
                                local_clipboard = Some(text);
                                advertise_clipboard(
                                    &mut active_stage,
                                    &mut framed,
                                    local_clipboard.as_deref(),
                                )
                                .await?;
                            }
                            // Refused, so the remote keeps whatever it had:
                            // advertising a partial copy would hand out a paste
                            // that looks complete. Both clients refuse this and
                            // say why before it reaches the gateway.
                            None => warn!(
                                "rdp: refusing {} bytes to the remote clipboard, over the {} byte limit",
                                text.len(),
                                crate::protocol::MAX_CLIPBOARD_BYTES
                            ),
                        }
                    }
                    continue;
                }
                if matches!(input, ClientMsg::ClipboardRequest) {
                    // Answered from the buffer the channel fills. Empty until
                    // the remote copies something, which reads in the panel as
                    // "nothing has been copied over there yet".
                    if clipboard {
                        let snapshot = remote_clipboard
                            .clone()
                            .unwrap_or_else(ClipboardSnapshot::unobserved);
                        sink
                            .msg(ServerMsg::Clipboard {
                                text: snapshot.text,
                                changed_at_ms: snapshot.changed_at_ms,
                                requested: true,
                                oversized_bytes: snapshot.oversized_bytes,
                            })
                            .await?;
                    }
                    continue;
                }
                let events = translate_input(input, &mut last_pos);
                if events.is_empty() {
                    continue;
                }
                active_stage
                    .process_fastpath_input(&mut image, &events)
                    .map_err(|e| anyhow::anyhow!("process input: {e}"))?
            }

            // The clipboard channel processor ran inside `active_stage.process`
            // above and left its findings here. Acting on them is a separate
            // turn of the loop because the callbacks cannot answer themselves.
            event = clipboard_event => {
                match event {
                    // Both mean "advertise what we have". The first
                    // `initiate_copy` is load-bearing beyond the advertisement
                    // itself: it carries the Capabilities and TemporaryDirectory
                    // PDUs that finish the handshake, so an empty clipboard
                    // still has to answer.
                    ClipboardEvent::Ready | ClipboardEvent::FormatListRequested => {
                        advertise_clipboard(
                            &mut active_stage,
                            &mut framed,
                            local_clipboard.as_deref(),
                        )
                        .await?;
                    }
                    // Ask straight away rather than waiting for the panel's
                    // Fetch, so a copy on the remote reaches the browser
                    // unprompted exactly as it does for VNC.
                    ClipboardEvent::RemoteFormats(formats) => {
                        match rdp_clipboard::pick_text_format(&formats) {
                            Some(format) => {
                                pending_clipboard_read =
                                    Some(PendingClipboardRead::new(format));
                                clipboard_retry_at = None;
                                request_clipboard(&mut active_stage, &mut framed, format).await?;
                            }
                            None => {
                                pending_clipboard_read = None;
                                clipboard_retry_at = None;
                                debug!("rdp: the remote copied no text format we can carry");
                                remote_clipboard = Some(ClipboardSnapshot::changed(
                                    String::new(),
                                    remote_clipboard.as_ref(),
                                ));
                            }
                        }
                    }
                    ClipboardEvent::RemoteData(text) => {
                        pending_clipboard_read = None;
                        clipboard_retry_at = None;
                        let snapshot = match rdp_clipboard::from_remote(&text) {
                            Ok(text) => {
                                debug!("rdp: remote clipboard updated, {} bytes", text.len());
                                ClipboardSnapshot::changed(text, remote_clipboard.as_ref())
                            }
                            // Reported as its size instead of the first 64 KiB
                            // of it: the panel can say what happened, where a
                            // truncated paste could not be told from a whole one.
                            Err(bytes) => {
                                debug!(
                                    "rdp: remote clipboard is {bytes} bytes, over the {} byte limit",
                                    crate::protocol::MAX_CLIPBOARD_BYTES
                                );
                                ClipboardSnapshot::oversized(bytes, remote_clipboard.as_ref())
                            }
                        };
                        remote_clipboard = Some(snapshot.clone());
                        sink
                            .msg(ServerMsg::Clipboard {
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
                    ClipboardEvent::RemoteDataRefused => {
                        if let Some(read) = pending_clipboard_read.as_mut() {
                            match read.retry_after_failure() {
                                Some(delay) => {
                                    debug!(
                                        "rdp: retrying refused remote clipboard read in {}ms",
                                        delay.as_millis()
                                    );
                                    clipboard_retry_at = Some(Instant::now() + delay);
                                }
                                None => {
                                    debug!("rdp: remote clipboard read exhausted its retries");
                                    pending_clipboard_read = None;
                                    clipboard_retry_at = None;
                                }
                            }
                        }
                    }
                    // Invalid bytes cannot become valid by repeating the same
                    // request. Keep the last good clipboard value and finish
                    // this read without scheduling a retry.
                    ClipboardEvent::RemoteDataMalformed => {
                        pending_clipboard_read = None;
                        clipboard_retry_at = None;
                    }
                    ClipboardEvent::DataRequested(format) => {
                        provide_clipboard(
                            &mut active_stage,
                            &mut framed,
                            format,
                            local_clipboard.as_deref(),
                        )
                        .await?;
                    }
                }
                continue;
            }
            _ = clipboard_retry => {
                clipboard_retry_at = None;
                if let Some(read) = pending_clipboard_read.as_ref() {
                    request_clipboard(&mut active_stage, &mut framed, read.format).await?;
                }
                continue;
            }
            _ = layout_retry => {
                layout_retry_at = None;
                let Some(pending) = pending_layout.as_mut() else {
                    continue; // a reactivation confirmed it first
                };
                let wanted = pending.layout;
                let asked = request_layout(
                    &mut active_stage,
                    &mut framed,
                    Layout::current(desktop, applied),
                    wanted,
                )
                .await?;
                match asked {
                    // Written, or not yet writable — either way the desktop has not
                    // moved, so ask again until it does or the schedule runs out.
                    // Dropping the request is all that giving up takes: `applied`
                    // was never advanced, so the announced scale still describes
                    // the desktop that is actually there.
                    Asked::Sent | Asked::NotReady => match pending.wait_again() {
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
                    // Nothing more to try: a refused layout fails identically on
                    // every attempt, and a redundant one means the desktop already
                    // agrees.
                    Asked::Redundant | Asked::Refused => pending_layout = None,
                }
                continue;
            }
        };

        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => {
                    framed
                        .write_all(&frame)
                        .await
                        .map_err(|e| anyhow::anyhow!("write response: {e}"))?;
                }
                ActiveStageOutput::GraphicsUpdate(region) => {
                    send_tiles(
                        &image,
                        Rect {
                            left: region.left,
                            top: region.top,
                            right: region.right,
                            bottom: region.bottom,
                        },
                        &mut shadow,
                        sink,
                    )
                    .await?;
                }
                ActiveStageOutput::Terminate(reason) => {
                    info!("rdp: session terminated by server: {reason:?}");
                    return Ok(());
                }
                ActiveStageOutput::DeactivateAll => {
                    // The server accepted a resolution change: run the
                    // Deactivation-Reactivation Sequence to learn the new size,
                    // rebuild the framebuffer, and tell the browser to resize.
                    //
                    // This is also the only confirmation a layout ever gets. The
                    // server acts on the most recent layout it was sent, so a
                    // reactivation while one is in flight is that layout — size and
                    // density together — taking effect. `applied` follows the
                    // density that just landed; the new size is read back from the
                    // reactivation itself, just below.
                    if let Some(pending) = pending_layout.take() {
                        applied = pending.layout.density;
                        layout_retry_at = None;
                    }
                    desktop = reactivate(&mut active_stage, &mut framed, &activation_factory)
                        .await?;
                    image = DecodedImage::new(PixelFormat::RgbA32, desktop.width, desktop.height);
                    shadow.resize(desktop.width, desktop.height);
                    last_pos = (
                        last_pos.0.min(desktop.width.saturating_sub(1)),
                        last_pos.1.min(desktop.height.saturating_sub(1)),
                    );
                    sink
                        .msg(ServerMsg::Resize {
                            w: desktop.width,
                            h: desktop.height,
                            scale: applied.scale(),
                        })
                        .await?;
                }
                _ => {}
            }
        }
    }

    shadow.report();
    Ok(())
}

/// Install `wanted` as the layout to ask the server for, or clear the schedule
/// when there is nothing to ask.
///
/// The first attempt belongs in the retry branch with the rest, so this only
/// records the want and dates the deadline now; it never writes to the channel.
/// Two cases need no schedule: the desktop is already `current` (so a screen
/// change that moved back before the remote caught up, or a viewport equal to the
/// desktop, asks for nothing), and the identical layout is already pending (so a
/// deduped-but-repeated report does not reset the attempt count). The comparison
/// is against the adjusted `wanted`, matching what [`request_layout`] will send.
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

/// Ask the server for a different desktop — a size, a density, or both — over the
/// Display Control channel. Sizes are adjusted to the protocol's constraints (even
/// width, 200 to 8192 per axis). The server answers by deactivating the session —
/// see the `DeactivateAll` arm.
///
/// Returns which of the four [`Asked`] outcomes happened, rather than whether
/// anything went out, because the retry branch has to know which layouts are
/// worth another attempt and only two of the four are. Note that even
/// [`Asked::Sent`] is not a confirmation: this
/// protocol acknowledges nothing, so a layout the server silently discards looks
/// from here exactly like one it is about to act on.
///
/// The first thing checked is whether the channel has had the server's
/// capabilities, because `encode_resize` does *not* check: it asks only whether
/// the channel has an id, and a layout written between the channel opening and its
/// caps arriving is discarded by the server with no reply — IronRDP's own
/// [`DisplayControlClient::new`] says so ("attempting to send messages before the
/// capabilities are received will result in an error or a silent failure"). That
/// window is under a second, which is why a human clicking "Resize to window"
/// never found it and a density sent automatically on connect finds it every time.
///
/// Also a no-op when the desktop is already that layout, and that guard earns its
/// place here rather than at the callers: this is the one engine where asking for
/// what you already have is *expensive*, since the server answers any request with
/// a full Deactivation-Reactivation. VNC drops an unchanged request itself, so
/// this is what makes the client requests idempotent across both engines —
/// which matters most for the automatic [`ClientMsg::DefaultSize`] a
/// mobile client sends on every reattach.
///
/// Compared after `adjust_display_size`, because that is the layout that would
/// actually be asked for: an odd width lands on the even one beside it, and
/// comparing before the adjustment would call that a change when it is not. The
/// density is part of that comparison, so a request that only changes the density
/// — which is what a client dragged between two screens of the same size sends —
/// is not mistaken for a repeat.
///
/// The density goes out as `DesktopScaleFactor`. IronRDP pins the companion
/// `DeviceScaleFactor` to 100% whenever a desktop scale factor is given, which is
/// also what FreeRDP's SDL clients send for a 2x display.
async fn request_layout(
    active_stage: &mut ActiveStage,
    framed: &mut UpgradedFramed,
    current: Layout,
    wanted: Layout,
) -> anyhow::Result<Asked> {
    let wanted = wanted.adjusted();
    if !display_control_ready(active_stage) {
        debug!("rdp: {wanted} requested before the Display Control channel has its capabilities");
        return Ok(Asked::NotReady);
    }
    if wanted == current {
        debug!("rdp: the desktop is already {wanted}; not asking for a reactivation");
        return Ok(Asked::Redundant);
    }
    match active_stage.encode_resize(wanted.w, wanted.h, Some(wanted.density.percent()), None) {
        Some(Ok(frame)) => {
            info!("rdp: requesting {wanted}");
            framed
                .write_all(&frame)
                .await
                .map_err(|e| anyhow::anyhow!("write resize: {e}"))?;
            Ok(Asked::Sent)
        }
        // The layout, not the channel: encoding the same one again would fail the
        // same way, so this is where a retry has to stop.
        Some(Err(e)) => {
            warn!("rdp: could not encode {wanted}: {e}");
            Ok(Asked::Refused)
        }
        None => {
            debug!("rdp: {wanted} requested before the Display Control channel is open");
            Ok(Asked::NotReady)
        }
    }
}

/// Whether the Display Control channel has had the server's capabilities, and so
/// whether a monitor layout written to it now would be read.
///
/// The channel tracks this itself and says so through `ready`, but nothing on the
/// path from `encode_resize` down consults it — see [`request_layout`], which is
/// the only caller and the reason this exists.
fn display_control_ready(active_stage: &mut ActiveStage) -> bool {
    active_stage
        .get_dvc::<DisplayControlClient>()
        .and_then(|dvc| dvc.channel_processor_downcast_ref::<DisplayControlClient>())
        .is_some_and(DisplayControlClient::ready)
}

/// Tell the remote what our clipboard now holds (MS-RDPECLIP Format List).
///
/// `text` of `None` advertises nothing, which is the honest answer before the
/// browser has sent anything and is still worth sending — see the handshake
/// note at the call site.
async fn advertise_clipboard(
    active_stage: &mut ActiveStage,
    framed: &mut UpgradedFramed,
    text: Option<&str>,
) -> anyhow::Result<()> {
    let formats: &[ClipboardFormat] = match text {
        Some(_) => &[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)],
        None => &[],
    };
    let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() else {
        return Ok(()); // the target did not opt in
    };
    let messages = cliprdr.initiate_copy(formats);
    write_clipboard(active_stage, framed, messages, "advertise").await
}

/// Ask the remote for its clipboard in `format` (Format Data Request).
async fn request_clipboard(
    active_stage: &mut ActiveStage,
    framed: &mut UpgradedFramed,
    format: ClipboardFormatId,
) -> anyhow::Result<()> {
    let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() else {
        return Ok(());
    };
    let messages = cliprdr.initiate_paste(format);
    write_clipboard(active_stage, framed, messages, "paste request").await
}

/// Hand the remote the text it just asked for (Format Data Response).
///
/// Answers with the PDU's error form when we hold nothing, or when the remote
/// asks for a format we never advertised. Staying silent instead would leave
/// the paste hanging until the remote's own timeout.
async fn provide_clipboard(
    active_stage: &mut ActiveStage,
    framed: &mut UpgradedFramed,
    format: ClipboardFormatId,
    text: Option<&str>,
) -> anyhow::Result<()> {
    let response = match text {
        Some(text) if format == ClipboardFormatId::CF_UNICODETEXT => {
            debug!("rdp: handing {} bytes to the remote's paste", text.len());
            FormatDataResponse::new_unicode_string(text)
        }
        Some(_) => {
            warn!("rdp: the remote asked for clipboard format {format:?}, which we never offered");
            FormatDataResponse::new_error()
        }
        None => FormatDataResponse::new_error(),
    };
    let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() else {
        return Ok(());
    };
    let messages = cliprdr.submit_format_data(response.into_owned());
    write_clipboard(active_stage, framed, messages, "data response").await
}

/// Encode clipboard PDUs onto the wire, reporting failures rather than raising
/// them.
///
/// Nothing here is worth ending a session over. The likeliest failure is a
/// server that accepted our channel registration and then never joined the
/// channel, which makes `process_svc_processor_messages` fail with "channel not
/// found" on every copy — the clipboard should degrade to doing nothing while
/// the desktop keeps working.
// `&mut` rather than the `&` that `process_svc_processor_messages` would
// accept: `ActiveStage` holds a `Box<dyn SvcProcessor>`, which is `Send` but
// not `Sync`, so a shared reference held across the `.await` below makes the
// whole engine future non-`Send`. The engine runs on its own current-thread
// runtime today (`session::spawn_engine`) and so does not need `Send`, but
// giving that up for nothing would be a poor trade.
async fn write_clipboard(
    active_stage: &mut ActiveStage,
    framed: &mut UpgradedFramed,
    messages: PduResult<CliprdrSvcMessages<Client>>,
    what: &str,
) -> anyhow::Result<()> {
    let messages = match messages {
        Ok(messages) => messages,
        Err(e) => {
            warn!("rdp: could not encode a clipboard {what}: {e}");
            return Ok(());
        }
    };
    match active_stage.process_svc_processor_messages(messages) {
        Ok(frame) => framed
            .write_all(&frame)
            .await
            .map_err(|e| anyhow::anyhow!("write clipboard {what}: {e}")),
        Err(e) => {
            warn!("rdp: could not send a clipboard {what}: {e}");
            Ok(())
        }
    }
}

/// Drive a fresh Deactivation-Reactivation Sequence after the server sent
/// DeactivateAll (its response to a resize). Returns the renegotiated desktop
/// size. The `ActiveStage` is kept — only its share id changes — so the live
/// channel set (and thus the Display Control channel) survives.
async fn reactivate(
    active_stage: &mut ActiveStage,
    framed: &mut UpgradedFramed,
    activation_factory: &ConnectionActivationFactory,
) -> anyhow::Result<DesktopSize> {
    let mut sequence: ConnectionActivationSequence = activation_factory.create();
    let mut buf = WriteBuf::new();
    loop {
        single_sequence_step(framed, &mut sequence, &mut buf)
            .await
            .map_err(|e| anyhow::anyhow!("reactivation: {}", describe(&e)))?;
        if let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            ..
        } = sequence.connection_activation_state()
        {
            active_stage.set_share_id(share_id);
            info!(
                "rdp: reactivated, desktop {}x{}",
                desktop_size.width, desktop_size.height
            );
            return Ok(desktop_size);
        }
    }
}

/// Translate one browser input message into RDP fast-path input events.
fn translate_input(input: ClientMsg, last_pos: &mut (u16, u16)) -> Vec<FastPathInputEvent> {
    match input {
        ClientMsg::MouseMove { x, y } => {
            let (x, y) = (clamp_u16(x), clamp_u16(y));
            *last_pos = (x, y);
            vec![FastPathInputEvent::MouseEvent(MousePdu {
                flags: PointerFlags::MOVE,
                number_of_wheel_rotation_units: 0,
                x_position: x,
                y_position: y,
            })]
        }
        // `clicks` goes nowhere: RDP carries button state alone, and Windows
        // counts the clicks itself from the events it receives.
        ClientMsg::MouseButton { button, pressed, .. } => {
            let flags = match button {
                MouseButton::Left => PointerFlags::LEFT_BUTTON,
                MouseButton::Right => PointerFlags::RIGHT_BUTTON,
                MouseButton::Middle => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
                // The side buttons travel in RDP's *extended* pointer PDU, which
                // this fast-path event cannot carry. Dropped rather than sent as
                // some other button, which would be worse than doing nothing.
                MouseButton::Back | MouseButton::Forward => return Vec::new(),
            };
            let mut flags = flags;
            if pressed {
                flags |= PointerFlags::DOWN;
            }
            vec![FastPathInputEvent::MouseEvent(MousePdu {
                flags,
                number_of_wheel_rotation_units: 0,
                x_position: last_pos.0,
                y_position: last_pos.1,
            })]
        }
        // The unit is dropped: RDP spends a notch as 120 rotation units whatever
        // the delta was measured in, and the guest applies its own scrolling.
        ClientMsg::Wheel { dx, dy, .. } => {
            let mut events = Vec::new();
            // RDP: positive rotation is up/forward. The DOM deltaY is positive
            // when scrolling down, so invert it. One notch ≈ 120 units.
            if dy != 0.0 {
                events.push(FastPathInputEvent::MouseEvent(MousePdu {
                    flags: PointerFlags::VERTICAL_WHEEL,
                    number_of_wheel_rotation_units: if dy > 0.0 { -120 } else { 120 },
                    x_position: last_pos.0,
                    y_position: last_pos.1,
                }));
            }
            if dx != 0.0 {
                events.push(FastPathInputEvent::MouseEvent(MousePdu {
                    flags: PointerFlags::HORIZONTAL_WHEEL,
                    number_of_wheel_rotation_units: if dx > 0.0 { 120 } else { -120 },
                    x_position: last_pos.0,
                    y_position: last_pos.1,
                }));
            }
            events
        }
        // `caps` is VNC-only: the RDP host tracks its own CapsLock from the
        // forwarded scancode.
        ClientMsg::Key { code, pressed, .. } => match keymap::scancode(&code) {
            Some((scancode, extended)) => {
                let mut flags = KeyboardFlags::empty();
                if !pressed {
                    flags |= KeyboardFlags::RELEASE;
                }
                if extended {
                    flags |= KeyboardFlags::EXTENDED;
                }
                vec![FastPathInputEvent::KeyboardEvent(flags, scancode)]
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
        // `Audio` is another: it subscribes that attachment to the queue this
        // engine already fills, so there is nothing to translate for the remote.
        ClientMsg::Connect { .. }
        | ClientMsg::Disconnect
        | ClientMsg::CacheReset
        | ClientMsg::Audio { .. } => Vec::new(),
        // An RDP session is one framebuffer spanning every monitor the server
        // composed into it, and its protocol has no way to ask for one of them.
        // So this engine never sends a display list, no client offers the
        // picker, and anything arriving here is a client that invented one.
        ClientMsg::SelectDisplay { .. } => Vec::new(),
    }
}

/// Send whatever part of `rect` the client does not already have, as tiles of at
/// most [`crate::protocol::CELL_H`] rows each.
///
/// Comparing against `shadow` earns its keep on this engine in particular. The RDP
/// pointer is composited into the framebuffer (`pointer_software_rendering:
/// true`), so *every* mouse event over a still desktop produces a damage
/// rectangle — and this engine also repaints regions that did not change, which
/// nothing upstream filters. Both come back as `None` here and cost nothing but a
/// pack and a `memcmp`.
///
/// The pack happens either way; what is skipped is the PNG encode, which is far
/// the more expensive half (~8–10× the hash it replaced, measured in
/// `protocol::tests::encode_cost_against_hash_cost`). That encode no longer happens
/// here — [`TileSink`] runs it elsewhere — so what this loop costs is the pack, the
/// `memcmp` and an allocation per band.
async fn send_tiles(
    image: &DecodedImage,
    rect: Rect,
    shadow: &mut Shadow,
    sink: &TileSink,
) -> anyhow::Result<()> {
    let (fb_w, fb_h) = shadow.size();
    if rect.left >= fb_w || rect.top >= fb_h {
        return Ok(());
    }
    let rect = Rect {
        left: rect.left,
        top: rect.top,
        right: rect.right.min(fb_w - 1),
        bottom: rect.bottom.min(fb_h - 1),
    };
    if rect.right < rect.left || rect.bottom < rect.top {
        return Ok(());
    }

    let mut buf = Vec::new();
    pack_rgb(image, rect, &mut buf);
    let Some(changed) = shadow.accept(rect, &buf) else {
        return Ok(());
    };

    for band in changed.bands() {
        // Its own buffer, not the one above: the encoder reads these pixels after
        // this loop has moved on, and `image` is overwritten by the next PDU.
        // Repacked rather than sliced, because a band of the *trimmed* rectangle is
        // narrower than the reported one, so its rows are not contiguous in `buf`.
        let mut pixels = Vec::new();
        pack_rgb(image, band, &mut pixels);
        sink.tile(band.left, band.top, band.w(), band.h(), pixels)
            .await?;
    }

    Ok(())
}

/// Pack `rect` out of the framebuffer into `buf` as RGB888.
///
/// The framebuffer alpha is meaningless for a screen (and IronRDP may leave it 0),
/// so it is dropped rather than shipped.
fn pack_rgb(image: &DecodedImage, rect: Rect, buf: &mut Vec<u8>) {
    let bpp = image.bytes_per_pixel();
    let stride = image.stride();
    let data = image.data();
    let w = usize::from(rect.w());

    buf.clear();
    buf.reserve(w * usize::from(rect.h()) * 3);
    for r in 0..rect.h() {
        let start = usize::from(rect.top + r) * stride + usize::from(rect.left) * bpp;
        for px in data[start..start + w * bpp].chunks_exact(bpp) {
            buf.extend_from_slice(&px[..3]);
        }
    }
}

/// Render an error together with its full `source()` chain, so wrappers like
/// IronRDP's `ConnectorError` reveal the underlying cause (e.g. the CredSSP /
/// SSPI reason) instead of just a top-level label.
fn describe(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        out.push_str(" -> ");
        out.push_str(&e.to_string());
        source = e.source();
    }
    out
}

/// Build the IronRDP connector config from our runtime config.
///
/// Enables both TLS and CredSSP/NLA so the server can negotiate the strongest
/// security it supports. Modeled on the IronRDP `screenshot` example.
fn build_connector_config(config: &TargetConfig) -> Config {
    let (enable_tls, enable_credssp) = config.security.flags();
    Config {
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone(),
        enable_tls,
        enable_credssp,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize {
            width: config.width,
            height: config.height,
        },
        bitmap: None,
        client_build: 0,
        client_name: "remotex".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),

        #[cfg(windows)]
        platform: MajorPlatformType::WINDOWS,
        #[cfg(target_os = "macos")]
        platform: MajorPlatformType::MACINTOSH,
        #[cfg(target_os = "linux")]
        platform: MajorPlatformType::UNIX,
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        platform: MajorPlatformType::UNIX,

        // Render the server pointer into the framebuffer so the cursor is visible.
        enable_server_pointer: true,
        pointer_software_rendering: true,
        request_data: None,
        autologon: false,
        // Load-bearing beyond the channel registration: left false, IronRDP puts
        // NO_AUDIO_PLAYBACK in the Client Info PDU, and the server then redirects
        // nothing however carefully RDPSND was negotiated.
        enable_audio_playback: config.audio,
        compression_type: None,
        multitransport_flags: None,
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        performance_flags: PerformanceFlags::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WheelUnit;

    fn one(input: ClientMsg, last_pos: &mut (u16, u16)) -> FastPathInputEvent {
        let mut events = translate_input(input, last_pos);
        assert_eq!(events.len(), 1, "expected exactly one event");
        events.remove(0)
    }

    #[test]
    fn mouse_move_sets_flags_and_updates_last_pos() {
        let mut pos = (0, 0);
        let event = one(ClientMsg::MouseMove { x: 40, y: 50 }, &mut pos);
        match event {
            FastPathInputEvent::MouseEvent(pdu) => {
                assert_eq!(pdu.flags, PointerFlags::MOVE);
                assert_eq!((pdu.x_position, pdu.y_position), (40, 50));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(pos, (40, 50));
    }

    #[test]
    fn negative_and_huge_coords_are_clamped() {
        let mut pos = (0, 0);
        let event = one(ClientMsg::MouseMove { x: -3, y: 100_000 }, &mut pos);
        match event {
            FastPathInputEvent::MouseEvent(pdu) => {
                assert_eq!((pdu.x_position, pdu.y_position), (0, u16::MAX));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn button_press_uses_last_pos_and_down_flag() {
        let mut pos = (12, 34);
        let event = one(
            ClientMsg::MouseButton {
                button: MouseButton::Left,
                pressed: true,
                clicks: 1,
            },
            &mut pos,
        );
        match event {
            FastPathInputEvent::MouseEvent(pdu) => {
                assert!(pdu.flags.contains(PointerFlags::LEFT_BUTTON));
                assert!(pdu.flags.contains(PointerFlags::DOWN));
                assert_eq!((pdu.x_position, pdu.y_position), (12, 34));
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Release drops the DOWN flag.
        let event = one(
            ClientMsg::MouseButton {
                button: MouseButton::Right,
                pressed: false,
                clicks: 1,
            },
            &mut pos,
        );
        match event {
            FastPathInputEvent::MouseEvent(pdu) => {
                assert!(pdu.flags.contains(PointerFlags::RIGHT_BUTTON));
                assert!(!pdu.flags.contains(PointerFlags::DOWN));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn wheel_down_is_negative_vertical() {
        let mut pos = (0, 0);
        let event = one(
            ClientMsg::Wheel { dx: 0.0, dy: 3.0, unit: WheelUnit::Pixel },
            &mut pos,
        );
        match event {
            FastPathInputEvent::MouseEvent(pdu) => {
                assert!(pdu.flags.contains(PointerFlags::VERTICAL_WHEEL));
                assert_eq!(pdu.number_of_wheel_rotation_units, -120);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn key_maps_scancode_release_and_extended() {
        let mut pos = (0, 0);

        match one(
            ClientMsg::Key {
                code: "KeyA".to_owned(),
                pressed: true,
                caps: false,
            },
            &mut pos,
        ) {
            FastPathInputEvent::KeyboardEvent(flags, code) => {
                assert_eq!(code, 0x1E);
                assert!(flags.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }

        match one(
            ClientMsg::Key {
                code: "ArrowUp".to_owned(),
                pressed: false,
                caps: false,
            },
            &mut pos,
        ) {
            FastPathInputEvent::KeyboardEvent(flags, code) => {
                assert_eq!(code, 0x48);
                assert!(flags.contains(KeyboardFlags::RELEASE));
                assert!(flags.contains(KeyboardFlags::EXTENDED));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unmapped_key_produces_no_events() {
        let mut pos = (0, 0);
        assert!(
            translate_input(
                ClientMsg::Key {
                    code: "Nonexistent".to_owned(),
                    pressed: true,
                    caps: false,
                },
                &mut pos,
            )
            .is_empty()
        );
    }

    /// Pinned because the failure it guards against is silent and total: with
    /// `enable_audio_playback` false, IronRDP puts `NO_AUDIO_PLAYBACK` in the
    /// Client Info PDU and the server redirects nothing — a registered RDPSND
    /// channel that simply never carries a byte, which looks exactly like a
    /// server that has no audio to offer.
    #[test]
    fn audio_playback_is_requested_exactly_for_an_audio_target() {
        let mut target = TargetConfig {
            name: "win".to_owned(),
            protocol: crate::config::Protocol::Rdp,
            subtype: None,
            host: "127.0.0.1".to_owned(),
            port: 3389,
            username: "tester".to_owned(),
            password: String::new(),
            vnc_password: String::new(),
            domain: None,
            width: 1280,
            height: 800,
            security: crate::config::Security::Auto,
            resize: false,
            clipboard: false,
            audio: false,
            render_type: crate::config::RenderType::Full,
            render_subtype: crate::config::RenderSubtype::Png,
            render_quality: None,
        };
        assert!(!build_connector_config(&target).enable_audio_playback);
        target.audio = true;
        assert!(build_connector_config(&target).enable_audio_playback);
    }

    /// The other half of the same silent failure, and the more surprising one.
    ///
    /// Windows redirects no audio unless **device** redirection is advertised
    /// beside it, so an audio target has to put `rdpdr` on the wire even though
    /// nothing is ever redirected through it. That was worth days to find (see
    /// the registration in [`register_channels`]) and nothing else in this crate
    /// would notice its removal: no test fails, no log line changes, and the
    /// symptom is a session that is merely quiet. Hence pinning the channel names
    /// themselves.
    ///
    /// It is pinned in both directions: a target that did not ask for audio must
    /// not get device redirection as a side effect.
    #[test]
    fn an_audio_target_advertises_rdpdr_beside_the_audio_channels() {
        use ironrdp::pdu::gcc::ChannelName;

        fn channels_for(audio: bool, resize: bool, clipboard: bool) -> Vec<ChannelName> {
            let target = TargetConfig {
                name: "win".to_owned(),
                protocol: crate::config::Protocol::Rdp,
                subtype: None,
                host: "127.0.0.1".to_owned(),
                port: 3389,
                username: "tester".to_owned(),
                password: String::new(),
                vnc_password: String::new(),
                domain: None,
                width: 1280,
                height: 800,
                security: crate::config::Security::Auto,
                resize,
                clipboard,
                audio,
                render_type: crate::config::RenderType::Full,
                render_subtype: crate::config::RenderSubtype::Png,
                render_quality: None,
            };
            let (clip_tx, _clip_rx) = mpsc::unbounded_channel();
            let bridge = audio.then(|| Arc::new(AudioBridge::new()));
            let connector = register_channels(
                ClientConnector::new(
                    build_connector_config(&target),
                    "127.0.0.1:0".parse().unwrap(),
                ),
                &target,
                clip_tx,
                bridge,
            );
            connector
                .static_channels
                .values()
                .map(|channel| channel.channel_name())
                .collect()
        }

        let audio_target = channels_for(true, false, false);
        assert!(
            audio_target.contains(&Rdpdr::NAME),
            "an audio target must advertise rdpdr or the remote redirects nothing: {audio_target:?}"
        );
        assert!(audio_target.contains(&Rdpsnd::NAME));
        assert!(audio_target.contains(&DrdynvcClient::NAME));

        // Everything else on, audio off: no rdpdr, because device redirection is
        // not a feature this gateway offers on its own.
        let quiet_target = channels_for(false, true, true);
        assert!(!quiet_target.contains(&Rdpdr::NAME), "{quiet_target:?}");
        assert!(!quiet_target.contains(&Rdpsnd::NAME));
    }

    #[test]
    fn refused_remote_clipboard_reads_retry_with_a_bound() {
        let mut read = PendingClipboardRead::new(ClipboardFormatId::CF_UNICODETEXT);
        assert_eq!(read.format, ClipboardFormatId::CF_UNICODETEXT);
        for expected in CLIPBOARD_READ_RETRY_DELAYS {
            assert_eq!(read.retry_after_failure(), Some(expected));
        }
        assert_eq!(read.retry_after_failure(), None);
        assert_eq!(read.retry_after_failure(), None);
    }

    /// The midpoint, and the two ends `scale_ratio` refuses.
    ///
    /// 150 is the boundary the Mac agent uses on this same message, so the two
    /// engines answer a 1.5x screen the same way; the out-of-range pair matter
    /// because a client computes the number from `devicePixelRatio` (or
    /// `backingScaleFactor`) and a screen that reports nonsense should read as the
    /// density that asks the remote for least, not as an absurd one.
    #[test]
    fn a_hosts_density_quantizes_at_the_agents_midpoint() {
        for scale in [0, 1, 99, 100, 125, 149] {
            assert_eq!(Density::from_host(scale), Density::One, "{scale}");
        }
        for scale in [150, 175, 200, 300, 400] {
            assert_eq!(Density::from_host(scale), Density::Two, "{scale}");
        }
        // Past `SCALE_MAX`, so `scale_ratio` reports 1.0 rather than clamping.
        for scale in [401, 500, u16::MAX] {
            assert_eq!(Density::from_host(scale), Density::One, "{scale}");
        }
    }

    /// Both percentages must sit inside MS-RDPBCGR's legal 100..=500, or the
    /// server ignores the scale factor entirely and the desktop comes back
    /// supersampled instead of scaled — a failure that looks like everything
    /// working at half size.
    #[test]
    fn a_density_is_a_legal_scale_factor_and_an_integral_scale() {
        assert_eq!(Density::One.percent(), 100);
        assert_eq!(Density::Two.percent(), 200);
        for density in [Density::One, Density::Two] {
            assert!((100..=500).contains(&density.percent()), "{density:?}");
        }
        assert_eq!(Density::One.scale(), UNSCALED);
        assert_eq!(Density::Two.scale(), 2.0);
    }

    /// `DefaultSize` is the one size here that is configured rather than measured,
    /// so it is the one that has to be read as points.
    #[test]
    fn the_configured_size_is_points_once_a_density_is_in_play() {
        assert_eq!(Density::One.pixels((1280, 800)), (1280, 800));
        assert_eq!(Density::Two.pixels((1280, 800)), (2560, 1600));
    }

    /// Two layouts of the same size but different densities are not the same
    /// request. Without this, a client dragged between a 1x and a 2x screen of the
    /// same size — or one whose window has not moved at all when the resolution
    /// clamps both requests to the same pixels — would have its density dropped as
    /// a repeat, and the browser never restates it.
    #[test]
    fn a_layouts_density_is_part_of_what_makes_it_a_new_request() {
        let one = Layout {
            w: 1280,
            h: 800,
            density: Density::One,
        };
        assert_eq!(one.adjusted(), one);
        assert_ne!(one, Layout { density: Density::Two, ..one });

        // The protocol's own limits, applied where the comparison happens: an odd
        // width lands on the even one beside it and both axes clamp to 200..=8192,
        // so a request already at the adjusted size is recognised as a repeat.
        let odd = Layout { w: 1281, ..one }.adjusted();
        assert_eq!((odd.w, odd.h), (1280, 800));
        let huge = Layout {
            w: 10000,
            h: 100,
            ..one
        }
        .adjusted();
        assert_eq!((huge.w, huge.h), (8192, 200));
    }

    /// A layout is asked for more than once, and a bounded number of times.
    ///
    /// Both halves matter and they pull against each other. More than once, because
    /// the Display Control channel may not have its capabilities yet — a resize
    /// reported from `connected` hits exactly that — and because Windows discards a
    /// layout sent while the session it is starting has not settled, even after the
    /// channel is ready; nothing asks again from the other end, since a client
    /// states a size or a density once per attach and dedupes it. Bounded, because
    /// a server that will never honour one must not be asked forever.
    #[test]
    fn a_layout_is_asked_for_more_than_once_and_not_forever() {
        let wanted = Layout { w: 2560, h: 1600, density: Density::Two };
        let mut pending = PendingLayout::new(wanted);
        assert_eq!(pending.layout, wanted);
        for expected in LAYOUT_RETRY_DELAYS {
            assert_eq!(pending.wait_again(), Some(expected));
        }
        assert_eq!(pending.wait_again(), None);
        assert_eq!(pending.wait_again(), None);
        // Long enough to outlast a logon, which is one of the things being waited out.
        let total: Duration = LAYOUT_RETRY_DELAYS.iter().sum();
        assert!(total >= Duration::from_secs(10), "{total:?}");
    }

    /// Re-expressing a size at another density keeps the *points* and scales the
    /// *pixels* — which is what lets a viewport reported at one density and a
    /// screen change to another merge into a single layout instead of two.
    #[test]
    fn a_size_carried_to_another_density_keeps_its_points() {
        let one = Layout { w: 1280, h: 800, density: Density::One };
        // 1x → 2x doubles the pixels; the same window, twice as sharp.
        assert_eq!(one.at_density(Density::Two), Layout { w: 2560, h: 1600, density: Density::Two });
        // The same density is a no-op, down to the struct.
        assert_eq!(one.at_density(Density::One), one);
        // And it round-trips.
        assert_eq!(one.at_density(Density::Two).at_density(Density::One), one);
    }

    /// `install_layout` schedules a genuinely new want, leaves the desktop alone
    /// when it is already there, and does not restart the clock on a repeat — the
    /// three cases that keep a deduped, retrying client from either stalling or
    /// spinning.
    #[test]
    fn a_layout_is_scheduled_only_when_it_is_new() {
        let current = Layout { w: 1280, h: 800, density: Density::One };
        let mut pending = None;
        let mut retry_at = None;

        // A different size schedules, with its first attempt due immediately.
        let bigger = Layout { w: 1920, h: 1080, density: Density::One };
        install_layout(bigger, current, &mut pending, &mut retry_at);
        assert_eq!(pending.as_ref().map(|p| p.layout), Some(bigger));
        assert!(retry_at.is_some());

        // A repeat of the same want does not reset the attempt count: burn one
        // attempt, then re-install the identical layout and confirm the count held.
        pending.as_mut().unwrap().wait_again();
        let attempts = pending.as_ref().unwrap().attempts;
        install_layout(bigger, current, &mut pending, &mut retry_at);
        assert_eq!(pending.as_ref().unwrap().attempts, attempts);

        // Asking for exactly what is already on screen clears the schedule — down
        // to an odd width the protocol would round to the desktop's even one.
        install_layout(Layout { w: 1281, ..current }, current, &mut pending, &mut retry_at);
        assert!(pending.is_none());
        assert!(retry_at.is_none());
    }

    /// The distinction the retry depends on: a channel that is not ready yet is
    /// worth asking again, a layout the encoder refused is not. Collapsing them
    /// would either abandon a density over a channel that was merely still opening,
    /// or re-encode a broken layout on every tick and warn each time.
    #[test]
    fn only_a_transient_outcome_is_worth_repeating() {
        let again = [Asked::Sent, Asked::NotReady];
        let done = [Asked::Redundant, Asked::Refused];
        for outcome in again {
            assert!(!done.contains(&outcome), "{outcome:?}");
        }
        // `Sent` is in the retry set deliberately: this protocol acknowledges
        // nothing, so a written layout the server silently dropped is
        // indistinguishable from one it is about to act on.
        assert!(again.contains(&Asked::Sent));
    }

    /// Why [`display_control_ready`] has to exist: IronRDP will happily encode a
    /// monitor layout for a channel that has not had the server's capabilities,
    /// and the server discards such a layout without answering.
    ///
    /// Pinned in both halves, so this fails if IronRDP ever grows the check
    /// itself — at which point the gate is redundant rather than load-bearing, and
    /// someone should know before deleting it. A Retina client sends its density
    /// on connect and so lands in that window every single time; the bug it caused
    /// was a desktop stuck at 1x while this end declared 2x on every later resize,
    /// which reads as "resize to window gives half the size I asked for".
    #[test]
    fn ironrdp_encodes_a_layout_for_a_channel_that_is_not_ready_yet() {
        let client = DisplayControlClient::new(|_caps| Ok(Vec::new()));
        assert!(
            !client.ready(),
            "a fresh Display Control channel must not claim to be usable"
        );
        assert!(
            client
                .encode_single_primary_monitor(1, 2560, 1600, Some(Density::Two.percent()), None)
                .is_ok(),
            "IronRDP encodes regardless, so nothing below this gate will refuse the write"
        );
    }

    /// The one thing here that is IronRDP's rather than ours, pinned because a
    /// version bump could remap it silently and the symptom would be a desktop
    /// that resizes but never sharpens.
    ///
    /// `DeviceScaleFactor` is forced to 100% beside the desktop factor, which is
    /// also what FreeRDP's SDL clients send for a Retina display — the field only
    /// admits 100/140/180, so it cannot carry 200 anyway.
    #[test]
    fn a_layout_reaches_the_monitor_layout_as_both_scale_factors() {
        use ironrdp::displaycontrol::pdu::{DeviceScaleFactor, DisplayControlMonitorLayout};

        let layout = DisplayControlMonitorLayout::new_single_primary_monitor(
            2560,
            1600,
            Some(Density::Two.percent()),
            None,
        )
        .expect("2560x1600 at 200% is a legal single-monitor layout");
        let monitor = &layout.monitors()[0];
        assert_eq!(monitor.dimensions(), (2560, 1600));
        assert_eq!(monitor.desktop_scale_factor(), Some(200));
        assert_eq!(
            monitor.device_scale_factor(),
            Some(DeviceScaleFactor::Scale100Percent)
        );
    }
}
