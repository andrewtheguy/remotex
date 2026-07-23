//! WebSocket endpoint bridging a browser to a server-side RDP session.
//!
//! Each connection spawns a [`crate::rdp`] session and shuttles messages between
//! it and the browser: inbound `ClientMsg` (input) go to the RDP engine, and
//! outbound `ServerMsg` (screen tiles, resize, errors) go to the browser as JSON
//! text frames.

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
    protocol::{ClientMsg, ServerMsg},
    rdp,
    server::AppState,
};

pub async fn handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| session(socket, state))
}

async fn session(socket: WebSocket, state: AppState) {
    info!(
        "ws: client connected; starting RDP session to {}:{}",
        state.config.rdp_host, state.config.rdp_port
    );

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (input_tx, input_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (frame_tx, mut frame_rx) = mpsc::channel::<ServerMsg>(64);

    // Drive the RDP session on a dedicated thread with its own current-thread
    // runtime. IronRDP's `read_pdu` future is not `Send`-general (it holds a
    // `&dyn PduHint` across await), so it can't live on the shared multi-thread
    // runtime via `tokio::spawn`; a current-thread runtime imposes no `Send`
    // bound. It ends when the browser goes away (input channel closed) or the
    // RDP host disconnects.
    let rdp_config = state.config.clone();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                warn!("ws: failed to build RDP runtime: {e}");
                return;
            }
        };
        rt.block_on(rdp::run(rdp_config, input_rx, frame_tx));
    });

    // Outbound: RDP screen updates -> browser.
    let outbound = tokio::spawn(async move {
        while let Some(msg) = frame_rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(json) => {
                    if ws_tx.send(Message::Text(json.into())).await.is_err() {
                        break; // browser gone
                    }
                }
                Err(e) => warn!("ws: serialize error: {e}"),
            }
        }
    });

    // Inbound: browser input -> RDP session.
    while let Some(msg) = ws_rx.next().await {
        match msg {
            Ok(Message::Text(text)) => match serde_json::from_str::<ClientMsg>(&text) {
                Ok(input) => {
                    if input_tx.send(input).is_err() {
                        break; // RDP session ended
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

    // Dropping `input_tx` (end of scope) tells the RDP session to stop.
    outbound.abort();
    info!("ws: client disconnected");
}
