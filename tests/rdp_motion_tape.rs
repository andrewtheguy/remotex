//! A tape of what a Windows host draws through the graphics pipeline, for the
//! motion encoder to be built and tuned against without the host.
//!
//! The session is opened with the pipeline passed, as the page has it, and every
//! run of commands the session passes is written to the tape as it came, with the
//! time it came at and the frame it ends; each run is composed here, as the page
//! composes it, so the host is acknowledged and keeps drawing. A replay feeds the
//! runs to a [`Compositor`] in order and has every picture and every rectangle
//! the host painted, at the times it painted them.
//!
//! The tape is a gzip stream: `RXTAPE1\n`, then one record for each event —
//! `u8 kind`, `u32 ms` since the session started, and for a run (`kind` 2)
//! `u32 frame` (`u32::MAX` for a run that ends none), `u32 len` and the commands;
//! `kind` 1 is a pipeline starting over, after which a replay composes from nothing.
//! Integers little-endian.
//!
//! ```sh
//! REMOTEX_UAT_TARGET=<rdp target in tmp/test_uat.toml> REMOTEX_TAPE=tmp/motion/video.tape \
//!   cargo test --release --test rdp_motion_tape -- --ignored --nocapture
//! ```
//!
//! Put up what is to be recorded before it starts, and keep everything else on the
//! screen still. The session takes the host's console over, so nobody can drive the
//! desktop while it records: what plays by itself is recorded as it is, and a scroll
//! or a window drag is driven from here — `REMOTEX_TAPE_SCROLL=x,y` turns the wheel
//! over that point, three notches a tick, down a stretch and back up, resting a
//! second between; `REMOTEX_TAPE_DRAG=x,y` drags the title bar at that point round a
//! circle and back, resting a second after each lap.

mod common;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use remotex::rdp_client::proto::gfx::H264;
use remotex::rdp_client::{AudioSink, Compositor, Connect, Event, Input, MouseButton, Session};

const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";
const TAPE_ENV: &str = "REMOTEX_TAPE";
const SECS_ENV: &str = "REMOTEX_TAPE_SECS";
const SCROLL_ENV: &str = "REMOTEX_TAPE_SCROLL";
const DRAG_ENV: &str = "REMOTEX_TAPE_DRAG";

/// The size the operator's QA runs at.
const SIZE: (u32, u32) = (1920, 980);

/// The grid the per-second report counts painted cells on: ZRLE's tile.
const CELL: u32 = 64;

const START_OVER: u8 = 1;
const RUN: u8 = 2;

/// What the recorder does to the desktop while it records.
enum Driver {
    Scroll { x: u16, y: u16 },
    Drag { x: i32, y: i32 },
}

impl Driver {
    fn from_env() -> Option<Self> {
        let point = |name: &str| {
            let at = std::env::var(name).ok()?;
            let (x, y) = at.split_once(',').unwrap_or_else(|| panic!("{name} is x,y"));
            Some((x.parse::<i32>().expect("an x"), y.parse::<i32>().expect("a y")))
        };
        if let Some((x, y)) = point(SCROLL_ENV) {
            return Some(Self::Scroll { x: x as u16, y: y as u16 });
        }
        point(DRAG_ENV).map(|(x, y)| Self::Drag { x, y })
    }

    /// Do step `step` of the driver's cycle, and say how long until the next.
    fn step(&self, input: &Input, step: u64) -> Duration {
        match *self {
            Self::Scroll { x, y } => {
                // Twenty ticks down, a rest, twenty up, a rest.
                let at = step % 42;
                if at == 0 {
                    input.mouse_move(x, y);
                }
                match at {
                    0..20 => input.wheel(-360, false, x, y),
                    20 => return Duration::from_secs(1),
                    21..41 => input.wheel(360, false, x, y),
                    _ => return Duration::from_secs(1),
                }
                Duration::from_millis(120)
            }
            Self::Drag { x, y } => {
                let point = |px: i32, py: i32| {
                    (px.clamp(0, SIZE.0 as i32 - 1) as u16, py.clamp(0, SIZE.1 as i32 - 1) as u16)
                };
                let (sx, sy) = point(x, y);
                // A press, 32 moves round a circle, a release, and a rest.
                match step % 35 {
                    0 => {
                        input.mouse_move(sx, sy);
                        input.mouse_button(MouseButton::Left, true, sx, sy);
                    }
                    lap @ 1..=32 => {
                        let angle = lap as f64 * std::f64::consts::TAU / 32.0;
                        let (px, py) =
                            point(x + (260.0 * angle.sin()) as i32, y + (160.0 * (1.0 - angle.cos())) as i32);
                        input.mouse_move(px, py);
                    }
                    33 => input.mouse_button(MouseButton::Left, false, sx, sy),
                    _ => return Duration::from_secs(1),
                }
                Duration::from_millis(40)
            }
        }
    }
}

