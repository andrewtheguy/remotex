//! Shared helpers for the e2e tests: claim the session slot over the HTTP
//! API, locate a container runtime, build a dummy-server image, and run it
//! with cleanup-on-drop. These exercise the wire directly; stable DOM-only
//! browser flows live separately under `tests/playwright/`.
//!
//! Each test binary uses a subset of these, so the helpers are individually
//! `#[allow(dead_code)]`.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::time::Duration;

/// The web-login credentials every test server is configured with.
#[allow(dead_code)]
pub const TEST_USER: &str = "admin";
#[allow(dead_code)]
pub const TEST_PASSWORD: &str = "hunter2";

/// A web login for [`TEST_USER`]/[`TEST_PASSWORD`], for building an `AppConfig`
/// directly. bcrypt's minimum cost keeps logins fast in tests.
///
/// The login rather than a token because these tests are about the gateway a
/// browser reaches; the embedded one has its own suite
/// (`tests/embedded_gateway_e2e.rs`).
#[allow(dead_code)]
pub fn test_auth() -> remotex::auth::GatewayAuth {
    let encoded = remotex::auth::generate(TEST_USER, TEST_PASSWORD, 4).unwrap();
    remotex::auth::GatewayAuth::Login(remotex::auth::SitePasswd::parse(&encoded).unwrap())
}

/// A directory that removes itself, for a test that needs somewhere to put a
/// config file.
///
/// Hand-rolled rather than a `tempfile` dependency — and keyed on a counter as well as
/// the pid, because one test binary makes several of these and two instance
/// directories that turned out to be the same directory would quietly share a
/// config.
#[allow(dead_code)]
pub struct ScratchDir(std::path::PathBuf);

#[allow(dead_code)]
impl ScratchDir {
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "remotex-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Write `contents` to `name` inside the directory.
    pub fn write(&self, name: &str, contents: &str) -> std::path::PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
    // "remotex_session=<token>; HttpOnly; …" → the name=token pair.
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

/// One record parsed out of a batch frame: a tile, or a reference to a tile the
/// client was told to keep.
///
/// A reference carries no payload and no size — those belong to whatever filled
/// the slot — so a test that measures painted area has to resolve it against the
/// tiles it has already seen, exactly as a client does.
#[allow(dead_code)]
pub enum BatchRecord {
    Tile(BatchTile),
    Reference { slot: u16, x: u16, y: u16 },
}

/// One `TILE` record parsed out of a batch frame.
#[derive(Clone)]
#[allow(dead_code)]
pub struct BatchTile {
    pub format: u8,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub slot: u16,
    pub payload: Vec<u8>,
}

/// Parse a server -> client binary frame into its records.
///
/// One parser for every test that looks at painted pixels, because four of them
/// used to decode the header by hand and a wire change had to be applied four
/// times to four subtly different copies. Asserts the envelope's own invariants on
/// the way through — kind, zero flags, a record count that matches the records
/// present, and records that exactly fill the frame — so every test that reads a
/// tile also checks the frame carrying it was well formed.
#[allow(dead_code)]
pub fn batch_records(frame: &[u8]) -> Vec<BatchRecord> {
    use remotex::protocol::batch;

    assert!(
        frame.len() >= batch::HEADER_LEN,
        "frame is shorter than a batch header"
    );
    assert_eq!(frame[0], batch::FRAME_KIND, "unexpected frame kind");
    assert_eq!(frame[1], 0, "flags must be zero");
    let count = u16::from_le_bytes([frame[2], frame[3]]);

    let mut at = batch::HEADER_LEN;
    let mut records = Vec::new();
    while at < frame.len() {
        let le = |o: usize| u16::from_le_bytes([frame[at + o], frame[at + o + 1]]);
        match frame[at] {
            batch::OP_TILE_REF => {
                let slot = le(1);
                assert!(slot < batch::SLOT_COUNT, "slot {slot} is outside the cache");
                records.push(BatchRecord::Reference {
                    slot,
                    x: le(3),
                    y: le(5),
                });
                at += batch::TILE_REF_LEN;
            }
            batch::OP_TILE => {
                let len = u32::from_le_bytes([
                    frame[at + 12],
                    frame[at + 13],
                    frame[at + 14],
                    frame[at + 15],
                ]) as usize;
                let slot = le(2);
                assert!(
                    slot == batch::NO_SLOT || slot < batch::SLOT_COUNT,
                    "slot {slot} is outside the cache"
                );
                let start = at + batch::TILE_HEADER_LEN;
                records.push(BatchRecord::Tile(BatchTile {
                    format: frame[at + 1],
                    slot,
                    x: le(4),
                    y: le(6),
                    w: le(8),
                    h: le(10),
                    payload: frame[start..start + len].to_vec(),
                }));
                at = start + len;
            }
            op => panic!("unknown record op {op}"),
        }
    }
    assert_eq!(at, frame.len(), "records must exactly fill the frame");
    assert_eq!(
        records.len(),
        usize::from(count),
        "the header's count must match the records present"
    );
    records
}

/// A client's-eye view of a batch stream: the tiles each frame *paints*, with
/// references resolved against the slots filled so far.
///
/// Every test that measures painted pixels needs this rather than the raw records,
/// because the gateway may send a tile the client already has as a slot and a
/// position. Keeping the resolution here — one implementation, shaped like a real
/// client's — also means the reference path is exercised by every one of those
/// tests instead of only by a unit test of the encoder.
#[allow(dead_code)]
pub struct TileStream {
    slots: Vec<Option<BatchTile>>,
    /// References seen, so a test can say whether the cache was exercised at all.
    pub references: u64,
}

#[allow(dead_code)]
impl TileStream {
    pub fn new() -> Self {
        Self {
            slots: vec![None; usize::from(remotex::protocol::batch::SLOT_COUNT)],
            references: 0,
        }
    }

