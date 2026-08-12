//! End-to-end tests of a managed embedded gateway: the handshake it prints,
//! the token it takes in a cookie, the login it refuses, the SPA it serves out of
//! the provided web root, and the way it dies with whatever started it.
//!
//! The real binary, spawned as a child the way a manager spawns it — because every
//! one of those is a property of the *process*, not of a router built in-process.
//! The handshake has to arrive on a pipe, the Unix socket has to be bound,
//! and the shutdown is the whole point: none of that can be observed from inside.
//!
//! No engine ever connects, so the targets point at a port nothing listens on.

mod common;

use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A running embedded gateway, its handshake already read.
struct Embedded {
    child: Child,
    socket: PathBuf,
    token: String,
    dir: common::ScratchDir,
}

impl Drop for Embedded {
    fn drop(&mut self) {
        // Belt and braces: every test that cares about shutdown asserts it
        // explicitly, and a test that failed early must not leave a gateway behind.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Embedded {
    /// Write `config` into a fresh instance directory, start the gateway, and read
    /// its handshake.
    fn start(config: &str) -> Self {
        let dir = common::ScratchDir::new("embedded");
        dir.write("remotex.toml", config);
        let web = web_root(&dir);
        let mut child = Command::new(env!("CARGO_BIN_EXE_remotex"))
            .arg("serve-embedded")
            .arg("--instance-dir")
            .arg(dir.path())
            .arg("--web-root")
            .arg(&web)
            // stdin is the liveness pipe: closing our end is how the parent tells this
            // process to stop, so it must be a pipe and not this test's terminal.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the gateway binary must be built");

        // Taken rather than borrowed, so the reader below owns what is left of stdout
        // after the handshake line.
        let mut stdout = BufReader::new(child.stdout.take().expect("a piped stdout"));
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .expect("the gateway must print a handshake line");
        let handshake: serde_json::Value =
            serde_json::from_str(line.trim_end()).unwrap_or_else(|e| {
                panic!("the handshake must be one line of JSON, got {line:?}: {e}")
            });

        // Both pipes are drained for the rest of the child's life. Nothing reads what
        // arrives — the assertions are all made over HTTP — but a pipe nobody empties
        // fills at 64 KiB and then blocks the gateway inside a `write`, which is a
        // test that hangs rather than fails. Today's output is nowhere near that; the
        // first test to drive real traffic through one of these would find out the
        // hard way, at which point the failure looks like anything but this.
        //
        // Detached deliberately: each thread ends by itself when its pipe closes, and
        // the child is killed on `Drop`.
        std::thread::spawn(move || std::io::copy(&mut stdout, &mut std::io::sink()));
        if let Some(mut stderr) = child.stderr.take() {
            std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
        }
        let socket = PathBuf::from(handshake["socket"].as_str().expect("a socket path"));
        assert_eq!(socket, dir.path().join("gateway.sock"));
        let token = handshake["token"].as_str().expect("a token").to_owned();
        Self {
            child,
            socket,
            token,
            dir,
        }
    }

    /// A `GET` carrying `cookie` verbatim as the `Cookie` header, or none.
    async fn get(&self, path: &str, cookie: Option<&str>) -> (u16, String) {
        let header = match cookie {
            Some(value) => format!("Cookie: {value}\r\n"),
            None => String::new(),
        };
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: embedded.remotex.localhost\r\n{header}Connection: close\r\n\r\n"
        );
        let (status, _head, body) = unix_http_request(&self.socket, &req).await;
        (status, body)
    }

    /// The cookie the master listener seeds, spelled the way a browser sends it
    /// back to the child.
    fn cookie(&self) -> String {
        format!("remotex_session={}", self.token)
    }

    async fn get_authorized(&self, path: &str) -> (u16, String) {
        self.get(path, Some(&self.cookie())).await
    }

    /// Close our end of the liveness pipe: the parent quitting, as the child sees it.
    fn close_stdin(&mut self) {
        drop(self.child.stdin.take());
    }

