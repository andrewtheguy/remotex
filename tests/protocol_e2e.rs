//! Protocol-level end-to-end tests.
//!
//! These drive the real axum server (HTTP + the `/ws` WebSocket) but do NOT
//! use a real browser or real remote-desktop servers. Two fakes stand in for
//! the remote end:
//!
//! - an "RDP" endpoint that drops every connection, so the session fails to
//!   connect and the failure is reported back over the WebSocket as a
//!   `ServerMsg::Error` — exercising the full bridge (claim → upgrade → input
//!   parsing → rdp session → serialized `ServerMsg` out);
//! - a scripted RFB 3.8 server (security None, 16x16 raw framebuffer) that
//!   stays alive, so the phase-6 session-slot semantics — claim conflicts,
//!   forced takeover with eviction, detach/reattach with a full repaint — run
//!   against a live engine deterministically.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::{Ws, connect_ws};
use futures_util::{SinkExt as _, StreamExt as _};
use rdpweb::config::{AppConfig, Protocol, Security, TargetConfig};
use rdpweb::server;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

const FAKE_DESKTOP: u16 = 16;

/// A fake "RDP" endpoint that accepts connections and immediately drops them.
///
/// This makes the RDP handshake fail deterministically (the peer resets before
/// negotiation completes) without racing on connection-refused and without
/// colliding with another parallel test's ephemeral port — we own this port for
/// the test's lifetime. Returns the port it listens on.
async fn spawn_rejecting_rdp() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream); // hang up immediately
        }
    });
    port
}

/// A scripted RFB 3.8 server: completes the handshake with security None,
/// announces a 16x16 desktop, then answers every **non-incremental**
/// FramebufferUpdateRequest with one full raw-encoded update (incremental
/// requests are left pending, like a real server with no screen changes).
/// Everything else the engine sends is consumed and ignored.
async fn spawn_fake_vnc() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = serve_fake_vnc(stream).await;
            });
        }
    });
    port
}

async fn serve_fake_vnc(mut stream: TcpStream) -> std::io::Result<()> {
    // Version + security (None) + ClientInit/ServerInit.
    stream.write_all(b"RFB 003.008\n").await?;
    stream.read_exact(&mut [0u8; 12]).await?; // client version
    stream.write_all(&[1, 1]).await?; // one security type: None
    stream.read_exact(&mut [0u8; 1]).await?; // client's choice
    stream.write_all(&0u32.to_be_bytes()).await?; // SecurityResult: ok
    stream.read_exact(&mut [0u8; 1]).await?; // ClientInit (shared flag)

    let mut server_init = Vec::new();
    server_init.extend_from_slice(&FAKE_DESKTOP.to_be_bytes());
    server_init.extend_from_slice(&FAKE_DESKTOP.to_be_bytes());
    server_init.extend_from_slice(&[0u8; 16]); // native pixel format (overridden)
    server_init.extend_from_slice(&4u32.to_be_bytes());
    server_init.extend_from_slice(b"fake");
    stream.write_all(&server_init).await?;

    loop {
        let mut msg_type = [0u8; 1];
        stream.read_exact(&mut msg_type).await?;
        match msg_type[0] {
            // SetPixelFormat
            0 => {
                stream.read_exact(&mut [0u8; 19]).await?;
            }
            // SetEncodings
            2 => {
                let mut head = [0u8; 3];
                stream.read_exact(&mut head).await?;
                let count = u16::from_be_bytes([head[1], head[2]]);
                let mut encodings = vec![0u8; usize::from(count) * 4];
                stream.read_exact(&mut encodings).await?;
            }
            // FramebufferUpdateRequest
            3 => {
                let mut req = [0u8; 9];
                stream.read_exact(&mut req).await?;
                if req[0] != 0 {
                    continue; // incremental: nothing changed, stay quiet
                }
                let mut update = vec![0u8, 0]; // FramebufferUpdate + padding
                update.extend_from_slice(&1u16.to_be_bytes()); // one rect
                update.extend_from_slice(&0u16.to_be_bytes()); // x
                update.extend_from_slice(&0u16.to_be_bytes()); // y
                update.extend_from_slice(&FAKE_DESKTOP.to_be_bytes());
                update.extend_from_slice(&FAKE_DESKTOP.to_be_bytes());
                update.extend_from_slice(&0i32.to_be_bytes()); // raw encoding
                // BGRX pixels (the format the engine forces).
                update.extend_from_slice(&vec![
                    0x40u8;
                    usize::from(FAKE_DESKTOP) * usize::from(FAKE_DESKTOP) * 4
                ]);
                stream.write_all(&update).await?;
            }
            // KeyEvent
            4 => {
                stream.read_exact(&mut [0u8; 7]).await?;
            }
            // PointerEvent
            5 => {
                stream.read_exact(&mut [0u8; 5]).await?;
            }
            // ClientCutText
            6 => {
                let mut head = [0u8; 7];
                stream.read_exact(&mut head).await?;
                let len = u32::from_be_bytes([head[3], head[4], head[5], head[6]]);
                tokio::io::copy(&mut (&mut stream).take(u64::from(len)), &mut tokio::io::sink())
                    .await?;
            }
            other => panic!("fake vnc server got unexpected message type {other}"),
        }
    }
}

