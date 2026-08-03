//! WebSocket endpoint bridging a browser to the server-side remote-desktop
//! session.
//!
//! Two endpoints, both presenting the claim token from `POST /api/session`.
//!
//! `/ws?session=<token>` is the session: it attaches to the single slot
//! ([`crate::session::SessionManager`]). Inbound `ClientMsg` split two ways —
//! session-control messages (`connect` to pick a target from the post-login picker,
//! `disconnect` to switch back to it) act on the slot; everything else is engine
//! input, routed to the current engine (or dropped in the picker state). Outbound
//! `ServerMsg` go to the browser as screen tiles in binary frames and control messages
//! (resize/error, the picker/connected status) as JSON text (see [`crate::protocol`]).
//!
//! `/ws/audio?session=<token>` is sound, and **opening it is the subscription** —
//! there is no message that turns audio on. It carries exactly two things, the format
//! and then packets, and it exists so that neither ever waits behind a picture: on a
//! `render_type = "video"` target the session socket's queue is four frames deep, and
//! an audio pump stuck behind it stops draining the bridge, which then drops wave
//! buffers outright. It is bound to the *claim* rather than to an attachment, so it
//! survives a reattach and a target switch and ends only when the claim does.
//!
//! Close codes tell the browser why either socket ended:
//! - `4000` — the token is missing or superseded; claim again.
//! - `4001` — evicted: another browser claimed the slot, or (on the audio socket) a
//!   newer audio socket replaced this one.
//!
//! Any other close on the session socket detaches the browser. Reattaching within the
//! grace period restores the picker or live engine; otherwise the engine ends for every
//! protocol. A closed audio socket ends nothing but the sound.

use axum::{
    extract::{
        Query, State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use futures_util::{SinkExt as _, StreamExt as _};
use log::{info, warn};
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::{
    protocol::{ClientMsg, WireFrame},
    server::AppState,
    session::{AttachEvent, REATTACH_GRACE_PERIOD, SessionManager},
    wire::Wire,
};

/// Close code: the session token is missing, invalid, or superseded.
const CLOSE_INVALID_TOKEN: u16 = 4000;
/// Close code: another browser took over the session slot.
const CLOSE_EVICTED: u16 = 4001;

/// WebSocket keepalive interval. Browsers answer protocol pings in the network
/// stack, so background-tab JavaScript timer throttling cannot suppress it.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct HeartbeatTimings {
    interval: Duration,
    timeout: Duration,
}

const HEARTBEAT_TIMINGS: HeartbeatTimings = HeartbeatTimings {
    interval: HEARTBEAT_INTERVAL,
    timeout: REATTACH_GRACE_PERIOD,
};

#[derive(Deserialize)]
pub struct WsParams {
    session: Option<String>,
}

pub async fn handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| {
        session(socket, state.sessions, params.session, HEARTBEAT_TIMINGS)
    })
}

pub async fn audio_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| audio(socket, state.sessions, params.session, HEARTBEAT_TIMINGS))
}

