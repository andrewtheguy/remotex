//! End-to-end test of `render_subtype = "classify"` against a real device.
//!
//! No container stands in here: the classifier's whole subject is what real
//! desktop pixels look like, so these tests borrow one of the operator's own QA
//! machines from `tmp/test_uat.toml`, override its render dial to a `classify`
//! base, and read the session WebSocket a browser would. One of them adds
//! `motion` on top, the pairing where a settled cell is classified while a moving
//! one takes the cheaper motion encode.
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
//! tile names PNG or the session's lossy still in its format byte, every payload
//! begins with the magic of the format it names, and a full repaint of the
//! announced desktop arrives tile by tile. Whether any given tile went lossy
//! depends on what the remote screen happens to show, so the split is *reported*
//! rather than asserted — with one exception: a real desktop always has flat
//! regions, so a classify session that produced no PNG at all is a classifier
//! that has stopped saying no.
//!
//! Ignored by default; each needs the named device reachable:
//!
//! ```sh
//! REMOTEX_UAT_TARGET=<target in tmp/test_uat.toml> \
//!   cargo test --test classify_render_e2e -- --ignored --nocapture
//! ```

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use remotex::config::{AppConfig, ClassifyLossy, RenderSubtype, TargetConfig};
use remotex::server;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// The wire's format bytes, spelled out rather than imported: this test is a
/// stand-in for a client, and a client only has the numbers.
const TILE_FORMAT_PNG: u8 = 1;
const TILE_FORMAT_JPEG: u8 = 2;
const TILE_FORMAT_WEBP: u8 = 3;

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

/// Put that target on a classify-base tiles dial, with the named lossy still
/// under the classifier, and with or without the motion discount on top of it.
fn uat_target(name: &str, lossy: ClassifyLossy, motion: bool) -> TargetConfig {
    let mut target = common::uat_target(name);
    target.render_subtype = Some(RenderSubtype::Classify);
    target.render_subtype_quality = Some(QUALITY);
    target.render_classify_lossy = Some(lossy);
    target.render_motion = motion;
    target.render_stream_quality = motion.then_some(MOTION_QUALITY);
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
    lossy: u64,
}

/// Connect to the operator's target on the classify dial and read tiles until the
/// announced desktop is fully painted. Every tile's format byte and payload magic
/// are checked on the way past; the lossless/lossy split comes back for reporting.
async fn paint_a_whole_desktop(lossy: ClassifyLossy, motion: bool) -> Tally {
    common::init_logging();
    let name = &target_name();
    let addr = spawn_app(uat_target(name, lossy, motion)).await;
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
                            check_tile(tile, lossy, &mut tally);
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

    println!(
        "{name}: {} png tile(s), {} {} tile(s)",
        tally.png,
        tally.lossy,
        lossy.name()
    );
    assert!(
        tally.png > 0,
        "{name}: a real desktop was painted whole without one PNG tile — the classifier \
         is not declining anything"
    );
    tally
}

/// One tile's wire claims, checked against each other: the format byte must be
/// PNG or the one lossy still this session configured — a third format would be
/// an encoder the operator did not ask for — and the payload must begin with that
/// format's magic, a JPEG in PNG clothing decoding as neither.
fn check_tile(tile: &common::BatchTile, lossy: ClassifyLossy, tally: &mut Tally) {
    assert!(tile.w > 0 && tile.h > 0, "empty tile {}x{}", tile.w, tile.h);
    let (format, magic): (u8, &[u8]) = match lossy {
        ClassifyLossy::Jpeg => (TILE_FORMAT_JPEG, &[0xFF, 0xD8, 0xFF]),
        // The RIFF container's form type sits at byte 8, past the four length
        // bytes, so the magic is checked in two pieces below.
        ClassifyLossy::Webp => (TILE_FORMAT_WEBP, b"RIFF"),
    };
    if tile.format == TILE_FORMAT_PNG {
        assert!(
            tile.payload.len() >= 8 && tile.payload[..8] == *b"\x89PNG\r\n\x1a\n",
            "a tile marked PNG does not carry a PNG stream"
        );
        tally.png += 1;
        return;
    }
    assert_eq!(
        tile.format,
        format,
        "a classify session on {} sent a tile in another format",
        lossy.name()
    );
    assert!(
        tile.payload.len() > magic.len() && tile.payload[..magic.len()] == *magic,
        "a tile marked {} does not carry one",
        lossy.name()
    );
    if lossy == ClassifyLossy::Webp {
        assert!(
            tile.payload.len() >= 12 && tile.payload[8..12] == *b"WEBP",
            "a tile marked WebP carries a RIFF container that is not WebP"
        );
    }
    tally.lossy += 1;
}

/// The classifier against a real desktop, on the encoder it was measured with.
/// Run it once per device worth covering — an RDP host, a VNC host, a Mac in High
/// Performance mode — by pointing [`TARGET_ENV`] at each in turn.
#[tokio::test]
#[ignore = "needs the real device REMOTEX_UAT_TARGET names in tmp/test_uat.toml"]
async fn classify_paints_a_real_desktop() {
    paint_a_whole_desktop(ClassifyLossy::Jpeg, false).await;
}

/// The same desktop with the classifier's other encoder underneath it: the
/// verdicts are the classifier's either way, and what this proves is that the
/// tiles it sends lossy arrive as WebP the browser can decode.
#[tokio::test]
#[ignore = "needs the real device REMOTEX_UAT_TARGET names in tmp/test_uat.toml"]
async fn classify_paints_a_real_desktop_through_its_webp_arm() {
    paint_a_whole_desktop(ClassifyLossy::Webp, false).await;
}

/// The classifier as the base of a motion plan: a settled cell is classified
/// while whatever is moving takes the stream instead.
#[tokio::test]
#[ignore = "needs the real device REMOTEX_UAT_TARGET names in tmp/test_uat.toml"]
async fn a_classify_base_paints_a_real_desktop_under_motion() {
    paint_a_whole_desktop(ClassifyLossy::Jpeg, true).await;
}