struct Silence;

impl AudioSink for Silence {
    fn negotiated(&self, _: remotex::rdp_client::proto::rdpsnd::Format) {}
    fn wave(&self, _: Vec<u8>) {}
    fn closed(&self) {}
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real Windows host, named by REMOTEX_UAT_TARGET"]
async fn a_real_host_is_recorded_to_a_tape() {
    common::init_logging();
    let name = std::env::var(TARGET_ENV)
        .unwrap_or_else(|_| panic!("set {TARGET_ENV} to the name of an rdp target in tmp/test_uat.toml"));
    let path = std::env::var(TAPE_ENV).unwrap_or_else(|_| panic!("set {TAPE_ENV} to the tape to write"));
    let secs: u64 = std::env::var(SECS_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(40);
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir).expect("making the tape's directory");
    }
    let mut tape = GzEncoder::new(std::fs::File::create(&path).expect("creating the tape"), Compression::fast());
    tape.write_all(b"RXTAPE1\n").expect("writing the tape");

    let target = common::uat_target(&name);
    println!("rdp_motion_tape: {name} ({}:{}) at {}x{} for {secs} s into {path}", target.host, target.port, SIZE.0, SIZE.1);
    let (session, mut events) = Session::start(Connect {
        host: target.host.clone(),
        port: target.port,
        username: target.username.clone(),
        password: target.password.clone(),
        domain: target.domain.clone(),
        width: SIZE.0,
        height: SIZE.1,
        scale_percent: 0,
        monitors: 1,
        placement: remotex::config::Placement::Right,
        resize: false,
        egfx: true,
        h264: H264::Refused,
        autodetect: false,
        multitransport: false,
        paced: false,
        audio: Some(Box::new(Silence) as Box<dyn AudioSink>),
        camera: None,
        microphone: None,
    });
    let input = session.input().clone();
    let mut compositor = Compositor::new();
    let driver = Driver::from_env();
    // Driven once the opening desktop has been drawn.
    let mut next = Instant::now() + Duration::from_secs(4);
    let mut step = 0u64;

