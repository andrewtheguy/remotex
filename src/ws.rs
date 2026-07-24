//! WebSocket endpoint bridging a browser to a server-side remote-desktop
//! session.
//!
//! Each connection spawns the protocol engine for the configured target
//! ([`crate::session`]) and shuttles messages between it and the browser:
//! inbound `ClientMsg` (input, JSON text) go to the engine, and outbound
//! `ServerMsg` go to the browser — screen tiles as binary frames, control
//! messages (resize/error) as JSON text (see [`crate::protocol`]).

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use futures_util::{SinkExt as _, StreamExt as _};
use log::{info, warn};
use tokio::sync::mpsc;

use crate::{
    protocol::{ClientMsg, ServerMsg, WireFrame},
    server::AppState,
    session,
};

pub async fn handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| session(socket, state))
}

async fn session(socket: WebSocket, state: AppState) {
    let target = &state.config.target;
    info!(
        "ws: client connected; starting {} session to {}:{}",
        target.protocol.name(),
        target.host,
        target.port
    );

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (input_tx, input_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (frame_tx, mut frame_rx) = mpsc::channel::<ServerMsg>(64);

    session::spawn(target.clone(), input_rx, frame_tx);

    // Outbound: engine screen updates -> browser. Byte counters are logged at the
    // end of the session so the transport can be measured in the field (this
    // link — backend to a possibly weak-signal WAN browser — is the bottleneck
    // phase 2 optimizes).
    let mut outbound = tokio::spawn(async move {
        let (mut tiles, mut tile_bytes, mut text_bytes) = (0u64, 0u64, 0u64);
        while let Some(msg) = frame_rx.recv().await {
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

    // Inbound: browser input -> protocol engine.
    while let Some(msg) = ws_rx.next().await {
        match msg {
            Ok(Message::Text(text)) => match serde_json::from_str::<ClientMsg>(&text) {
                Ok(input) => {
                    if input_tx.send(input).is_err() {
                        break; // engine session ended
                    }
                }
                Err(e) => warn!("ws: bad client message: {e} (raw: {text})"),
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {} // Binary/Ping/Pong: nothing to do
            Err(e) => {
                warn!("ws: receive error: {e}");
                break;
            }
        }
    }

    // Dropping `input_tx` tells the engine to stop; it then drops `frame_tx`,
    // which ends the outbound task naturally so its totals line still gets
    // logged (an abort here would cancel it mid-recv). Bounded: if the engine
    // is stuck (e.g. hung TCP connect) the task holds `frame_rx` open longer,
    // and then it is simply aborted.
    drop(input_tx);
    if tokio::time::timeout(std::time::Duration::from_secs(5), &mut outbound)
        .await
        .is_err()
    {
        outbound.abort();
    }
    info!("ws: client disconnected");
}
