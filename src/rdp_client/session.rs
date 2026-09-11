//! The session: its configuration, its thread, and the loop that drives it.

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use ironrdp::connector::connection_activation::{ConnectionActivationFactory, ConnectionActivationState};
use ironrdp::connector::sspi::generator::NetworkRequest;
use ironrdp::connector::{
    self, ClientConnector, ConnectionResult, ConnectorError, ConnectorErrorExt as _, ConnectorResult,
    Credentials, DesktopSize, ServerName,
};
use ironrdp::core::{WriteBuf, encode_vec};
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::displaycontrol::pdu::{
    DeviceScaleFactor, DisplayControlMonitorLayout, DisplayControlPdu, MonitorLayoutEntry,
};
use ironrdp::dvc::{DrdynvcClient, DvcMessage, encode_dvc_messages};
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::Action;
use ironrdp::pdu::gcc::{ConnectionType, KeyboardType};
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp::pdu::input::fast_path::{FastPathInput, FastPathInputEvent};
use ironrdp::pdu::mcs::{DisconnectProviderUltimatum, DisconnectReason, McsMessage};
use ironrdp::pdu::rdp::capability_sets::{MajorPlatformType, RailSupportLevel};
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::pdu::x224::X224;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput, GracefulDisconnectReason};
use ironrdp::svc::ChannelFlags;
use ironrdp_tokio::{FramedWrite as _, NetworkClient, TokioFramed, single_sequence_step_read};
use log::{debug, info, warn};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Duration;

use super::egfx::{self, Update};
use super::error::Error;
use super::framebuffer::{Framebuffer, Rect};
use super::input::{Command, Input};
use super::pointer::{self, Cursor};
use crate::config::Security;
use crate::engine;

// ------------------------------------------------------------------ configuration

/// Everything needed to open a session.
pub struct Connect {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    /// The desktop size to ask for. The server may answer with something else,
    /// which arrives as [`Event::Connected`] and, later, as [`Event::Resize`].
    pub width: u32,
    pub height: u32,
    pub security: Security,
    /// Whether to open Display Control, which is what makes
    /// [`Input::resize`] do anything.
    ///
    /// A server answers a monitor layout by resizing the desktop: a graphics reset
    /// under [`Connect::egfx`], and without it a Deactivation-Reactivation
    /// Sequence that tears down the desktop and the capability set and builds them
    /// again. Either way this client sees only an [`Event::Resize`] afterwards.
    pub resize: bool,
    /// Whether to advertise the graphics pipeline (MS-RDPEGFX), deliberately
    /// independent of [`Connect::resize`], because the two paths trade against each
    /// other after a resize. With the pipeline, a monitor layout is answered by a
    /// graphics reset — no reactivation, channels untouched — but a Windows host
    /// then renders text that stays blurry for the rest of the session. Without it,
    /// the same layout costs a full reactivation after which the server renders the
    /// new desktop from scratch, sharp.
    pub egfx: bool,
}

// ------------------------------------------------------------------ events

/// Something the session did.
#[derive(Clone, Debug)]
pub enum Event {
    /// The desktop exists and its size is settled. Always the first event of a
    /// successful session; a failed one goes straight to [`Event::Ended`].
    Connected { width: u32, height: u32 },
    /// This rectangle of the framebuffer changed.
    Paint(Rect),
    /// The server finished a frame: every [`Event::Paint`] since the last `Frame`
    /// belongs to one coherent picture. Sent only when the server says so itself —
    /// the graphics pipeline's `EndFrame` — never guessed from timing. The legacy
    /// path marks no frames, so a consumer keeps whatever pacing it had and treats
    /// this as the upgrade it is.
    Frame,
    /// The desktop was redefined — resized, or rebuilt at the same size — and the
    /// framebuffer has already been resized and cleared, so everything is about to
    /// be repainted.
    ///
    /// Sent whether the change was asked for or not: a server may resize a session
    /// on its own, and that arrives here identically to the answer to an
    /// [`Input::resize`]. A server normally repaints afterwards but is not obliged
    /// to, and the framebuffer is blank until it does; a caller that cannot show a
    /// blank desktop should follow this with [`Input::refresh`].
    Resize { width: u32, height: u32 },
    /// The server offered Display Control, so [`Input::resize`] now has somewhere
    /// to go. Only ever sent on a session configured with [`Connect::resize`], and
    /// not at all by a server that does not implement MS-RDPEDISP.
    ///
    /// It is **not** a promise that the next resize will be honoured: a Windows host
    /// sends this and then ignores layouts for several seconds more, silently — see
    /// [`Input::resize`].
    ///
    /// `max_area` is the largest total monitor area the server will accept, in
    /// pixels; this client asks for one monitor, so it bounds `width * height`.
    ResizeReady { max_area: u64 },
    Cursor(Cursor),
    /// The session is over, and the channel is about to close. `Ok(())` is an
    /// orderly disconnection from either side.
    Ended(Result<(), Error>),
}

