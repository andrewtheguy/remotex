//! Protocol-level end-to-end tests.
//!
//! These drive the real axum server (HTTP + the `/ws` WebSocket) but do NOT use
//! a real browser or a real RDP server. The RDP target is pointed at a closed
//! port, so the session fails to connect and the server reports the failure back
//! over the WebSocket as a `ServerMsg::Error` — exercising the full bridge:
//! WebSocket upgrade → input parsing → rdp session → serialized `ServerMsg` out.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use rdpweb::config::{AppConfig, Security};
use rdpweb::server;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

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

/// Install the ring crypto provider once (the binary does this in `main`; tests
/// don't run `main`, so a code path that reaches TLS would otherwise panic).
fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
}

/// Start the server on an ephemeral port with the RDP target pointed at the
/// rejecting endpoint. Returns the bound address.
async fn spawn_app() -> SocketAddr {
    ensure_crypto_provider();
    let dead_rdp_port = spawn_rejecting_rdp().await;
    let config = AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        rdp_host: "127.0.0.1".to_owned(),
        rdp_port: dead_rdp_port,
        rdp_username: "tester".to_owned(),
        rdp_password: "s3cr3t-should-not-leak".to_owned(),
        rdp_domain: None,
        rdp_width: 1280,
        rdp_height: 800,
        rdp_security: Security::Auto,
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Minimal HTTP/1.1 GET returning the response body as a string (connection is
/// closed by the server after the response, so we read to EOF).
async fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    let (_, body) = text.split_once("\r\n\r\n").expect("response has a body");
    body.to_owned()
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let addr = spawn_app().await;
    let body = http_get(addr, "/api/health").await;
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn config_endpoint_exposes_target_but_never_credentials() {
    let addr = spawn_app().await;
    let body = http_get(addr, "/api/config").await;
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
    let addr = spawn_app().await;
    let url = format!("ws://{addr}/ws");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url).await.unwrap();

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