    /// Whether the child is gone within `timeout`, polled rather than waited on so
    /// a hang is a failed assertion instead of a hung test.
    fn exited_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }
}

/// Send raw HTTP over the child's private transport. The browser-facing TCP hop
/// belongs to the master control plane and is tested with its router.
async fn unix_http_request(path: &std::path::Path, request: &str) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .expect("response has a status code");
    let (head, body) = text.split_once("\r\n\r\n").expect("response has a body");
    (status, head.to_owned(), body.to_owned())
}

/// A stand-in for the built frontend directory: an `index.html` and one asset
/// beside it, which is the whole shape the gateway cares about.
fn web_root(dir: &common::ScratchDir) -> std::path::PathBuf {
    let web = dir.path().join("web");
    std::fs::create_dir_all(web.join("assets")).unwrap();
    std::fs::write(web.join("index.html"), "<!doctype html><title>spa</title>").unwrap();
    std::fs::write(web.join("assets").join("index-abc123.js"), "export {}\n").unwrap();
    web
}

/// One VNC target pointing at the discard port. Never dialed.
fn one_target() -> &'static str {
    "[[targets]]\n\
     name = \"unreachable\"\n\
     protocol = \"vnc\"\n\
     host = \"127.0.0.1\"\n\
     port = 9\n"
}

/// The handshake is the parent's only way in, so it has to be complete, and the socket
/// in it has to be the one that answers — a path printed before the bind would
/// pass a parse and fail a connection.
#[tokio::test]
async fn the_handshake_names_a_socket_that_answers_and_a_token_that_works() {
    let embedded = Embedded::start(one_target());

    assert!(embedded.socket.exists(), "the socket is bound before the handshake");
    assert_eq!(embedded.token.len(), 43, "32 bytes of base64url: {}", embedded.token);

    let (status, body) = embedded.get_authorized("/api/health").await;
    assert_eq!((status, body.as_str()), (200, "ok"));

    let (status, body) = embedded.get_authorized("/api/targets").await;
    assert_eq!(status, 200, "the token must unlock the API: {body}");
    assert!(body.contains("\"unreachable\""), "{body}");
}

/// The launch token is the only way in, and only in the cookie a browser would
/// have been given by a login. A header is not a second way: the page makes these
/// requests, and a page cannot set one.
#[tokio::test]
async fn nothing_but_the_token_gets_past_the_guard() {
    let embedded = Embedded::start(one_target());

    for cookie in [
        None,
        Some("remotex_session="),
        Some("remotex_session=not-the-token"),
        // The right value under the wrong name.
        Some(&format!("session={}", embedded.token) as &str),
    ] {
        let (status, _) = embedded.get("/api/targets", cookie).await;
        assert_eq!(status, 401, "must refuse {cookie:?}");
    }

    // Among other cookies is still found, which is what a real browser sends.
    let (status, _) = embedded
        .get("/api/targets", Some(&format!("other=1; {}", embedded.cookie())))
        .await;
    assert_eq!(status, 200);

    // A bearer header is not another spelling of the cookie credential.
    let req = format!(
        "GET /api/targets HTTP/1.1\r\nHost: embedded.remotex.localhost\r\nAuthorization: Bearer {}\r\n\
         Connection: close\r\n\r\n",
        embedded.token
    );
    let (status, _, _) = unix_http_request(&embedded.socket, &req).await;
    assert_eq!(status, 401, "the credential lives in the cookie now");
}