// ------------------------------------------------------------------ the session handle

/// A live RDP session.
///
/// Dropping this asks the session thread to disconnect and waits for it, so a
/// `Session` that has gone out of scope has really stopped — no detached thread
/// still holding a socket open, and no session on the server claimed by a client
/// nobody is watching.
pub struct Session {
    input: Input,
    framebuffer: Arc<Framebuffer>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Connect, on a thread of its own.
    ///
    /// Returns immediately: the connection happens on the new thread and its outcome
    /// arrives as the first [`Event`] — [`Event::Connected`] or [`Event::Ended`].
    /// Connecting takes seconds (TCP, TLS, CredSSP, licensing, the first desktop),
    /// and a `start` that blocked for them would have to be called from a thread the
    /// caller was willing to lose anyway.
    pub fn start(config: Connect) -> (Self, mpsc::UnboundedReceiver<Event>) {
        install_crypto_provider();
        let (events, receiver) = mpsc::unbounded_channel();
        let (commands_tx, commands) = mpsc::unbounded_channel();
        let input = Input::new(commands_tx);
        let framebuffer = Arc::new(Framebuffer::new());

        let spawned = std::thread::Builder::new().name("rdp".into()).spawn({
            let framebuffer = Arc::clone(&framebuffer);
            let events = events.clone();
            move || {
                // A panic here would otherwise take the thread down with no `Ended`
                // event, and the caller would wait on the receiver forever. Converted
                // into the disconnection it really is.
                let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    thread_main(config, commands, &framebuffer, &events)
                }));
                let result = outcome
                    .unwrap_or_else(|_| Err(Error::new("the RDP session thread panicked")));
                let _ = events.send(Event::Ended(result));
            }
        });
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(e) => {
                // The closure never ran, so nothing else will end this session.
                let _ = events.send(Event::Ended(Err(Error::new(format!(
                    "could not start the RDP session thread: {e}"
                )))));
                None
            }
        };
        (Self { input, framebuffer, thread }, receiver)
    }

    /// Keyboard, mouse, refresh and resize.
    pub fn input(&self) -> &Input {
        &self.input
    }

    /// The framebuffer, kept up to date by the session thread.
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.input.shutdown();
        if let Some(thread) = self.thread.take() {
            // Joined rather than detached, and bounded: the thread's loop — and its
            // connect, which races the same queue — wakes on the command, sends its
            // disconnect, and returns.
            let _ = thread.join();
        }
    }
}

/// rustls needs a process-wide crypto provider before the first TLS handshake,
/// and `ring` is the one in the tree. Installed here rather than in `main`, so
/// every path that opens a session — the gateway, a test, a probe — has it.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // An error means some other code in the process installed one first,
        // which is just as good.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
}

fn thread_main(
    config: Connect,
    mut commands: mpsc::UnboundedReceiver<Command>,
    framebuffer: &Framebuffer,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(format!("could not start the RDP session runtime: {e}")))?;
    runtime.block_on(async {
        let display = DisplayCaps::default();
        let graphics = egfx::Updates::default();
        let (result, framed) = tokio::select! {
            // Biased so a session dropped mid-connect stops at the next await rather
            // than finishing a handshake nobody is waiting for.
            biased;
            () = shutdown_requested(&mut commands) => {
                return Err(Error::new("the session was ended before it connected"));
            }
            connected = connect(&config, &display, &graphics) => connected?,
        };
        let desktop = result.desktop_size;
        info!("rdp: connected, desktop {}x{}", desktop.width, desktop.height);
        framebuffer.resize(u32::from(desktop.width), u32::from(desktop.height));
        let _ = events.send(Event::Connected {
            width: u32::from(desktop.width),
            height: u32::from(desktop.height),
        });
        Active::new(result, framed, framebuffer, events, display, graphics).run(&mut commands).await
    })
}

