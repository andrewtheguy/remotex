//! The `rxa` engine: the gateway half of the purpose-built macOS agent
//! protocol (`crates/rxa-proto`, `crates/rxa-agent`,
//! docs/mac-agent-architecture.md).
//!
//! Same seam as [`crate::rdp`] and [`crate::vnc`] — connect, announce the
//! desktop size, then pump tiles out and [`ClientMsg`] input back in — with two
//! differences that are the entire reason this engine exists:
//!
//! **Tiles pass straight through.** The agent encodes each dirty rectangle as
//! PNG or JPEG on the Mac and this engine relays the bytes into
//! [`Tile::encoded`] without decoding a pixel. There is no framebuffer here and
//! no strip loop; the agent already split the work.
//!
//! **A dropped agent link reconnects silently while the browser is live.**
//! Against Apple's Screen Sharing every disconnect meant a fresh credential
//! prompt, because the prompt belonged to Apple's server. Here the PSK lives in
//! the config file, so an established link retries with capped backoff rather
//! than surfacing an error and bouncing the browser back to the picker. The
//! shared session layer ends the engine when the browser stays absent; RXA's
//! own ping/pong detects a half-open agent link and drives reconnection.
//!
//! Silently, but not forever: see [`RECONNECT_GIVE_UP`]. The link that comes back
//! is the one worth hiding, and a Mac that was switched off or had its lid closed
//! never does — retrying it indefinitely left the browser holding a frozen
//! desktop that claimed to be live, which is worse than the picker and an error.
//!
//! An *initial* connect failure is still fatal and reported: a wrong host or a
//! wrong PSK has to be visible immediately, not hidden behind an infinite retry.

use std::time::Duration;

use log::{debug, info, warn};
use rxa_proto::frame::{FrameReader, FrameWriter};
use rxa_proto::msg::{AgentMsg, GatewayMsg};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, timeout};

use crate::config::TargetConfig;
use crate::engine::{self, clamp_u16, host_port};
use crate::protocol::{
    ClientMsg, ClipboardSnapshot, CursorShape, DisplayInfo, MouseButton, ServerMsg, Tile,
    clipboard_fits,
};

/// How long a connect + handshake + `Hello` may take before we give up on this
/// attempt. Guards against a host that accepts the TCP connection and then says
/// nothing, which no socket timeout would catch.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Reconnect backoff bounds for an established session that dropped.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(15);

/// How long an established link may stay down before the browser is told.
///
/// The silent retry exists for the link that comes back — a Wi-Fi roam, a DHCP
/// renewal, an agent restarting after a settings save. A Mac that was switched
/// off, or whose lid was closed, never does, and retrying it forever leaves a
/// frozen desktop on screen claiming to be live. That is worse than the picker.
///
/// Twice [`BACKOFF_MAX`], so the window always holds at least five attempts
/// (t ≈ 1, 3, 7, 15, 30) including one at the cap, and under the session layer's
/// 60-second reattach grace, so the user learns the reason rather than watching
/// the engine expire for an unrelated one.
///
/// Measured from the moment the link dropped and restarted by a successful
/// connect, which means the agent answered a handshake and sent `Hello`. An agent
/// that does that and dies immediately, over and over, is still retried
/// indefinitely — a flap is not something this bound tries to catch.
const RECONNECT_GIVE_UP: Duration = Duration::from_secs(30);

/// The reconnect policy, as a value so tests can shrink the clock.
///
/// Injected rather than read from the constants directly for the same reason
/// [`crate::ws`] injects its heartbeat timings — and here it is the only option:
/// each engine runs on its own thread with its own current-thread runtime (see
/// [`crate::session`]), so `tokio::time::pause` in a test cannot reach this
/// timer, and a test on the real constants would take half a minute.
#[derive(Clone, Copy)]
struct Retry {
    backoff_min: Duration,
    backoff_max: Duration,
    give_up_after: Duration,
}

const RETRY: Retry = Retry {
    backoff_min: BACKOFF_MIN,
    backoff_max: BACKOFF_MAX,
    give_up_after: RECONNECT_GIVE_UP,
};

/// Application-level keepalive, on top of the socket's own.
///
/// [`crate::engine`] arms `SO_KEEPALIVE` with a roughly 25-second budget, which
/// is what the *kernel* can promise: that the agent's host is still answering.
/// This ping is both faster and different in kind — it is answered by the agent
/// process, so it also catches a Mac that is reachable while the agent is wedged,
/// which is the failure keepalive is blind to.
///
/// It is also why this engine's own socket keepalive effectively never arms: a
/// ping every five seconds means the connection is never idle, and idle is the
/// only state the kernel probes in.
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// A link with no `Pong` for this long is treated as dead and reconnected.
const PONG_TIMEOUT: Duration = Duration::from_secs(15);

/// Buffer of decoded agent messages between the reader task and the pump.
/// Bounded so a slow browser backpressures through to the agent's TCP window
/// instead of queueing frames here.
const AGENT_BUFFER: usize = 32;

/// Connect to the Mac agent and drive the session until the browser goes away.
///
/// `input_rx` carries browser input; `frame_tx` carries screen updates back.
/// Unlike the other engines this returns only when the *session* ends (the
/// session layer dropped the input channel, or the browser link closed) — a
/// broken agent link is retried, not reported.
pub async fn run(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) {
    run_with(config, RETRY, input_rx, frame_tx).await
}

