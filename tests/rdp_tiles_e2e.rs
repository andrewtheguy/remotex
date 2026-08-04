//! End-to-end test of the tile transport against a real RDP server.
//!
//! Starts the dummy xrdp container (`tests/xrdp-dummy/`) with podman or
//! docker, points the real axum server at it, and connects a raw WebSocket
//! client. Browser automation deliberately does not validate canvas paint
//! timing or pixels (see CLAUDE.md). The gateway's autologon attempt fails in
//! the container — sesman runs but the user does not exist — so xrdp paints
//! its login screen with the error, and real bitmap updates flow through
//! the whole pipeline: IronRDP session -> `ServerMsg::Tile` -> binary WS
//! frames, which this test validates byte-for-byte against the wire layout
//! documented in `src/protocol.rs` / `frontend/src/protocol.ts`.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use remotex::config::{AppConfig, Protocol, Security, TargetConfig};
use remotex::server;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

/// `Tile::FORMAT_PNG`, spelled out rather than imported: this test is a
/// stand-in for a client, and a client only has the number.
const TILE_FORMAT_PNG: u8 = 1;

/// Wait until xrdp actually answers RDP on the published port.
///
/// A bare TCP-accept probe is not enough: rootless podman's port forwarder
/// accepts immediately and then resets if nothing listens inside yet. So the
/// probe sends an X.224 Connection Request (TPKT-framed) and requires xrdp to
/// send bytes back.
async fn wait_for_rdp_port(port: u16) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    // TPKT header (len 11) + X.224 CR TPDU, no negotiation payload.
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

/// Start the real server pointed at the dummy RDP target (the autologon the
/// gateway always requests fails there, leaving the login screen on show).
async fn spawn_app(rdp_port: u16) -> SocketAddr {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });

    let config = AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        static_dir: "frontend/dist".into(),
        auth: common::test_auth(),
        branding: "remotex".to_owned(),
        dev_hostname: None,
            allow_shell_origin: false,
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
            audio_codec: None,
            render_type: remotex::config::RenderType::Full,
            render_subtype: remotex::config::RenderSubtype::Png,
            render_quality: None,
            render_motion_subtype: None,
            render_motion_quality: None,
            render_motion_debug: false,
            video_codec: None,
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

/// Validate one binary batch frame against the documented layout, returning the
/// rectangles it painted.
fn check_tile_frame(stream: &mut common::TileStream, frame: &[u8]) -> Vec<(u32, u32, u32, u32)> {
    let painted = stream.paint(frame);
    assert!(!painted.is_empty(), "a batch frame with no tiles in it");
    let tiles: Vec<_> = painted
        .into_iter()
        .map(|record| match record {
            common::Painted::Tile(tile) => tile,
            // RFB's, and only RFB's: an RDP session never sends a copy, because
            // nothing in that protocol says a region moved.
            common::Painted::Copy { .. } => panic!("an RDP session sent a copy record"),
        })
        .collect();
    for tile in &tiles {
        assert_eq!(tile.format, TILE_FORMAT_PNG, "unexpected tile format byte");
        assert!(tile.w > 0 && tile.h > 0, "empty tile {}x{}", tile.w, tile.h);
        // Length first: a malformed payload is exactly what these markers are here
        // to catch, and slicing a short one would panic on the index instead of
        // reporting what was wrong.
        assert!(
            tile.payload.len() >= 8,
            "payload is {} bytes, too short to be a PNG",
            tile.payload.len()
        );
        assert_eq!(
            &tile.payload[..8],
            b"\x89PNG\r\n\x1a\n",
            "payload is not a PNG stream"
        );
    }
    tiles
        .iter()
        .map(|tile| {
            (
                u32::from(tile.x),
                u32::from(tile.y),
                u32::from(tile.w),
                u32::from(tile.h),
            )
        })
        .collect()
}

/// Pixel coverage for one announced desktop. Tiles may overlap or repaint a
/// region, so a pixel advances the completion count at most once.
struct TileCoverage {
    width: u32,
    height: u32,
    pixels: Vec<bool>,
    covered: u64,
}

impl TileCoverage {
    fn new(width: u32, height: u32) -> Self {
        let pixels = usize::try_from(u64::from(width) * u64::from(height))
            .expect("desktop is too large to track coverage");
        Self {
            width,
            height,
            pixels: vec![false; pixels],
            covered: 0,
        }
    }

    fn add(&mut self, (x, y, w, h): (u32, u32, u32, u32)) {
        let right = x.saturating_add(w).min(self.width);
        let bottom = y.saturating_add(h).min(self.height);
        let x = x.min(self.width);
        let y = y.min(self.height);
        for y in y..bottom {
            for x in x..right {
                let at = usize::try_from(u64::from(y) * u64::from(self.width) + u64::from(x))
                    .expect("desktop index does not fit usize");
                if !self.pixels[at] {
                    self.pixels[at] = true;
                    self.covered += 1;
                }
            }
        }
    }