    let started = Instant::now();
    let until = started + Duration::from_secs(secs);
    let mut second = 0u64;
    let (mut frames, mut bytes, mut cells) = (0u64, 0u64, BTreeSet::new());
    let (mut all_frames, mut all_bytes) = (0u64, 0u64);
    let report = |second: u64, frames: u64, bytes: u64, cells: &BTreeSet<(u32, u32)>| {
        println!(
            "  {second:>3}s: {frames:>3} frames, {:>8.3} MB of commands, {:>4} cells painted",
            bytes as f64 / 1e6,
            cells.len()
        );
    };
    while Instant::now() < until {
        let wake = if driver.is_some() { next.min(until) } else { until };
        let event = tokio::select! {
            event = events.recv() => event,
            () = tokio::time::sleep_until(wake.into()) => {
                if let Some(driver) = &driver && Instant::now() >= next {
                    next = Instant::now() + driver.step(&input, step);
                    step += 1;
                }
                continue;
            }
        };
        let ms = started.elapsed().as_millis() as u32;
        let now = u64::from(ms) / 1000;
        while second < now {
            report(second, frames, bytes, &cells);
            (frames, bytes) = (0, 0);
            cells.clear();
            second += 1;
        }
        match event.expect("an event") {
            Event::FramesMarked => {
                compositor = Compositor::new();
                tape.write_all(&[START_OVER]).expect("writing the tape");
                tape.write_all(&ms.to_le_bytes()).expect("writing the tape");
            }
            Event::Graphics { commands, frame } => {
                tape.write_all(&[RUN]).expect("writing the tape");
                tape.write_all(&ms.to_le_bytes()).expect("writing the tape");
                tape.write_all(&frame.unwrap_or(u32::MAX).to_le_bytes()).expect("writing the tape");
                tape.write_all(&(commands.len() as u32).to_le_bytes()).expect("writing the tape");
                tape.write_all(&commands).expect("writing the tape");
                bytes += commands.len() as u64;
                all_bytes += commands.len() as u64;
                let composed = compositor.compose(&commands).expect("the passed commands compose");
                for rect in &composed.painted {
                    for row in rect.y / CELL..(rect.y + rect.height).div_ceil(CELL) {
                        for col in rect.x / CELL..(rect.x + rect.width).div_ceil(CELL) {
                            cells.insert((col, row));
                        }
                    }
                }
                frames += u64::from(composed.frames);
                all_frames += u64::from(composed.frames);
                if let Some(frame) = frame {
                    input.frame_composed(frame);
                }
            }
            Event::Ended(result) => panic!("the session ended: {result:?}"),
            _ => {}
        }
    }
    report(second, frames, bytes, &cells);
    tape.finish().expect("finishing the tape");
    // The picture the tape ends on, beside it: where a scroll or a drag should be
    // aimed is read off it.
    compositor.framebuffer().with(|frame| {
        let mut rgba = frame.pixels.clone();
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 0xFF;
        }
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        writer.finish().expect("png finish");
        std::fs::write(format!("{path}.png"), out).expect("writing the last picture");
    });
    let written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "rdp_motion_tape: {all_frames} frames, {:.1} MB of commands, the tape {:.1} MB",
        all_bytes as f64 / 1e6,
        written as f64 / 1e6
    );
    drop(session);
}

/// One tape read back: each event as it was written.
enum Taped {
    StartOver,
    Run { ms: u32, frame: Option<u32>, commands: Vec<u8> },
}

fn read_tape(path: &str) -> Vec<Taped> {
    use std::io::Read as _;
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).expect("opening the tape"))
        .read_to_end(&mut raw)
        .expect("reading the tape");
    assert!(raw.starts_with(b"RXTAPE1\n"), "{path} is not a tape");
    let mut at = 8;
    let u32_at = |at: usize| u32::from_le_bytes(raw[at..at + 4].try_into().unwrap());
    let mut events = Vec::new();
    while at < raw.len() {
        let kind = raw[at];
        let ms = u32_at(at + 1);
        at += 5;
        match kind {
            START_OVER => events.push(Taped::StartOver),
            RUN => {
                let frame = u32_at(at);
                let len = u32_at(at + 4) as usize;
                at += 8;
                let commands = raw[at..at + len].to_vec();
                at += len;
                events.push(Taped::Run { ms, frame: (frame != u32::MAX).then_some(frame), commands });
            }
            other => panic!("a tape record of kind {other}"),
        }
    }
    events
}