async fn run_with(
    config: TargetConfig,
    retry: Retry,
    mut input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) {
    // Already validated in `ConfigFile::parse`, so this only fires if a target
    // reached us without going through the config loader.
    let psk = match rxa_proto::psk::parse(&config.psk) {
        Ok(psk) => psk,
        Err(e) => {
            warn!("rxa: unusable psk for target {:?}: {e}", config.name);
            let _ = frame_tx
                .send(ServerMsg::Error {
                    message: format!("rxa target has an invalid psk: {e}"),
                })
                .await;
            return;
        }
    };

    let mut session = match connect(&config, &psk).await {
        Ok(session) => session,
        Err(e) => {
            warn!("rxa: connect failed: {e:#}");
            let _ = frame_tx
                .send(ServerMsg::Error {
                    message: format!("rxa connect failed: {e}"),
                })
                .await;
            return;
        }
    };

    let caps = Caps {
        clipboard: config.clipboard,
        resize: config.resize,
    };
    let mut backoff = retry.backoff_min;
    // What the browser has been told about the desktop, carried across
    // reconnects: a `Resize` costs the frontend its canvas contents, so a silent
    // reconnect to an unchanged display must not announce one.
    let mut announced: Option<Announced> = None;
    // The agent supplies the activity time. Keep the last snapshot across a
    // silent agent reconnect so an otherwise-identical Fetch does not lose a
    // timestamp merely because the transport link changed underneath it.
    let mut clipboard_snapshot: Option<ClipboardSnapshot> = None;
    // Likewise the display list. A reconnecting agent re-sends it beside its
    // `Hello`, but a browser attaching in the gap between the two would
    // otherwise find no menu at all.
    let mut displays: Option<(u32, Vec<DisplayInfo>)> = None;
    loop {
        let size = (session.width, session.height);
        info!(
            "rxa: session up, desktop {}x{} at {}x",
            size.0, size.1, session.scale
        );
        match pump(
            session,
            &mut input_rx,
            &frame_tx,
            &mut announced,
            &mut displays,
            &mut clipboard_snapshot,
            caps,
        )
        .await
        {
            Ok(()) => {
                info!("rxa: session ended");
                return;
            }
            // The link broke. Everything below is the silent-reconnect path.
            Err(e) => warn!("rxa: link lost: {e:#}"),
        }

        // Restarted per outage, which is what makes a successful reconnect reset
        // the window — see [`RECONNECT_GIVE_UP`].
        let lost_at = Instant::now();
        session = loop {
            if !idle(backoff, &mut input_rx, &frame_tx).await {
                info!("rxa: session ended while reconnecting");
                return;
            }
            match connect(&config, &psk).await {
                Ok(session) => break session,
                Err(e) => {
                    debug!("rxa: reconnect failed, retrying: {e:#}");
                    // Checked after the attempt, so a link that is down for less
                    // than the window always gets at least one try at coming back.
                    if lost_at.elapsed() >= retry.give_up_after {
                        warn!(
                            "rxa: giving up on the agent link after {}s",
                            lost_at.elapsed().as_secs()
                        );
                        let _ = frame_tx
                            .send(ServerMsg::Error {
                                message: format!(
                                    "the Mac agent did not come back within {}s",
                                    retry.give_up_after.as_secs()
                                ),
                            })
                            .await;
                        // Returning drops `frame_tx`, and the session layer turns
                        // that into a `Picker` for the browser (see
                        // [`crate::session`]).
                        return;
                    }
                    backoff = (backoff * 2).min(retry.backoff_max);
                }
            }
        };
        backoff = retry.backoff_min;
    }
}

/// The desktop geometry as the browser last heard it: pixel size and the density
/// those pixels are drawn at. The density belongs here because it changes on its
/// own — the same panel has HiDPI and 1x modes — and an unannounced change would
/// leave the desktop presented at half or twice its size.
type Announced = (u16, u16, f32);

/// What the target's profile opted this session into, as one value.
///
/// One struct rather than two bare `bool` parameters, because both are `bool` and
/// they would sit next to each other: transposing them at a call site compiles,
/// and the transposition writes the Mac's pasteboard for a target that never
/// opted into the clipboard bridge. Named fields make that a type error.
#[derive(Clone, Copy)]
struct Caps {
    /// `clipboard` on the target: the pasteboard bridge in both directions, and
    /// the watcher this engine enables per attach.
    clipboard: bool,
    /// `resize` on the target: whether a client may ask the agent to change the
    /// size of the display it is sharing. Only the operator's half of that — the
    /// agent refuses any display it did not make itself.
    resize: bool,
}

/// An established, handshaken link to the agent.
struct Session {
    reader: FrameReader<OwnedReadHalf>,
    writer: FrameWriter<OwnedWriteHalf>,
    width: u16,
    height: u16,
    /// The shared display's backing scale: 2.0 for a Retina Mac, whose
    /// framebuffer is twice the desktop it shows. Clients divide by it, and it
    /// travels with every size the agent reports.
    scale: f32,
}

