//! End-to-end test of `render_subtype = "classify"` against real devices.
//!
//! No container stands in here: the classifier's whole subject is what real
//! desktop pixels look like, so these tests borrow the operator's own QA
//! machines from `tmp/test_uat.toml` — the Windows box over RDP, the TigerVNC
//! workstation, and the Mac in High Performance mode — override each target's
//! render dial to a `classify` base, and read the session WebSocket a
//! browser would. The Linux workstation is also driven with that base under
//! `motion`, the pairing where a settled cell is classified while a moving one
//! takes the cheaper motion encode.
//!
//! What is asserted is the system's decisions, not the devices' content: every
//! tile names PNG or JPEG in its format byte, every payload begins with the
//! magic of the format it names, and a full repaint of the announced desktop
//! arrives tile by tile. Whether any given tile went lossy depends on what the
//! remote screen happens to show, so the PNG/JPEG split is *reported* rather
//! than asserted — with one exception: a real desktop always has flat regions,
//! so a classify session that produced no PNG at all is a classifier that has
//! stopped saying no.
//!
//! Ignored by default; each needs its device reachable:
//!
//! ```sh
//! cargo test --test classify_render_e2e -- --ignored --nocapture
//! ```

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use remotex::config::{AppConfig, RenderSubtype, RenderType, TargetConfig};
use remotex::server;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// The wire's format bytes, spelled out rather than imported: this test is a
/// stand-in for a client, and a client only has the numbers.
const TILE_FORMAT_PNG: u8 = 1;
const TILE_FORMAT_JPEG: u8 = 2;

/// The quality the classifier's JPEG side runs at here. Any legal value would
/// do — the assertions are about formats, not fidelity.
const QUALITY: u8 = 60;

/// The moving encode's quality when the strategy is `motion`. As arbitrary as
/// [`QUALITY`], and lower for the same reason an operator's would be.
const MOTION_QUALITY: u8 = 15;

/// Put the operator's `name` target on a classify-base dial under `strategy`
/// — [`RenderType::Tiles`] or [`RenderType::Motion`].
fn uat_target(name: &str, strategy: RenderType) -> TargetConfig {
    let mut target = common::uat_target(name);
    target.render_type = strategy;
    target.render_subtype = RenderSubtype::Classify;
    target.render_quality = Some(QUALITY);
    // Under motion the moving encode defaults to jpeg — a still tile either
    // way, so the batch parser below reads every plan this test drives.
    target.render_motion_subtype = None;
    target.render_motion_quality =
        (strategy == RenderType::Motion).then_some(MOTION_QUALITY);
    target.render_motion_debug = false;
    // The outlines are for eyes on a browser; on the wire they would only
    // perturb the payloads this test checks the magic of.
    target.render_classify_debug = false;
    target.render_adaptive = false;
    target.render_adaptive_min = None;
    target
}