/// There is no login here, and saying so is not the same as pretending the route
/// was never built: 403 says the request was understood and refused, where a 404
/// would send somebody hunting for a routing bug.
#[tokio::test]
async fn the_login_routes_refuse_rather_than_vanish() {
    let embedded = Embedded::start(one_target());

    let body = r#"{"username":"admin","password":"hunter2"}"#;
    let req = format!(
        "POST /api/auth/login HTTP/1.1\r\nHost: embedded.remotex.localhost\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (status, head, _) = unix_http_request(&embedded.socket, &req).await;
    assert_eq!(status, 403, "there is no login to attempt");
    assert!(
        !head.to_lowercase().contains("set-cookie"),
        "and certainly no cookie: {head}"
    );

    // The public branding route still answers before any cookie exists.
    let (status, body) = embedded.get("/api/config", None).await;
    assert_eq!(status, 200, "the branding is still readable: {body}");
    assert!(body.contains("branding"), "{body}");
}

/// `status` is the exception among the auth routes, and it has to be: the SPA asks
/// it before it renders anything, and on this gateway the honest answer is yes —
/// the master seeded the launch token before proxying the first load. A 403 here
/// would put a login form in front of somebody with nothing to type into it.
#[tokio::test]
async fn the_status_route_answers_for_the_token() {
    let embedded = Embedded::start(one_target());

    let (status, body) = embedded.get_authorized("/api/auth/status").await;
    assert_eq!(status, 200);
    assert!(body.contains("\"authenticated\":true"), "{body}");

    let (status, body) = embedded.get("/api/auth/status", None).await;
    assert_eq!(status, 200, "the question is public; the answer is not yes");
    assert!(body.contains("\"authenticated\":false"), "{body}");
}

/// The cookie has to work on the upgrade too, which is the one request that is not
/// an ordinary HTTP call: `require_auth` runs before the handshake, so getting this
/// wrong is a bare 401 with a desktop that never appears. It is also why the
/// credential is a cookie at all — the page opens this socket, and a page cannot
/// put a header on it.
#[tokio::test]
async fn the_socket_upgrade_takes_the_cookie() {
    let embedded = Embedded::start(one_target());
    let url = "ws://embedded.remotex.localhost/ws?session=not-a-claim";

    let stream = tokio::net::UnixStream::connect(&embedded.socket).await.unwrap();
    let err = tokio_tungstenite::client_async(url, stream)
        .await
        .expect_err("an upgrade with no credential must be refused");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), 401)
        }
        other => panic!("expected an HTTP 401 handshake failure, got: {other:?}"),
    }

    // With the cookie the upgrade completes. The session token is nonsense, so the
    // gateway closes it immediately afterwards (4000) — which is a *session*
    // refusal, past the door this test is about.
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Cookie", embedded.cookie().parse().unwrap());
    let stream = tokio::net::UnixStream::connect(&embedded.socket).await.unwrap();
    let (_socket, response) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("the token must get the upgrade past require_auth");
    assert_eq!(response.status(), 101);
}

/// This gateway serves the same SPA as `remotex serve`: real files as
/// themselves, unknown paths as the index, and — the part worth pinning — the
/// document itself without any credential, because the browser has to be able to
/// load the page before its own scripts can present the cookie to anything.
#[tokio::test]
async fn the_spa_is_served_and_unknown_api_paths_are_not() {
    let embedded = Embedded::start(one_target());

    let (status, body) = embedded.get("/", None).await;
    assert_eq!(status, 200, "the document is public");
    assert!(body.contains("<title>spa</title>"), "{body}");

    let (status, body) = embedded.get_authorized("/assets/index-abc123.js").await;
    assert_eq!((status, body.as_str()), (200, "export {}\n"));

    // A client-side route is the index with a 200, not a 404.
    let (status, body) = embedded.get_authorized("/login").await;
    assert_eq!(status, 200);
    assert!(body.contains("<title>spa</title>"), "{body}");

    // But an unknown API path is still an honest 404 rather than an SPA shell.
    let (status, body) = embedded.get_authorized("/api/nope").await;
    assert_eq!(status, 404);
    assert!(!body.contains("<title>"), "{body}");
}

/// The guarantee the whole arrangement rests on: when the process that started this
/// one goes away, this one goes away — with no signal sent and no cooperation from
/// the parent, which is what a crash or a `kill -9` leaves behind.
#[tokio::test]
async fn closing_the_liveness_pipe_stops_the_gateway() {
    let mut embedded = Embedded::start(one_target());
    // Alive and serving first, so the exit below cannot be a gateway that never
    // started.
    let (status, _) = embedded.get_authorized("/api/health").await;
    assert_eq!(status, 200);

    embedded.close_stdin();

    assert!(
        embedded.exited_within(Duration::from_secs(3)),
        "the gateway must not outlive the parent that started it"
    );
    assert!(
        !embedded.socket.exists(),
        "a graceful liveness-pipe exit removes the private socket"
    );
}

