//! The RDP client against a real host, with the graphics pipeline or without it.
//!
//! No container stands in here: the client speaks NLA to a current Windows host and
//! nothing else, and what this exercises — the graphics pipeline's surfaces and
//! codecs, a graphics reset, or bitmap updates and a Deactivation-Reactivation
//! Sequence — is that host's behaviour. So, like `classify_render_e2e`, it
//! borrows a target from the operator's `tmp/test_uat.toml`, named by
//! [`TARGET_ENV`] rather than written here, and drives [`remotex::rdp_client`]
//! directly with no gateway in front of it.
//!
//! It connects, waits for a painted desktop, asks for a new size the way the
//! engine does — repeating the layout until the host answers, because a Windows
//! host ignores the first few seconds of them — puts the size back, takes the
//! remote clipboard over, and disconnects.
//!
//! What is asserted is the client's contract: a desktop arrives, gets painted,
//! and a requested size comes back as a resize of that size with a framebuffer to
//! match. Counts are printed, not asserted — they are the host's business.
//!
//! ## The graphics pipeline, measured
//!
//! [`EGFX_ENV`] chooses the path, and the pipeline is the default, as it is in the
//! gateway. Under it the client decodes every codec the sandbox sends — ClearCodec
//! with all three subcodecs, RemoteFX Progressive, planar and uncompressed — so the
//! probe asserts a lit desktop there as it does on the bitmap path, and after each
//! resize. Which codecs and commands the host chose is still the measurement, said at
//! the end of the session at `info`: run with `RUST_LOG=remotex=info` to read it, and
//! set [`DUMP_ENV`] to a directory to get the framebuffer as PNG at each stage, for
//! the check only eyes can make.
//!
//! ## The clipboard
//!
//! Both directions of MS-RDPECLIP are lazy — a copy announces which formats it can be
//! had in, and the bytes cost a second round trip that happens only when somebody
//! pastes — so the thing worth proving against a real host is that laziness, in both
//! directions, and the second test here does it: this end takes the remote clipboard
//! over, the remote pastes it and copies what it pasted, and the bytes that come back
//! are compared with the bytes that went out. See [`round_trip`], which drives the one
//! application every Windows desktop has to do it.
//!
//! The first test carries the clipboard channel without provoking it: it asserts the
//! host opens the channel and negotiates, answers any paste that happens to arrive,
//! and reports the rest. That the session survives all of it beside the desktop, the
//! pointer and two resizes is its own claim — a clipboard is a second static channel,
//! and a client that gets its chunk flags wrong loses the *other* channel rather than
//! this one.
//!
//! ```sh
//! REMOTEX_UAT_TARGET=<rdp target in tmp/test_uat.toml> \
//!   cargo test --test rdp_client_probe -- --ignored --nocapture --test-threads 1
//! ```

mod common;

use std::time::{Duration, Instant};

use remotex::rdp_client::{Connect, Event, Input, Session};
use remotex::rdp_clipboard::{self, CF_UNICODETEXT};
use tokio::sync::mpsc::Receiver;

/// Which target in `tmp/test_uat.toml` to drive — see the module docs.
const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";

/// Whether to offer the graphics pipeline: anything but `0` or `false` does, and so
/// does leaving it unset.
const EGFX_ENV: &str = "REMOTEX_UAT_EGFX";

/// The opening size, and the one each case asks to move to. Both even, both well
/// inside what any host accepts.
const OPENING: (u32, u32) = (1280, 800);
const RESIZED: (u32, u32) = (1600, 900);

/// How long a host gets to answer a layout before the case fails, and how often
/// the layout is repeated meanwhile.
const RESIZE_BUDGET: Duration = Duration::from_secs(30);
const RESIZE_RETRY: Duration = Duration::from_secs(2);

/// What this end puts on the remote's clipboard. Non-ASCII on purpose: the payload is
/// UTF-16 and a host that mangles it says so in the bytes that come back. One line,
/// because the round trip below pastes it into a single-line box on the remote.
const COPIED: &str = "remotex probe — 画面 ☕ round trip";

