//! The native control plane as one process graph: two real gateway children,
//! their private endpoints — Unix sockets, or named pipes on Windows — and one
//! browser-facing TCP port routed by subdomain.

mod common;

use std::net::Ipv4Addr;

use zdesk::embedded::manager::{SharedPort, Supervisor};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn instances_start_stop_and_share_the_master_port() {
    let root = common::ScratchDir::new("tui-instances");

    for name in ["one", "two"] {
        let dir = root.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("zdesk.toml"),
            format!("[branding]\ntext = \"{name}\"\n"),
        )
        .unwrap();
    }

    let mut supervisor =
        Supervisor::open(root.path().to_path_buf(), env!("CARGO_BIN_EXE_zdesk").into())
            .await
            .unwrap();
    let router = SharedPort::bind(free_port(), supervisor.routes()).await.unwrap();
    supervisor.start("one", None).await.unwrap();
    supervisor.start("two", None).await.unwrap();

    let one_cookie = seed_cookie(router.port(), "one").await;
    let two_cookie = seed_cookie(router.port(), "two").await;
    let one = request(router.port(), "one", "/api/config", Some(&one_cookie)).await;
    let two = request(router.port(), "two", "/api/config", Some(&two_cookie)).await;
    assert!(one.contains("\"branding\":\"one\""), "{one}");
    assert!(two.contains("\"branding\":\"two\""), "{two}");

    let landing = request_master(router.port()).await;
    assert!(landing.contains("one.zdesk.localhost"), "{landing}");
    assert!(landing.contains("two.zdesk.localhost"), "{landing}");

    supervisor.stop("one").await.unwrap();
    let stopped = request(router.port(), "one", "/api/config", Some(&one_cookie)).await;
    assert!(stopped.starts_with("HTTP/1.1 503 Service Unavailable"), "{stopped}");
    let still_running = request(router.port(), "two", "/api/config", Some(&two_cookie)).await;
    assert!(still_running.contains("\"branding\":\"two\""), "{still_running}");
    #[cfg(unix)]
    assert!(!root.path().join("one/gateway.sock").exists());

    supervisor.shutdown().await;
    #[cfg(unix)]
    assert!(!root.path().join("two/gateway.sock").exists());
    let gone = request(router.port(), "two", "/api/config", Some(&two_cookie)).await;
    assert!(gone.starts_with("HTTP/1.1 503 Service Unavailable"), "{gone}");
}

/// A port nothing is listening on, found by taking one and letting it go. The
/// control plane never asks the kernel for its port — it is typed into a browser —
/// so a test that wants one out of the way picks it here instead.
fn free_port() -> u16 {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn seed_cookie(port: u16, instance: &str) -> String {
    let response = request(port, instance, "/", None).await;
    assert!(response.starts_with("HTTP/1.1 307 Temporary Redirect"), "{response}");
    response
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("set-cookie")
                .then(|| value.trim().split(';').next().unwrap().to_owned())
        })
        .expect("the control plane seeds the child token cookie")
}

async fn request(port: u16, instance: &str, path: &str, cookie: Option<&str>) -> String {
    let cookie = cookie.map_or(String::new(), |value| format!("Cookie: {value}\r\n"));
    raw_request(
        port,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: {instance}.zdesk.localhost:{port}\r\n{cookie}Connection: close\r\n\r\n"
        ),
    )
    .await
}

async fn request_master(port: u16) -> String {
    raw_request(
        port,
        &format!(
            "GET / HTTP/1.1\r\nHost: zdesk.localhost:{port}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
}

async fn raw_request(port: u16, request: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}