    fn is_complete(&self) -> bool {
        self.covered == u64::from(self.width) * u64::from(self.height)
    }
}

#[tokio::test]
#[ignore = "requires Docker or Podman"]
async fn tiles_arrive_as_binary_frames_after_resize_text() {
    common::init_logging();
    let runtime = common::container_runtime();
    let (_container, rdp_port) =
        common::start_dummy_server(runtime, "remotex-e2e-xrdp", "xrdp-dummy", 3389);
    wait_for_rdp_port(rdp_port).await;

    let addr = spawn_app(rdp_port).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = common::connect_ws(addr, &token, &cookie).await;
    // The fresh attach lands on the picker; pick the target to start the engine.
    common::connect_target(&mut ws, "xrdp-dummy").await;

    // The current desktop, from the last `resize` control message. There can be
    // more than one: xrdp's auto-login path hands the connection to its session
    // module through a deactivation-reactivation, which may re-announce the
    // desktop (and at a different height than it first offered).
    let mut coverage: Option<TileCoverage> = None;
    let mut sent_refresh = false;
    // Whether the server's pointer reached this client as a shape. RDP does not
    // draw the cursor into the framebuffer and this end no longer composites it
    // either, so `cursor` is the only way a pointer arrives at all — an
    // attachment that never hears one hides its own and draws nothing.
    let mut owns_pointer = false;
    // Resolves cache references, so this counts pixels *painted* rather than
    // pixels that happened to be on the wire.
    let mut stream = common::TileStream::new();

    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    // The only text frames are control messages; the session
                    // must not fail, and a resize must precede the first tile.
                    assert!(
                        !text.contains(r#""type":"error""#),
                        "session failed: {text}"
                    );
                    let control: serde_json::Value =
                        serde_json::from_str(&text).expect("control message is JSON");
                    if control["type"] == "resize" {
                        let w = u32::try_from(control["w"].as_u64().expect("resize carries w"))
                            .expect("resize width fits u32");
                        let h = u32::try_from(control["h"].as_u64().expect("resize carries h"))
                            .expect("resize height fits u32");
                        assert!(w > 0 && h > 0, "resize dimensions must be positive: {w}x{h}");
                        // A new surface starts blank; what was painted on the
                        // old one no longer counts toward covering it, and it
                        // gets its own refresh.
                        coverage = Some(TileCoverage::new(w, h));
                        sent_refresh = false;
                    }
                    if control["type"] == "cursor" {
                        // `image` is present either way: a base64 PNG, or null
                        // for the client's own arrow.
                        assert!(
                            control.get("image").is_some(),
                            "a cursor message carries an image field: {text}"
                        );
                        // A shape is the whole path proved — the server's
                        // pointer PDU decoded, encoded and put on the wire
                        // instead of into the framebuffer. Only that counts
                        // here; `null` is also what an attach sends before any
                        // pointer PDU has arrived.
                        if !control["image"].is_null() {
                            let w = control["w"].as_u64().expect("a shape carries w");
                            let h = control["h"].as_u64().expect("a shape carries h");
                            assert!(w > 0 && h > 0, "a cursor shape of {w}x{h}");
                            owns_pointer = true;
                        }
                    }
                }
                Message::Binary(frame) => {
                    let coverage = coverage.as_mut().expect("tile arrived before resize");
                    // Area rather than a tile count: how many tiles a paint
                    // splits into is the encoder's business, and how much of
                    // the mostly-black failed-login screen survives the
                    // shadow-copy trim is too. So once tiles flow at all, ask
                    // for a `refresh` — it forgets the shadow and repaints the
                    // whole desktop — and a desktop's worth of painted pixels
                    // becomes the deterministic finish line.
                    for tile in check_tile_frame(&mut stream, &frame) {
                        coverage.add(tile);
                    }
                    if coverage.is_complete() {
                        assert!(
                            owns_pointer,
                            "a whole desktop was painted without the pointer arriving as a shape"
                        );
                        return;
                    }
                    if !sent_refresh {
                        sent_refresh = true;
                        ws.send(Message::Text(r#"{"type":"refresh"}"#.into()))
                            .await
                            .expect("send refresh");
                    }
                }
                _ => {}
            }
        }
        panic!(
            "websocket closed after {} uniquely covered pixels without covering the desktop",
            coverage.as_ref().map_or(0, |coverage| coverage.covered)
        );
    })
    .await
    .expect("timed out waiting for tile frames");
}
