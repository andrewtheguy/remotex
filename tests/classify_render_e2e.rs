//! End-to-end test of `render_subtype = "classify"` against a real device.
//!
//! No container stands in here: the classifier's whole subject is what real
//! desktop pixels look like, so this test borrows one of the operator's own QA
//! machines from `tmp/test_uat.toml`, overrides its render dial to a `classify`
//! base, and reads the session WebSocket a browser would. One case adds `motion`
//! on top, the pairing where a settled cell is classified while a moving one
//! takes the cheaper motion encode.
//!
//! **Which machine is the operator's to say, and this file names none.** A
//! hostname or a target name written here is a fact about somebody's lab that
//! ages out of date on its own, silently, until a run fails for a reason that has
//! nothing to do with the classifier. So [`TARGET_ENV`] names the target inside
//! that config, and the run says which device it drove. Point it at the Windows
//! box, at a Linux VNC host, at the Mac in High Performance mode — the assertions
//! below are about this gateway's decisions and hold for any of them.
//!
//! What is asserted is the system's decisions, not the device's content: every
//! tile names PNG or WebP in its format byte, every payload
//! begins with the magic of the format it names, and a full repaint of the
//! announced desktop arrives tile by tile. Whether any given tile went lossy
//! depends on what the remote screen happens to show, so the split is *reported*
//! rather than asserted — with one exception: a real desktop always has flat
//! regions, so a classify session that produced no PNG at all is a classifier
//! that has stopped saying no.
//!
//! Ignored by default, and one test rather than two: the cases share a device,
//! so they run in sequence inside it. It needs the named device reachable:
//!
//! ```sh
//! REMOTEX_UAT_TARGET=<target in tmp/test_uat.toml> \
//!   cargo test --test classify_render_e2e -- --ignored --nocapture
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
const TILE_FORMAT_WEBP: u8 = 2;

/// The quality the classifier's lossy side runs at here. Any legal value would
/// do — the assertions are about formats, not fidelity.
const QUALITY: u8 = 60;

/// The moving encode's quality when `render_motion` is on. As arbitrary as
/// [`QUALITY`], and lower for the same reason an operator's would be.
const MOTION_QUALITY: u8 = 15;

/// Which target in `tmp/test_uat.toml` these tests drive. An environment
/// variable rather than a name in this file: see the module docs — the devices
/// are the operator's, and the ones written into a test go stale where nobody is
/// looking.
const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";

/// The target the operator pointed this run at.
fn target_name() -> String {
    std::env::var(TARGET_ENV).unwrap_or_else(|_| {
        panic!(
            "set {TARGET_ENV} to the name of a target in tmp/test_uat.toml — these tests \
             drive a real desktop and this file deliberately names none"
        )
    })
}

/// Put that target on a classify-base tiles dial, with or without the motion
/// discount on top of it.
fn uat_target(name: &str, motion: bool) -> TargetConfig {
    let mut target = common::uat_target(name);
    // Whatever the operator has this target set to. A target already on
    // `render_type = "video"` resolves to a whole-desktop VP9 plan and never
    // reads the subtype at all, so the classify dial below would be set and
    // ignored — no tiles, and a timeout blaming the device.
    target.render_type = RenderType::Tiles;
    target.render_subtype = Some(RenderSubtype::Classify);
    target.image_quality = Some(QUALITY);
    target.render_motion = motion;
    target.video_quality = motion.then_some(MOTION_QUALITY);
    target.render_motion_debug = false;
    // The outlines are for eyes on a browser; on the wire they would only
    // perturb the payloads this test checks the magic of.
    target.render_classify_debug = false;
    // The walk is on by default; this test wants the one quality it configured,
    // and off is only a key a streaming target may write.
    target.render_adaptive = motion.then_some(false);
    target.render_adaptive_min = None;
    target
}

/// Serve the real gateway with one real-device target, on an ephemeral port
/// with the shared test login.
async fn spawn_app(target: TargetConfig) -> SocketAddr {
    let config = AppConfig {
        listen: remotex::config::ListenAddr::Tcp("127.0.0.1:0".to_owned()),
        auth: common::test_auth(),
        branding: remotex::config::Branding { text: "remotex".to_owned(), logo: None },
        dev_hostname: None,
        meter: None,
        airplay: None,
        targets: vec![target],
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config, Default::default(), None);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// The per-format tally one session produced.
#[derive(Default)]
struct Tally {
    png: u64,
    lossy: u64,
}

/// Connect to the operator's target on the classify dial and read tiles until the
/// announced desktop is fully painted. Every tile's format byte and payload magic
/// are checked on the way past; the lossless/lossy split comes back for reporting.
async fn paint_a_whole_desktop(motion: bool) -> Tally {
    common::init_logging();
    let name = &target_name();
    let addr = spawn_app(uat_target(name, motion)).await;
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

    println!("{name}: {} png tile(s), {} webp tile(s)", tally.png, tally.lossy);
    assert!(
        tally.png > 0,
        "{name}: a real desktop was painted whole without one PNG tile — the classifier \
         is not declining anything"
    );
    tally
}

/// One tile's wire claims, checked against each other: the format byte must be
/// PNG or WebP — a third format would be an encoder the operator did not ask for
/// — and the payload must begin with that format's magic, a WebP in PNG clothing
/// decoding as neither.
fn check_tile(tile: &common::BatchTile, tally: &mut Tally) {
    assert!(tile.w > 0 && tile.h > 0, "empty tile {}x{}", tile.w, tile.h);
    if tile.format == TILE_FORMAT_PNG {
        assert!(
            tile.payload.len() >= 8 && tile.payload[..8] == *b"\x89PNG\r\n\x1a\n",
            "a tile marked PNG does not carry a PNG stream"
        );
        tally.png += 1;
        return;
    }
    assert_eq!(
        tile.format, TILE_FORMAT_WEBP,
        "a classify session sent a tile in another format"
    );
    // The RIFF container's form type sits at byte 8, past the four length bytes.
    assert!(
        tile.payload.len() >= 12
            && tile.payload[..4] == *b"RIFF"
            && tile.payload[8..12] == *b"WEBP",
        "a tile marked WebP does not carry a WebP stream"
    );
    tally.lossy += 1;
}

/// The classifier against a real desktop, two sessions deep. Run it once per
/// device worth covering — an RDP host, a VNC host, a Mac in High Performance
/// mode — by pointing [`TARGET_ENV`] at each in turn.
///
/// **One test rather than two, because the two share a device.** Rust runs
/// the tests of a binary concurrently, so both of these would open two
/// sessions to the same desktop at once — which an RDP host or a Mac answers by
/// evicting or refusing, and the loser fails for a reason that is about the test
/// harness and nothing about the classifier. Sequential here is not a
/// simplification of parallel; it is the only shape that matches one device.
///
/// The two cases in order:
///
/// - the classifier as the whole of the base;
/// - the classifier as the base of a motion plan, where a settled cell is
///   classified while whatever is moving takes the stream instead.
#[tokio::test]
#[ignore = "needs the real device REMOTEX_UAT_TARGET names in tmp/test_uat.toml"]
async fn classify_paints_a_real_desktop() {
    paint_a_whole_desktop(false).await;
    paint_a_whole_desktop(true).await;
}
