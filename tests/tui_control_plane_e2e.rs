//! The native control plane as one process graph: two real gateway children,
//! private Unix sockets, and one browser-facing TCP port routed by subdomain.

mod common;

use std::net::Ipv4Addr;

use remotex::embedded::manager::{SharedPort, Supervisor};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn instances_start_stop_and_share_the_master_port() {
    let root = common::ScratchDir::new("tui-instances");
    // The SPA lives outside the instances root, because every directory under that
    // root is an instance: a `web/` beside them is a valid instance name, so
    // `Supervisor::rescan` would adopt it — bootstrapping a `remotex.toml` into the
    // web root and listing the page itself in the TUI.
    let spa = common::ScratchDir::new("tui-web");
    let web = spa.path().join("web");
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(web.join("index.html"), "<!doctype html><title>spa</title>").unwrap();

    for name in ["one", "two"] {
        let dir = root.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("remotex.toml"),
            format!("[branding]\ntext = \"{name}\"\n"),
        )
        .unwrap();
    }

    let mut supervisor = Supervisor::open(
        root.path().to_path_buf(),
        env!("CARGO_BIN_EXE_remotex").into(),
        web,
    )
    .await
    .unwrap();
    let router = SharedPort::bind(free_port(), supervisor.routes()).await.unwrap();
    supervisor.start("one").await.unwrap();
    supervisor.start("two").await.unwrap();

    let one_cookie = seed_cookie(router.port(), "one").await;
    let two_cookie = seed_cookie(router.port(), "two").await;
    let one = request(router.port(), "one", "/api/config", Some(&one_cookie)).await;
    let two = request(router.port(), "two", "/api/config", Some(&two_cookie)).await;
    assert!(one.contains("\"branding\":\"one\""), "{one}");
    assert!(two.contains("\"branding\":\"two\""), "{two}");

    let landing = request_master(router.port()).await;
    assert!(landing.contains("one.remotex.localhost"), "{landing}");
    assert!(landing.contains("two.remotex.localhost"), "{landing}");

    supervisor.stop("one").await.unwrap();
    let stopped = request(router.port(), "one", "/api/config", Some(&one_cookie)).await;
    assert!(stopped.starts_with("HTTP/1.1 503 Service Unavailable"), "{stopped}");
    let still_running = request(router.port(), "two", "/api/config", Some(&two_cookie)).await;
    assert!(still_running.contains("\"branding\":\"two\""), "{still_running}");
    assert!(!root.path().join("one/gateway.sock").exists());

    supervisor.shutdown().await;
    assert!(!root.path().join("two/gateway.sock").exists());
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
            "GET {path} HTTP/1.1\r\nHost: {instance}.remotex.localhost:{port}\r\n{cookie}Connection: close\r\n\r\n"
        ),
    )
    .await
}

async fn request_master(port: u16) -> String {
    raw_request(
        port,
        &format!(
            "GET / HTTP/1.1\r\nHost: remotex.localhost:{port}\r\nConnection: close\r\n\r\n"
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