/// The scancodes the round trip needs. Driving the remote's own clipboard is the one
/// thing this end cannot do for itself, and a keystroke is all it has to do it with.
const LWIN: u8 = 0x5B;
const ESCAPE: u8 = 0x01;
const LCTRL: u8 = 0x1D;
const KEY_R: u8 = 0x13;
const KEY_A: u8 = 0x1E;
const KEY_C: u8 = 0x2E;
const KEY_V: u8 = 0x2F;

/// Whether this run offers the graphics pipeline — see [`EGFX_ENV`].
fn egfx() -> bool {
    !matches!(std::env::var(EGFX_ENV).as_deref(), Ok("0") | Ok("false"))
}

fn connect() -> (Session, Receiver<Event>) {
    let name = std::env::var(TARGET_ENV).unwrap_or_else(|_| {
        panic!("set {TARGET_ENV} to the name of an rdp target in tmp/test_uat.toml")
    });
    let target = common::uat_target(&name);
    println!("rdp_client_probe: {name} ({}:{}), egfx {}", target.host, target.port, egfx());
    Session::start(Connect {
        host: target.host.clone(),
        port: target.port,
        username: target.username.clone(),
        password: target.password.clone(),
        domain: target.domain.clone(),
        width: OPENING.0,
        height: OPENING.1,
        resize: true,
        egfx: egfx(),
        clipboard: true,
    })
}

/// What a stretch of the session did.
#[derive(Default, Debug)]
struct Tally {
    paints: u64,
    /// Frame boundaries the host marked, which only the graphics pipeline does.
    frames: u64,
    cursors: u64,
    resizes: Vec<(u32, u32)>,
    resize_ready: bool,
    /// The host opened its clipboard channel and started the negotiation.
    clipboard_ready: bool,
    /// What the remote clipboard announced, each time it announced something.
    remote_formats: Vec<Vec<u32>>,
    /// The formats the remote asked this end for — a paste over there, answered as
    /// it arrived.
    pastes: Vec<u32>,
    /// Text that came back from the remote clipboard, and the two ways it did not.
    remote_text: Vec<String>,
    refusals: u64,
    oversized: Vec<u64>,
}

/// Read events until `until` says stop or `deadline` passes. Panics on an ended
/// session: every case here expects the session to survive.
async fn pump(
    session: &Session,
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
            Event::Frame => tally.frames += 1,
            Event::FramesMarked => {}
            Event::Cursor(_) => tally.cursors += 1,
            Event::Resize { width, height } => tally.resizes.push((width, height)),
            Event::ResizeReady { .. } => tally.resize_ready = true,
            Event::ClipboardReady => tally.clipboard_ready = true,
            // Asked for at once, which is what the engine does and for the same
            // reason: a copy on the remote should be in hand before anybody asks.
            Event::ClipboardFormats(formats) => {
                if formats.contains(&CF_UNICODETEXT) {
                    session.input().request_clipboard(CF_UNICODETEXT);
                }
                tally.remote_formats.push(formats);
            }
            Event::ClipboardData(data) => {
                let text = rdp_clipboard::decode_unicode(&data)
                    .expect("the remote's clipboard text decodes as UTF-16");
                tally.remote_text.push(text);
            }
            Event::ClipboardRefused => tally.refusals += 1,
            Event::ClipboardOversized { bytes } => tally.oversized.push(bytes),
            // Answered here, and now: the application pasting on the far end is
            // blocked until this arrives. Answered the way the engine answers —
            // the one text format, encoded — so what the host reads back is what
            // the gateway would really hand it.
            Event::ClipboardWanted { format } => {
                tally.pastes.push(format);
                let data = (format == CF_UNICODETEXT)
                    .then(|| rdp_clipboard::encode_unicode(COPIED));
                session.input().send_clipboard(data);
            }
            Event::Connected { .. } => panic!("a second Connected"),
            Event::Ended(result) => panic!("the session ended: {result:?}"),
        }
    }
    true
}

/// Directory to write the framebuffer to as PNG at each stage, for eyes to check
/// what the counts cannot: that the decoded desktop looks like a desktop.
const DUMP_ENV: &str = "REMOTEX_UAT_DUMP";