/// The audio socket: one task, because there is nothing inbound to do.
///
/// It still reads `ws_rx`, because that is the only way pongs and the browser's close
/// frame arrive, but it acts on neither beyond keeping the heartbeat alive and noticing
/// the end. Everything else is a one-way stream of the format and its packets.
async fn audio(
    mut socket: WebSocket,
    sessions: Arc<SessionManager>,
    token: Option<String>,
    heartbeat_timings: HeartbeatTimings,
) {
    let attachment = token.and_then(|t| sessions.attach_audio(&t).ok());
    let Some(attachment) = attachment else {
        warn!("ws: rejected an audio connection without a valid session token");
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: CLOSE_INVALID_TOKEN,
                reason: "invalid session token".into(),
            })))
            .await;
        return;
    };

    info!("ws: an audio socket attached");

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (audio_id, mut packets, mut evicted) =
        (attachment.id, attachment.packets, attachment.evicted);
    // The same encoder the session socket uses, so the two cannot disagree about a
    // frame layout, and so `Totals` keeps reporting audio bytes where the field
    // measurement already reads them. Its tile machinery simply never sees a tile.
    let mut wire = Wire::default();
    let mut heartbeat = interval(heartbeat_timings.interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_heartbeat = Instant::now();

    loop {
        tokio::select! {
            // Biased, and eviction first: a takeover has to stop this browser hearing
            // the session *now*, not after however much sound is still queued for it.
            biased;
            _ = &mut evicted => {
                info!("ws: audio socket evicted");
                let _ = ws_tx
                    .send(Message::Close(Some(CloseFrame {
                        code: CLOSE_EVICTED,
                        reason: "session taken over".into(),
                    })))
                    .await;
                break;
            }
            msg = packets.recv() => {
                // `None` cannot happen while the slot holds the sender, so it means the
                // session dropped this socket without evicting it — end quietly.
                let Some(msg) = msg else { break };
                // No run collection: every audio message is its own frame regardless,
                // so batching would only add latency to the one thing that cannot
                // absorb any.
                for frame in wire.encode(vec![msg]) {
                    let frame = match frame {
                        WireFrame::Binary(bytes) => Message::Binary(bytes.into()),
                        WireFrame::Text(json) => Message::Text(json.into()),
                    };
                    if ws_tx.send(frame).await.is_err() {
                        return finish(&sessions, audio_id, &wire);
                    }
                }
            }
            frame = ws_rx.next() => {
                match frame {
                    Some(Ok(Message::Pong(_))) => last_heartbeat = Instant::now(),
                    Some(Ok(_)) => {}
                    // A close frame or a dead socket. Nothing here is inbound, so
                    // anything else the browser sends is ignored rather than refused.
                    Some(Err(_)) | None => break,
                }
            }
            _ = heartbeat.tick() => {
                // A browser that vanished without a FIN would otherwise leave its slot
                // registered, and the next `connect` would arm a pump into a socket
                // nobody reads — the exact stall this endpoint exists to remove, for a
                // listener who is not there.
                //
                // Detaching audio is *all* this does. It must never reach
                // `expire_attachment`: the session socket is the authority on whether
                // the browser is alive, and a desktop has to survive its sound dying.
                if last_heartbeat.elapsed() >= heartbeat_timings.timeout {
                    warn!(
                        "ws: audio socket heartbeat timed out after {}s",
                        heartbeat_timings.timeout.as_secs()
                    );
                    break;
                }
                if ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    finish(&sessions, audio_id, &wire);
}

fn finish(sessions: &Arc<SessionManager>, audio_id: u64, wire: &Wire) {
    sessions.detach_audio(audio_id);
    info!("ws: audio totals: {}", wire.totals);
}

async fn session(
    mut socket: WebSocket,
    sessions: Arc<SessionManager>,
    token: Option<String>,
    heartbeat_timings: HeartbeatTimings,
) {
    let attachment = token.and_then(|t| sessions.attach(&t).ok());
    let Some(attachment) = attachment else {
        warn!("ws: rejected connection without a valid session token");
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: CLOSE_INVALID_TOKEN,
                reason: "invalid session token".into(),
            })))
            .await;
        return;
    };

    info!("ws: client attached to the session slot");

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (attach_id, mut events) = (attachment.id, attachment.events);

    // How many times the client has said it lost its tile cache. The inbound half
    // bumps it; the outbound half, which owns the cache, notices before its next
    // batch. A counter and not a flag or a channel: the outbound task must not have
    // to *wait* for this, an extra clear is always harmless, and comparing two
    // numbers needs no lock and cannot deadlock against the send path.
    let cache_epoch = Arc::new(AtomicU64::new(0));
    let inbound_epoch = Arc::clone(&cache_epoch);

    // Outbound: session events -> browser, batched through [`Wire`], whose
    // counters are logged when the attachment ends so the transport can be
    // measured in the field. Ends on eviction (explicit close) or engine death.
    let mut outbound = tokio::spawn(async move {
        let mut wire = Wire::default();
        let mut seen_epoch = 0u64;
        let mut heartbeat = interval(heartbeat_timings.interval);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        'outbound: loop {
            let event = tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    event
                }
                _ = heartbeat.tick() => {
                    if ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                    continue;
                }
            };

            // Before anything is encoded: if the client has said it lost its
            // cache, stop believing it holds anything. Checked here rather than on
            // arrival because this is the task that knows, and one iteration of
            // latency costs at most a batch of references the client would answer
            // with another reset.
            let epoch = cache_epoch.load(Ordering::Relaxed);
            if epoch != seen_epoch {
                seen_epoch = epoch;
                wire.reset_cache();
            }

            // Take everything already queued behind the first message, so a burst
            // of tiles is batched instead of costing a frame each. `try_recv` and
            // not another `await`: a batch must never *wait* for more work, only
            // collect what is already there. Under a slow link the channel fills
            // and the batches grow, which is the adaptation wanted — bigger writes
            // exactly when per-frame overhead hurts most.
            let mut run = Vec::new();
            let mut evicted = false;
            for event in std::iter::once(event).chain(std::iter::from_fn(|| events.try_recv().ok()))
            {
                match event {
                    AttachEvent::Msg(msg) => run.push(msg),
                    AttachEvent::Evicted => {
                        evicted = true;
                        break;
                    }
                }
            }

            // Whatever was already encoded still goes out first: eviction is not a
            // reason to drop paint the client was owed.
            for frame in wire.encode(run) {
                let frame = match frame {
                    WireFrame::Binary(bytes) => Message::Binary(bytes.into()),
                    WireFrame::Text(json) => Message::Text(json.into()),
                };
                if ws_tx.send(frame).await.is_err() {
                    break 'outbound; // browser gone
                }
            }

            if evicted {
                info!("ws: evicted by a session takeover");
                let _ = ws_tx
                    .send(Message::Close(Some(CloseFrame {
                        code: CLOSE_EVICTED,
                        reason: "session taken over".into(),
                    })))
                    .await;
                break;
            }
        }
        info!("ws: outbound totals: {}", wire.totals);
    });

    // Inbound: browser input -> protocol engine. Also ends when the outbound
    // side finishes (eviction / engine death), so a socket that lingers after
    // eviction can't keep injecting input into the session.
    let mut outbound_done = false;
    let mut heartbeat_check = interval(heartbeat_timings.interval);
    heartbeat_check.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_heartbeat = Instant::now();
    loop {
        let msg = tokio::select! {
            res = &mut outbound => {
                if let Err(e) = res {
                    warn!("ws: outbound task failed: {e}");
                }
                outbound_done = true;
                break;
            }
            msg = ws_rx.next() => msg,
            _ = heartbeat_check.tick() => {
                if last_heartbeat.elapsed() >= heartbeat_timings.timeout {
                    warn!(
                        "ws: browser heartbeat timed out after {}s",
                        last_heartbeat.elapsed().as_secs()
                    );
                    // Waiting for the missing pong already consumed the
                    // reattach grace period, so end the engine immediately.
                    sessions.expire_attachment(attach_id);
                    break;
                }
                continue;
            }
        };
        match msg {
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMsg>(&text) {
                // Session-control messages act on the slot, not an engine: pick a
                // target from the picker, or tear the session down and go back to
                // it ("switch target").
                Ok(ClientMsg::Connect { target }) => {
                    if let Err(e) = sessions.connect(attach_id, &target) {
                        warn!("ws: connect to {target:?} refused: {e}");
                    }
                }
                Ok(ClientMsg::Disconnect) => sessions.disconnect(attach_id),
                // The client lost the tiles it was told to remember. Both halves
                // have to act: the outbound task forgets the slots (through the
                // epoch), and the engine repaints — a repaint alone would come back
                // as the same references and miss again.
                Ok(ClientMsg::CacheReset) => {
                    inbound_epoch.fetch_add(1, Ordering::Relaxed);
                    sessions.forward_input(attach_id, ClientMsg::Refresh);
                }
                // Everything else is engine input, routed to the current engine
                // (dropped in the picker state). Routing through the manager —
                // rather than a captured engine sender — means it always reaches
                // the engine that is live *now*, across connect/disconnect.
                Ok(input) => sessions.forward_input(attach_id, input),
                Err(e) => warn!("ws: bad client message: {e} (raw: {text})"),
            },
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(Message::Pong(_))) => {
                last_heartbeat = Instant::now();
            }
            Some(Ok(_)) => {} // Binary/Ping: nothing to do
            Some(Err(e)) => {
                warn!("ws: receive error: {e}");
                break;
            }
        }
    }

    // Give the slot back and start the shared reattach grace period. If the
    // slot already moved on (takeover or heartbeat expiry) this is a no-op.
    sessions.detach(attach_id);

    // Let the outbound task drain (its totals line should still be logged),
    // but don't wait on a hung socket forever.
    if !outbound_done
        && tokio::time::timeout(std::time::Duration::from_secs(5), &mut outbound)
            .await
            .is_err()
    {
        outbound.abort();
    }
    info!("ws: client detached");
}

