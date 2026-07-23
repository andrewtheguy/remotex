//! WebSocket endpoint for a remote-desktop session.
//!
//! Skeleton behaviour: accept the upgrade, read `ClientMsg` input events off the
//! socket and log them. No frames are sent back yet — this only proves the
//! browser ⇄ backend wiring end to end.
//!
//! TODO(phase1): hand the socket to [`crate::rdp::Session`] — forward decoded
//! `ClientMsg` input into the RDP engine and stream `ServerMsg::Tile` frames back.

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use log::{info, warn};

use crate::{protocol::ClientMsg, server::AppState};

pub async fn handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| session(socket, state))
}

async fn session(mut socket: WebSocket, state: AppState) {
    info!(
        "ws: client connected (RDP target {}:{} — placeholder, no connection made)",
        state.config.rdp_host, state.config.rdp_port
    );

    // TODO(phase1): let mut rdp = rdp::Session::connect(&state.config).await?;
    //               then splice rdp frames -> socket and socket input -> rdp.

    while let Some(msg) = socket.recv().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(e) => {
                warn!("ws: receive error: {e}");
                break;
            }
        };

        match msg {
            Message::Text(text) => match serde_json::from_str::<ClientMsg>(&text) {
                Ok(input) => {
                    info!("ws: input {input:?}");
                    // TODO(phase1): rdp.send_input(input).await;
                }
                Err(e) => warn!("ws: bad client message: {e} (raw: {text})"),
            },
            Message::Binary(bytes) => {
                info!("ws: {} bytes of binary (ignored in skeleton)", bytes.len());
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    info!("ws: client disconnected");
}
