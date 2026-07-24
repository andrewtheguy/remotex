//! Shared helpers for the e2e tests: claim the session slot over the HTTP
//! API, locate a container runtime, build a dummy-server image, and run it
//! with cleanup-on-drop. (Never a headless browser — see CLAUDE.md.)
//!
//! Each test binary uses a subset of these, so the helpers are individually
//! `#[allow(dead_code)]`.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;

/// The web-login credentials every test server is configured with (phase 7).
#[allow(dead_code)]
pub const TEST_USER: &str = "admin";
#[allow(dead_code)]
pub const TEST_PASSWORD: &str = "hunter2";

/// The parsed `site_passwd` for [`TEST_USER`]/[`TEST_PASSWORD`], for building
/// an `AppConfig` directly. bcrypt's minimum cost keeps logins fast in tests.
#[allow(dead_code)]
pub fn test_site_passwd() -> rdpweb::auth::SitePasswd {
    let encoded = rdpweb::auth::generate(TEST_USER, TEST_PASSWORD, 4).unwrap();
    rdpweb::auth::SitePasswd::parse(&encoded).unwrap()
}

/// Send a raw HTTP/1.1 request (the tests don't pull in an HTTP client) and
/// return the status code, the raw response header block, and the body.
#[allow(dead_code)]
pub async fn http_request(addr: SocketAddr, request: &str) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("response has a status code");
    let (head, body) = text.split_once("\r\n\r\n").expect("response has a body");
    (status, head.to_owned(), body.to_owned())
}

/// Log in with the test credentials and return the `name=token` cookie pair to
/// send back in `Cookie` headers.
#[allow(dead_code)]
pub async fn login(addr: SocketAddr) -> String {
    let body = format!(r#"{{"username":"{TEST_USER}","password":"{TEST_PASSWORD}"}}"#);
    let req = format!(
        "POST /api/auth/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (status, head, body) = http_request(addr, &req).await;
    assert_eq!(status, 200, "login failed: {body}");
    let cookie = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("set-cookie").then(|| value.trim())
        })
        .expect("login sets the session cookie");
    // "rdpweb_session=<token>; HttpOnly; …" → the name=token pair.
    cookie.split(';').next().unwrap().to_owned()
}

/// POST a JSON body to `/api/session` with the login cookie. Returns the
/// status code and body.
#[allow(dead_code)]
pub async fn post_session(addr: SocketAddr, cookie: &str, body: &str) -> (u16, String) {
    let req = format!(
        "POST /api/session HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         Cookie: {cookie}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (status, _head, body) = http_request(addr, &req).await;
    (status, body)
}

/// Claim the single session slot, panicking on refusal. Returns the token to
/// present as `/ws?session=<token>`.
#[allow(dead_code)]
pub async fn claim_session(addr: SocketAddr, cookie: &str) -> String {
    let (status, body) = post_session(addr, cookie, "{}").await;
    assert_eq!(status, 200, "session claim failed: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    json["sessionId"]
        .as_str()
        .expect("claim response carries a sessionId")
        .to_owned()
}

pub type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// Open the session WebSocket with a claim token and the login cookie.
#[allow(dead_code)]
pub async fn connect_ws(addr: SocketAddr, token: &str, cookie: &str) -> Ws {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = format!("ws://{addr}/ws?session={token}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    let (ws, _resp) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws
}

/// Locate a container runtime. The dummy remote-desktop server is part of the
/// e2e contract, so a machine without one fails loudly instead of silently
/// skipping the coverage.
#[allow(dead_code)]
pub fn container_runtime() -> &'static str {
    for runtime in ["podman", "docker"] {
        if Command::new(runtime)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
        {
            return runtime;
        }
    }
    panic!("this e2e test needs podman or docker to start the dummy server");
}

/// Kills the container on drop so a failed test doesn't leak it
/// (`--rm` then removes it).
#[allow(dead_code)]
pub struct Container {
    runtime: &'static str,
    name: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime)
            .args(["rm", "-f", &self.name])
            .output();
    }
}

/// Build the image from `tests/<context>` (cached after the first run) and
/// start it with the container's `internal_port` published on an ephemeral
/// localhost port. Returns the container guard and the published port.
#[allow(dead_code)]
pub fn start_dummy_server(
    runtime: &'static str,
    image: &str,
    context: &str,
    internal_port: u16,
) -> (Container, u16) {
    let context_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(context);
    let build = Command::new(runtime)
        .args(["build", "-t", image])
        .arg(&context_dir)
        .output()
        .expect("run container build");
    assert!(
        build.status.success(),
        "container build failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    // Grab a free port; the tiny window before the container binds it is fine.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let name = format!("{image}-{port}");
    let container = Container { runtime, name: name.clone() };
    let run = Command::new(runtime)
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "-p",
            &format!("127.0.0.1:{port}:{internal_port}"),
            image,
        ])
        .output()
        .expect("run container");
    assert!(
        run.status.success(),
        "container start failed:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    (container, port)
}