/// Resolves once the caller has asked this session to stop, or dropped every
/// handle that could. Anything else queued before the desktop exists is dropped:
/// there is nothing yet for input to act on.
async fn shutdown_requested(commands: &mut mpsc::UnboundedReceiver<Command>) {
    while let Some(command) = commands.recv().await {
        if matches!(command, Command::Shutdown) {
            return;
        }
    }
}

// ------------------------------------------------------------------ connecting

type Tls = ironrdp_tls::TlsStream<TcpStream>;
type Reader = TokioFramed<ReadHalf<Tls>>;
type Writer = TokioFramed<WriteHalf<Tls>>;

/// What the Display Control channel reports when the server's capabilities
/// arrive: the largest monitor area it accepts. Written from inside the channel's
/// callback, which only IronRDP can call.
#[derive(Clone, Default)]
struct DisplayCaps(Arc<Mutex<Option<u64>>>);

impl DisplayCaps {
    fn max_area(&self) -> u64 {
        self.0.lock().map_or(0, |caps| caps.unwrap_or(0))
    }
}

/// TCP, X.224 negotiation, the TLS upgrade, then CredSSP and the rest of the
/// connection sequence up to the first desktop.
///
/// The socket is [`engine::tcp_connect`]'s, so an RDP host that is switched off is
/// noticed on the same keepalive schedule as every other engine's.
async fn connect(
    config: &Connect,
    display: &DisplayCaps,
    graphics: &egfx::Updates,
) -> Result<(ConnectionResult, TokioFramed<Tls>), Error> {
    let dest = engine::host_port(&config.host, config.port);
    let stream = engine::tcp_connect(&dest).await?;
    let client_addr = stream.local_addr().context("reading the local address")?;
    // The name the certificate and CredSSP are checked against. A bracketed IPv6
    // literal is how `host_port` writes one, not a name either check understands.
    let server_name = config.host.trim_start_matches('[').trim_end_matches(']').to_owned();

    let mut framed = TokioFramed::new(stream);
    let mut connector = ClientConnector::new(connector_config(config), client_addr);
    if config.resize || config.egfx {
        // One `drdynvc` for every dynamic channel this session wants, because there
        // is only one to have.
        let mut drdynvc = DrdynvcClient::new();
        if config.resize {
            let caps = display.clone();
            drdynvc = drdynvc.with_dynamic_channel(DisplayControlClient::new(move |received| {
                if let Ok(mut slot) = caps.0.lock() {
                    *slot = Some(received.max_monitor_area());
                }
                Ok(Vec::new())
            }));
        }
        if config.egfx {
            drdynvc = drdynvc.with_dynamic_channel(egfx::Channel::new(graphics.clone()));
        }
        connector = connector.with_static_channel(drdynvc);
    }

    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .map_err(|e| Error::chain("RDP negotiation", &e))?;
    let (tcp, leftover) = framed.into_inner();
    // Any certificate is accepted, for this session only — see the module doc.
    let (tls, certificate) = ironrdp_tls::upgrade(tcp, &server_name)
        .await
        .map_err(|e| Error::chain("TLS upgrade", &e))?;
    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = TokioFramed::new_with_leftover(tls, leftover);
    let server_public_key = ironrdp_tls::extract_tls_server_public_key(&certificate)
        .ok_or_else(|| Error::new("TLS upgrade: the server certificate carries no public key"))?
        .to_owned();

    let result = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut NoKerberos,
        ServerName::new(server_name),
        server_public_key,
        None,
    )
    .await
    .map_err(|e| Error::chain("RDP activation", &e))?;
    Ok((result, framed))
}

/// CredSSP's network client, which only Kerberos needs — to reach a KDC. A target
/// carries a user name and a password, which is NTLM, so there is nothing for this
/// to fetch; a server that insists on Kerberos gets an answer that says so rather
/// than a hang.
struct NoKerberos;

