//! End-to-end test of the VNC engine against a real VNC server.
//!
//! Starts the dummy TigerVNC container (`tests/vnc-dummy/`, VncAuth-protected)
//! with podman or docker, points the real axum server at it, and connects a
//! raw WebSocket client (never a headless browser — see CLAUDE.md). Xtigervnc
//! serves its root window with no session behind it, so the first
//! non-incremental update request drives real raw-encoded pixels through the
//! whole pipeline: RFB handshake + DES auth -> `ServerMsg::Tile` -> the same
//! binary WS frames the RDP engine emits (tests/rdp_tiles_e2e.rs).

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

const DESKTOP_W: u32 = 1024;
const DESKTOP_H: u32 = 768;

/// Wait until the VNC server actually answers RFB on the published port.
///
/// A bare TCP-accept probe is not enough: rootless podman's port forwarder
/// accepts immediately and then resets if nothing listens inside yet. An RFB
/// server leads with its 12-byte version greeting, so require that.
async fn wait_for_vnc_port(port: u16) {
    use tokio::io::AsyncReadExt as _;

    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let attempt = async {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
                let mut greeting = [0u8; 12];
                stream.read_exact(&mut greeting).await.ok()?;
                greeting.starts_with(b"RFB ").then_some(())
            };
            match tokio::time::timeout(Duration::from_secs(2), attempt).await {
                Ok(Some(())) => return,
                _ => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    })
    .await
    .expect("dummy VNC server never sent an RFB greeting");
}

/// Start the real server pointed at the dummy VNC target.
async fn spawn_app(vnc_port: u16) -> SocketAddr {
    let config = AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        static_dir: "frontend/dist".into(),
        target: TargetConfig {
            name: "tigervnc-dummy".to_owned(),
            protocol: Protocol::Vnc,
            host: "127.0.0.1".to_owned(),
            port: vnc_port,
            username: String::new(),
            // Must match tests/vnc-dummy/Containerfile — exercises VncAuth.
            password: "secret42".to_owned(),
            domain: None,
            width: 1280,
            height: 800,
            security: Security::Auto, // RDP-only knob, ignored for VNC
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

/// Validate one binary tile frame and return its pixel area.
fn check_tile_frame(frame: &[u8]) -> u64 {
    assert!(frame.len() >= TILE_HEADER_LEN, "frame shorter than the header");
    assert_eq!(frame[0], TILE_FRAME_KIND, "unexpected frame kind");
    assert_eq!(frame[1], TILE_FORMAT_PNG, "unexpected tile format byte");
    let x = u16::from_le_bytes([frame[2], frame[3]]);
    let y = u16::from_le_bytes([frame[4], frame[5]]);
    let w = u16::from_le_bytes([frame[6], frame[7]]);
    let h = u16::from_le_bytes([frame[8], frame[9]]);
    assert!(w > 0 && h > 0, "empty tile {w}x{h}");
    assert!(
        u32::from(x) + u32::from(w) <= DESKTOP_W && u32::from(y) + u32::from(h) <= DESKTOP_H,
        "tile {w}x{h}+{x}+{y} exceeds the {DESKTOP_W}x{DESKTOP_H} desktop"
    );
    assert_eq!(
        &frame[TILE_HEADER_LEN..TILE_HEADER_LEN + 8],
        b"\x89PNG\r\n\x1a\n",
        "payload is not a PNG stream"
    );
    u64::from(w) * u64::from(h)
}

#[tokio::test]
async fn vnc_session_paints_the_full_desktop_as_tiles() {
    let runtime = common::container_runtime();
    let (_container, vnc_port) =
        common::start_dummy_server(runtime, "rdpweb-e2e-tigervnc", "vnc-dummy", 5900);
    wait_for_vnc_port(vnc_port).await;

    let addr = spawn_app(vnc_port).await;
    let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();

    let mut got_resize = false;
    let mut covered: u64 = 0;

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
                        assert_eq!(covered, 0, "resize arrived after tiles");
                        // The size announced must be the VNC server's actual
                        // desktop, not the (RDP-oriented) configured 1280x800.
                        assert_eq!(
                            text,
                            format!(r#"{{"type":"resize","w":{DESKTOP_W},"h":{DESKTOP_H}}}"#)
                        );
                        got_resize = true;
                    }
                }
                Message::Binary(frame) => {
                    assert!(got_resize, "tile arrived before resize");
                    covered += check_tile_frame(&frame);
                    // The first (non-incremental) update must repaint the whole
                    // desktop; once that much area has arrived, the raw->tile
                    // path is proven.
                    if covered >= u64::from(DESKTOP_W) * u64::from(DESKTOP_H) {
                        return;
                    }
                }
                _ => {}
            }
        }
        panic!("websocket closed after {covered} px of tiles without a full paint");
    })
    .await
    .expect("timed out waiting for the full-desktop paint");
}
