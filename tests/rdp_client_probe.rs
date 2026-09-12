//! The RDP client against a real host.
//!
//! No container stands in here: the client speaks NLA to a current Windows host and
//! nothing else, and what this exercises — bitmap updates and a
//! Deactivation-Reactivation Sequence — is that host's behaviour. So, like
//! `classify_render_e2e`, it
//! borrows a target from the operator's `tmp/test_uat.toml`, named by
//! [`TARGET_ENV`] rather than written here, and drives [`remotex::rdp_client`]
//! directly with no gateway in front of it.
//!
//! It connects, waits for a painted desktop, asks for a new size the way the
//! engine does — repeating the layout until the host answers, because a Windows
//! host ignores the first few seconds of them — then puts the size back and
//! disconnects.
//!
//! What is asserted is the client's contract: a desktop arrives, gets painted,
//! and a requested size comes back as a resize of that size with a framebuffer to
//! match. Counts are printed, not asserted — they are the host's business.
//!
//! ```sh
//! REMOTEX_UAT_TARGET=<rdp target in tmp/test_uat.toml> \
//!   cargo test --test rdp_client_probe -- --ignored --nocapture --test-threads 1
//! ```

mod common;

use std::time::{Duration, Instant};

use remotex::rdp_client::{Connect, Event, Session};
use tokio::sync::mpsc::Receiver;

/// Which target in `tmp/test_uat.toml` to drive — see the module docs.
const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";

/// The opening size, and the one each case asks to move to. Both even, both well
/// inside what any host accepts.
const OPENING: (u32, u32) = (1280, 800);
const RESIZED: (u32, u32) = (1600, 900);

/// How long a host gets to answer a layout before the case fails, and how often
/// the layout is repeated meanwhile.
const RESIZE_BUDGET: Duration = Duration::from_secs(30);
const RESIZE_RETRY: Duration = Duration::from_secs(2);

fn connect() -> (Session, Receiver<Event>) {
    let name = std::env::var(TARGET_ENV).unwrap_or_else(|_| {
        panic!("set {TARGET_ENV} to the name of an rdp target in tmp/test_uat.toml")
    });
    let target = common::uat_target(&name);
    println!("rdp_client_probe: {name} ({}:{})", target.host, target.port);
    Session::start(Connect {
        host: target.host.clone(),
        port: target.port,
        username: target.username.clone(),
        password: target.password.clone(),
        domain: target.domain.clone(),
        width: OPENING.0,
        height: OPENING.1,
        resize: true,
    })
}

/// What a stretch of the session did.
#[derive(Default, Debug)]
struct Tally {
    paints: u64,
    cursors: u64,
    resizes: Vec<(u32, u32)>,
    resize_ready: bool,
}

/// Read events until `until` says stop or `deadline` passes. Panics on an ended
/// session: every case here expects the session to survive.
async fn pump(
    events: &mut Receiver<Event>,
    tally: &mut Tally,
    deadline: Instant,
    mut until: impl FnMut(&Tally) -> bool,
) -> bool {
    while !until(tally) {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = tokio::time::timeout(left, events.recv()).await else {
            return false;
        };
        match event.expect("the event channel closed without an Ended") {
            Event::Paint(_) => tally.paints += 1,
            Event::Cursor(_) => tally.cursors += 1,
            Event::Resize { width, height } => tally.resizes.push((width, height)),
            Event::ResizeReady { .. } => tally.resize_ready = true,
            Event::Connected { .. } => panic!("a second Connected"),
            Event::Ended(result) => panic!("the session ended: {result:?}"),
        }
    }
    true
}

/// How many of the framebuffer's pixels are not black: a desktop that decoded to
/// nothing — the classic failure of a codec that is not really working — is all 0.
fn lit(session: &Session) -> (u64, u64) {
    session.framebuffer().with(|frame| {
        let lit = frame.pixels.as_chunks::<4>().0.iter().filter(|px| px[..3] != [0, 0, 0]).count();
        (lit as u64, u64::from(frame.width) * u64::from(frame.height))
    })
}

/// Ask for `size` until the host resizes to it, the way the engine's retry ladder
/// does. Returns how long the host took.
async fn resize_to(
    session: &Session,
    events: &mut Receiver<Event>,
    tally: &mut Tally,
    size: (u32, u32),
) -> Duration {
    let started = Instant::now();
    let deadline = started + RESIZE_BUDGET;
    loop {
        session.input().resize(size.0, size.1, 100);
        let retry = (Instant::now() + RESIZE_RETRY).min(deadline);
        if pump(events, tally, retry, |t| t.resizes.last() == Some(&size)).await {
            return started.elapsed();
        }
        assert!(
            Instant::now() < deadline,
            "the host never resized to {}x{} (resizes seen: {:?})",
            size.0,
            size.1,
            tally.resizes
        );
    }
}

async fn case() {
    common::init_logging();
    let (session, mut events) = connect();

    let first = tokio::time::timeout(Duration::from_secs(60), events.recv())
        .await
        .expect("no first event within 60s")
        .expect("the event channel closed");
    let Event::Connected { width, height } = first else {
        panic!("the session did not connect: {first:?}");
    };
    println!("  connected at {width}x{height}");

    // A desktop worth of paint, and Display Control, before anything is resized.
    let mut tally = Tally::default();
    pump(&mut events, &mut tally, Instant::now() + Duration::from_secs(8), |t| {
        t.resize_ready && t.paints > 0
    })
    .await;
    // And a moment more, so a desktop still arriving is counted whole.
    pump(&mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false).await;
    let (on, total) = lit(&session);
    println!("  opening: {tally:?}, {on} of {total} pixels lit");
    assert!(tally.paints > 0, "nothing was painted");
    assert!(on > 0, "the desktop decoded to pure black");
    assert!(tally.resize_ready, "the host never offered Display Control");

    for size in [RESIZED, OPENING] {
        let took = resize_to(&session, &mut events, &mut tally, size).await;
        // Let the repaint after the resize land.
        tally.paints = 0;
        pump(&mut events, &mut tally, Instant::now() + Duration::from_secs(3), |_| false).await;
        let (on, total) = lit(&session);
        let framebuffer = session.framebuffer().with(|frame| (frame.width, frame.height));
        println!(
            "  resized to {}x{} after {:.1}s; {} paints since, {on} of {total} pixels lit",
            size.0,
            size.1,
            took.as_secs_f32(),
            tally.paints
        );
        assert_eq!(framebuffer, size, "the framebuffer did not follow the resize");
        assert!(tally.paints > 0, "nothing was repainted after the resize");
        assert!(on > 0, "the resized desktop decoded to pure black");
    }

    // Typing and pointing must not upset the session: a few moves and a key that
    // changes nothing on a desktop (Shift), then a moment for any error to arrive.
    let input = session.input();
    for step in 0..20u16 {
        input.mouse_move(100 + step * 10, 100 + step * 5);
    }
    input.key(0x2A, false, true);
    input.key(0x2A, false, false);
    pump(&mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false).await;
    println!("  after input: {} cursor updates in all", tally.cursors);

    drop(session);
    // The drop disconnected and joined the thread, so its last word is here.
    let mut ended = None;
    while let Ok(event) = events.try_recv() {
        if let Event::Ended(result) = event {
            ended = Some(result);
        }
    }
    println!("  ended: {ended:?}");
    assert!(matches!(ended, Some(Ok(()))), "a disconnect this end asked for is an orderly end");
}

#[tokio::test]
#[ignore = "drives a real RDP host named in tmp/test_uat.toml"]
async fn a_real_host_paints_and_resizes() {
    case().await;
}