impl NetworkClient for NoKerberos {
    async fn send(&mut self, _request: &NetworkRequest) -> ConnectorResult<Vec<u8>> {
        Err(ConnectorError::general(
            "Kerberos is not supported; the target needs an account NTLM can authenticate",
        ))
    }
}

fn connector_config(config: &Connect) -> connector::Config {
    let (enable_tls, enable_credssp) = config.security.flags();
    connector::Config {
        desktop_size: DesktopSize { width: narrow(config.width), height: narrow(config.height) },
        monitor_layout: None,
        desktop_scale_factor: 0,
        enable_tls,
        enable_credssp,
        enable_standard_rdp_security: false,
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone(),
        client_build: 0,
        client_name: "remotex".to_owned(),
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
        // **The link is a LAN, and this says so rather than letting the server
        // measure it.** A gateway sits beside the hosts it serves and re-encodes for
        // whatever link the *browser* is on, pacing that itself; a server pacing its
        // own updates from an estimate of this hop throttles a stream nobody asked
        // it to. With no multitransport offered either, the server has nothing to
        // probe but RTT, which IronRDP answers on its own.
        connection_type: ConnectionType::Lan,
        ime_file_name: String::new(),
        // No bitmap codecs: the legacy path is plain bitmaps — lossless, which is the
        // point of turning the pipeline off — and the pipeline carries its own.
        bitmap: None,
        dig_product_id: String::new(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: RailSupportLevel::empty(),
        #[cfg(windows)]
        platform: MajorPlatformType::WINDOWS,
        #[cfg(target_os = "macos")]
        platform: MajorPlatformType::MACINTOSH,
        #[cfg(not(any(windows, target_os = "macos")))]
        platform: MajorPlatformType::UNIX,
        hardware_id: None,
        request_data: None,
        // INFO_AUTOLOGON: a target always carries credentials, so the server should
        // use them rather than show its own logon screen pre-filled with them.
        autologon: true,
        enable_audio_playback: false,
        enable_audio_capture: false,
        // Wallpaper, theming, full-window drag and menu animations all off. Every
        // position of a dragged window is a full window of damage through decode,
        // diff, encode, socket and paint, all for pixels that are gone the moment
        // the drag ends; damage that is never created needs no other optimization
        // downstream.
        performance_flags: PerformanceFlags::DISABLE_WALLPAPER
            | PerformanceFlags::DISABLE_FULLWINDOWDRAG
            | PerformanceFlags::DISABLE_MENUANIMATIONS
            | PerformanceFlags::DISABLE_THEMING,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: None,
        // Take the server's pointer as a shape rather than drawn into the desktop —
        // see `pointer`.
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: config.egfx,
    }
}

// ------------------------------------------------------------------ the active session

/// How many queued commands one turn of the loop takes before it goes back to the
/// socket: enough to fill a fast-path input PDU, few enough that a burst of input
/// cannot starve the desktop.
const COMMANDS_PER_TURN: usize = FastPathInput::MAX_EVENTS;

struct Active<'a> {
    stage: ActiveStage,
    /// The legacy path's decoded desktop. `ActiveStage` paints bitmap updates into
    /// it and names the rectangle, which is then copied into `framebuffer`.
    image: DecodedImage,
    reader: Reader,
    writer: Writer,
    activation: ConnectionActivationFactory,
    refresh_rect: bool,
    suppress_output: bool,
    framebuffer: &'a Framebuffer,
    events: &'a mpsc::UnboundedSender<Event>,
    graphics: egfx::Updates,
    display: DisplayCaps,
    /// Whether [`Event::ResizeReady`] has gone out.
    resize_ready: bool,
    /// The most recent size asked for before the channel was ready — only the most
    /// recent, since a resize supersedes every earlier one rather than queueing
    /// behind it.
    pending_resize: Option<(u32, u32, u32)>,
}