    /// The tiles `frame` paints, in wire order.
    ///
    /// Panics on a reference to an empty slot: a real client answers that with a
    /// `cacheReset`, but in a test it means the gateway and the client disagree about
    /// what was sent, which is the bug this would otherwise hide.
    pub fn paint(&mut self, frame: &[u8]) -> Vec<BatchTile> {
        let mut painted = Vec::new();
        for record in batch_records(frame) {
            match record {
                BatchRecord::Tile(tile) => {
                    if tile.slot != remotex::protocol::batch::NO_SLOT {
                        self.slots[usize::from(tile.slot)] = Some(tile.clone());
                    }
                    painted.push(tile);
                }
                BatchRecord::Reference { slot, x, y } => {
                    self.references += 1;
                    let held = self.slots[usize::from(slot)]
                        .clone()
                        .unwrap_or_else(|| panic!("reference to empty slot {slot}"));
                    painted.push(BatchTile { x, y, ..held });
                }
            }
        }
        painted
    }
}

impl Default for TileStream {
    fn default() -> Self {
        Self::new()
    }
}

/// Let the gateway log during an opted-in e2e run.
///
/// The point is the per-attachment wire totals (`ws: outbound totals:`,
/// `src/ws.rs`): they are the only measurement of the browser link this repo has,
/// and without this they could only be read off a hand-driven live session. A
/// container run is repeatable, so it is the baseline a transport change gets
/// compared against — visible with `-- --ignored --nocapture` and `RUST_LOG=info`.
///
/// Silent unless `RUST_LOG` asks for output, and idempotent because several tests
/// in one binary may call it.
#[allow(dead_code)]
pub fn init_logging() {
    let _ = env_logger::try_init();
}

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

/// Pick a target from the picker over an attached WebSocket, starting its
/// engine. A fresh attach lands on the picker (no engine); the browser sends
/// this `connect` to begin a session. Reattach/takeover to a running engine
/// need no connect — the slot announces `connected` on its own.
#[allow(dead_code)]
pub async fn connect_target(ws: &mut Ws, target: &str) {
    use futures_util::SinkExt as _;
    use tokio_tungstenite::tungstenite::Message;

    ws.send(Message::text(format!(
        r#"{{"type":"connect","target":"{target}"}}"#
    )))
    .await
    .unwrap();
}

/// Locate a *working* container runtime. These tests are ignored by default, so
/// an explicitly opted-in run fails loudly when neither runtime can be reached.
///
/// `info` rather than `--version`: on a machine with both installed, the first
/// one on PATH is often not the one that is actually running — a `podman`
/// binary with no `podman machine` started answers `--version` happily and then
/// fails every real command. Only `info` talks to the engine.
///
/// The probe is bounded: a daemon can be *wedged* rather than absent — a hung
/// Docker Desktop answers the socket and then never replies — and an unbounded
/// `info` would hang the test run with no output instead of moving on to the
/// other runtime.
///
/// `REMOTEX_TEST_CONTAINER_RUNTIME` forces the choice when both work.
#[allow(dead_code)]
pub fn container_runtime() -> &'static str {
    let usable = |runtime: &str| runtime_responds(runtime, Duration::from_secs(10));
    if let Ok(forced) = std::env::var("REMOTEX_TEST_CONTAINER_RUNTIME") {
        let forced: &'static str = Box::leak(forced.into_boxed_str());
        assert!(usable(forced), "REMOTEX_TEST_CONTAINER_RUNTIME={forced} cannot be reached");
        return forced;
    }
    for runtime in ["podman", "docker"] {
        if usable(runtime) {
            return runtime;
        }
    }
    panic!("this e2e test needs a running podman or docker to start the dummy server");
}