/// TCP connect → Noise handshake → read the agent's `Hello`.
async fn connect(config: &TargetConfig, psk: &[u8; 32]) -> anyhow::Result<Session> {
    let dest = host_port(&config.host, config.port);
    timeout(CONNECT_TIMEOUT, async {
        let mut stream = engine::tcp_connect(&dest).await?;

        let transport = rxa_proto::noise::initiate(&mut stream, psk)
            .await
            .map_err(|e| anyhow::anyhow!("handshake with {dest}: {e}"))?;
        let (read_half, write_half) = stream.into_split();
        let (mut reader, writer) = rxa_proto::frame::split(read_half, write_half, transport);

        // `Hello` is the agent's first frame; anything else means we are not
        // talking to an agent that agrees with us about the protocol.
        let (width, height, scale) = match AgentMsg::decode(&reader.recv().await?)? {
            AgentMsg::Hello {
                version,
                agent_version,
                w,
                h,
                scale,
            } => {
                anyhow::ensure!(
                    version == rxa_proto::VERSION,
                    "agent speaks rxa version {version}, this build speaks {}",
                    rxa_proto::VERSION
                );
                let scale = rxa_proto::msg::scale_ratio(scale);
                info!("rxa: agent {agent_version} at {dest}, screen {w}x{h} at {scale}x");
                (w, h, scale)
            }
            // Most likely a missing Screen Recording grant — say what the agent
            // said rather than "unexpected message".
            AgentMsg::Error { message } => anyhow::bail!("agent reported: {message}"),
            other => anyhow::bail!("expected Hello from the agent, got {other:?}"),
        };
        Ok(Session {
            reader,
            writer,
            width,
            height,
            scale,
        })
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out connecting to {dest}"))?
}

/// Drive one established link.
///
/// `Ok(())` means the *session* is over and there is nothing to reconnect for;
/// `Err` means this link broke and the caller should reconnect.
async fn pump(
    session: Session,
    input_rx: &mut mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: &mpsc::Sender<ServerMsg>,
    announced: &mut Option<Announced>,
    // The agent's last display list, held across reconnects for the same reason
    // `announced` is: a browser that attaches mid-session has missed it, and it
    // is the whole of what the client's display menu knows.
    displays: &mut Option<(u32, Vec<DisplayInfo>)>,
    clipboard_snapshot: &mut Option<ClipboardSnapshot>,
    caps: Caps,
) -> anyhow::Result<()> {
    let Session {
        reader,
        mut writer,
        mut width,
        mut height,
        mut scale,
    } = session;

    // Nothing to discover: the agent only builds for macOS, so reaching one at
    // all settles it.
    if frame_tx
        .send(ServerMsg::RemoteOs { macos: true })
        .await
        .is_err()
    {
        return Ok(()); // browser link already gone
    }

    // On the initial connect, and afterwards only when the Mac came back a
    // different size. Unchanged, the browser keeps the canvas it already has and
    // the reconnect stays invisible beyond a pause in frames.
    if *announced != Some((width, height, scale)) {
        if frame_tx
            .send(ServerMsg::Resize {
                w: width,
                h: height,
                scale,
            })
            .await
            .is_err()
        {
            return Ok(()); // browser link already gone
        }
        *announced = Some((width, height, scale));
    }

    // `FrameReader::recv` is not cancel-safe, so it cannot live in the
    // `select!` below — it gets its own task and forwards decoded messages.
    let (agent_tx, mut agent_rx) = mpsc::channel(AGENT_BUFFER);
    let read_task = tokio::spawn(read_loop(reader, agent_tx));

    // Guarantees the reader task is joined however this function exits — an
    // orphan would hold the socket open across a reconnect.
    let _abort = AbortOnDrop(read_task);

    writer.send(&GatewayMsg::Attach.encode()).await?;

    // Sent per attach, not once per process: this runs again after every
    // reconnect, and the agent's watch state died with the old session. Only
    // for an opted-in target — the agent reads nothing unprompted otherwise.
    if caps.clipboard {
        writer
            .send(&GatewayMsg::ClipboardWatch { enabled: true }.encode())
            .await?;
    }

    let mut ping = interval(PING_INTERVAL);
    // A blocked browser must not cause a burst of catch-up pings.
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut nonce: u64 = 0;
    let mut last_seen = Instant::now();

    loop {
        tokio::select! {
            agent = agent_rx.recv() => {
                let Some(msg) = agent else {
                    anyhow::bail!("agent link closed");
                };
                last_seen = Instant::now();
                match msg {
                    AgentMsg::Tile { format, x, y, w, h, data } => {
                        let tile = Tile::encoded(format, x, y, w, h, data);
                        if frame_tx.send(ServerMsg::Tile(tile)).await.is_err() {
                            return Ok(());
                        }
                    }
                    AgentMsg::Cursor(shape) => {
                        let shape = shape.map(|c| CursorShape {
                            w: c.w,
                            h: c.h,
                            hx: c.hx,
                            hy: c.hy,
                            png: c.png,
                        });
                        if frame_tx.send(ServerMsg::Cursor(shape)).await.is_err() {
                            return Ok(());
                        }
                    }
                    AgentMsg::DisplaySize { w, h, scale: reported } => {
                        scale = rxa_proto::msg::scale_ratio(reported);
                        info!("rxa: display reconfigured to {w}x{h} at {scale}x");
                        (width, height) = (w, h);
                        *announced = Some((w, h, scale));
                        if frame_tx.send(ServerMsg::Resize { w, h, scale }).await.is_err() {
                            return Ok(());
                        }
                    }
                    // A second Hello on a live link (the agent restarted its
                    // stream) carries the current size; treat it as a resize.
                    AgentMsg::Hello { w, h, scale: reported, .. } => {
                        scale = rxa_proto::msg::scale_ratio(reported);
                        (width, height) = (w, h);
                        *announced = Some((w, h, scale));
                        if frame_tx.send(ServerMsg::Resize { w, h, scale }).await.is_err() {
                            return Ok(());
                        }
                    }
                    // Relayed as it arrives, and kept: a browser that attaches
                    // later gets it from the cache below rather than waiting for
                    // the agent's next change.
                    AgentMsg::Displays { active, displays: reported } => {
                        let reported: Vec<DisplayInfo> = reported
                            .into_iter()
                            .map(|display| DisplayInfo {
                                id: display.id,
                                main: display.is_main(),
                                virtual_display: display.is_owned(),
                                label: display.label,
                                detail: display.detail,
                            })
                            .collect();
                        *displays = Some((active, reported.clone()));
                        if frame_tx
                            .send(ServerMsg::Displays { active, displays: reported })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    AgentMsg::Pong { .. } => {}
                    // Either a reply to a ClipboardRequest this pump sent, or
                    // an unprompted push from the agent's pasteboard watcher.
                    // The payload shares one path, but `requested` stays intact
                    // so only watcher pushes drive browser OS-clipboard sync.
                    //
                    // Dropped outright for a target that didn't opt in: this
                    // pump then never asked and never enabled the watch, so
                    // anything arriving is an agent that disagrees with us —
                    // and the browser writes an incoming clipboard into the
                    // real OS clipboard. Same belt-and-braces as VNC's
                    // ServerCutText.
                    AgentMsg::Clipboard {
                        text,
                        changed_at_ms,
                        requested,
                        oversized_bytes,
                    } => {
                        if let Some(bytes) = oversized_bytes {
                            debug!(
                                "rxa: the Mac's pasteboard holds {bytes} bytes, over the {} byte limit",
                                crate::protocol::MAX_CLIPBOARD_BYTES
                            );
                        }
                        let changed_at_ms = changed_at_ms.or_else(|| {
                            clipboard_snapshot
                                .as_ref()
                                .filter(|snapshot| snapshot.text == text)
                                .and_then(|snapshot| snapshot.changed_at_ms)
                        });
                        let snapshot = ClipboardSnapshot {
                            text,
                            changed_at_ms,
                            oversized_bytes,
                        };
                        *clipboard_snapshot = Some(snapshot.clone());
                        if caps.clipboard
                            && frame_tx
                                .send(ServerMsg::Clipboard {
                                    text: snapshot.text,
                                    changed_at_ms: snapshot.changed_at_ms,
                                    requested,
                                    oversized_bytes: snapshot.oversized_bytes,
                                })
                                .await
                                .is_err()
                        {
                            return Ok(());
                        }
                    }
                    // The agent can't proceed — a missing TCC grant, typically.
                    // Reconnecting would just rediscover it, so this is fatal.
                    AgentMsg::Error { message } => {
                        warn!("rxa: agent error: {message}");
                        let _ = frame_tx
                            .send(ServerMsg::Error {
                                message: format!("Mac agent: {message}"),
                            })
                            .await;
                        return Ok(());
                    }
                }
            }

            input = input_rx.recv() => {
                let Some(msg) = input else {
                    return Ok(()); // the session layer tore this engine down
                };
                // A reattaching browser has a blank canvas and no idea what the
                // remote runs: re-announce both before asking the agent to
                // repaint.
                if matches!(msg, ClientMsg::Refresh) {
                    if frame_tx
                        .send(ServerMsg::Resize { w: width, h: height, scale })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    if frame_tx.send(ServerMsg::RemoteOs { macos: true }).await.is_err() {
                        return Ok(());
                    }
                    // And no display menu. Re-sent from here rather than asked
                    // for, because `Refresh` reaches the agent as a repaint and
                    // a repaint is all it should be: the list has not changed
                    // just because a browser came back to look at it.
                    if let Some((active, list)) = displays.clone()
                        && frame_tx
                            .send(ServerMsg::Displays { active, displays: list })
                            .await
                            .is_err()
                    {
                        return Ok(());
                    }
                }
                if let Some(out) = to_agent(&msg, caps, scale) {
                    writer.send(&out.encode()).await?;
                }
            }

            _ = ping.tick() => {
                if last_seen.elapsed() > PONG_TIMEOUT {
                    anyhow::bail!(
                        "no response from the agent for {}s",
                        last_seen.elapsed().as_secs()
                    );
                }
                nonce += 1;
                writer.send(&GatewayMsg::Ping { nonce }.encode()).await?;
            }
        }
    }
}

/// Read frames off the link, decode them, and forward until either end closes.
async fn read_loop(mut reader: FrameReader<OwnedReadHalf>, tx: mpsc::Sender<AgentMsg>) {
    loop {
        let frame = match reader.recv().await {
            Ok(frame) => frame,
            Err(e) => {
                debug!("rxa: read ended: {e}");
                return;
            }
        };
        let msg = match AgentMsg::decode(&frame) {
            Ok(msg) => msg,
            Err(e) => {
                // Undecodable means the two halves disagree about the wire;
                // dropping the link is the honest response.
                warn!("rxa: undecodable message from the agent: {e}");
                return;
            }
        };
        if tx.send(msg).await.is_err() {
            return; // the pump is gone
        }
    }
}

/// Aborts the reader task on drop, so no `pump` exit path leaks it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Translate browser input into an agent message.
///
/// `None` for messages this engine has nothing to send for:
/// `Connect`/`Disconnect` because the session layer handles those and never
/// forwards them, and the clipboard pair and `Viewport` when the target did not
/// opt in — the clients hide those controls then, so this is the belt to that
/// UI's braces.
///
/// `Viewport` used to be dropped unconditionally, and why it no longer is wants
/// stating precisely, because most of the old reason still holds. A Mac's
/// resolution is the Mac's: nobody's physical panel changes because someone
/// connected to it, and there is still no way to ask one to. What *can* be asked
/// for is the size of a display the agent **made** — a display that exists only to
/// be looked at from here, and whose only user is the person asking. So the
/// request goes through for a target whose profile allows it, and the agent
/// refuses it for anything else.
///
/// Two gates, answering different questions, which is why neither is enough
/// alone. `resize` is the operator's — may this target be resized at all — and is
/// checked here because it is this process's fact. "Is the shared display one I
/// made" is the agent's, and is checked there because it is the only place that
/// cannot be stale; this engine deliberately does not add a third from its own
/// cached display list, which is retained across a silent agent reconnect and can
/// therefore outlive the display it describes.
///
/// `w`/`h` arrive as the remote's **pixels** (see [`ClientMsg::Viewport`]) and go
/// out as **points**, divided by the `scale` this engine last announced. That
/// division is exact by construction — it undoes the multiplication the client
/// did against the same number — where dividing by the agent's live density would
/// not be, since a display publishes no mode to read for tens of milliseconds
/// around a density change.
///
/// `SelectDisplay` and `HostScale` pass through too, and the three together are
/// the whole of what a client decides: *which* screen to look at, *how dense* to
/// draw it, and *how large* to make it. The first is about the person looking; the
/// last two apply only to a display that exists for them.
fn to_agent(msg: &ClientMsg, caps: Caps, scale: f32) -> Option<GatewayMsg> {
    Some(match msg {
        ClientMsg::MouseMove { x, y } => GatewayMsg::PointerMove {
            x: clamp_u16(*x),
            y: clamp_u16(*y),
        },
        ClientMsg::MouseButton { button, pressed } => GatewayMsg::PointerButton {
            // DOM `MouseEvent.button` numbering, unchanged — the agent maps it
            // to a `CGMouseButton`.
            button: match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
            },
            pressed: *pressed,
        },
        // Raw DOM deltas; the agent owns the sign and unit conversion, because
        // only it can verify the direction against a real trackpad.
        ClientMsg::Wheel { dx, dy } => GatewayMsg::Wheel { dx: *dx, dy: *dy },
        ClientMsg::Key {
            code,
            pressed,
            caps,
        } => GatewayMsg::Key {
            code: code.clone(),
            pressed: *pressed,
            caps: *caps,
        },
        ClientMsg::Refresh => GatewayMsg::Refresh,
        ClientMsg::SelectDisplay { id } => GatewayMsg::SelectDisplay { id: *id },
        // Forwarded whatever display is being shared: only the agent knows
        // whether the current one is a display it made, and only it can act.
        ClientMsg::HostScale { scale } => GatewayMsg::HostScale { scale: *scale },
        // Pixels in, points out. `scale` is guarded rather than trusted: it comes
        // from `scale_ratio`, which already refuses nonsense, but a zero here
        // would divide a window into infinity and the fallback that matters is
        // "assume the two units agree", which is what every 1x target is anyway.
        ClientMsg::Viewport { w, h } if caps.resize => {
            let scale = if scale > 0.0 { scale } else { 1.0 };
            let points = |px: u16| clamp_u16((f32::from(px) / scale).round() as i32);
            GatewayMsg::ResizeDisplay {
                w: points(*w),
                h: points(*h),
            }
        }
        // The agent reads its pasteboard only when asked, so a fetch is a real
        // round trip rather than a cached value (unlike VNC, where the server
        // pushes and the engine caches).
        ClientMsg::ClipboardRequest if caps.clipboard => GatewayMsg::ClipboardRequest,
        // Refused rather than truncated, so the Mac's pasteboard keeps what it
        // had instead of gaining a partial copy that looks whole. The browser
        // and the viewer both refuse this themselves and say why; reaching here
        // means one of them let it through.
        ClientMsg::Clipboard { text } if caps.clipboard && !clipboard_fits(text) => {
            warn!(
                "rxa: refusing {} bytes to the Mac's pasteboard, over the {} byte limit",
                text.len(),
                crate::protocol::MAX_CLIPBOARD_BYTES
            );
            return None;
        }
        ClientMsg::Clipboard { text } if caps.clipboard => GatewayMsg::Clipboard {
            text: text.clone(),
        },
        // The opted-out cases of the two gates above, plus the pair the session
        // layer never forwards. A `Viewport` reaching here is a target whose
        // profile says no, which is also the only state either client offers the
        // button in — so this is the belt, not the braces.
        ClientMsg::ClipboardRequest
        | ClientMsg::Clipboard { .. }
        | ClientMsg::Viewport { .. }
        | ClientMsg::Connect { .. }
        | ClientMsg::Disconnect => {
            return None;
        }
    })
}

/// Wait out a reconnect backoff, discarding input that arrives meanwhile.
///
/// Returns `false` when there is no longer anything to reconnect *for* — the
/// session layer dropped the input channel or output receiver. Input
/// buffered during an outage is deliberately thrown away rather than replayed:
/// a mouse position from eight seconds ago is worse than no event at all, and
/// an undrained unbounded channel would grow for as long as the outage lasts.
async fn idle(
    backoff: Duration,
    input_rx: &mut mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> bool {
    let sleep = tokio::time::sleep(backoff);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return !frame_tx.is_closed(),
            input = input_rx.recv() => {
                if input.is_none() {
                    return false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy with the same shape as [`RETRY`] and a clock a test can wait on.
    ///
    /// The real one gives up after 30 seconds; nothing here should take longer
    /// than a blink. See [`Retry`] for why this is injected rather than paused.
    const FAST_RETRY: Retry = Retry {
        backoff_min: Duration::from_millis(10),
        backoff_max: Duration::from_millis(20),
        give_up_after: Duration::from_millis(100),
    };

    fn rxa_target(port: u16, psk: &str) -> TargetConfig {
        TargetConfig {
            name: "mac".to_owned(),
            protocol: crate::config::Protocol::Rxa,
            subtype: None,
            host: "127.0.0.1".to_owned(),
            port,
            username: String::new(),
            password: String::new(),
            vnc_password: String::new(),
            domain: None,
            width: 1,
            height: 1,
            security: crate::config::Security::Auto,
            resize: false,
            clipboard: false,
            psk: psk.to_owned(),
        }
    }

    /// Serve one link — handshake, `Hello`, one paint — then hang up.
    ///
    /// The listener is returned so the caller decides whether the agent ever
    /// comes back: dropping it is a Mac that was switched off, keeping it is one
    /// whose link merely blipped.
    async fn serve_one_link(listener: &tokio::net::TcpListener, psk: [u8; 32]) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let transport = rxa_proto::noise::respond(&mut stream, &psk).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        let (mut reader, mut writer) = rxa_proto::frame::split(read_half, write_half, transport);
        writer
            .send(
                &AgentMsg::Hello {
                    version: rxa_proto::VERSION,
                    agent_version: "fake-agent".to_owned(),
                    w: 800,
                    h: 600,
                    scale: rxa_proto::msg::SCALE_ONE,
                }
                .encode(),
            )
            .await
            .unwrap();
        // The gateway asks for the stream before any pixels flow.
        match GatewayMsg::decode(&reader.recv().await.unwrap()).unwrap() {
            GatewayMsg::Attach => {}
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    // The bug this bound exists for: a Mac that is switched off was retried
    // forever while the browser held a frozen desktop that claimed to be live.
    #[tokio::test]
    async fn an_agent_that_never_comes_back_is_reported_instead_of_retried_forever() {
        let psk_text = rxa_proto::psk::generate();
        let psk = rxa_proto::psk::parse(&psk_text).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let agent = tokio::spawn(async move {
            serve_one_link(&listener, psk).await;
            // Everything goes, listener included: nothing will answer again.
        });

        let (_input_tx, input_rx) = mpsc::unbounded_channel();
        let (frame_tx, mut frame_rx) = mpsc::channel(16);
        run_with(rxa_target(port, &psk_text), FAST_RETRY, input_rx, frame_tx).await;
        agent.await.unwrap();

        // `run_with` returning at all is half the point — that is what drops
        // `frame_tx` and lands the browser on the picker.
        let mut reported = None;
        while let Ok(msg) = frame_rx.try_recv() {
            if let ServerMsg::Error { message } = msg {
                reported = Some(message);
            }
        }
        let message = reported.expect("the browser was told nothing");
        assert!(message.contains("did not come back"), "{message}");
    }

    // And the behaviour that must survive it: a link that comes back is still a
    // non-event, which is the whole reason the silent retry exists.
    #[tokio::test]
    async fn a_reconnect_that_succeeds_restarts_the_give_up_window() {
        let psk_text = rxa_proto::psk::generate();
        let psk = rxa_proto::psk::parse(&psk_text).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let agent = tokio::spawn(async move {
            // Hang up twice, each time well inside the window, then stop
            // answering. A bound measured from the *first* outage rather than
            // per-outage would have given up during this.
            for _ in 0..3 {
                serve_one_link(&listener, psk).await;
            }
        });

        let (_input_tx, input_rx) = mpsc::unbounded_channel();
        let (frame_tx, mut frame_rx) = mpsc::channel(16);
        run_with(rxa_target(port, &psk_text), FAST_RETRY, input_rx, frame_tx).await;
        agent.await.unwrap();

        // It still ends up reporting — the agent stops answering eventually —
        // but only after all three links were served, which is what says the
        // window restarted rather than running from the first drop.
        let mut errors = 0;
        while let Ok(msg) = frame_rx.try_recv() {
            if matches!(msg, ServerMsg::Error { .. }) {
                errors += 1;
            }
        }
        assert_eq!(errors, 1, "exactly one report, after the last link");
    }

    /// A target that opted into nothing, which is the default and what most of
    /// the translation below is indifferent to.
    const NO_CAPS: Caps = Caps {
        clipboard: false,
        resize: false,
    };
    const CLIPBOARD: Caps = Caps {
        clipboard: true,
        resize: false,
    };
    const RESIZE: Caps = Caps {
        clipboard: false,
        resize: true,
    };
    /// The density in every case where the conversion is not what is under test.
    /// A 1x remote is the one scale at which viewport pixels and display points
    /// are the same number, so it never hides an arithmetic mistake behind an
    /// identity — which is why the conversion has tests of its own.
    const UNSCALED: f32 = 1.0;

    #[test]
    fn pointer_moves_carry_clamped_framebuffer_coordinates() {
        assert_eq!(
            to_agent(&ClientMsg::MouseMove { x: 1279, y: 799 }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::PointerMove { x: 1279, y: 799 })
        );
        // A drag off the canvas edge pins to the edge instead of vanishing.
        assert_eq!(
            to_agent(&ClientMsg::MouseMove { x: -5, y: 70_000 }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::PointerMove { x: 0, y: u16::MAX })
        );
    }

    #[test]
    fn buttons_keep_the_dom_numbering() {
        for (button, expected) in [
            (MouseButton::Left, 0),
            (MouseButton::Middle, 1),
            (MouseButton::Right, 2),
        ] {
            assert_eq!(
                to_agent(&ClientMsg::MouseButton {
                    button,
                    pressed: true
                }, NO_CAPS, UNSCALED),
                Some(GatewayMsg::PointerButton {
                    button: expected,
                    pressed: true
                })
            );
        }
        assert_eq!(
            to_agent(&ClientMsg::MouseButton {
                button: MouseButton::Left,
                pressed: false
            }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::PointerButton {
                button: 0,
                pressed: false
            })
        );
    }

    // Wheel deltas pass through untouched: the agent owns sign and units, so
    // the gateway must not "helpfully" normalize them.
    #[test]
    fn wheel_deltas_pass_through_unmodified() {
        assert_eq!(
            to_agent(&ClientMsg::Wheel { dx: 0.0, dy: -2.5 }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::Wheel { dx: 0.0, dy: -2.5 })
        );
    }

    #[test]
    fn keys_carry_the_dom_code_and_the_caps_flag_verbatim() {
        assert_eq!(
            to_agent(&ClientMsg::Key {
                code: "KeyA".to_owned(),
                pressed: true,
                caps: true,
            }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::Key {
                code: "KeyA".to_owned(),
                pressed: true,
                caps: true,
            })
        );
        // Codes this build has no macOS keycode for are still forwarded — the
        // table lives in rxa-proto and the agent is the one that consults it.
        assert_eq!(
            to_agent(&ClientMsg::Key {
                code: "MediaPlayPause".to_owned(),
                pressed: false,
                caps: false,
            }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::Key {
                code: "MediaPlayPause".to_owned(),
                pressed: false,
                caps: false,
            })
        );
    }

    // The client's screen density reaches the agent whatever display is being
    // shared: only the agent knows whether the current one is a display it made,
    // and the gateway holding an opinion here would be a second place for the
    // answer to be wrong.
    #[test]
    fn the_hosts_density_reaches_the_agent() {
        assert_eq!(
            to_agent(&ClientMsg::HostScale { scale: 200 }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::HostScale { scale: 200 })
        );
        assert_eq!(
            to_agent(&ClientMsg::HostScale { scale: 100 }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::HostScale { scale: 100 })
        );
    }

    #[test]
    fn refresh_asks_the_agent_for_a_full_repaint() {
        assert_eq!(to_agent(&ClientMsg::Refresh, NO_CAPS, UNSCALED), Some(GatewayMsg::Refresh));
    }

    // The session layer never forwards Connect/Disconnect, and a viewport from a
    // target that did not opt in is dropped rather than acted on — the opted-in
    // case is the test below this one.
    #[test]
    fn messages_with_no_agent_equivalent_are_dropped() {
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 2560, h: 1440 }, NO_CAPS, UNSCALED),
            None
        );
        assert_eq!(
            to_agent(
                &ClientMsg::Connect {
                    target: "mac".to_owned()
                },
                NO_CAPS,
                UNSCALED
            ),
            None
        );
        assert_eq!(to_agent(&ClientMsg::Disconnect, NO_CAPS, UNSCALED), None);
    }

    // A viewport is remote *pixels* and a display mode is *points*, so the one
    // thing this translation must get right is the division — by the scale this
    // engine last announced, which is the number the client multiplied its window
    // by. Getting it wrong is invisible on a 1x target and doubles or halves the
    // Mac's desktop on a Retina one.
    #[test]
    fn a_viewport_becomes_a_resize_in_display_points() {
        // A 1440x900-point window on a 2x display: the client sent the pixels its
        // canvas is laid out in, and the Mac must be asked for the points.
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 2880, h: 1800 }, RESIZE, 2.0),
            Some(GatewayMsg::ResizeDisplay { w: 1440, h: 900 })
        );
        // At 1x the two units coincide and nothing is scaled away.
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 1280, h: 800 }, RESIZE, UNSCALED),
            Some(GatewayMsg::ResizeDisplay { w: 1280, h: 800 })
        );
        // A fractional ratio rounds rather than truncating, so a window is never
        // asked for one point less than it has.
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 1281, h: 801 }, RESIZE, 1.5),
            Some(GatewayMsg::ResizeDisplay { w: 854, h: 534 })
        );
        // A scale that never happened. `scale_ratio` already refuses nonsense, so
        // reaching here means something upstream broke — and dividing by it would
        // be an infinity, where assuming the units agree is merely wrong.
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 1280, h: 800 }, RESIZE, 0.0),
            Some(GatewayMsg::ResizeDisplay { w: 1280, h: 800 })
        );
    }

    // The two flags gate different messages, and this is the test that catches
    // them being swapped at a call site — which, while they were two bare bools
    // sitting next to each other, compiled.
    #[test]
    fn the_two_capability_flags_gate_different_messages() {
        assert_eq!(to_agent(&ClientMsg::ClipboardRequest, RESIZE, UNSCALED), None);
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 1280, h: 800 }, CLIPBOARD, UNSCALED),
            None
        );
    }

    // The clipboard pair is the only thing the flag gates, and it gates both
    // directions: a target that didn't opt in neither reads nor writes the
    // Mac's pasteboard, whatever the browser sends.
    #[test]
    fn clipboard_messages_reach_the_agent_only_when_the_target_opted_in() {
        assert_eq!(
            to_agent(&ClientMsg::ClipboardRequest, CLIPBOARD, UNSCALED),
            Some(GatewayMsg::ClipboardRequest)
        );
        assert_eq!(
            to_agent(
                &ClientMsg::Clipboard {
                    text: "copied — 画面".to_owned()
                },
                CLIPBOARD,
                UNSCALED
            ),
            Some(GatewayMsg::Clipboard {
                text: "copied — 画面".to_owned()
            })
        );

        assert_eq!(
            to_agent(&ClientMsg::ClipboardRequest, NO_CAPS, UNSCALED),
            None
        );
        assert_eq!(
            to_agent(
                &ClientMsg::Clipboard {
                    text: "copied".to_owned()
                },
                NO_CAPS,
                UNSCALED
            ),
            None
        );
    }

    // An oversized paste is dropped at the gateway rather than handed to the
    // agent truncated: the Mac keeps the pasteboard it had, instead of gaining a
    // partial copy that looks like the whole thing.
    #[test]
    fn oversized_clipboard_text_never_reaches_the_agent() {
        // Two bytes per char, so this is twice the ceiling.
        let text = "é".repeat(crate::protocol::MAX_CLIPBOARD_BYTES);
        assert_eq!(to_agent(&ClientMsg::Clipboard { text }, CLIPBOARD, UNSCALED), None);

        // At the ceiling it goes through untouched, so the boundary is
        // inclusive and nothing is rewritten on the way past.
        let text = "a".repeat(crate::protocol::MAX_CLIPBOARD_BYTES);
        match to_agent(
            &ClientMsg::Clipboard {
                text: text.clone(),
            },
            CLIPBOARD,
            UNSCALED,
        ) {
            Some(GatewayMsg::Clipboard { text: sent }) => assert_eq!(sent, text),
            other => panic!("expected a clipboard message, got {other:?}"),
        }
    }

    #[test]
    fn backoff_climbs_to_the_cap_and_stops_there() {
        let mut backoff = BACKOFF_MIN;
        let mut steps = 0;
        while backoff < BACKOFF_MAX {
            backoff = (backoff * 2).min(BACKOFF_MAX);
            steps += 1;
            assert!(steps < 10, "backoff should reach the cap quickly");
        }
        assert_eq!(backoff, BACKOFF_MAX);
        // The cap is a fixed point, so the retry loop never runs away.
        assert_eq!((backoff * 2).min(BACKOFF_MAX), BACKOFF_MAX);
    }

    #[tokio::test]
    async fn idle_discards_input_during_an_outage_and_still_returns() {
        tokio::time::pause();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (frame_tx, _frame_rx) = mpsc::channel(4);
        // A browser dragging the mouse through the whole outage.
        for x in 0..100 {
            input_tx.send(ClientMsg::MouseMove { x, y: x }).unwrap();
        }
        assert!(idle(BACKOFF_MIN, &mut input_rx, &frame_tx).await);
        // Every stale event was drained rather than left to replay.
        assert!(input_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn idle_gives_up_when_the_session_layer_drops_the_engine() {
        tokio::time::pause();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<ClientMsg>();
        let (frame_tx, _frame_rx) = mpsc::channel(4);
        drop(input_tx);
        assert!(!idle(BACKOFF_MIN, &mut input_rx, &frame_tx).await);
    }

    #[tokio::test]
    async fn idle_gives_up_when_the_browser_link_is_gone() {
        tokio::time::pause();
        let (_input_tx, mut input_rx) = mpsc::unbounded_channel::<ClientMsg>();
        let (frame_tx, frame_rx) = mpsc::channel::<ServerMsg>(4);
        drop(frame_rx);
        assert!(!idle(BACKOFF_MIN, &mut input_rx, &frame_tx).await);
    }
}