impl<'a> Active<'a> {
    fn new(
        result: ConnectionResult,
        framed: TokioFramed<Tls>,
        framebuffer: &'a Framebuffer,
        events: &'a mpsc::UnboundedSender<Event>,
        display: DisplayCaps,
        graphics: egfx::Updates,
    ) -> Self {
        let desktop = result.desktop_size;
        let (reader, writer) = ironrdp_tokio::split_tokio_framed(framed);
        let stage = ActiveStageBuilder {
            static_channels: result.static_channels,
            user_channel_id: result.user_channel_id,
            io_channel_id: result.io_channel_id,
            message_channel_id: result.message_channel_id,
            share_id: result.share_id,
            compression_type: result.compression_type,
            enable_server_pointer: result.enable_server_pointer,
            pointer_software_rendering: result.pointer_software_rendering,
        }
        .build();
        Self {
            stage,
            image: DecodedImage::new(PixelFormat::RgbA32, desktop.width, desktop.height),
            reader,
            writer,
            activation: result.activation_factory,
            refresh_rect: result.refresh_rect_support,
            suppress_output: result.suppress_output_support,
            framebuffer,
            events,
            graphics,
            display,
            resize_ready: false,
            pending_resize: None,
        }
    }

    async fn run(mut self, commands: &mut mpsc::UnboundedReceiver<Command>) -> Result<(), Error> {
        loop {
            tokio::select! {
                pdu = self.reader.read_pdu() => {
                    let (action, frame) = pdu.map_err(|e| {
                        Error::new(format!("the connection to the host failed: {e}"))
                    })?;
                    if let Some(ended) = self.on_pdu(action, &frame).await? {
                        return ended;
                    }
                }
                command = commands.recv() => {
                    let stop = match command {
                        Some(command) => self.on_commands(command, commands).await?,
                        // Every handle is gone, which is a shutdown nobody sent.
                        None => true,
                    };
                    if stop {
                        self.disconnect().await;
                        return Ok(());
                    }
                }
            }
        }
    }

    fn send(&self, event: Event) {
        // A closed receiver means the caller stopped listening while keeping the
        // `Session`; the session carries on and events go nowhere.
        let _ = self.events.send(event);
    }

    async fn write(&mut self, frame: &[u8]) -> Result<(), Error> {
        if frame.is_empty() {
            return Ok(());
        }
        self.writer
            .write_all(frame)
            .await
            .map_err(|e| Error::new(format!("the connection to the host failed: {e}")))
    }

    /// One PDU from the server. `Some` is the session's end.
    async fn on_pdu(&mut self, action: Action, frame: &[u8]) -> Result<Option<Result<(), Error>>, Error> {
        let outputs = self
            .stage
            .process(&mut self.image, action, frame)
            .map_err(|e| Error::chain("the session", &e))?;
        for output in outputs {
            match output {
                ActiveStageOutput::ResponseFrame(frame) => self.write(&frame).await?,
                ActiveStageOutput::GraphicsUpdate(region) => self.paint_image(region),
                ActiveStageOutput::PointerBitmap(decoded) => {
                    if let Some(image) = pointer::image(&decoded) {
                        self.send(Event::Cursor(Cursor::Image(image)));
                    }
                }
                ActiveStageOutput::PointerHidden => self.send(Event::Cursor(Cursor::Hidden)),
                ActiveStageOutput::PointerDefault => self.send(Event::Cursor(Cursor::Default)),
                ActiveStageOutput::Terminate(reason) => {
                    info!("rdp: the server ended the session: {reason}");
                    return Ok(Some(match reason {
                        GracefulDisconnectReason::UserInitiated
                        | GracefulDisconnectReason::ServerInitiated => Ok(()),
                        GracefulDisconnectReason::Other(why) => Err(Error::new(why)),
                    }));
                }
                ActiveStageOutput::DeactivateAll => self.reactivate().await?,
                // Where the server thinks the pointer is, the server's own view of its
                // monitors, logon and reconnect bookkeeping, and network measurements:
                // nothing here acts on any of them.
                _ => {}
            }
        }
        // A malformed legacy bitmap was dropped, so part of the desktop is stale
        // until the server paints it again.
        if self.stage.take_bitmap_recovery_request() {
            debug!("rdp: a bitmap update was discarded; asking for a repaint");
            self.refresh().await?;
        }
        self.apply_graphics();
        self.poll_display_control().await?;
        Ok(None)
    }

    /// Copy a rectangle the legacy path painted into `image` out to the framebuffer.
    fn paint_image(&self, region: InclusiveRectangle) {
        if region.right < region.left || region.bottom < region.top {
            return;
        }
        let rect = Rect {
            x: u32::from(region.left),
            y: u32::from(region.top),
            width: u32::from(region.right - region.left) + 1,
            height: u32::from(region.bottom - region.top) + 1,
        };
        if self.framebuffer.blit(self.image.data(), self.image.stride(), rect) {
            self.send(Event::Paint(rect));
        }
    }

