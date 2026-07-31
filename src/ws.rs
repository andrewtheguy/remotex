//! WebSocket endpoint bridging a browser to the server-side remote-desktop
//! session.
//!
//! Each connection presents its claim token (`/ws?session=<token>`, obtained
//! from `POST /api/session`) and attaches to the single session slot
//! ([`crate::session::SessionManager`]). Inbound `ClientMsg` split two ways:
//! session-control messages (`connect` to pick a target from the post-login
//! picker, `disconnect` to switch back to it, `audio` to hear the remote) act on
//! the slot; everything else is engine input, routed to the current engine (or
//! dropped in the picker state). Outbound `ServerMsg` go to the browser — screen
//! tiles and audio as binary frames, control messages (resize/error, the
//! picker/connected status, the audio format) as JSON text (see
//! [`crate::protocol`]).
//!
//! Audio arrives here only for a client that asked. That is what let audio join this
//! socket without changing what any existing client received — see
//! [`crate::protocol::PROTOCOL_VERSION`], which held at 4 for exactly that reason and
//! moved only when the viewer gained a *floor* worth reporting.
//!
//! Close codes tell the browser why the socket ended:
//! - `4000` — the token is missing or superseded; claim again.
//! - `4001` — evicted: another browser force-claimed the slot (takeover).
//!
//! Any other close detaches the browser. Reattaching within the grace period
//! restores the picker or live engine; otherwise the engine ends for every
//! protocol.

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
                // Audio is this attachment's, not the engine's: it subscribes the
                // socket to a queue the engine is already filling, so it acts on the
                // slot like the two above rather than being forwarded.
                Ok(ClientMsg::Audio { enabled }) => sessions.set_audio(attach_id, enabled),
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

    #[tokio::test]
    async fn unanswered_websocket_pings_expire_the_session_engine() {
        let target = TargetConfig {
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
            audio: false,
        };
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
}
