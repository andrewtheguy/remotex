//! Server-side RDP session driven by [IronRDP](https://crates.io/crates/ironrdp).
//!
//! The web server never speaks RDP to the browser: [`crate::ws`] bridges a
//! browser WebSocket to [`run`] here over a pair of channels. `run` connects to
//! the configured RDP host (TCP → TLS → RDP activation), then drives the active
//! session — decoding the framebuffer into [`ServerMsg::Tile`] updates and
//! injecting [`ClientMsg`] input as RDP fast-path PDUs.
//!
//! See docs/architecture.md for the design.

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
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::reqwest::ReqwestNetworkClient;
use ironrdp_tokio::{FramedWrite as _, TokioFramed, single_sequence_step};
use log::{debug, info, warn};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::config::TargetConfig;
use crate::encode::TileSink;
use crate::engine::{self, clamp_u16, host_port};
use crate::keymap;
use crate::protocol::{ClientMsg, ClipboardSnapshot, MouseButton, ServerMsg, UNSCALED};
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
pub async fn run(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) {
    let sink = TileSink::new("rdp", frame_tx);
    session(config, input_rx, &sink).await;
    sink.finish().await;
}

async fn session(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    sink: &TileSink,
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
        |stream| connect(&config, stream, clip_tx),
    )
    .await
    else {
        return;
    };

    let desktop = connection_result.desktop_size;
    info!("rdp: connected, desktop {}x{}", desktop.width, desktop.height);
    if sink
        .msg(ServerMsg::Resize {
            w: desktop.width,
            h: desktop.height,
            scale: UNSCALED,
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
async fn connect(
    config: &TargetConfig,
    stream: tokio::net::TcpStream,
    clip_tx: mpsc::UnboundedSender<ClipboardEvent>,
) -> anyhow::Result<(ConnectionResult, UpgradedFramed)> {
    let server_name = config.host.clone();

    let client_addr = stream
        .local_addr()
        .map_err(|e| anyhow::anyhow!("get local address: {e}"))?;

    let mut framed = TokioFramed::new(stream);
    let mut connector = ClientConnector::new(build_connector_config(config), client_addr);
    if config.resize {
        // Negotiate the Display Control Virtual Channel so the session can drive
        // the remote resolution from the browser viewport (client-initiated
        // resize). The capabilities callback is a no-op — `encode_resize` reads
        // the channel state directly once the server answers.
        connector = connector.with_static_channel(
            DrdynvcClient::new()
                .with_dynamic_channel(DisplayControlClient::new(|_caps| Ok(Vec::new()))),
        );
    }
    if config.clipboard {
        // MS-RDPECLIP. Registered only on opt-in, so a target without the flag
        // never even negotiates the channel and the remote cannot advertise a
        // clipboard at us.
        connector = connector.with_static_channel(CliprdrClient::new(Box::new(
            rdp_clipboard::Backend::new(clip_tx),
        )));
    }

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
    /// The target's configured `width`/`height` — the size this session asked the
    /// server for at connect, and so what [`ClientMsg::DefaultSize`] means here.
    default_size: (u16, u16),
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
                            scale: UNSCALED,
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
                // A viewport report is a client-initiated resize. Unlike VNC's
                // automatic resize, the browser only sends this when the user
                // asks for it (the menu's "Resize to window") — reactivation is
                // heavier than VNC's SetDesktopSize. Ignored unless negotiated.
                if let ClientMsg::Viewport { w, h } = input {
                    if resize {
                        resize_desktop(&mut active_stage, &mut framed, desktop, w, h).await?;
                    }
                    continue;
                }
                // The same request without a size on it: the target's configured
                // `width`/`height` is what "default" means here, and it is already
                // the size this session connected at — so this is a no-op unless
                // something moved the desktop off it. See
                // [`ClientMsg::DefaultSize`].
                if matches!(input, ClientMsg::DefaultSize) {
                    if resize {
                        let (w, h) = default_size;
                        resize_desktop(&mut active_stage, &mut framed, desktop, w, h).await?;
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
                    // unprompted exactly as it does for VNC and rxa.
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
                            scale: UNSCALED,
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

/// Request a client-initiated resolution change over the Display Control
/// channel. Sizes are adjusted to the protocol's constraints (even width, 200
/// to 8192 per axis). A no-op if the channel isn't connected yet (the server
/// hasn't sent its capabilities); the browser can simply ask again. The server
/// answers by deactivating the session — see the `DeactivateAll` arm.
///
/// Also a no-op when the desktop is already that size, and that guard earns its
/// place here rather than at the callers: this is the one engine where asking for
/// the size you already have is *expensive*, since the server answers any request
/// with a full Deactivation-Reactivation. VNC and `rxa` both drop an unchanged
/// request themselves, so this is what makes the two client requests idempotent
/// across all three — which matters most for the automatic
/// [`ClientMsg::DefaultSize`] a mobile client sends on every reattach.
///
/// Compared after `adjust_display_size`, because that is the size that would
/// actually be asked for: an odd width lands on the even one beside it, and
/// comparing before the adjustment would call that a change when it is not.
async fn resize_desktop(
    active_stage: &mut ActiveStage,
    framed: &mut UpgradedFramed,
    current: DesktopSize,
    w: u16,
    h: u16,
) -> anyhow::Result<()> {
    let (w, h) = MonitorLayoutEntry::adjust_display_size(u32::from(w), u32::from(h));
    if (w, h) == (u32::from(current.width), u32::from(current.height)) {
        debug!("rdp: the desktop is already {w}x{h}; not asking for a reactivation");
        return Ok(());
    }
    match active_stage.encode_resize(w, h, None, None) {
        Some(Ok(frame)) => {
            info!("rdp: requesting resize to {w}x{h}");
            framed
                .write_all(&frame)
                .await
                .map_err(|e| anyhow::anyhow!("write resize: {e}"))?;
        }
        Some(Err(e)) => warn!("rdp: could not encode resize: {e}"),
        None => debug!("rdp: resize requested before the Display Control channel is ready"),
    }
    Ok(())
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
        ClientMsg::MouseButton { button, pressed } => {
            let mut flags = match button {
                MouseButton::Left => PointerFlags::LEFT_BUTTON,
                MouseButton::Right => PointerFlags::RIGHT_BUTTON,
                MouseButton::Middle => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
            };
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
        ClientMsg::Wheel { dx, dy } => {
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
        // Handled by the active loop (client-initiated resize) before
        // translation, so these arms are unreachable in practice.
        ClientMsg::Viewport { .. } | ClientMsg::DefaultSize => Vec::new(),
        // Handled by the active loop (full repaint) before translation.
        ClientMsg::Refresh => Vec::new(),
        // Handled by the active loop (MS-RDPECLIP, a static virtual channel)
        // before translation.
        ClientMsg::Clipboard { .. } | ClientMsg::ClipboardRequest => Vec::new(),
        // Session-control messages act on the slot, not an engine — the ws
        // bridge handles them and they never reach here. `CacheReset` is one of
        // them: it empties that socket's tile cache and injects its own `Refresh`.
        ClientMsg::Connect { .. } | ClientMsg::Disconnect | ClientMsg::CacheReset => Vec::new(),
        // An RDP session is one framebuffer spanning every monitor the server
        // composed into it, and its protocol has no way to ask for one of them.
        // So this engine never sends a display list, no client offers the
        // picker, and anything arriving here is a client that invented one.
        ClientMsg::SelectDisplay { .. } => Vec::new(),
        // Nothing to act on: an RDP server draws at one density and has no
        // notion of a backing scale to change. Clients send this unconditionally
        // rather than asking what the engine is, so it is ignored here rather
        // than treated as a client error.
        ClientMsg::HostScale { .. } => Vec::new(),
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
/// The pack happens either way; what is skipped is the WebP encode, which is far
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
        enable_audio_playback: false,
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
        let event = one(ClientMsg::Wheel { dx: 0.0, dy: 3.0 }, &mut pos);
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
}