/// Whether `<runtime> info` succeeds within `budget`.
///
/// `Command::output()` would wait forever, so this spawns and polls instead.
/// Output is discarded rather than captured: nothing reads it, and a killed
/// child's pipes are one more thing to get wrong.
#[allow(dead_code)]
fn runtime_responds(runtime: &str, budget: Duration) -> bool {
    use std::process::Stdio;

    let Ok(mut child) = Command::new(runtime)
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false; // not installed
    };
    let deadline = std::time::Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            // Wedged, or unwaitable. Reap it so the run leaves nothing behind.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
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

/// The address these tests reach a published container port on.
///
/// `127.0.0.1` for a local engine. Set `REMOTEX_TEST_CONTAINER_HOST` to the
/// engine host's address when the engine is **remote** — a `podman system
/// connection` or `docker context` pointing at another machine over SSH. A
/// remote engine publishes ports on *its own* loopback, so a test connecting to
/// its own `127.0.0.1` finds nothing there; this is the one thing that has to
/// change for a remote engine to work.
///
/// Needed because macOS cannot always run a container engine locally: inside a
/// VM, `podman machine` fails with "Virtualization is not available on this
/// hardware" (no nested virt), and there is nothing to fix on this side.
#[allow(dead_code)]
pub fn container_host() -> String {
    std::env::var("REMOTEX_TEST_CONTAINER_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned())
}

/// Build the image from `tests/<context>` (cached after the first run) and
/// start it with the container's `internal_port` published on an ephemeral port
/// of [`container_host`]. Returns the container guard and the published port.
#[allow(dead_code)]
pub fn start_dummy_server(
    runtime: &'static str,
    image: &str,
    context: &str,
    internal_port: u16,
) -> (Container, u16) {
    let context_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(context);
    // With a remote engine the build context is sent over the connection, so a
    // local path still works.
    //
    // `-f` is spelled out because the file is a `Containerfile`: podman looks
    // for that name by default, docker only for `Dockerfile`, and both accept
    // the explicit flag.
    let build = Command::new(runtime)
        .args(["build", "-t", image, "-f"])
        .arg(context_dir.join("Containerfile"))
        .arg(&context_dir)
        .output()
        .expect("run container build");
    assert!(
        build.status.success(),
        "container build failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    // The engine picks the host port, not this process. The bind happens on the
    // *engine's* host, so a port a local `TcpListener` found free says nothing
    // about a remote engine — and an ephemeral port with no address before the
    // colon is the one form both podman and docker read as "choose one".
    //
    // A remote engine also has to publish on all interfaces for this machine to
    // reach it; a local one stays on loopback so a test never exposes a service
    // to the network.
    let host = container_host();
    let publish = if host == "127.0.0.1" || host == "localhost" {
        format!("127.0.0.1::{internal_port}")
    } else {
        format!("0.0.0.0::{internal_port}")
    };

    // Unique without a port to name it after: pid plus a counter, so two tests
    // in one binary never collide.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let name = format!(
        "{image}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, AtomicOrdering::Relaxed)
    );
    let container = Container { runtime, name: name.clone() };
    let run = Command::new(runtime)
        .args([
            "run", "-d", "--rm", "--name", &name, "-p", &publish, image,
        ])
        .output()
        .expect("run container");
    assert!(
        run.status.success(),
        "container start failed:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    (container, published_port(runtime, &name, internal_port))
}

/// Ask the engine which host port it published `internal_port` on.
///
/// `<runtime> port <name> <port>/tcp` prints one `address:port` line per
/// binding — podman adds an IPv6 one — and every line carries the same host
/// port, so the first parseable one is the answer.
#[allow(dead_code)]
fn published_port(runtime: &'static str, name: &str, internal_port: u16) -> u16 {
    let out = Command::new(runtime)
        .args(["port", name, &format!("{internal_port}/tcp")])
        .output()
        .expect("query the container's published port");
    assert!(
        out.status.success(),
        "could not read the published port:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|line| line.trim().rsplit_once(':')?.1.parse::<u16>().ok())
        .find(|port| *port != 0)
        .unwrap_or_else(|| panic!("no published port in {runtime} port output: {text:?}"))
}
