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
//! An *initial* connect failure is still fatal and reported: a wrong host or a
//! wrong PSK has to be visible immediately, not hidden behind an infinite retry.

use std::time::Duration;

use log::{debug, info, warn};
use rxa_proto::frame::{FrameReader, FrameWriter};
use rxa_proto::msg::{AgentMsg, GatewayMsg};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, timeout};

use crate::config::TargetConfig;
use crate::engine::{clamp_u16, host_port};
use crate::protocol::{
    ClientMsg, ClipboardSnapshot, CursorShape, MouseButton, ServerMsg, Tile, clipboard_fits,
};

/// How long a connect + handshake + `Hello` may take before we give up on this
/// attempt. Guards against a host that accepts the TCP connection and then says
/// nothing, which no socket timeout would catch.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Reconnect backoff bounds for an established session that dropped.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(15);

/// Application-level keepalive. `SO_KEEPALIVE` would take minutes to notice a
/// half-open TCP connection — a Wi-Fi drop where no FIN ever arrives — which is
/// exactly the failure this project is about.
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

    let mut backoff = BACKOFF_MIN;
    // The size the browser has been told about, carried across reconnects: a
    // `Resize` costs the frontend its canvas contents, so a silent reconnect to
    // an unchanged desktop must not announce one.
    let mut announced: Option<(u16, u16)> = None;
    // The agent supplies the activity time. Keep the last snapshot across a
    // silent agent reconnect so an otherwise-identical Fetch does not lose a
    // timestamp merely because the transport link changed underneath it.
    let mut clipboard_snapshot: Option<ClipboardSnapshot> = None;
    loop {
        let size = (session.width, session.height);
        info!("rxa: session up, desktop {}x{}", size.0, size.1);
        match pump(
            session,
            &mut input_rx,
            &frame_tx,
            &mut announced,
            &mut clipboard_snapshot,
            config.clipboard,
            config.resize,
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

        session = loop {
            if !idle(backoff, &mut input_rx, &frame_tx).await {
                info!("rxa: session ended while reconnecting");
                return;
            }
            match connect(&config, &psk).await {
                Ok(session) => break session,
                Err(e) => {
                    debug!("rxa: reconnect failed, retrying: {e:#}");
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        };
        backoff = BACKOFF_MIN;
    }
}

/// An established, handshaken link to the agent.
struct Session {
    reader: FrameReader<OwnedReadHalf>,
    writer: FrameWriter<OwnedWriteHalf>,
    width: u16,
    height: u16,
    /// Whether the Mac will change its display resolution on request — true
    /// only for a virtual display, which is the agent's call, not ours.
    resizable: bool,
}

/// TCP connect → Noise handshake → read the agent's `Hello`.
async fn connect(config: &TargetConfig, psk: &[u8; 32]) -> anyhow::Result<Session> {
    let dest = host_port(&config.host, config.port);
    timeout(CONNECT_TIMEOUT, async {
        let mut stream = TcpStream::connect(&dest)
            .await
            .map_err(|e| anyhow::anyhow!("TCP connect to {dest}: {e}"))?;
        // Input events are tiny and latency-critical; never coalesce them.
        stream.set_nodelay(true).ok();

        let transport = rxa_proto::noise::initiate(&mut stream, psk)
            .await
            .map_err(|e| anyhow::anyhow!("handshake with {dest}: {e}"))?;
        let (read_half, write_half) = stream.into_split();
        let (mut reader, writer) = rxa_proto::frame::split(read_half, write_half, transport);

        // `Hello` is the agent's first frame; anything else means we are not
        // talking to an agent that agrees with us about the protocol.
        let (width, height, resizable) = match AgentMsg::decode(&reader.recv().await?)? {
            AgentMsg::Hello {
                version,
                agent_version,
                w,
                h,
                resizable,
            } => {
                anyhow::ensure!(
                    version == rxa_proto::VERSION,
                    "agent speaks rxa version {version}, this build speaks {}",
                    rxa_proto::VERSION
                );
                info!(
                    "rxa: agent {agent_version} at {dest}, screen {w}x{h}, \
                     resizable={resizable}"
                );
                (w, h, resizable)
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
            resizable,
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
    announced: &mut Option<(u16, u16)>,
    clipboard_snapshot: &mut Option<ClipboardSnapshot>,
    clipboard: bool,
    resize: bool,
) -> anyhow::Result<()> {
    let Session {
        reader,
        mut writer,
        mut width,
        mut height,
        resizable,
    } = session;

    // Both halves have to agree before the browser is offered a resolution
    // menu: the target opted in (`resize = true`) and the Mac says its display
    // can take it. Either one alone would produce a control that does nothing.
    let resize = resize && resizable;
    // The last menu sent to the browser, so a reattaching browser can be given
    // one without waiting for the next reconfigure — the same reason `announced`
    // exists for the desktop size.
    let mut modes: Vec<(u16, u16)> = Vec::new();

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
    if *announced != Some((width, height)) {
        if frame_tx
            .send(ServerMsg::Resize { w: width, h: height })
            .await
            .is_err()
        {
            return Ok(()); // browser link already gone
        }
        *announced = Some((width, height));
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
    if clipboard {
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
                    AgentMsg::DisplaySize { w, h } => {
                        info!("rxa: display reconfigured to {w}x{h}");
                        (width, height) = (w, h);
                        *announced = Some((w, h));
                        if frame_tx.send(ServerMsg::Resize { w, h }).await.is_err() {
                            return Ok(());
                        }
                    }
                    // Dropped for a target that didn't opt in, or an agent that
                    // said it was not resizable: offering a menu the engine will
                    // then refuse to act on is worse than offering none.
                    AgentMsg::DisplayModes { modes: offered } => {
                        if resize {
                            modes = offered.clone();
                            if frame_tx
                                .send(ServerMsg::DisplayModes { modes: offered })
                                .await
                                .is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                    // A second Hello on a live link (the agent restarted its
                    // stream) carries the current size; treat it as a resize.
                    AgentMsg::Hello { w, h, .. } => {
                        (width, height) = (w, h);
                        *announced = Some((w, h));
                        if frame_tx.send(ServerMsg::Resize { w, h }).await.is_err() {
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
                        if clipboard
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
                // A reattaching browser has a blank canvas, an empty menu and
                // no idea what the remote runs: re-announce all three before
                // asking the agent to repaint.
                if matches!(msg, ClientMsg::Refresh) {
                    if frame_tx.send(ServerMsg::Resize { w: width, h: height }).await.is_err() {
                        return Ok(());
                    }
                    if frame_tx.send(ServerMsg::RemoteOs { macos: true }).await.is_err() {
                        return Ok(());
                    }
                    if !modes.is_empty()
                        && frame_tx
                            .send(ServerMsg::DisplayModes { modes: modes.clone() })
                            .await
                            .is_err()
                    {
                        return Ok(());
                    }
                }
                if let Some(out) = to_agent(&msg, clipboard, resize) {
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
/// [`ClientMsg::Viewport`] because a Mac's display takes sizes off a fixed list
/// rather than following a viewport (the browser picks one with
/// [`ClientMsg::SetResolution`] instead), `Connect`/`Disconnect` because the
/// session layer handles those and never forwards them, and the clipboard pair
/// and `SetResolution` when the target did not opt in — the browser hides those
/// controls then, so this is the belt to that UI's braces.
fn to_agent(msg: &ClientMsg, clipboard: bool, resize: bool) -> Option<GatewayMsg> {
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
        // The agent reads its pasteboard only when asked, so a fetch is a real
        // round trip rather than a cached value (unlike VNC, where the server
        // pushes and the engine caches).
        ClientMsg::ClipboardRequest if clipboard => GatewayMsg::ClipboardRequest,
        // Refused rather than truncated, so the Mac's pasteboard keeps what it
        // had instead of gaining a partial copy that looks whole. The browser
        // and the viewer both refuse this themselves and say why; reaching here
        // means one of them let it through.
        ClientMsg::Clipboard { text } if clipboard && !clipboard_fits(text) => {
            warn!(
                "rxa: refusing {} bytes to the Mac's pasteboard, over the {} byte limit",
                text.len(),
                crate::protocol::MAX_CLIPBOARD_BYTES
            );
            return None;
        }
        ClientMsg::Clipboard { text } if clipboard => GatewayMsg::Clipboard {
            text: text.clone(),
        },
        ClientMsg::SetResolution { w, h } if resize => GatewayMsg::SetDisplaySize { w: *w, h: *h },
        ClientMsg::ClipboardRequest
        | ClientMsg::Clipboard { .. }
        | ClientMsg::SetResolution { .. }
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

    #[test]
    fn pointer_moves_carry_clamped_framebuffer_coordinates() {
        assert_eq!(
            to_agent(&ClientMsg::MouseMove { x: 1279, y: 799 }, false, false),
            Some(GatewayMsg::PointerMove { x: 1279, y: 799 })
        );
        // A drag off the canvas edge pins to the edge instead of vanishing.
        assert_eq!(
            to_agent(&ClientMsg::MouseMove { x: -5, y: 70_000 }, false, false),
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
                }, false, false),
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
            }, false, false),
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
            to_agent(&ClientMsg::Wheel { dx: 0.0, dy: -2.5 }, false, false),
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
            }, false, false),
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
            }, false, false),
            Some(GatewayMsg::Key {
                code: "MediaPlayPause".to_owned(),
                pressed: false,
                caps: false,
            })
        );
    }

    #[test]
    fn refresh_asks_the_agent_for_a_full_repaint() {
        assert_eq!(to_agent(&ClientMsg::Refresh, false, false), Some(GatewayMsg::Refresh));
    }

    // Viewport reports mean nothing to a display that only takes sizes off a
    // list — even with resize on, where the browser sends SetResolution
    // instead — and the session layer never forwards Connect/Disconnect.
    #[test]
    fn messages_with_no_agent_equivalent_are_dropped() {
        for resize in [false, true] {
            assert_eq!(
                to_agent(&ClientMsg::Viewport { w: 2560, h: 1440 }, false, resize),
                None,
                "viewport reports are never forwarded (resize={resize})"
            );
        }
        assert_eq!(
            to_agent(
                &ClientMsg::Connect {
                    target: "mac".to_owned()
                },
                false,
                false
            ),
            None
        );
        assert_eq!(to_agent(&ClientMsg::Disconnect, false, false), None);
    }

    // The browser only shows the resolution menu for a target that opted in,
    // and this is the belt to that UI's braces — a stray pick from a stale tab
    // must not reach the Mac.
    #[test]
    fn a_resolution_pick_reaches_the_agent_only_when_the_target_opted_in() {
        assert_eq!(
            to_agent(&ClientMsg::SetResolution { w: 1280, h: 800 }, false, true),
            Some(GatewayMsg::SetDisplaySize { w: 1280, h: 800 })
        );
        assert_eq!(
            to_agent(&ClientMsg::SetResolution { w: 1280, h: 800 }, false, false),
            None
        );
    }

    // The clipboard pair is the only thing the flag gates, and it gates both
    // directions: a target that didn't opt in neither reads nor writes the
    // Mac's pasteboard, whatever the browser sends.
    #[test]
    fn clipboard_messages_reach_the_agent_only_when_the_target_opted_in() {
        assert_eq!(
            to_agent(&ClientMsg::ClipboardRequest, true, false),
            Some(GatewayMsg::ClipboardRequest)
        );
        assert_eq!(
            to_agent(
                &ClientMsg::Clipboard {
                    text: "copied — 画面".to_owned()
                },
                true
            , false),
            Some(GatewayMsg::Clipboard {
                text: "copied — 画面".to_owned()
            })
        );

        assert_eq!(to_agent(&ClientMsg::ClipboardRequest, false, false), None);
        assert_eq!(
            to_agent(
                &ClientMsg::Clipboard {
                    text: "copied".to_owned()
                },
                false
            , false),
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
        assert_eq!(to_agent(&ClientMsg::Clipboard { text }, true, false), None);

        // At the ceiling it goes through untouched, so the boundary is
        // inclusive and nothing is rewritten on the way past.
        let text = "a".repeat(crate::protocol::MAX_CLIPBOARD_BYTES);
        match to_agent(
            &ClientMsg::Clipboard {
                text: text.clone(),
            },
            true,
            false,
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
