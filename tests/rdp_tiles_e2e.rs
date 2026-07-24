//! End-to-end test of the phase-2 tile transport against a real RDP server.
//!
//! Starts the dummy xrdp container (`tests/xrdp-dummy/`) with podman or
//! docker, points the real axum server at it, and connects a raw WebSocket
//! client (never a headless browser — see CLAUDE.md). xrdp paints its login
//! screen even with no session backend, so real bitmap updates flow through
//! the whole pipeline: IronRDP session -> `ServerMsg::Tile` -> binary WS
//! frames, which this test validates byte-for-byte against the wire layout
//! documented in `src/protocol.rs` / `frontend/src/protocol.ts`.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt as _;
use rdpweb::config::{AppConfig, Protocol, Security, TargetConfig};
use rdpweb::server;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

const TILE_FRAME_KIND: u8 = 0x01;
const TILE_FORMAT_PNG: u8 = 1;
const TILE_HEADER_LEN: usize = 10;

/// Wait until xrdp actually answers RDP on the published port.
///
/// A bare TCP-accept probe is not enough: rootless podman's port forwarder
/// accepts immediately and then resets if nothing listens inside yet. So the
/// probe sends an X.224 Connection Request (TPKT-framed) and requires xrdp to
/// send bytes back.
async fn wait_for_rdp_port(port: u16) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    // TPKT header (len 11) + X.224 CR TPDU, no negotiation payload.
    const X224_CONNECT: [u8; 11] = [3, 0, 0, 11, 6, 0xe0, 0, 0, 0, 0, 0];

    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let attempt = async {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
                stream.write_all(&X224_CONNECT).await.ok()?;
                let mut buf = [0u8; 4];
                stream.read_exact(&mut buf).await.ok()
            };
            match tokio::time::timeout(Duration::from_secs(2), attempt).await {
                Ok(Some(_)) => return,
                _ => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    })
    .await
    .expect("dummy RDP server never answered the X.224 probe");
}

/// Start the real server pointed at the dummy RDP target (xrdp ignores the
/// credentials until login is submitted, which this test never does).
async fn spawn_app(rdp_port: u16) -> SocketAddr {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });

    let config = AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        static_dir: "frontend/dist".into(),
        target: TargetConfig {
            name: "xrdp-dummy".to_owned(),
            protocol: Protocol::Rdp,
            host: "127.0.0.1".to_owned(),
            port: rdp_port,
            username: "dummy".to_owned(),
            password: "dummy".to_owned(),
            domain: None,
            width: 1280,
            height: 800,
            security: Security::Auto,
            resize: false,
        },
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Validate one binary tile frame against the documented layout.
fn check_tile_frame(frame: &[u8]) {
    assert!(frame.len() >= TILE_HEADER_LEN, "frame shorter than the header");
    assert_eq!(frame[0], TILE_FRAME_KIND, "unexpected frame kind");
    assert_eq!(frame[1], TILE_FORMAT_PNG, "unexpected tile format byte");
    let w = u16::from_le_bytes([frame[6], frame[7]]);
    let h = u16::from_le_bytes([frame[8], frame[9]]);
    assert!(w > 0 && h > 0, "empty tile {w}x{h}");
    assert_eq!(
        &frame[TILE_HEADER_LEN..TILE_HEADER_LEN + 8],
        b"\x89PNG\r\n\x1a\n",
        "payload is not a PNG stream"
    );
}

#[tokio::test]
async fn tiles_arrive_as_binary_frames_after_resize_text() {
    let runtime = common::container_runtime();
    let (_container, rdp_port) =
        common::start_dummy_server(runtime, "rdpweb-e2e-xrdp", "xrdp-dummy", 3389);
    wait_for_rdp_port(rdp_port).await;

    let addr = spawn_app(rdp_port).await;
    let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();

    let mut got_resize = false;
    let mut tiles = 0u32;

    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    // The only text frames are control messages; the session
                    // must not fail, and resize must precede any tile.
                    assert!(
                        !text.contains(r#""type":"error""#),
                        "session failed: {text}"
                    );
                    if text.contains(r#""type":"resize""#) {
                        assert_eq!(tiles, 0, "resize arrived after tiles");
                        got_resize = true;
                    }
                }
                Message::Binary(frame) => {
                    assert!(got_resize, "tile arrived before resize");
                    check_tile_frame(&frame);
                    tiles += 1;
                    // The xrdp login screen paints in well over 20 strips;
                    // that's enough to call the transport exercised.
                    if tiles >= 20 {
                        return;
                    }
                }
                _ => {}
            }
        }
        panic!("websocket closed after {tiles} tiles without reaching the target");
    })
    .await
    .expect("timed out waiting for tile frames");

    assert!(got_resize, "never received the resize control message");
}