/// Install the ring crypto provider once (the binary does this in `main`; tests
/// don't run `main`, so a code path that reaches TLS would otherwise panic).
fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
}

/// Start the server on an ephemeral port against the given target. Returns
/// the bound address.
async fn spawn_app(target: TargetConfig) -> SocketAddr {
    ensure_crypto_provider();
    let config = AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        static_dir: "frontend/dist".into(),
        target,
        site_passwd: common::test_site_passwd(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

fn target(protocol: Protocol, port: u16) -> TargetConfig {
    TargetConfig {
        name: "test-target".to_owned(),
        protocol,
        host: "127.0.0.1".to_owned(),
        port,
        username: "tester".to_owned(),
        password: "s3cr3t-should-not-leak".to_owned(),
        domain: None,
        width: 1280,
        height: 800,
        security: Security::Auto,
        resize: false,
    }
}

/// Start the app against the connection-dropping RDP endpoint.
async fn spawn_app_dead_rdp() -> SocketAddr {
    let dead_rdp_port = spawn_rejecting_rdp().await;
    spawn_app(target(Protocol::Rdp, dead_rdp_port)).await
}

/// Minimal HTTP/1.1 GET (optionally with the login cookie) returning the
/// response body as a string.
async fn http_get(addr: SocketAddr, path: &str, cookie: Option<&str>) -> String {
    let cookie_line = cookie.map(|c| format!("Cookie: {c}\r\n")).unwrap_or_default();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{cookie_line}Connection: close\r\n\r\n"
    );
    let (_status, _head, body) = common::http_request(addr, &req).await;
    body
}

/// Read from the socket until a `resize` control message arrives; fails on an
/// `error` message or a close.
async fn expect_resize(ws: &mut Ws, w: u16, h: u16) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                    if text.contains(r#""type":"resize""#) {
                        assert_eq!(text, format!(r#"{{"type":"resize","w":{w},"h":{h}}}"#));
                        return;
                    }
                }
                Message::Close(frame) => panic!("closed while waiting for resize: {frame:?}"),
                _ => {}
            }
        }
        panic!("websocket ended while waiting for resize");
    })
    .await
    .expect("timed out waiting for resize");
}

/// Read from the socket until a binary tile frame arrives.
async fn expect_tile(ws: &mut Ws) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Binary(frame) => {
                    assert_eq!(frame[0], 0x01, "unexpected frame kind");
                    return;
                }
                Message::Text(text) => {
                    assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                }
                Message::Close(frame) => panic!("closed while waiting for a tile: {frame:?}"),
                _ => {}
            }
        }
        panic!("websocket ended while waiting for a tile");
    })
    .await
    .expect("timed out waiting for a tile");
}

/// Read from the socket until it closes; returns the close code (if any).
async fn expect_close(ws: &mut Ws) -> Option<u16> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Close(frame)) => return frame.map(|f| u16::from(f.code)),
                Ok(_) => {}
                Err(_) => return None, // connection dropped without a close frame
            }
        }
        None
    })
    .await
    .expect("timed out waiting for the close")
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let addr = spawn_app_dead_rdp().await;
    // Health stays public (it's a liveness probe) — no login cookie.
    let body = http_get(addr, "/api/health", None).await;
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn config_endpoint_exposes_target_but_never_credentials() {
    let addr = spawn_app_dead_rdp().await;
    let cookie = common::login(addr).await;
    let body = http_get(addr, "/api/config", Some(&cookie)).await;
    assert!(body.contains("127.0.0.1"), "config should report the host: {body}");
    // Credentials must never be serialized to the browser.
    assert!(
        !body.contains("s3cr3t-should-not-leak"),
        "config leaked the password: {body}"
    );
    assert!(!body.contains("tester"), "config leaked the username: {body}");
    assert!(!body.contains("password"), "config mentions a password field: {body}");
}

