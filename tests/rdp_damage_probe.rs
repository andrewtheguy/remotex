//! Whether the RDP client reports every pixel it paints.
//!
//! The gateway sends a client only what the engine was told changed: `Event::Paint`
//! names a rectangle, the engine compares that rectangle against its shadow, and
//! a pixel inside no rectangle is a pixel the browser is never offered. So a
//! framebuffer written outside a reported rectangle is a pixel that goes stale on
//! the far end and stays stale — the leftover pieces a moving picture leaves behind.
//!
//! This probe is the gateway's shadow with nothing else attached. It keeps a mirror
//! of the framebuffer and refreshes it *only* inside the rectangles the client
//! reports, exactly as `Shadow` does, then compares the mirror with the framebuffer
//! at every frame boundary. Agreement is the contract. A disagreement that survives
//! [`STREAK`] consecutive frames is a leak rather than the race between the client's
//! thread painting frame N+1 and this one reading frame N — that race heals on the
//! next frame, because the paint that caused it is already on its way.
//!
//! ```sh
//! REMOTEX_UAT_TARGET=<rdp target in tmp/test_uat.toml> \
//!   cargo test --release --test rdp_damage_probe -- --ignored --nocapture
//! ```
//!
//! Play something on the remote first: a still desktop reports nothing and proves
//! nothing.

mod common;

use std::time::{Duration, Instant};

use remotex::rdp_client::{AudioSink, Connect, Event, MouseButton, Session};
use tokio::sync::mpsc::Receiver;

const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";
const RUN_ENV: &str = "REMOTEX_DAMAGE_RUN_SECS";
const DUMP_ENV: &str = "REMOTEX_DAMAGE_DUMP";

/// The size the operator's QA runs at.
const SIZE: (u32, u32) = (1920, 980);

/// The cell a leak is counted in.
const CELL: u32 = 64;

/// How many consecutive frames a cell must disagree before it is a leak and not the
/// read-ahead race.
const STREAK: u32 = 30;

/// How far apart two pixels must be to count, so that nothing here rests on a codec
/// rounding a colour differently between passes.
const TOLERANCE: i32 = 8;

