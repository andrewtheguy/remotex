//! WebSocket endpoint bridging a browser to the server-side remote-desktop
//! session.
//!
//! Each connection presents its claim token (`/ws?session=<token>`, obtained
//! from `POST /api/session`) and attaches to the single session slot
//! ([`crate::session::SessionManager`]). Inbound `ClientMsg` split two ways:
//! session-control messages (`connect` to pick a target from the post-login
//! picker, `disconnect` to switch back to it) act on the slot; everything else
//! is engine input, routed to the current engine (or dropped in the picker
//! state). Outbound `ServerMsg` go to the browser — screen tiles as binary
//! frames, control messages (resize/error, plus the picker/connected status)
//! as JSON text (see [`crate::protocol`]).
//!
//! Close codes tell the browser why the socket ended:
//! - `4000` — the token is missing or superseded; claim again.
//! - `4001` — evicted: another browser force-claimed the slot (takeover).
//!
//! Any other close leaves the session (picker or a running engine) in place;
//! reattaching restores it — the picker, or a full repaint from the
//! server-owned framebuffer.

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

use crate::{
    protocol::{ClientMsg, WireFrame},
    server::AppState,
    session::AttachEvent,
};

/// Close code: the session token is missing, invalid, or superseded.
const CLOSE_INVALID_TOKEN: u16 = 4000;
/// Close code: another browser took over the session slot.
const CLOSE_EVICTED: u16 = 4001;

#[derive(Deserialize)]
pub struct WsParams {
    session: Option<String>,
}

pub async fn handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| session(socket, state, params.session))
}

async fn session(mut socket: WebSocket, state: AppState, token: Option<String>) {
    let attachment = token.and_then(|t| state.sessions.attach(&t).ok());
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

    // Outbound: session events -> browser. Byte counters are logged at the end
    // of the attachment so the transport can be measured in the field (this
    // link — backend to a possibly weak-signal WAN browser — is the bottleneck
    // the binary tile transport optimizes). Ends on eviction (explicit close)
    // or engine death.
    let mut outbound = tokio::spawn(async move {
        let (mut tiles, mut tile_bytes, mut text_bytes) = (0u64, 0u64, 0u64);
        while let Some(event) = events.recv().await {
            let msg = match event {
                AttachEvent::Msg(msg) => msg,
                AttachEvent::Evicted => {
                    info!("ws: evicted by a session takeover");
                    let _ = ws_tx
                        .send(Message::Close(Some(CloseFrame {
                            code: CLOSE_EVICTED,
                            reason: "session taken over".into(),
                        })))
                        .await;
                    break;
                }
            };
            let frame = match msg.encode() {
                WireFrame::Binary(bytes) => {
                    tiles += 1;
                    tile_bytes += bytes.len() as u64;
                    Message::Binary(bytes.into())
                }
                WireFrame::Text(json) => {
                    text_bytes += json.len() as u64;
                    Message::Text(json.into())
                }
            };
            if ws_tx.send(frame).await.is_err() {
                break; // browser gone
            }
        }
        info!("ws: outbound totals: {tiles} tiles / {tile_bytes} bytes binary, {text_bytes} bytes text");
    });

    // Inbound: browser input -> protocol engine. Also ends when the outbound
    // side finishes (eviction / engine death), so a socket that lingers after
    // eviction can't keep injecting input into the session.
    let mut outbound_done = false;
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
        };
        match msg {
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMsg>(&text) {
                // Session-control messages act on the slot, not an engine: pick a
                // target from the picker, or tear the session down and go back to
                // it ("switch target").
                Ok(ClientMsg::Connect { target }) => {
                    if let Err(e) = state.sessions.connect(attach_id, &target) {
                        warn!("ws: connect to {target:?} refused: {e}");
                    }
                }
                Ok(ClientMsg::Disconnect) => state.sessions.disconnect(attach_id),
                // Everything else is engine input, routed to the current engine
                // (dropped in the picker state). Routing through the manager —
                // rather than a captured engine sender — means it always reaches
                // the engine that is live *now*, across connect/disconnect.
                Ok(input) => state.sessions.forward_input(attach_id, input),
                Err(e) => warn!("ws: bad client message: {e} (raw: {text})"),
            },
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(_)) => {} // Binary/Ping/Pong: nothing to do
            Some(Err(e)) => {
                warn!("ws: receive error: {e}");
                break;
            }
        }
    }

    // Give the slot back; the engine keeps running detached. If the slot has
    // already moved on (takeover) this is a no-op.
    state.sessions.detach(attach_id);

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