/// What a tape's host drew with: each command's count and bytes, each codec's
/// bytes and the area it drew, and the area copies moved.
///
/// ```sh
/// REMOTEX_TAPE=tmp/motion/scroll.tape cargo test --release --test rdp_motion_tape -- --ignored --nocapture summarised
/// ```
#[test]
#[ignore = "reads a tape named by REMOTEX_TAPE"]
fn a_tape_is_summarised() {
    use remotex::rdp_client::proto::gfx::{self, Message};
    use std::collections::BTreeMap;
    let path = std::env::var(TAPE_ENV).unwrap_or_else(|_| panic!("set {TAPE_ENV} to the tape to read"));
    let events = read_tape(&path);
    let mut commands: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
    let mut codecs: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
    let (mut copied, mut cached, mut filled) = (0u64, 0u64, 0u64);
    let (mut runs, mut frames, mut last_ms) = (0u64, 0u64, 0u32);
    for event in &events {
        let Taped::Run { ms, commands: buffer, frame } = event else { continue };
        runs += 1;
        frames += u64::from(frame.is_some());
        last_ms = *ms;
        for command in gfx::commands(buffer) {
            let command = command.expect("a command");
            let bytes = (command.end - command.start) as u64;
            let entry = commands.entry(gfx::command_name(command.id)).or_default();
            entry.0 += 1;
            entry.1 += bytes;
            let Some(message) = gfx::messages(&buffer[command.start..command.end]).next() else { continue };
            match message.expect("a message") {
                Message::WireToSurface1 { codec, rect, .. } => {
                    let entry = codecs.entry(gfx::codec_name(codec)).or_default();
                    entry.0 += bytes;
                    entry.1 += u64::from(rect.width()) * u64::from(rect.height());
                }
                Message::WireToSurface2 { codec, .. } => codecs.entry(gfx::codec_name(codec)).or_default().0 += bytes,
                Message::SurfaceToSurface { rect, points, .. } => {
                    copied += u64::from(rect.width()) * u64::from(rect.height()) * points.len() as u64;
                }
                Message::CacheToSurface { points, .. } => cached += points.len() as u64,
                Message::SolidFill { rects, .. } => {
                    filled += rects.iter().map(|r| u64::from(r.width()) * u64::from(r.height())).sum::<u64>();
                }
                _ => {}
            }
        }
    }
    println!("{path}: {runs} runs, {frames} frames, over {:.1} s", f64::from(last_ms) / 1000.0);
    for (name, (count, bytes)) in &commands {
        println!("  command {name:<22} {count:>7} {:>10.3} MB", *bytes as f64 / 1e6);
    }
    for (name, (bytes, area)) in &codecs {
        println!("  codec   {name:<22} {:>10.3} MB over {:>8.2} Mpx", *bytes as f64 / 1e6, *area as f64 / 1e6);
    }
    println!(
        "  surface copies moved {:.2} Mpx, cache stamps {cached}, fills {:.2} Mpx",
        copied as f64 / 1e6,
        filled as f64 / 1e6
    );
}