/// Write the framebuffer as `<dir>/<name>.png` when [`DUMP_ENV`] names a directory.
fn dump(session: &Session, name: &str) {
    let Ok(dir) = std::env::var(DUMP_ENV) else { return };
    std::fs::create_dir_all(&dir).expect("creating the framebuffer dump directory");
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    let bytes = session.framebuffer().with(|frame| {
        let mut rgba = frame.pixels.clone();
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 0xFF;
        }
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        writer.finish().expect("png finish");
        out
    });
    std::fs::write(&path, bytes).expect("writing the framebuffer dump");
    println!("  framebuffer written to {}", path.display());
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
        if pump(session, events, tally, retry, |t| t.resizes.last() == Some(&size)).await {
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

    // A desktop worth of drawing, and Display Control, before anything is resized.
    // Under the pipeline the drawing shows as frames; on the bitmap path, as paint.
    let egfx = egfx();
    let drawn = |t: &Tally| if egfx { t.frames > 0 } else { t.paints > 0 };
    let mut tally = Tally::default();
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(8), |t| {
        t.resize_ready && t.clipboard_ready && drawn(t)
    })
    .await;
    // And a moment more, so a desktop still arriving is counted whole.
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;
    let (on, total) = lit(&session);
    println!("  opening: {tally:?}, {on} of {total} pixels lit");
    dump(&session, "opening");
    assert!(drawn(&tally), "the host drew nothing");
    assert!(on > 0, "the desktop decoded to pure black");
    assert!(tally.resize_ready, "the host never offered Display Control");
    assert!(tally.clipboard_ready, "the host never opened its clipboard channel");

    for size in [RESIZED, OPENING] {
        let took = resize_to(&session, &mut events, &mut tally, size).await;
        // Let the repaint after the resize land.
        tally.paints = 0;
        tally.frames = 0;
        pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(3), |_| false)
            .await;
        let (on, total) = lit(&session);
        let framebuffer = session.framebuffer().with(|frame| (frame.width, frame.height));
        println!(
            "  resized to {}x{} after {:.1}s; {} paints and {} frames since, {on} of {total} \
             pixels lit",
            size.0,
            size.1,
            took.as_secs_f32(),
            tally.paints,
            tally.frames,
        );
        dump(&session, &format!("resized-{}x{}", size.0, size.1));
        assert_eq!(framebuffer, size, "the framebuffer did not follow the resize");
        assert!(drawn(&tally), "nothing was drawn after the resize");
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
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;
    println!("  after input: {} cursor updates in all", tally.cursors);

    // The clipboard. Taking it over is this end's half of the bargain: one format
    // list, no bytes, and the host asks for those if and when anything over there
    // pastes. Whether it does is the host's business — Windows with clipboard
    // history on reads a newly owned clipboard itself — so what is *asserted* is
    // that the session survives the exchange, and that a paste that did arrive was
    // answered with the format it asked for.
    let before = tally.pastes.len();
    input.advertise_clipboard(vec![CF_UNICODETEXT]);
    // And ask the remote for whatever it holds. Nothing over there has copied
    // anything, and this end has just taken the clipboard over, so what comes back is
    // nothing at all — a Windows host does not answer a request for a format nobody
    // advertised, not even with the `CB_RESPONSE_FAIL` the specification allows. The
    // point of asking is that the session must not mind.
    input.request_clipboard(CF_UNICODETEXT);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(4), |_| false)
        .await;
    println!(
        "  clipboard: the remote announced {:?}, asked this end for {:?}, sent {:?} \
         ({} refusals, oversized {:?})",
        tally.remote_formats,
        tally.pastes,
        tally.remote_text,
        tally.refusals,
        tally.oversized,
    );
    for format in &tally.pastes[before..] {
        assert_eq!(*format, CF_UNICODETEXT, "the host asked for a format never offered");
    }

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

/// Tap one key, with modifiers held around it.
///
/// Held and released here rather than left down: a modifier this end forgets is one
/// the remote desktop keeps, and every keystroke after it — including a person's,
/// later — arrives wearing it.
fn chord(input: &Input, modifiers: &[(u8, bool)], key: u8, extended: bool) {
    for &(scancode, ext) in modifiers {
        input.key(scancode, ext, true);
    }
    input.key(key, extended, true);
    input.key(key, extended, false);
    for &(scancode, ext) in modifiers.iter().rev() {
        input.key(scancode, ext, false);
    }
}