#[cfg(test)]
mod tests {
    use axum::{Router, routing::any};
    use futures_util::SinkExt as _;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message as ClientFrame;

    use super::*;
    use crate::config::{Protocol, Security, TargetConfig};
    use crate::protocol::ServerMsg;
    use crate::session::SessionManager;

    fn fake_target(audio: bool) -> TargetConfig {
        TargetConfig {
            name: "fake".to_owned(),
            protocol: Protocol::Vnc,
            subtype: None,
            host: "127.0.0.1".to_owned(),
            port: 1,
            username: String::new(),
            password: String::new(),
            vnc_password: String::new(),
            domain: None,
            width: 1,
            height: 1,
            security: Security::Auto,
            resize: false,
            clipboard: false,
            audio,
            audio_codec: None,
            render_type: crate::config::RenderType::Full,
            render_subtype: crate::config::RenderSubtype::Png,
            render_quality: None,
            render_motion_subtype: None,
            render_motion_quality: None,
            render_motion_debug: false,
            video_codec: None,
        }
    }

    #[tokio::test]
    async fn unanswered_websocket_pings_expire_the_session_engine() {
        let target = fake_target(false);
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
        let sessions = Arc::new(SessionManager::with_test_spawner(
            vec![target],
            move |_target, input_rx, frame_tx, _audio| {
                engine_tx.send((input_rx, frame_tx)).unwrap();
            },
        ));
        let token = sessions.claim(false, None).unwrap();
        let assertions = Arc::clone(&sessions);
        let timings = HeartbeatTimings {
            interval: Duration::from_secs(1),
            timeout: REATTACH_GRACE_PERIOD,
        };
        let app = Router::new().route(
            "/ws",
            any(move |ws: WebSocketUpgrade| {
                let sessions = Arc::clone(&sessions);
                let token = token.clone();
                async move {
                    ws.on_upgrade(move |socket| session(socket, sessions, Some(token), timings))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        // Establish the OS-backed WebSocket under real time. Pausing before the
        // handshake lets Tokio auto-advance idle time through heartbeat ticks
        // while the runtime waits for socket I/O, which can expire the session
        // before this test reaches its first assertion under a busy test suite.
        client
            .send(ClientFrame::Pong(Vec::new().into()))
            .await
            .unwrap();
        client
            .send(ClientFrame::text(r#"{"type":"connect","target":"fake"}"#))
            .await
            .unwrap();
        let (input_rx, _frame_tx) = engine_rx.recv().await.unwrap();
        // WebSocket frames are processed in order, so engine creation proves
        // the preceding Pong reset the heartbeat baseline. Freeze only the
        // timeout window that the assertions below control.
        tokio::time::pause();

        // Never poll `client`: its protocol stack cannot read Ping frames and
        // therefore cannot enqueue automatic Pong replies.
        tokio::time::advance(REATTACH_GRACE_PERIOD - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(!input_rx.is_closed(), "engine expired before the heartbeat timeout");

        tokio::time::advance(Duration::from_secs(1) + Duration::from_millis(1)).await;
        for _ in 0..10 {
            if input_rx.is_closed() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(input_rx.is_closed(), "heartbeat timeout did not stop the engine");
        let replacement_token = assertions
            .claim(false, None)
            .expect("heartbeat timeout did not release the browser attachment");
        let mut replacement = assertions.attach(&replacement_token).unwrap();
        assert!(matches!(
            replacement.events.recv().await,
            Some(AttachEvent::Msg(ServerMsg::Picker))
        ));

        drop(client);
        server.abort();
    }

    /// The mirror of the test above, and the one that pins the difference between the
    /// two endpoints: an audio socket that stops answering reaps *itself* and nothing
    /// else. The session socket is the authority on whether the browser is alive, so a
    /// desktop has to survive its sound dying.
    #[tokio::test]
    async fn unanswered_audio_pings_close_the_audio_socket_and_leave_the_engine_alone() {
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
        let sessions = Arc::new(SessionManager::with_test_spawner(
            vec![fake_target(true)],
            move |_target, input_rx, frame_tx, audio| {
                engine_tx.send((input_rx, frame_tx, audio)).unwrap();
            },
        ));
        let token = sessions.claim(false, None).unwrap();
        // A live desktop, driven in process: this test is about the audio socket, and
        // the session socket only has to exist for `connect` to be legal.
        let mut att = sessions.attach(&token).unwrap();
        assert!(matches!(
            att.events.recv().await,
            Some(AttachEvent::Msg(ServerMsg::Picker))
        ));
        sessions.connect(att.id, "fake").unwrap();
        let (input_rx, _frame_tx, bridge) = engine_rx.recv().await.unwrap();
        let bridge = bridge.expect("an audio target's engine is given a bridge");

        let timings = HeartbeatTimings {
            interval: Duration::from_secs(1),
            timeout: REATTACH_GRACE_PERIOD,
        };
        let served = Arc::clone(&sessions);
        let app = Router::new().route(
            "/ws/audio",
            any(move |ws: WebSocketUpgrade| {
                let sessions = Arc::clone(&served);
                let token = token.clone();
                async move {
                    ws.on_upgrade(move |socket| audio(socket, sessions, Some(token), timings))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/audio"))
            .await
            .unwrap();
        // Same reasoning as above: establish the socket under real time, and prove the
        // baseline was reset before freezing the window the assertions control.
        // No opening Pong, unlike the session test above: this socket has no inbound
        // message to prove one was processed in order, and a Pong still in flight when
        // the clock froze would reset the baseline *after* the advance below and hold
        // the socket open forever. The baseline is the task's own start instead, which
        // is established under real time by the wait here.
        //
        // The subscriber count is the observable, as in `session`'s own audio tests: it
        // says the socket attached *and* armed a pump, and the session answers it
        // rather than socket I/O, which a paused clock cannot drive.
        expect_listeners(&bridge, 1).await;
        tokio::time::pause();

        // Never poll `client`: its protocol stack cannot read Ping frames, and
        // therefore cannot answer them, unless it is polled.
        tokio::time::advance(REATTACH_GRACE_PERIOD + Duration::from_secs(1)).await;
        expect_listeners(&bridge, 0).await;
        assert!(
            !input_rx.is_closed(),
            "an audio socket timing out must not take the desktop with it"
        );

        drop(client);
        server.abort();
    }

    /// Wait for the bridge's subscriber count to settle at `want`, bounded so a count
    /// that never settles fails rather than hangs. The twin of `session`'s helper of
    /// the same name, and polled for the same reason: stopping audio aborts a task, so
    /// the listener goes when the runtime drops it — prompt, but not synchronous.
    async fn expect_listeners(bridge: &crate::audio::AudioBridge, want: usize) {
        for _ in 0..1000 {
            if bridge.listener_count() == want {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "expected {want} audio listener(s), found {}",
            bridge.listener_count()
        );
    }
}