    /// Everything the graphics pipeline queued while the last PDU was processed.
    fn apply_graphics(&mut self) {
        for update in self.graphics.take() {
            match update {
                Update::Reset { width, height } => {
                    info!("rdp: graphics reset, desktop {width}x{height}");
                    self.redefine_desktop(width, height);
                }
                Update::Paint { rect, rgba } => {
                    if self.framebuffer.blit_packed(rect, &rgba) {
                        self.send(Event::Paint(rect));
                    }
                }
                Update::FrameEnd => self.send(Event::Frame),
            }
        }
    }

    /// The desktop is now `width` × `height`: both copies of it start again, blank,
    /// and the caller is told.
    fn redefine_desktop(&mut self, width: u32, height: u32) {
        self.image = DecodedImage::new(PixelFormat::RgbA32, narrow(width), narrow(height));
        self.framebuffer.resize(width, height);
        self.send(Event::Resize { width, height });
    }

    /// Announce Display Control the first time it is ready, and send whatever size
    /// was asked for while it was not.
    async fn poll_display_control(&mut self) -> Result<(), Error> {
        if !self.resize_ready && self.stage.display_control_ready() == Some(true) {
            self.resize_ready = true;
            self.send(Event::ResizeReady { max_area: self.display.max_area() });
            self.send_layout().await?;
        }
        Ok(())
    }

    /// Send the pending monitor layout, if there is one and a channel to carry it.
    ///
    /// Built here rather than with `ActiveStage::encode_resize`, which marks a
    /// monitor taller than it is wide as portrait-rotated: a phone held upright
    /// would ask the host to turn its desktop on its side. Orientation stays 0, as
    /// every desktop client sends it, and the physical size stays 0 — MS-RDPEDISP's
    /// "unknown", which is the honest answer from a client with no display of its
    /// own. `DeviceScaleFactor` is pinned to 100 beside the caller's
    /// `DesktopScaleFactor`, because a server that finds either out of range ignores
    /// both.
    async fn send_layout(&mut self) -> Result<(), Error> {
        if !self.resize_ready {
            return Ok(()); // held until the channel is ready
        }
        let Some((width, height, scale_percent)) = self.pending_resize.take() else {
            return Ok(());
        };
        let Some(channel) = self.stage.get_dvc::<DisplayControlClient>() else {
            return Ok(()); // the server closed the channel; nothing can carry it
        };
        let channel_id = channel.channel_id();
        let messages = MonitorLayoutEntry::new_primary(width, height)
            .and_then(|entry| entry.with_desktop_scale_factor(scale_percent))
            .map(|entry| entry.with_device_scale_factor(DeviceScaleFactor::Scale100Percent))
            .and_then(|entry| DisplayControlMonitorLayout::new(&[entry]))
            .and_then(|layout| {
                let pdu: DvcMessage = Box::new(DisplayControlPdu::from(layout));
                encode_dvc_messages(channel_id, vec![pdu], ChannelFlags::empty())
            });
        let messages = match messages {
            Ok(messages) => messages,
            Err(e) => {
                warn!("rdp: could not encode a {width}x{height} layout at {scale_percent}%: {e}");
                return Ok(());
            }
        };
        let frame = self
            .stage
            .encode_dvc_messages(messages)
            .map_err(|e| Error::chain("encoding a monitor layout", &e))?;
        debug!("rdp: sending a {width}x{height} monitor layout at {scale_percent}%");
        self.write(&frame).await
    }