/// A tape played through the compositor and the motion encoder as a session with
/// Motion plays it, at the times it was recorded: what the picture sent for motion
/// costs, and whether the page's picture ends exact.
///
/// The host's paints go through the gateway's shadow, as the engine's do, so a
/// repaint of the same pixels costs nothing; a round is taken no more than every
/// 33 ms and whenever one comes due. The page is played by an independent reading
/// of the records: PNGs decoded and drawn, and the regions' rectangles marked as not
/// exact until a tile draws over them.
///
/// ```sh
/// REMOTEX_TAPE=tmp/motion/video-window.tape cargo test --release --test rdp_motion_tape -- --ignored --nocapture replayed
/// ```
#[test]
#[ignore = "reads a tape named by REMOTEX_TAPE"]
fn a_tape_is_replayed_for_motion() {
    use remotex::motion::MotionStream;
    use remotex::protocol::ServerMsg;
    use remotex::shadow::{Rect as ShadowRect, Shadow};

    const ROUND: Duration = Duration::from_millis(33);
    let path = std::env::var(TAPE_ENV).unwrap_or_else(|_| panic!("set {TAPE_ENV} to the tape to read"));
    let events = read_tape(&path);
    let base = Instant::now();
    let mut compositor = Compositor::new();
    let mut stream: Option<(MotionStream, Shadow, (u16, u16))> = None;
    let mut page: Vec<u8> = Vec::new();
    let mut lossy: Vec<ShadowRect> = Vec::new();
    let (mut tiles, mut tile_bytes, mut units, mut unit_bytes, mut keyframes, mut regions) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut encoding = Duration::ZERO;
    let mut last_round = base;
    let mut last_ms = 0;

    // Take and play rounds owed at `now`.
    let mut play = |stream: &mut MotionStream, size: (u16, u16), page: &mut Vec<u8>, lossy: &mut Vec<ShadowRect>, now: Instant| {
        let Some(mut round) = stream.take_round(now, false).expect("a round") else { return false };
        let started = Instant::now();
        let records = round.encode().expect("an encode");
        encoding += started.elapsed();
        stream.put_back(round, now);
        let stride = usize::from(size.0) * 3;
        for record in records {
            match record {
                ServerMsg::TilesStart => {
                    page.clear();
                    page.resize(stride * usize::from(size.1), 0);
                    lossy.clear();
                }
                ServerMsg::Tile(tile) => {
                    tiles += 1;
                    tile_bytes += tile.png.len() as u64;
                    let mut reader = png::Decoder::new(std::io::Cursor::new(tile.png)).read_info().unwrap();
                    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
                    reader.next_frame(&mut buf).unwrap();
                    let width = usize::from(tile.w) * 3;
                    for row in 0..usize::from(tile.h) {
                        let at = (usize::from(tile.y) + row) * stride + usize::from(tile.x) * 3;
                        page[at..at + width].copy_from_slice(&buf[row * width..(row + 1) * width]);
                    }
                }
                ServerMsg::Region(unit) => {
                    units += 1;
                    unit_bytes += unit.data.len() as u64;
                    keyframes += u64::from(unit.keyframe);
                    lossy.push(ShadowRect::from_size(unit.x, unit.y, unit.w, unit.h).unwrap());
                }
                ServerMsg::RegionFormat { .. } => regions += 1,
                ServerMsg::RegionEnd { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        true
    };

    for event in &events {
        match event {
            Taped::StartOver => compositor = Compositor::new(),
            Taped::Run { ms, commands, .. } => {
                last_ms = *ms;
                let now = base + Duration::from_millis(u64::from(*ms));
                let composed = compositor.compose(commands).expect("the tape composes");
                let (w, h) = compositor.framebuffer().with(|frame| (frame.width as u16, frame.height as u16));
                if w == 0 {
                    continue;
                }
                if stream.as_ref().is_none_or(|(_, _, size)| *size != (w, h)) {
                    let mut motion = MotionStream::new(60, false, false);
                    motion.want(w, h);
                    stream = Some((motion, Shadow::new("tape", w, h), (w, h)));
                }
                let (motion, shadow, size) = stream.as_mut().unwrap();
                for rect in composed.painted {
                    let rect = ShadowRect::from_size(rect.x as u16, rect.y as u16, rect.width as u16, rect.height as u16).unwrap();
                    let rgb = compositor.framebuffer().with(|frame| {
                        let mut rgb = Vec::with_capacity(usize::from(rect.w()) * usize::from(rect.h()) * 3);
                        for row in rect.top..=rect.bottom {
                            let at = usize::from(row) * frame.stride + usize::from(rect.left) * 4;
                            for px in frame.pixels[at..at + usize::from(rect.w()) * 4].as_chunks::<4>().0 {
                                rgb.extend_from_slice(&px[..3]);
                            }
                        }
                        rgb
                    });
                    if let Some((changed, pixels)) = shadow.changed(rect, &rgb) {
                        motion.blit(changed, &pixels, now).unwrap();
                    }
                }
                if composed.frames > 0 && now >= last_round + ROUND {
                    // Rounds the gateway would have taken since, as they came due.
                    if play(motion, *size, &mut page, &mut lossy, now) {
                        last_round = now;
                    }
                }
            }
        }
    }
    // The tape's end: everything owed comes due with nothing changing.
    let (motion, _, size) = stream.as_mut().expect("the tape drew something");
    let mut now = base + Duration::from_millis(u64::from(last_ms));
    while let Some(due) = motion.next_due(now) {
        now = due.max(now + ROUND);
        play(motion, *size, &mut page, &mut lossy, now);
        assert!(now < base + Duration::from_millis(u64::from(last_ms)) + Duration::from_secs(30), "it settles");
    }
    let exact = compositor.framebuffer().with(|frame| {
        let mut rgb = Vec::new();
        for px in frame.pixels.as_chunks::<4>().0 {
            rgb.extend_from_slice(&px[..3]);
        }
        rgb == page
    });
    let secs = f64::from(last_ms) / 1000.0;
    println!(
        "{path}: {secs:.1} s; {tiles} tiles {:.2} MB, {units} region units {:.2} MB ({keyframes} keyframes) over {regions} regions; \
         {:.2} MB/s in all; encoding {:.2} s; exact once settled: {exact}",
        tile_bytes as f64 / 1e6,
        unit_bytes as f64 / 1e6,
        (tile_bytes + unit_bytes) as f64 / 1e6 / secs.max(0.001),
        encoding.as_secs_f64()
    );
    assert!(exact, "the page's picture is the host's once everything has settled");
}
