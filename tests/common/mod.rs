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

/// Pull `name` out of the operator's UAT config, for a test that borrows a
/// real device. The config is gitignored and machine-local, which is the
/// point: the hosts and credentials of real devices stay out of the
/// repository, and the same file that drives manual QA drives these tests.
/// The caller overrides whatever dial settings its assertions need.
#[allow(dead_code)]
pub fn uat_target(name: &str) -> remotex::config::TargetConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tmp/test_uat.toml");
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("these tests need the local UAT config at {path}: {e}"));
    let config = remotex::config::ConfigFile::parse(&text).expect("tmp/test_uat.toml should parse");
    config
        .targets
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("tmp/test_uat.toml names no target {name:?}"))
}

/// A directory that removes itself, for a test that needs somewhere to put a
/// config file.
///
/// `tempfile` does the naming and the removal — a unique directory per call, which
/// matters because one test binary makes several of these and two instance
/// directories that turned out to be the same directory would quietly share a
/// config. What is left here is the `tag`, so a path that survives a failure says
/// which test made it, and the `write` helper below.
#[allow(dead_code)]
pub struct ScratchDir(tempfile::TempDir);

#[allow(dead_code)]
impl ScratchDir {
    pub fn new(tag: &str) -> Self {
        Self(
            tempfile::Builder::new()
                .prefix(&format!("remotex-{tag}-"))
                .tempdir()
                .unwrap(),
        )
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// Write `contents` to `name` inside the directory.
    pub fn write(&self, name: &str, contents: &str) -> std::path::PathBuf {
        let path = self.0.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path
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

/// One WebSocket to the gateway.
pub type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// A browser's view of its session: the session socket and the first display's
/// socket beside it, read as one stream, as the page reads them into one handler.
///
/// Messages leave on the session socket, except a paint acknowledgment, which
/// belongs to the display socket whose batch it acknowledges. The stream ends when
/// both sockets have.
pub struct Ws {
    session: Socket,
    display: Socket,
    session_done: bool,
    display_done: bool,
    /// Which socket is polled first next time, so neither starves the other.
    display_first: bool,
}

impl futures_util::Stream for Ws {
    type Item = Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = &mut *self;
        this.display_first = !this.display_first;
        for display in [this.display_first, !this.display_first] {
            let (socket, done) = if display {
                (&mut this.display, &mut this.display_done)
            } else {
                (&mut this.session, &mut this.session_done)
            };
            if *done {
                continue;
            }
            match std::pin::Pin::new(socket).poll_next(cx) {
                Poll::Ready(Some(item)) => return Poll::Ready(Some(item)),
                Poll::Ready(None) => *done = true,
                Poll::Pending => {}
            }
        }
        if this.session_done && this.display_done {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

impl futures_util::Sink<tokio_tungstenite::tungstenite::Message> for Ws {
    type Error = tokio_tungstenite::tungstenite::Error;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        use std::task::Poll;
        let this = &mut *self;
        match std::pin::Pin::new(&mut this.session).poll_ready(cx) {
            Poll::Ready(Ok(())) => std::pin::Pin::new(&mut this.display).poll_ready(cx),
            other => other,
        }
    }

    fn start_send(
        mut self: std::pin::Pin<&mut Self>,
        item: tokio_tungstenite::tungstenite::Message,
    ) -> Result<(), Self::Error> {
        let to_display = matches!(&item, tokio_tungstenite::tungstenite::Message::Text(text)
            if text.as_str().contains(r#""type":"paintAck""#));
        let this = &mut *self;
        if to_display {
            std::pin::Pin::new(&mut this.display).start_send(item)
        } else {
            std::pin::Pin::new(&mut this.session).start_send(item)
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        use std::task::Poll;
        let this = &mut *self;
        match std::pin::Pin::new(&mut this.session).poll_flush(cx) {
            Poll::Ready(Ok(())) => std::pin::Pin::new(&mut this.display).poll_flush(cx),
            other => other,
        }
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        use std::task::Poll;
        let this = &mut *self;
        match std::pin::Pin::new(&mut this.session).poll_close(cx) {
            Poll::Ready(Ok(())) => std::pin::Pin::new(&mut this.display).poll_close(cx),
            other => other,
        }
    }
}

impl Ws {
    /// The session socket alone: what is not a display's.
    #[allow(dead_code)]
    pub fn session_socket(&mut self) -> &mut Socket {
        &mut self.session
    }

    /// The first display's socket alone: the picture, and its size and pointer.
    #[allow(dead_code)]
    pub fn display_socket(&mut self) -> &mut Socket {
        &mut self.display
    }
}

/// Open display `display`'s socket with nothing but the login cookie, as a page
/// of the browser holding the session does.
#[allow(dead_code)]
pub async fn connect_display_ws(addr: SocketAddr, cookie: &str, display: u32) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = format!("ws://{addr}/ws/display?display={display}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    let (ws, _resp) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws
}

/// One `VIDEO` record parsed out of a batch frame: an access unit of a stream the
/// remote coded, passed. No test here decodes it; what is checked is the envelope and
/// the geometry.
#[allow(dead_code)]
pub struct BatchUnit {
    pub keyframe: bool,
    pub w: u16,
    pub h: u16,
    pub payload: Vec<u8>,
}

/// One `TILE` record parsed out of a batch frame: a rectangle of the desktop in the
/// tiles the gateway codes, its image a JPEG or a PNG.
#[allow(dead_code)]
pub struct BatchTile {
    pub format: u8,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub image: Vec<u8>,
}

/// One `REGION` record parsed out of a batch frame: an access unit of a moving
/// region's stream in wlshare's tiles, passed.
#[allow(dead_code)]
pub struct BatchRegion {
    pub stream: u8,
    pub keyframe: bool,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub payload: Vec<u8>,
}

/// One picture record of a batch frame.
#[allow(dead_code)]
pub enum BatchRecord {
    Video(BatchUnit),
    Tile(BatchTile),
    Region(BatchRegion),
}

/// The acknowledgment a browser's painter sends once it has drawn the batch `frame`,
/// for a test standing in for a browser: a client that acknowledges nothing is one the
/// gateway counts as not drawing, and paces as one.
#[allow(dead_code)]
pub fn paint_ack(frame: &[u8]) -> String {
    let sequence = u32::from_le_bytes(frame[4..8].try_into().expect("a batch header"));
    format!(r#"{{"type":"paintAck","sequence":{sequence},"queuedMs":0,"drawMs":0}}"#)
}

/// Parse a server -> client binary frame into its picture records.
///
/// One parser for every test that looks at what was painted, so a wire change is
/// applied once. Asserts the envelope's own invariants on the way through — kind,
/// zero flags, a record count that matches the records present, and records that
/// exactly fill the frame — so every test that reads a record also checks the frame
/// carrying it was well formed.
#[allow(dead_code)]
pub fn batch_records(frame: &[u8]) -> Vec<BatchRecord> {
    use remotex::protocol::batch;

    assert!(frame.len() >= batch::HEADER_LEN, "frame is shorter than a batch header");
    assert_eq!(frame[0], batch::FRAME_KIND, "unexpected frame kind");
    assert_eq!(frame[1], 0, "flags must be zero");
    let count = u16::from_le_bytes([frame[2], frame[3]]);

    let mut at = batch::HEADER_LEN;
    let mut records = Vec::new();
    while at < frame.len() {
        let le = |o: usize| u16::from_le_bytes([frame[at + o], frame[at + o + 1]]);
        let le32 = |o: usize| {
            u32::from_le_bytes([frame[at + o], frame[at + o + 1], frame[at + o + 2], frame[at + o + 3]])
                as usize
        };
        match frame[at] {
            batch::OP_VIDEO => {
                let flags = frame[at + 1];
                assert_eq!(flags & !batch::VIDEO_KEYFRAME, 0, "unknown record flags {flags:#x}");
                let len = le32(6);
                let start = at + batch::VIDEO_HEADER_LEN;
                records.push(BatchRecord::Video(BatchUnit {
                    keyframe: flags & batch::VIDEO_KEYFRAME != 0,
                    w: le(2),
                    h: le(4),
                    payload: frame[start..start + len].to_vec(),
                }));
                at = start + len;
            }
            batch::OP_TILE => {
                let format = frame[at + 1];
                assert!(
                    format == batch::TILE_JPEG || format == batch::TILE_PNG,
                    "unknown tile format {format:#x}"
                );
                let len = le32(10);
                let start = at + batch::TILE_HEADER_LEN;
                let image = frame[start..start + len].to_vec();
                assert!(!image.is_empty(), "a tile with no image");
                let magic: &[u8] = if format == batch::TILE_JPEG { &[0xff, 0xd8] } else { b"\x89PNG" };
                assert!(image.starts_with(magic), "a tile that is not the image its format says");
                records.push(BatchRecord::Tile(BatchTile { format, x: le(2), y: le(4), w: le(6), h: le(8), image }));
                at = start + len;
            }
            batch::OP_REGION => {
                let flags = frame[at + 2];
                assert_eq!(flags & !batch::REGION_KEYFRAME, 0, "unknown region flags {flags:#x}");
                let len = le32(11);
                let start = at + batch::REGION_HEADER_LEN;
                let payload = frame[start..start + len].to_vec();
                assert!(!payload.is_empty(), "a region's unit with no payload");
                records.push(BatchRecord::Region(BatchRegion {
                    stream: frame[at + 1],
                    keyframe: flags & batch::REGION_KEYFRAME != 0,
                    x: le(3),
                    y: le(5),
                    w: le(7),
                    h: le(9),
                    payload,
                }));
                at = start + len;
            }
            op => panic!("unknown record op {op}"),
        }
    }
    assert_eq!(at, frame.len(), "records must exactly fill the frame");
    assert_eq!(records.len(), usize::from(count), "the header's count must match the records present");
    records
}

/// [`batch_records`] for a frame of a passed stream: its access units, and no tile.
#[allow(dead_code)]
pub fn batch_units(frame: &[u8]) -> Vec<BatchUnit> {
    batch_records(frame)
        .into_iter()
        .map(|record| match record {
            BatchRecord::Video(unit) => unit,
            BatchRecord::Tile(tile) => panic!("a tile at ({}, {}) in a passed stream's batch", tile.x, tile.y),
            BatchRecord::Region(region) => panic!("region {}'s unit in a passed stream's batch", region.stream),
        })
        .collect()
}

/// Which of a desktop's 64×64 tiles the browser has been sent, so a test can tell
/// when the whole desktop has been painted.
#[allow(dead_code)]
pub struct Coverage {
    w: u16,
    h: u16,
    sent: std::collections::HashSet<(u16, u16)>,
}

#[allow(dead_code)]
impl Coverage {
    pub fn new(w: u16, h: u16) -> Self {
        Self { w, h, sent: std::collections::HashSet::new() }
    }

    /// A tile record sent, which must lie on the tile grid and inside the desktop.
    pub fn add(&mut self, tile: &BatchTile) {
        const TILE: u16 = remotex::tiles::TILE;
        assert!(
            tile.x.is_multiple_of(TILE) && tile.y.is_multiple_of(TILE),
            "a tile at ({}, {}) off the {TILE}-pixel grid",
            tile.x,
            tile.y
        );
        assert!(
            u32::from(tile.x) + u32::from(tile.w) <= u32::from(self.w) && u32::from(tile.y) + u32::from(tile.h) <= u32::from(self.h),
            "a {}x{} tile at ({}, {}) past the {}x{} desktop",
            tile.w,
            tile.h,
            tile.x,
            tile.y,
            self.w,
            self.h
        );
        for col in tile.x / TILE..(tile.x + tile.w).div_ceil(TILE) {
            for row in tile.y / TILE..(tile.y + tile.h).div_ceil(TILE) {
                self.sent.insert((col, row));
            }
        }
    }

    /// Whether every tile of the desktop has been sent.
    pub fn whole(&self) -> bool {
        self.sent.len() == usize::from(self.w.div_ceil(remotex::tiles::TILE)) * usize::from(self.h.div_ceil(remotex::tiles::TILE))
    }
}

/// One `STANDARD` record parsed out of a batch frame: one of a Mac's rectangles in
/// its own picture, its body as the Mac sent it.
#[allow(dead_code)]
pub struct StandardRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub body: Vec<u8>,
}

/// Parse a server -> client binary frame into its `STANDARD` records, checking the
/// envelope as [`batch_units`] does.
#[allow(dead_code)]
pub fn standard_rects(frame: &[u8]) -> Vec<StandardRect> {
    use remotex::protocol::batch;

    assert!(frame.len() >= batch::HEADER_LEN, "frame is shorter than a batch header");
    assert_eq!(frame[0], batch::FRAME_KIND, "unexpected frame kind");
    assert_eq!(frame[1], 0, "flags must be zero");
    let count = u16::from_le_bytes([frame[2], frame[3]]);

    let mut at = batch::HEADER_LEN;
    let mut records = Vec::new();
    while at < frame.len() {
        let le = |o: usize| u16::from_le_bytes([frame[at + o], frame[at + o + 1]]);
        assert_eq!(frame[at], batch::OP_STANDARD, "unknown record op {}", frame[at]);
        let len = u32::from_le_bytes(frame[at + 9..at + 13].try_into().unwrap()) as usize;
        let start = at + batch::STANDARD_HEADER_LEN;
        assert!(len > 0, "a STANDARD record with no body");
        records.push(StandardRect {
            x: le(1),
            y: le(3),
            w: le(5),
            h: le(7),
            body: frame[start..start + len].to_vec(),
        });
        at = start + len;
    }
    assert_eq!(at, frame.len(), "records must exactly fill the frame");
    assert_eq!(records.len(), usize::from(count), "the header's count must match the records present");
    records
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

/// Open the session WebSocket with a claim token and the login cookie, and the
/// first display's socket beside it.
///
/// `apple_media=false` is a browser that takes no Mac's stream; the session socket
/// requires every answer, and no target here passes a stream, so these tests give
/// the ordinary desktop ones.
#[allow(dead_code)]
pub async fn connect_ws(addr: SocketAddr, token: &str, cookie: &str) -> Ws {
    connect_ws_stating(addr, token, cookie, false).await
}

/// [`connect_ws`] stating whether this browser decodes a High Performance Mac's
/// stream, which a session started with it passed is served to no other. It
/// composes an RDP host's graphics pipeline either way, decodes no H.264 on
/// one, and decodes 4:4:4 VP9.
#[allow(dead_code)]
pub async fn connect_ws_stating(addr: SocketAddr, token: &str, cookie: &str, apple_media: bool) -> Ws {
    connect_ws_decoding(addr, token, cookie, apple_media, true).await
}

/// [`connect_ws_stating`] stating too whether this browser's own decoder takes
/// 4:4:4 VP9, which decides the chroma a `wlshare` target's stream is sent at.
#[allow(dead_code)]
pub async fn connect_ws_decoding(addr: SocketAddr, token: &str, cookie: &str, apple_media: bool, vp9_444: bool) -> Ws {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = format!(
        "ws://{addr}/ws?session={token}&apple_media={apple_media}&page_hevc=false&rdp_graphics=true&rdp_h264=false&vp9_444={vp9_444}"
    )
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    let (session, _resp) = tokio_tungstenite::connect_async(request).await.unwrap();
    let display = connect_display_ws(addr, cookie, 1).await;
    Ws { session, display, session_done: false, display_done: false, display_first: false }
}

/// Open the audio WebSocket with a claim token and the login cookie.
///
/// The same shape as [`connect_ws`] and deliberately so — a second endpoint that took
/// its credential differently would be a second thing to get wrong.
#[allow(dead_code)]
pub async fn connect_audio_ws(addr: SocketAddr, token: &str, cookie: &str) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = format!("ws://{addr}/ws/audio?session={token}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    let (ws, _resp) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws
}

/// Open the camera WebSocket with a claim token and the login cookie, as
/// [`connect_audio_ws`] opens the audio one. Its first message must be the
/// `cameraFormat` that plugs the camera.
#[allow(dead_code)]
pub async fn connect_camera_ws(addr: SocketAddr, token: &str, cookie: &str) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = format!("ws://{addr}/ws/camera?session={token}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
    let (ws, _resp) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws
}

/// Open the microphone WebSocket with a claim token and the login cookie, as
/// [`connect_audio_ws`] opens the audio one. Opening it is the browser enabling its
/// microphone.
#[allow(dead_code)]
pub async fn connect_mic_ws(addr: SocketAddr, token: &str, cookie: &str) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = format!("ws://{addr}/ws/mic?session={token}")
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
/// this `connect` to begin a session, a takeover's included. A reattach to a
/// running engine needs no connect — the slot announces `connected` on its own. The size is
/// the one the target keeps, with no sound and no passthrough.
#[allow(dead_code)]
pub async fn connect_target(ws: &mut Ws, target: &str) {
    use futures_util::SinkExt as _;
    use tokio_tungstenite::tungstenite::Message;

    ws.send(Message::text(format!(r#"{{"type":"connect","target":"{target}","choices":{{"size":"target"}}}}"#)))
        .await
        .unwrap();
}

/// [`connect_target`] with what the picker's Start would carry: `choices` is the
/// JSON object of the choices ticked under the target, each false unless named.
#[allow(dead_code)]
pub async fn connect_target_with(ws: &mut Ws, target: &str, choices: &str) {
    use futures_util::SinkExt as _;
    use tokio_tungstenite::tungstenite::Message;

    ws.send(Message::text(format!(
        r#"{{"type":"connect","target":"{target}","choices":{choices}}}"#
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

impl Container {
    /// Run a command inside the container and return its stdout, failing the
    /// test if it fails — for asking the server what it did rather than only
    /// what it told the gateway.
    #[allow(dead_code)]
    pub fn exec(&self, args: &[&str]) -> String {
        let out = Command::new(self.runtime)
            .args(["exec", &self.name])
            .args(args)
            .output()
            .expect("run container exec");
        assert!(
            out.status.success(),
            "{args:?} failed in the container:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("container exec output is UTF-8")
    }

    /// What the container's server has written to its stdout and stderr, together:
    /// how a test asks the server what it chose.
    #[allow(dead_code)]
    pub fn logs(&self) -> String {
        let out = Command::new(self.runtime)
            .args(["logs", &self.name])
            .output()
            .expect("run container logs");
        assert!(out.status.success(), "container logs failed:\n{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
    }
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
    start_server_image(runtime, image, &context_dir, &context_dir, internal_port)
}

/// [`start_dummy_server`] with the `Containerfile` in `containerfile_dir` and a
/// build context of its own — for a server built from another checkout, whose
/// sources are the context and whose `.dockerignore` then applies.
#[allow(dead_code)]
pub fn start_server_image(
    runtime: &'static str,
    image: &str,
    containerfile_dir: &Path,
    context_dir: &Path,
    internal_port: u16,
) -> (Container, u16) {
    // With a remote engine the build context is sent over the connection, so a
    // local path still works.
    //
    // `-f` is spelled out because the file is a `Containerfile`: podman looks
    // for that name by default, docker only for `Dockerfile`, and both accept
    // the explicit flag.
    let build = Command::new(runtime)
        .args(["build", "-t", image, "-f"])
        .arg(containerfile_dir.join("Containerfile"))
        .arg(context_dir)
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