/// Serve the real gateway with one real-device target, on an ephemeral port
/// with the shared test login.
async fn spawn_app(target: TargetConfig) -> SocketAddr {
    let config = AppConfig {
        listen: remotex::config::ListenAddr::Tcp("127.0.0.1:0".to_owned()),
        static_dir: "frontend/dist".into(),
        auth: common::test_auth(),
        branding: remotex::config::Branding { text: "remotex".to_owned(), logo: None },
        dev_hostname: None,
        targets: vec![target],
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// The per-format tally one session produced.
#[derive(Default)]
struct Tally {
    png: u64,
    jpeg: u64,
}

/// Connect to `name` on the classify dial and read tiles until the announced
/// desktop is fully painted. Every tile's format byte and payload magic are
/// checked on the way past; the PNG/JPEG split comes back for reporting.
async fn paint_a_whole_desktop(name: &str, strategy: RenderType) -> Tally {
    common::init_logging();
    let addr = spawn_app(uat_target(name, strategy)).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = common::connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, name).await;

    let mut coverage: Option<common::TileCoverage> = None;
    let mut sent_refresh = false;
    let mut stream = common::TileStream::new();
    let mut tally = Tally::default();

    tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                    let control: serde_json::Value =
                        serde_json::from_str(&text).expect("control message is JSON");
                    if control["type"] == "resize" {
                        let w = u32::try_from(control["w"].as_u64().expect("resize carries w"))
                            .expect("resize width fits u32");
                        let h = u32::try_from(control["h"].as_u64().expect("resize carries h"))
                            .expect("resize height fits u32");
                        assert!(w > 0 && h > 0, "resize dimensions must be positive: {w}x{h}");
                        // A new surface starts blank; nothing painted on the
                        // old one counts toward covering it.
                        coverage = Some(common::TileCoverage::new(w, h));
                        sent_refresh = false;
                    }
                }
                Message::Binary(frame) => {
                    let coverage = coverage.as_mut().expect("tile arrived before resize");
                    for painted in stream.paint(&frame) {
                        if let common::Painted::Tile(tile) = &painted {
                            check_tile(tile, &mut tally);
                        }
                        // A copy paints pixels the client already checked when
                        // they first arrived; only its geometry counts here.
                        coverage.add(painted.rect());
                    }
                    if coverage.is_complete() {
                        return;
                    }
                    // Tiles flow, so the engine is live: ask for the repaint
                    // that makes a desktop's worth of pixels the finish line.
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
            coverage.as_ref().map_or(0, |coverage| coverage.covered())
        );
    })
    .await
    .expect("timed out before the desktop was fully painted");

    println!("{name}: {} png tile(s), {} jpeg tile(s)", tally.png, tally.jpeg);
    assert!(
        tally.png > 0,
        "{name}: a real desktop was painted whole without one PNG tile — the classifier \
         is not declining anything"
    );
    tally
}

/// One tile's wire claims, checked against each other: the format byte must be
/// one of the two the classifier chooses between, and the payload must begin
/// with that format's magic — a JPEG in PNG clothing would decode as neither.
fn check_tile(tile: &common::BatchTile, tally: &mut Tally) {
    assert!(tile.w > 0 && tile.h > 0, "empty tile {}x{}", tile.w, tile.h);
    match tile.format {
        TILE_FORMAT_PNG => {
            assert!(
                tile.payload.len() >= 8 && tile.payload[..8] == *b"\x89PNG\r\n\x1a\n",
                "a tile marked PNG does not carry a PNG stream"
            );
            tally.png += 1;
        }
        TILE_FORMAT_JPEG => {
            assert!(
                tile.payload.len() >= 3 && tile.payload[..3] == [0xFF, 0xD8, 0xFF],
                "a tile marked JPEG does not carry a JPEG stream"
            );
            tally.jpeg += 1;
        }
        other => panic!("unexpected tile format byte {other}"),
    }
}

#[tokio::test]
#[ignore = "needs the real Windows RDP host from tmp/test_uat.toml"]
async fn classify_paints_the_windows_desktop_over_rdp() {
    paint_a_whole_desktop("windows", RenderType::Tiles).await;
}

#[tokio::test]
#[ignore = "needs the real TigerVNC workstation from tmp/test_uat.toml"]
async fn classify_paints_the_linux_desktop_over_vnc() {
    paint_a_whole_desktop("workstationlinux", RenderType::Tiles).await;
}

#[tokio::test]
#[ignore = "needs the real TigerVNC workstation from tmp/test_uat.toml"]
async fn a_classify_base_paints_the_linux_desktop_under_motion() {
    paint_a_whole_desktop("workstationlinux", RenderType::Motion).await;
}

#[tokio::test]
#[ignore = "needs the real Mac in High Performance mode from tmp/test_uat.toml"]
async fn classify_paints_the_mac_desktop_in_high_performance_mode() {
    paint_a_whole_desktop("sandbox2highperf", RenderType::Tiles).await;
}
