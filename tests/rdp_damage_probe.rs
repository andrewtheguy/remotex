//! Whether the RDP client reports every pixel it paints.
//!
//! The gateway sends a client only what the engine was told changed: `Event::Paint`
//! names a rectangle, the tile path compares that rectangle against its shadow, and
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

use remotex::rdp_client::{Connect, Event, Session};
use tokio::sync::mpsc::Receiver;

const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";
const RUN_ENV: &str = "REMOTEX_DAMAGE_RUN_SECS";
const DUMP_ENV: &str = "REMOTEX_DAMAGE_DUMP";

/// The size the operator's QA runs at.
const SIZE: (u32, u32) = (1920, 980);

/// The tile the report is cut into, and the grid the gateway's shadow uses.
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
    println!("rdp_damage_probe: {name} ({}:{}) at {}x{}", target.host, target.port, SIZE.0, SIZE.1);
    let (session, events) = Session::start(Connect {
        host: target.host.clone(),
        port: target.port,
        username: target.username.clone(),
        password: target.password.clone(),
        domain: target.domain.clone(),
        width: SIZE.0,
        height: SIZE.1,
        resize: true,
        egfx: true,
        clipboard: false,
        audio: None,
    });
    (session, events)
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

    /// Take the framebuffer's pixels inside one reported rectangle, as the tile path
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

    let (_, _, frame) = snapshot(&session);
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
    dump("framebuffer", shadow.width, shadow.height, &frame);
    let mut mask = vec![0u8; shadow.pixels.len()];
    for (i, out) in mask.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (a, b) = (&shadow.pixels[i * 4..i * 4 + 3], &frame[i * 4..i * 4 + 3]);
        let differs = (0..3).any(|c| (i32::from(a[c]) - i32::from(b[c])).abs() > TOLERANCE);
        *out = if differs { [0xFF, 0, 0, 0xFF] } else { [0, 0, 0, 0xFF] };
    }
    dump("leak-mask", shadow.width, shadow.height, &mask);
}
