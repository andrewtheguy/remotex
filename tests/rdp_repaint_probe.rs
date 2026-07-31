//! What a full-desktop repaint costs on a real RDP target, and how much of it the
//! encoder can be made to overlap.
//!
//! An instrument, not a test: it asserts nothing. It exists to pick and defend
//! `encode::ENCODE_DEPTH`, which is a dial with no right answer that can be reasoned
//! out — a repaint is one frame's worth of bands pushed in a tight loop, and how much
//! of that compresses at once is the whole question.
//!
//! Unlike `rdp_bytes_probe`, this needs a **real** host: the dummy xrdp login screen
//! is nearly blank, and a blank repaint encodes in microseconds no matter what the
//! depth is. So the target comes out of a config file that is not in the tree:
//!
//!   REMOTEX_PROBE_CONFIG=tmp/test_config.toml \
//!   REMOTEX_PROBE_TARGET=desktop-vnvgdaf \
//!     cargo test --release --test rdp_repaint_probe -- --ignored --nocapture
//!
//! `--release` is not optional here. A debug PNG encode is several times slower, which
//! would flatter every depth equally and tell you nothing about the real one.
//!
//! Read two lines out of a run: this probe's own `PROBE:` line for wall clock per
//! repaint, and the gateway's `rdp: encode totals:` for what the encoder did with it.
//! In those totals, `encoding across workers` against `of waiting` *is* the achieved
//! parallelism, and `engine stalled` is what the read loop still pays.
//!
//! Two runs only compare at the same surface size — a repaint's cost is its pixels —
//! so the line prints the desktop it measured. Byte counts are *not* comparable
//! between runs the way `rdp_bytes_probe`'s are: a real desktop draws its own clock
//! while the probe watches. The record count is (13 bands each), and the timings
//! are, which is what this measures.
//!
//! What it produced, on a 12-core arm64 Mac against a 1280x800 Windows desktop, is
//! the table in `encode::ENCODE_DEPTH` — the measurement that picked the depth.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use remotex::server;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// How many repaints to drive.
///
/// 40 rather than 20 because it has to be: at 20, single repaints over 200ms turned
/// up at several depths and vanished at 40, so a short run reads the desktop's own
/// drawing as though it were the encoder's tail.
const REPAINTS: usize = 40;

/// A gap in the frame stream that means the repaint finished.
const QUIET: Duration = Duration::from_millis(300);

/// The real target, out of a config the tree does not carry.
///
/// Only the target profile is taken from it: the site password is the tests' own, so
/// that `common::login` works, and the listener is ephemeral on localhost.
async fn spawn_app() -> (SocketAddr, String, (u16, u16)) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });

    let path = std::env::var("REMOTEX_PROBE_CONFIG")
        .expect("set REMOTEX_PROBE_CONFIG to a config file holding a real RDP target");
    let target = std::env::var("REMOTEX_PROBE_TARGET")
        .expect("set REMOTEX_PROBE_TARGET to the name of the target to measure");
    let (file, _) = remotex::config::load(Some(Path::new(&path))).expect("read the probe config");
    let mut config = file.resolve().expect("resolve the probe config");
    config.targets.retain(|t| t.name == target);
    let profile = config
        .targets
        .first()
        .unwrap_or_else(|| panic!("no target named {target} in {path}"))
        .clone();
    let size = (profile.width, profile.height);

    config.host = "127.0.0.1".to_owned();
    config.port = 0;
    config.static_dir = Some("frontend/dist".into());
    config.auth = common::test_auth();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, target, size)
}

#[tokio::test]
#[ignore = "instrument, not a test; needs a real RDP host"]
async fn full_desktop_repaints_against_a_real_host() {
    common::init_logging();
    let (addr, target, size) = spawn_app().await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = common::connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, &target).await;

    // Let the desktop paint once and settle, so the measured repaints are of a
    // finished screen rather than of the session coming up.
    let settle = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < settle {
        let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    }

    // `refresh` forgets the shadow and repaints the whole desktop from the
    // framebuffer the engine already holds: one frame's worth of bands, pushed in a
    // tight loop, with no dependence on what the remote happens to be drawing. That
    // is exactly the workload the depth dial is for, and it is repeatable.
    let mut millis = Vec::with_capacity(REPAINTS);
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut records = 0usize;
    for _ in 0..REPAINTS {
        ws.send(Message::Text(r#"{"type":"refresh"}"#.into())).await.unwrap();
        let started = tokio::time::Instant::now();
        let mut last = started;
        loop {
            match tokio::time::timeout(QUIET, ws.next()).await {
                Ok(Some(Ok(Message::Binary(frame)))) => {
                    frames += 1;
                    bytes += frame.len() as u64;
                    records += common::batch_records(&frame).len();
                    last = tokio::time::Instant::now();
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_)) | None) => panic!("the gateway closed mid-probe"),
                // Quiet for `QUIET`: this repaint is done.
                Err(_) => break,
            }
        }
        millis.push((last - started).as_secs_f64() * 1000.0);
    }

    // Back to the picker, which ends the engine — and an engine that ends is what
    // logs `encode totals`, the other half of the reading.
    ws.send(Message::Text(r#"{"type":"disconnect"}"#.into())).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;

    // Median and p90, not mean: a real desktop draws its own clock between repaints,
    // and one such frame lands in the sample often enough to move a mean and never
    // often enough to be the thing under test.
    millis.sort_by(f64::total_cmp);
    println!(
        "PROBE: {} repaints of {}x{}: {:.1}ms median, {:.1}ms p90, {:.1}ms worst; \
         {frames} binary frames / {bytes} bytes / {records} records",
        millis.len(),
        size.0,
        size.1,
        millis[millis.len() / 2],
        millis[millis.len() * 9 / 10],
        millis[millis.len() - 1],
    );
}
