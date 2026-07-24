//! WebSocket endpoint bridging a browser to a server-side RDP session.
//!
//! Each connection spawns a [`crate::rdp`] session and shuttles messages between
//! it and the browser: inbound `ClientMsg` (input, JSON text) go to the RDP
//! engine, and outbound `ServerMsg` go to the browser — screen tiles as binary
//! frames, control messages (resize/error) as JSON text (see [`crate::protocol`]).

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
    rdp,
    server::AppState,
};

pub async fn handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| session(socket, state))
}

async fn session(socket: WebSocket, state: AppState) {
    info!(
        "ws: client connected; starting RDP session to {}:{}",
        state.config.target.host, state.config.target.port
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
    //
    // Scalability: this costs one OS thread + one current-thread runtime per
    // connection — fine here, since multi session is permanently out of scope
    // (single user, one active session at a time; see CLAUDE.md).
    let rdp_config = state.config.target.clone();
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

    // Outbound: RDP screen updates -> browser. Byte counters are logged at the
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

    // Dropping `input_tx` tells the RDP session to stop; it then drops
    // `frame_tx`, which ends the outbound task naturally so its totals line
    // still gets logged (an abort here would cancel it mid-recv). Bounded:
    // if the RDP side is stuck (e.g. hung TCP connect) the task holds
    // `frame_rx` open longer, and then it is simply aborted.
    drop(input_tx);
    if tokio::time::timeout(std::time::Duration::from_secs(5), &mut outbound)
        .await
        .is_err()
    {
        outbound.abort();
    }
    info!("ws: client disconnected");
}