/// The clipboard, all the way around and back.
///
/// The half this end cannot do for itself is the remote's: something over there has to
/// paste, and something has to copy. The Run dialog is the application every Windows
/// desktop has — Win+R opens it, Ctrl+V pastes into its one-line box, Ctrl+A and
/// Ctrl+C copy that back out, and Escape closes it — so the whole round trip is five
/// keystrokes and no typing. Nothing is entered and no Enter is sent, so the dialog
/// leaves the desktop as it found it.
///
/// Both halves of the protocol's laziness are what this proves:
///
/// - the paste makes the host ask this end to render the text it advertised, which is
///   delayed rendering in the direction that matters — a request left unanswered is an
///   application over there stopped inside its own paste;
/// - the copy makes the host announce a format list, which this end asks for and
///   reads, and the bytes that come back are the bytes that went out.
async fn round_trip() {
    common::init_logging();
    let (session, mut events) = connect();
    let first = tokio::time::timeout(Duration::from_secs(60), events.recv())
        .await
        .expect("no first event within 60s")
        .expect("the event channel closed");
    assert!(matches!(first, Event::Connected { .. }), "the session did not connect: {first:?}");

    let mut tally = Tally::default();
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(10), |t| {
        t.clipboard_ready && t.paints > 0
    })
    .await;
    assert!(tally.clipboard_ready, "the host never opened its clipboard channel");

    // 1. Take the remote clipboard over. Nothing is transferred: the host now knows
    //    this end holds text, and will ask for it if anything pastes.
    let input = session.input();
    input.advertise_clipboard(vec![CF_UNICODETEXT]);

    // 2. Open the Run dialog and paste into it. The dialog needs a moment to exist
    //    and take focus before the paste can land in it.
    chord(input, &[(LWIN, true)], KEY_R, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;
    // Select what the dialog pre-filled itself with before pasting over it: it opens
    // holding whatever was last run on that desktop, and a paste that landed beside
    // it would be read back as somebody else's text with this end's appended.
    chord(input, &[(LCTRL, false)], KEY_A, false);
    chord(input, &[(LCTRL, false)], KEY_V, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(4), |t| {
        !t.pastes.is_empty()
    })
    .await;
    assert_eq!(
        tally.pastes,
        vec![CF_UNICODETEXT],
        "the remote never asked this end to render what it had advertised; the Run dialog \
         may not have taken the paste"
    );

    // 3. Select what was pasted and copy it, which hands the clipboard back to the
    //    remote — and this end asks for it the moment the format list arrives.
    chord(input, &[(LCTRL, false)], KEY_A, false);
    chord(input, &[(LCTRL, false)], KEY_C, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(6), |t| {
        !t.remote_text.is_empty()
    })
    .await;

    // 4. Close the dialog, whatever happened above, before anything is asserted: an
    //    open dialog left on the desktop is this test's litter.
    chord(input, &[], ESCAPE, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;

    println!(
        "  round trip: pasted {:?}, the remote then announced {:?} and sent {:?} \
         ({} refusals, oversized {:?})",
        tally.pastes, tally.remote_formats, tally.remote_text, tally.refusals, tally.oversized,
    );
    let announced = tally.remote_formats.last().expect("the remote announced its copy");
    assert!(announced.contains(&CF_UNICODETEXT), "the copy carried no Unicode text: {announced:?}");
    assert_eq!(
        tally.remote_text,
        vec![COPIED.to_owned()],
        "what came back off the remote clipboard is not what went out"
    );

    drop(session);
    let mut ended = None;
    while let Ok(event) = events.try_recv() {
        if let Event::Ended(result) = event {
            ended = Some(result);
        }
    }
    assert!(matches!(ended, Some(Ok(()))), "a disconnect this end asked for is an orderly end");
}

#[tokio::test]
#[ignore = "drives a real RDP host named in tmp/test_uat.toml"]
async fn a_real_host_paints_and_resizes() {
    case().await;
}

#[tokio::test]
#[ignore = "drives a real RDP host named in tmp/test_uat.toml, and its Run dialog"]
async fn a_real_host_round_trips_the_clipboard() {
    round_trip().await;
}