    /// The Deactivation-Reactivation Sequence: the server tore the desktop down and
    /// is building it again — its answer to a monitor layout on the legacy path.
    async fn reactivate(&mut self) -> Result<(), Error> {
        debug!("rdp: the server deactivated the desktop; reactivating");
        let mut sequence = self.activation.create();
        let mut buf = WriteBuf::new();
        loop {
            let written = single_sequence_step_read(&mut self.reader, &mut sequence, &mut buf)
                .await
                .map_err(|e| Error::chain("reactivation", &e))?;
            if written.size().is_some() {
                let frame = buf.filled().to_vec();
                self.write(&frame).await?;
            }
            if let ConnectionActivationState::Finalized {
                desktop_size,
                share_id,
                enable_server_pointer,
                pointer_software_rendering,
                static_channel_chunk_size,
                refresh_rect_support,
                suppress_output_support,
                ..
            } = sequence.connection_activation_state()
            {
                if !self.stage.reactivate(
                    sequence.io_channel_id(),
                    sequence.user_channel_id(),
                    share_id,
                    enable_server_pointer,
                    pointer_software_rendering,
                    static_channel_chunk_size,
                ) {
                    return Err(Error::new("reactivation: the server sent an invalid channel chunk size"));
                }
                self.refresh_rect = refresh_rect_support;
                self.suppress_output = suppress_output_support;
                info!("rdp: reactivated, desktop {}x{}", desktop_size.width, desktop_size.height);
                self.redefine_desktop(u32::from(desktop_size.width), u32::from(desktop_size.height));
                return Ok(());
            }
        }
    }

    /// `first`, then whatever else is already queued behind it, with consecutive
    /// input batched into as few PDUs as it fits. `true` means stop.
    async fn on_commands(
        &mut self,
        first: Command,
        commands: &mut mpsc::UnboundedReceiver<Command>,
    ) -> Result<bool, Error> {
        let mut batch = Vec::new();
        let mut next = Some(first);
        let mut taken = 0;
        while let Some(command) = next.take() {
            taken += 1;
            match command {
                Command::Input(event) => batch.push(event),
                other => {
                    // Order is kept: input queued before a refresh goes out before it.
                    self.send_input(&mut batch).await?;
                    match other {
                        Command::Shutdown => return Ok(true),
                        Command::Refresh => self.refresh().await?,
                        Command::Resize { width, height, scale_percent } => {
                            self.pending_resize = Some((width, height, scale_percent));
                            self.send_layout().await?;
                        }
                        Command::Input(_) => unreachable!("matched above"),
                    }
                }
            }
            if taken < COMMANDS_PER_TURN {
                next = commands.try_recv().ok();
            }
        }
        self.send_input(&mut batch).await?;
        Ok(false)
    }

    async fn send_input(&mut self, batch: &mut Vec<FastPathInputEvent>) -> Result<(), Error> {
        if batch.is_empty() {
            return Ok(());
        }
        let outputs = self
            .stage
            .process_fastpath_input(&mut self.image, batch)
            .map_err(|e| Error::chain("encoding input", &e))?;
        batch.clear();
        for output in outputs {
            if let ActiveStageOutput::ResponseFrame(frame) = output {
                self.write(&frame).await?;
            }
        }
        Ok(())
    }

    /// Ask for the whole desktop again, by whichever means this server allows —
    /// IronRDP prefers the Suppress Output toggle, the documented workaround for
    /// hosts that ignore Refresh Rect.
    async fn refresh(&mut self) -> Result<(), Error> {
        let (width, height) = (self.image.width(), self.image.height());
        if width == 0 || height == 0 {
            return Ok(());
        }
        let frames = self
            .stage
            .request_full_redraw(width, height, self.refresh_rect, self.suppress_output)
            .map_err(|e| Error::chain("requesting a repaint", &e))?;
        for frame in frames {
            self.write(&frame).await?;
        }
        Ok(())
    }

    /// Leave the way a client that meant to leaves: an MCS Disconnect Provider
    /// Ultimatum, which the server reads as the user disconnecting — the session
    /// stays on the host, logged on, for the next connection — then a TLS close.
    /// Best effort and bounded, because the connection may already be gone.
    async fn disconnect(&mut self) {
        let ultimatum = McsMessage::DisconnectProviderUltimatum(DisconnectProviderUltimatum::from_reason(
            DisconnectReason::UserRequested,
        ));
        let goodbye = async {
            if let Ok(frame) = encode_vec(&X224(ultimatum)) {
                let _ = self.writer.write_all(&frame).await;
            }
            let (stream, _) = self.writer.get_inner_mut();
            let _ = tokio::io::AsyncWriteExt::shutdown(stream).await;
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), goodbye).await;
    }
}

/// A desktop dimension as the `u16` IronRDP counts in, saturating rather than
/// wrapping: nothing real exceeds RDP's own 8192 a side.
fn narrow(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}