fn run_secs() -> u64 {
    std::env::var(RUN_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(45)
}

fn connect() -> (Session, Receiver<Event>) {
    let name = std::env::var(TARGET_ENV)
        .unwrap_or_else(|_| panic!("set {TARGET_ENV} to the name of an rdp target in tmp/test_uat.toml"));
    let target = common::uat_target(&name);
    println!(
        "rdp_damage_probe: {name} ({}:{}) at {}x{}, resize {} clipboard {} audio {}",
        target.host, target.port, SIZE.0, SIZE.1, target.resize, target.clipboard, target.audio
    );
    // The session the gateway would open for this target, so the host draws for the
    // same client: the channels it names change what the host sends.
    let (session, events) = Session::start(Connect {
        host: target.host.clone(),
        port: target.port,
        username: target.username.clone(),
        password: target.password.clone(),
        domain: target.domain.clone(),
        width: SIZE.0,
        height: SIZE.1,
        scale_percent: 0,
        resize: target.resize,
        egfx: target.egfx(),
        clipboard: target.clipboard,
        audio: target.audio.then(|| Box::new(Silence) as Box<dyn AudioSink>),
        // A camera draws nothing, and one the browser never plugs costs the host a
        // channel and nothing else.
        camera: None,
        microphone: None,
    });
    (session, events)
}

/// Audio the probe asks for, as the gateway does, and throws away.
struct Silence;

impl AudioSink for Silence {
    fn negotiated(&self, _: remotex::rdp_client::proto::rdpsnd::Format) {}
    fn wave(&self, _: Vec<u8>) {}
    fn closed(&self) {}
}

/// The mirror, and how long each cell has disagreed with the framebuffer.
struct Shadow {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    cols: usize,
    rows: usize,
    /// Consecutive frames each cell has disagreed for.
    streak: Vec<u32>,
    /// The longest streak each cell ever reached.
    worst: Vec<u32>,
}

impl Shadow {
    fn new(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        let cols = width.div_ceil(CELL) as usize;
        let rows = height.div_ceil(CELL) as usize;
        Self { width, height, pixels, cols, rows, streak: vec![0; cols * rows], worst: vec![0; cols * rows] }
    }

    fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// Take the framebuffer's pixels inside one reported rectangle, as the engine
    /// does when it sends them.
    fn accept(&mut self, frame: &[u8], rect: remotex::rdp_client::Rect) {
        let right = rect.x.saturating_add(rect.width).min(self.width);
        let bottom = rect.y.saturating_add(rect.height).min(self.height);
        if rect.x >= right || rect.y >= bottom {
            return;
        }
        let stride = self.stride();
        let bytes = (right - rect.x) as usize * 4;
        for y in rect.y..bottom {
            let at = y as usize * stride + rect.x as usize * 4;
            self.pixels[at..at + bytes].copy_from_slice(&frame[at..at + bytes]);
        }
    }

    /// Compare every cell with the framebuffer, and age the ones that disagree.
    /// Returns how many cells are past [`STREAK`] and how many pixels they hold.
    fn compare(&mut self, frame: &[u8]) -> (usize, u64) {
        let stride = self.stride();
        let (mut cells, mut pixels) = (0, 0);
        for row in 0..self.rows {
            for col in 0..self.cols {
                let x0 = col as u32 * CELL;
                let y0 = row as u32 * CELL;
                let x1 = (x0 + CELL).min(self.width);
                let y1 = (y0 + CELL).min(self.height);
                let mut differs = 0u64;
                for y in y0..y1 {
                    let at = y as usize * stride + x0 as usize * 4;
                    let width = (x1 - x0) as usize;
                    let mine = &self.pixels[at..at + width * 4];
                    let theirs = &frame[at..at + width * 4];
                    for (a, b) in mine.as_chunks::<4>().0.iter().zip(theirs.as_chunks::<4>().0) {
                        if (0..3).any(|c| (i32::from(a[c]) - i32::from(b[c])).abs() > TOLERANCE) {
                            differs += 1;
                        }
                    }
                }
                let index = row * self.cols + col;
                if differs > 0 {
                    self.streak[index] += 1;
                    self.worst[index] = self.worst[index].max(self.streak[index]);
                    if self.streak[index] >= STREAK {
                        cells += 1;
                        pixels += differs;
                    }
                } else {
                    self.streak[index] = 0;
                }
            }
        }
        (cells, pixels)
    }
}

fn snapshot(session: &Session) -> (u32, u32, Vec<u8>) {
    session.framebuffer().with(|frame| (frame.width, frame.height, frame.pixels.clone()))
}

fn dump(name: &str, width: u32, height: u32, pixels: &[u8]) {
    let Ok(dir) = std::env::var(DUMP_ENV) else { return };
    std::fs::create_dir_all(&dir).expect("creating the dump directory");
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    let mut rgba = pixels.to_vec();
    for px in rgba.as_chunks_mut::<4>().0 {
        px[3] = 0xFF;
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&rgba).expect("png data");
    writer.finish().expect("png finish");
    std::fs::write(&path, out).expect("writing the dump");
    println!("  wrote {}", path.display());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real Windows host, named by REMOTEX_UAT_TARGET"]
async fn a_real_host_reports_every_pixel_it_paints() {
    let (session, mut events) = connect();
    let deadline = Instant::now() + Duration::from_secs(30);
    // The desktop the session opens with, and the paints that fill it, are the
    // client's first refresh: mirror it whole once that has landed.
    let mut frames = 0u64;
    while frames < 3 {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = tokio::time::timeout(left, events.recv()).await.expect("a first frame").expect("an event");
        match event {
            Event::Frame => frames += 1,
            Event::Ended(result) => panic!("the session ended: {result:?}"),
            _ => {}
        }
    }
    let (width, height, pixels) = snapshot(&session);
    println!("  desktop {width}x{height}, mirrored whole after {frames} frames");
    let mut shadow = Shadow::new(width, height, pixels);

    let until = Instant::now() + Duration::from_secs(run_secs());
    let (mut paints, mut marks, mut reported) = (0u64, 0u64, 0u64);
    let (mut wide, mut wide_px, mut widest) = (0u64, 0u64, 0u32);
    let mut rects = Vec::new();
    let mut peak = (0usize, 0u64);
    while Instant::now() < until {
        let left = until.saturating_duration_since(Instant::now());
        let Ok(event) = tokio::time::timeout(left, events.recv()).await else { break };
        match event.expect("an event") {
            Event::Paint(rect) => {
                paints += 1;
                reported += u64::from(rect.width) * u64::from(rect.height);
                widest = widest.max(rect.width);
                if rect.width > shadow.width / 2 {
                    wide += 1;
                    wide_px += u64::from(rect.width) * u64::from(rect.height);
                }
                rects.push(rect);
            }
            Event::Frame => {
                marks += 1;
                session.framebuffer().with(|frame| {
                    if frame.width != shadow.width || frame.height != shadow.height {
                        return;
                    }
                    for rect in rects.drain(..) {
                        shadow.accept(&frame.pixels, rect);
                    }
                    let (cells, pixels) = shadow.compare(&frame.pixels);
                    peak = peak.max((cells, pixels));
                });
                rects.clear();
            }
            Event::Resize { width, height } => {
                let (w, h, pixels) = snapshot(&session);
                println!("  resized to {width}x{height}; mirror restarted at {w}x{h}");
                shadow = Shadow::new(w, h, pixels);
            }
            Event::Ended(result) => panic!("the session ended: {result:?}"),
            _ => {}
        }
    }

    let (width, height, frame) = snapshot(&session);
    println!("  {paints} paints over {marks} frames, {reported} pixels reported");
    println!(
        "  {reported} px is {:.1} whole desktops; the average rectangle is {} px",
        reported as f64 / f64::from(shadow.width * shadow.height),
        reported / paints.max(1)
    );
    println!(
        "  {wide} of {paints} rectangles ({:.0}%) are wider than half the desktop, carrying {:.0}% of the \
         reported pixels; the widest was {widest} px",
        100.0 * wide as f64 / paints.max(1) as f64,
        100.0 * wide_px as f64 / reported.max(1) as f64,
    );
    let leaked: Vec<_> = (0..shadow.rows * shadow.cols)
        .filter(|&i| shadow.worst[i] >= STREAK)
        .map(|i| ((i % shadow.cols) as u32 * CELL, (i / shadow.cols) as u32 * CELL, shadow.worst[i]))
        .collect();
    println!("  peak leak: {} cells, {} pixels", peak.0, peak.1);
    println!("  {} of {} cells ever leaked for {STREAK}+ frames", leaked.len(), shadow.streak.len());
    for (x, y, worst) in leaked.iter().take(40) {
        println!("    cell at {x},{y} disagreed for up to {worst} frames");
    }
    dump("mirror", shadow.width, shadow.height, &shadow.pixels);
    // A resize between the last frame boundary and this read leaves a framebuffer
    // that is not the mirror's desktop at all. Nothing below can say anything about
    // it — the two do not describe the same pixels — and the leak counts above are
    // already the run's answer.
    if (width, height) != (shadow.width, shadow.height) || frame.len() != shadow.pixels.len() {
        println!("  the desktop became {width}x{height} after the last frame; no mask for it");
        return;
    }
    dump("framebuffer", shadow.width, shadow.height, &frame);
    let mut mask = vec![0u8; shadow.pixels.len()];
    for (i, out) in mask.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (a, b) = (&shadow.pixels[i * 4..i * 4 + 3], &frame[i * 4..i * 4 + 3]);
        let differs = (0..3).any(|c| (i32::from(a[c]) - i32::from(b[c])).abs() > TOLERANCE);
        *out = if differs { [0xFF, 0, 0, 0xFF] } else { [0, 0, 0, 0xFF] };
    }
    dump("leak-mask", shadow.width, shadow.height, &mask);
}

/// `x,y` to wheel at while the decode probe runs; unset leaves the remote's input alone.
const SCROLL_ENV: &str = "REMOTEX_DAMAGE_SCROLL";

/// `x,y` of a window's title bar to drag and cycle through maximize, restore and
/// minimize while the decode probe runs.
const DRAG_ENV: &str = "REMOTEX_DAMAGE_DRAG";

/// RDP scancodes, with their E0 flag.
const WIN: (u8, bool) = (0x5B, true);
const SHIFT: (u8, bool) = (0x2A, false);
const UP: (u8, bool) = (0x48, true);
const DOWN: (u8, bool) = (0x50, true);
const M: (u8, bool) = (0x32, false);

/// Drain events for `for_`, failing if the session ends.
async fn pump(events: &mut Receiver<Event>, for_: Duration) {
    let until = Instant::now() + for_;
    while let Ok(Some(event)) = tokio::time::timeout_at(until.into(), events.recv()).await {
        if let Event::Ended(result) = event {
            panic!("the session ended: {result:?}");
        }
    }
}

/// How long the event stream must stay empty for the desktop to count as settled.
const QUIET: Duration = Duration::from_millis(1500);

/// Drain events until none has arrived for [`QUIET`], or `cap` passes; says which.
async fn settle(events: &mut Receiver<Event>, cap: Duration) -> (u64, bool) {
    let until = Instant::now() + cap;
    let mut frames = 0;
    while Instant::now() < until {
        match tokio::time::timeout(QUIET, events.recv()).await {
            Err(_) => return (frames, true),
            Ok(Some(Event::Frame)) => frames += 1,
            Ok(Some(Event::Ended(result))) => panic!("the session ended: {result:?}"),
            Ok(Some(_)) => {}
            Ok(None) => panic!("the event stream closed"),
        }
    }
    (frames, false)
}

/// Cells where two framebuffers disagree by more than [`TOLERANCE`], with their counts.
fn differing_cells(width: u32, height: u32, a: &[u8], b: &[u8]) -> Vec<(u32, u32, u64)> {
    let stride = width as usize * 4;
    let mut out = Vec::new();
    for y0 in (0..height).step_by(CELL as usize) {
        for x0 in (0..width).step_by(CELL as usize) {
            let mut differs = 0;
            for y in y0..(y0 + CELL).min(height) {
                for x in x0..(x0 + CELL).min(width) {
                    let at = y as usize * stride + x as usize * 4;
                    if (0..3).any(|c| (i32::from(a[at + c]) - i32::from(b[at + c])).abs() > TOLERANCE) {
                        differs += 1;
                    }
                }
            }
            if differs > 0 {
                out.push((x0, y0, differs));
            }
        }
    }
    out
}

/// Whether the client's own decoders leave the framebuffer holding what the host drew.
///
/// Damage reporting can be perfect and the picture still wrong: a Progressive tile, a
/// ClearCodec cache or a surface copy decoded wrongly writes wrong pixels *and reports
/// them*, and the gateway's `refresh` repaints from this framebuffer, so neither the
/// damage probe nor a browser reload can see it. Only the host can, by drawing the
/// desktop again. So: settle, snapshot what was decoded, ask the host for a redraw
/// twice, and count the cells where the decoded picture disagrees with a redraw that
/// agrees with itself — the second redraw is what rules out a clock or an animation.
///
/// ```sh
/// REMOTEX_UAT_TARGET=<rdp target> REMOTEX_DAMAGE_SCROLL=960,500 REMOTEX_DAMAGE_DUMP=tmp/qa/decode \
///   cargo test --release --test rdp_damage_probe a_host_redraw -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real Windows host, named by REMOTEX_UAT_TARGET"]
async fn a_host_redraw_matches_what_was_decoded() {
    common::init_logging();
    let (session, mut events) = connect();
    let (frames, _) = settle(&mut events, Duration::from_secs(30)).await;
    println!("  connected; {frames} frames before the desktop settled");

    let until = Instant::now() + Duration::from_secs(run_secs());
    if let Ok(at) = std::env::var(SCROLL_ENV) {
        let (x, y) = at.split_once(',').expect("REMOTEX_DAMAGE_SCROLL is x,y");
        let (x, y): (u16, u16) = (x.parse().expect("scroll x"), y.parse().expect("scroll y"));
        session.input().mouse_move(x, y);
        if std::env::var("REMOTEX_DAMAGE_HOME").is_ok() {
            // Home, so every run scrolls the same stretch of the page: a wheel cycle
            // does not land back where it started, and runs drift down the page.
            session.input().key(0x47, true, true);
            session.input().key(0x47, true, false);
            settle(&mut events, Duration::from_secs(20)).await;
        }
        let mut step = 0u64;
        while Instant::now() < until {
            // Three notches a tick, reversing every twenty ticks: down a page and back.
            let delta = if (step / 20).is_multiple_of(2) { -360 } else { 360 };
            session.input().wheel(delta, false, x, y);
            step += 1;
            let tick = Instant::now() + Duration::from_millis(120);
            while let Ok(Some(event)) = tokio::time::timeout_at(tick.into(), events.recv()).await {
                if let Event::Ended(result) = event {
                    panic!("the session ended: {result:?}");
                }
            }
        }
        println!("  scrolled at {x},{y} for {step} ticks");
        // Off whatever the wheel ended over: a pointer resting on a link grows a
        // tooltip, which a redraw may leave out, and that would read as a decoder fault.
        session.input().mouse_move(1900, 600);
    } else if let Ok(at) = std::env::var(DRAG_ENV) {
        let (x, y) = at.split_once(',').expect("REMOTEX_DAMAGE_DRAG is x,y");
        let (x, y): (i32, i32) = (x.parse().expect("drag x"), y.parse().expect("drag y"));
        let point = |px: i32, py: i32| {
            (px.clamp(0, SIZE.0 as i32 - 1) as u16, py.clamp(0, SIZE.1 as i32 - 1) as u16)
        };
        let input = session.input();
        let chord = |keys: &[(u8, bool)]| {
            for &(code, extended) in keys {
                input.key(code, extended, true);
            }
            for &(code, extended) in keys.iter().rev() {
                input.key(code, extended, false);
            }
        };
        let mut cycles = 0u64;
        while Instant::now() < until {
            // Round a circle by the title bar and back to where it started, so the
            // window ends each cycle where the next one expects its title bar.
            let (sx, sy) = point(x, y);
            input.mouse_move(sx, sy);
            pump(&mut events, Duration::from_millis(150)).await;
            input.mouse_button(MouseButton::Left, true, sx, sy);
            for step in 1..=32 {
                let angle = f64::from(step) * std::f64::consts::TAU / 32.0;
                let (px, py) = point(x + (260.0 * angle.sin()) as i32, y + (160.0 * (1.0 - angle.cos())) as i32);
                input.mouse_move(px, py);
                pump(&mut events, Duration::from_millis(25)).await;
            }
            input.mouse_button(MouseButton::Left, false, sx, sy);
            pump(&mut events, Duration::from_millis(800)).await;
            // Maximize, restore, minimize, and bring back what was minimized — each
            // with the animation the host plays for it.
            for keys in [&[WIN, UP][..], &[WIN, DOWN], &[WIN, DOWN], &[WIN, SHIFT, M]] {
                chord(keys);
                pump(&mut events, Duration::from_millis(1200)).await;
            }
            cycles += 1;
        }
        println!("  dragged from {x},{y} and cycled the window's states {cycles} times");
        session.input().mouse_move(1900, 600);
    } else {
        while Instant::now() < until {
            let left = until.saturating_duration_since(Instant::now());
            if let Ok(Some(Event::Ended(result))) = tokio::time::timeout(left, events.recv()).await {
                panic!("the session ended: {result:?}");
            }
        }
    }
    // A desktop that never goes quiet — a video playing — is snapshotted mid-picture,
    // and its difference from a later redraw is the video moving, not a decoder fault.
    let (frames, quiet) = settle(&mut events, Duration::from_secs(20)).await;
    if !quiet {
        println!("  RESULT inconclusive: the desktop never settled after {frames} frames");
        return;
    }
    let (width, height, decoded) = snapshot(&session);
    log::info!("PROBE-MARK decoded snapshot");
    println!("  decoded snapshot after {frames} frames; settled");

    let mut redraws = Vec::new();
    for pass in 0..2 {
        log::info!("PROBE-MARK redraw {pass}");
        session.input().refresh();
        let (frames, quiet) = settle(&mut events, Duration::from_secs(20)).await;
        let (w, h, pixels) = snapshot(&session);
        assert_eq!((w, h), (width, height), "the desktop resized during the probe");
        println!("  host redraw took {frames} frames; {}", if quiet { "settled" } else { "NEVER SETTLED" });
        // A redraw that drew nothing is the decoded picture compared with itself, and
        // one that never went quiet is a picture caught mid-change.
        if !quiet || frames == 0 {
            println!("  RESULT inconclusive: host redraw {pass} did not complete");
            return;
        }
        redraws.push(pixels);
    }

    let unsteady: std::collections::HashSet<_> =
        differing_cells(width, height, &redraws[0], &redraws[1]).into_iter().map(|(x, y, _)| (x, y)).collect();
    let wrong: Vec<_> = differing_cells(width, height, &decoded, &redraws[0])
        .into_iter()
        .filter(|(x, y, _)| !unsteady.contains(&(*x, *y)))
        .collect();
    let pixels: u64 = wrong.iter().map(|(_, _, n)| n).sum();
    println!(
        "  RESULT decoded_wrong_cells={} decoded_wrong_px={pixels} unsteady_cells={}",
        wrong.len(),
        unsteady.len()
    );
    for (x, y, n) in wrong.iter().take(40) {
        println!("    cell at {x},{y}: {n} px differ from the host's redraw");
    }
    dump("decoded", width, height, &decoded);
    dump("redraw", width, height, &redraws[0]);
}