#[tokio::test]
async fn websocket_reports_rdp_connect_failure_as_error_message() {
    let addr = spawn_app_dead_rdp().await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;

    // Send a realistic input event (also proves inbound JSON parses without error).
    ws.send(Message::text(r#"{"type":"mouseMove","x":10,"y":20}"#))
        .await
        .unwrap();

    // The RDP target is dead, so the server should push back an error ServerMsg.
    let got_error = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            if let Ok(Message::Text(text)) = msg
                && text.contains(r#""type":"error""#)
            {
                return true;
            }
        }
        false
    })
    .await
    .expect("timed out waiting for an error ServerMsg");

    assert!(got_error, "expected an error ServerMsg after a failed RDP connect");
}

#[tokio::test]
async fn websocket_without_a_valid_token_is_closed_with_4000() {
    let addr = spawn_app_dead_rdp().await;
    let cookie = common::login(addr).await;

    // No token at all (authenticated, so the upgrade itself succeeds).
    let mut ws = connect_ws(addr, "", &cookie).await;
    assert_eq!(expect_close(&mut ws).await, Some(4000));

    // A made-up token.
    let mut ws = connect_ws(addr, "not-a-real-token", &cookie).await;
    assert_eq!(expect_close(&mut ws).await, Some(4000));
}

#[tokio::test]
async fn takeover_evicts_the_attached_browser_and_repaints_for_the_new_one() {
    let vnc_port = spawn_fake_vnc().await;
    let addr = spawn_app(target(Protocol::Vnc, vnc_port)).await;
    let cookie = common::login(addr).await;

    // Browser A claims and attaches; the engine connects to the fake VNC
    // server and paints the desktop.
    let token_a = common::claim_session(addr, &cookie).await;
    let mut ws_a = connect_ws(addr, &token_a, &cookie).await;
    expect_resize(&mut ws_a, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws_a).await;

    // Browser B: a plain claim is refused while A is attached…
    let (status, _) = common::post_session(addr, &cookie, "{}").await;
    assert_eq!(status, 409, "a live attachment must block a plain claim");
    // …and A's own token reclaims without force (the reconnect path).
    let (status, _) =
        common::post_session(addr, &cookie, &format!(r#"{{"sessionId":"{token_a}"}}"#)).await;
    assert_eq!(status, 200, "the holder reclaims with its token");
    // That reclaim evicted A's socket; reattach A to a fresh one.
    assert_eq!(expect_close(&mut ws_a).await, Some(4001));
    let token_a = common::claim_session(addr, &cookie).await; // nothing attached now
    let mut ws_a = connect_ws(addr, &token_a, &cookie).await;
    expect_resize(&mut ws_a, FAKE_DESKTOP, FAKE_DESKTOP).await;

    // B takes over with force: A is evicted with 4001, A's token dies, and B
    // gets the desktop repainted from the same still-running engine session.
    let (status, body) = common::post_session(addr, &cookie, r#"{"force":true}"#).await;
    assert_eq!(status, 200, "force takeover must succeed: {body}");
    let token_b = serde_json::from_str::<serde_json::Value>(&body).unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();

    assert_eq!(expect_close(&mut ws_a).await, Some(4001));
    let mut ws_stale = connect_ws(addr, &token_a, &cookie).await;
    assert_eq!(expect_close(&mut ws_stale).await, Some(4000));

    let mut ws_b = connect_ws(addr, &token_b, &cookie).await;
    expect_resize(&mut ws_b, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws_b).await;
}

#[tokio::test]
async fn detach_keeps_the_engine_and_reattach_repaints() {
    let vnc_port = spawn_fake_vnc().await;
    let addr = spawn_app(target(Protocol::Vnc, vnc_port)).await;
    let cookie = common::login(addr).await;

    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    expect_resize(&mut ws, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws).await;

    // Detach: the browser goes away, the engine keeps running.
    ws.close(None).await.unwrap();
    drop(ws);

    // Reattach (same token, reclaim): the engine must re-announce the size
    // and repaint the whole desktop from the running session.
    let (status, body) =
        common::post_session(addr, &cookie, &format!(r#"{{"sessionId":"{token}"}}"#)).await;
    assert_eq!(status, 200, "reclaim after detach failed: {body}");
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut ws = connect_ws(addr, &token, &cookie).await;
    expect_resize(&mut ws, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws).await;
}