/// A first launch has nothing configured, and that is a state to be served rather
/// than an error: the browser's picker is what says "no targets are configured".
#[tokio::test]
async fn a_config_with_no_targets_still_serves() {
    let embedded = Embedded::start("");

    let (status, body) = embedded.get_authorized("/api/targets").await;
    assert_eq!((status, body.as_str()), (200, "[]"));
}

/// `[server]` is the launcher's to decide, so a config claiming it is refused — loudly,
/// on stderr, and by not starting at all. The message has to name the block and say
/// what does belong, because a manager may show it in a config editor.
#[test]
fn a_server_block_refuses_the_start() {
    let dir = common::ScratchDir::new("embedded-server-block");
    dir.write(
        "remotex.toml",
        &format!("[server]\nlisten = \"0.0.0.0:1234\"\n{}", one_target()),
    );

    let web = web_root(&dir);
    let output = Command::new(env!("CARGO_BIN_EXE_remotex"))
        .arg("serve-embedded")
        .arg("--instance-dir")
        .arg(dir.path())
        .arg("--web-root")
        .arg(&web)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "it must not start: {} said {stderr:?} and printed {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains("[server]"), "{stderr}");
    assert!(stderr.contains("[[targets]]"), "{stderr}");
    assert!(
        output.stdout.is_empty(),
        "and nothing that failed to start may print a handshake: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// `check-config` is what a manager's editor calls before it writes, so its verdicts
/// are the gateway's own: the same text that starts a gateway passes, and the same
/// text that refuses one fails with the reason on stderr.
#[test]
fn check_config_agrees_with_what_the_gateway_would_do() {
    let check = |text: &str, embedded: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remotex"));
        command.arg("check-config");
        if embedded {
            command.arg("--embedded");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), text.as_bytes()).unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    let (ok, stderr) = check(one_target(), true);
    assert!(ok, "a config the gateway serves must pass: {stderr}");
    // An empty one too — that is a first launch.
    assert!(check("", true).0, "no targets yet is not an error for an instance");

    let (ok, stderr) = check("[server]\n", true);
    assert!(!ok, "the embedded-instance rules apply");
    assert!(stderr.contains("[server]"), "{stderr}");

    // Silence is not a pass: the served audience has its own rules, and the same
    // empty file fails there.
    let (ok, stderr) = check("", false);
    assert!(!ok, "a served gateway needs a target");
    assert!(stderr.contains("[[targets]]"), "{stderr}");

    // Malformed TOML is reported as such rather than as a missing field.
    let (ok, stderr) = check("[[targets]\nname = \"x\"\n", true);
    assert!(!ok);
    assert!(stderr.to_lowercase().contains("toml"), "{stderr}");
}

/// Two instances are two gateways: separate directories, separate sockets, separate
/// tokens. Nothing is shared, which is what makes a second one safe to start.
#[tokio::test]
async fn two_instances_share_nothing() {
    let a = Embedded::start(one_target());
    let b = Embedded::start(one_target());

    assert_ne!(a.socket, b.socket);
    assert_ne!(a.token, b.token);
    assert_ne!(a.dir.path(), b.dir.path());

    // And one's cookie is refused by the other, which is the part that matters —
    // spelled the way a browser would send it, because a value in the wrong shape
    // would be refused by a gateway that shared the token and prove nothing.
    let (status, _) = b.get("/api/targets", Some(&a.cookie())).await;
    assert_eq!(status, 401);
    // The same cookie is what gets in at home, so the refusal above is about which
    // gateway minted it and not about the request.
    let (status, _) = a.get("/api/targets", Some(&a.cookie())).await;
    assert_eq!(status, 200);
}
