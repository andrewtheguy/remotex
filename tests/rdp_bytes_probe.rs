//! How many bytes a mouse sweep costs on an RDP target.
//!
//! An instrument, not a test: it asserts nothing and prints one line. It exists
//! because the repo has no benchmark harness and because a claim about bytes on the
//! wire is worthless without a figure that can be reproduced on demand.
//!
//!   REMOTEX_TEST_CONTAINER_RUNTIME=docker cargo test --release \
//!     --test rdp_bytes_probe -- --ignored --nocapture
//!
//! It is deterministic to the byte — the dummy xrdp login screen does not animate
//! and the sweep is scripted — so two revisions are directly comparable. Measured
//! figures, three runs each:
//!
//! | revision | frames | bytes |
//! |---|---|---|
//! | full-width strips, no comparison | 244–245 | 115,747–115,751 |
//! | trimmed against a shadow copy | 244 | 86,891 |
//! | and with the tile cache | 244 | 27,634 |
//! | outward-snapped 320x64 cells + hash gate (rejected) | 334 | 1,030,637 |
//!
//! Use `--release`: byte counts are the same in either profile, but a debug `png`
//! build is several times slower, so nothing about *time* can be read off one.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use remotex::config::{AppConfig, Protocol, Security, TargetConfig};
use remotex::server;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

async fn wait_for_rdp_port(port: u16) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    const X224_CONNECT: [u8; 11] = [3, 0, 0, 11, 6, 0xe0, 0, 0, 0, 0, 0];
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let attempt = async {
                let mut stream = TcpStream::connect((common::container_host(), port)).await.ok()?;
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

async fn spawn_app(rdp_port: u16) -> SocketAddr {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
    let config = AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        static_dir: Some("frontend/dist".into()),
        auth: common::test_auth(),
        branding: "remotex".to_owned(),
        dev_hostname: None,
        targets: vec![TargetConfig {
            name: "xrdp-dummy".to_owned(),
            protocol: Protocol::Rdp,
            subtype: None,
            host: common::container_host(),
            port: rdp_port,
            username: "dummy".to_owned(),
            password: "dummy".to_owned(),
            vnc_password: String::new(),
            domain: None,
            width: 1280,
            height: 800,
            security: Security::Auto,
            resize: false,
            clipboard: false,
            audio: false,
            agent_public_key: String::new(),
            gateway_private_key: String::new(),
        }],
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test]
#[ignore = "instrument, not a test"]
async fn a_mouse_sweep_over_a_static_desktop() {
    common::init_logging();
    let runtime = common::container_runtime();
    let (_container, rdp_port) =
        common::start_dummy_server(runtime, "remotex-probe-xrdp", "xrdp-dummy", 3389);
    wait_for_rdp_port(rdp_port).await;

    let addr = spawn_app(rdp_port).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = common::connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, "xrdp-dummy").await;

    // Let the login screen finish painting, so the sweep is measured against a
    // desktop that has settled rather than against its first repaint.
    let settle = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < settle {
        let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    }

    let (mut tx, mut rx) = ws.split();
    let sweep = tokio::spawn(async move {
        // 240 positions tracing a diagonal figure across a 1280x800 desktop: the
        // union box IronRDP reports for one of these spans both ends of the move.
        for i in 0..240u32 {
            let x = 40 + (i * 5) % 1200;
            let y = 40 + (i * 3) % 720;
            let msg = format!(r#"{{"type":"mouseMove","x":{x},"y":{y}}}"#);
            if tx.send(Message::Text(msg.into())).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
    });

    let mut frames = 0u64;
    let mut bytes = 0u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.next()).await {
            Ok(Some(Ok(Message::Binary(frame)))) => {
                frames += 1;
                bytes += frame.len() as u64;
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_)) | None) => break,
            Err(_) => {}
        }
    }
    sweep.abort();

    println!("PROBE: {frames} binary frames / {bytes} bytes over a 240-position mouse sweep");
}
