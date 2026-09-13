//! Diagnostic: a model client that paints everything the gateway sends for a live
//! RDP target into its own canvas, then asks for a full repaint and compares the
//! canvas before and after. Pixels that differ outside the moving regions are pixels
//! the incremental path left stale on the client.
//!
//! ```sh
//! REMOTEX_UAT_TARGET=<rdp target> REMOTEX_STALE_RUN_SECS=25 REMOTEX_STALE_DUMP=tmp/qa/stale \
//!   cargo test --test stale_probe -- --ignored --nocapture
//! ```

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::raw::c_int;
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use remotex::config::{AppConfig, ChromaChoice, ClassifyLossy, RenderSubtype, RenderType, TargetConfig};
use remotex::protocol::batch;
use remotex::server;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use vpx_sys as vpx;

const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";
const RUN_ENV: &str = "REMOTEX_STALE_RUN_SECS";
const DUMP_ENV: &str = "REMOTEX_STALE_DUMP";
/// `png` for a lossless base (exact comparison), anything else for the classify/webp dial.
const BASE_ENV: &str = "REMOTEX_STALE_BASE";
const MOTION_ENV: &str = "REMOTEX_STALE_MOTION";

fn uat_target(name: &str) -> TargetConfig {
    let mut target = common::uat_target(name);
    target.width = Some(1920);
    target.height = Some(980);
    target.resize = false;
    target.render_type = RenderType::Tiles;
    if std::env::var(BASE_ENV).as_deref() == Ok("png") {
        target.render_subtype = Some(RenderSubtype::Png);
        target.render_classify_lossy = None;
    } else {
        target.render_subtype = Some(RenderSubtype::Classify);
        target.render_classify_lossy = Some(ClassifyLossy::Webp);
    }
    target.render_subtype_quality = Some(60);
    target.render_motion = !matches!(std::env::var(MOTION_ENV).as_deref(), Ok("0") | Ok("false"));
    target.render_stream_quality = Some(60);
    target.render_chroma = Some(ChromaChoice::Full);
    target.render_motion_debug = false;
    target.render_classify_debug = false;
    target.render_grid_debug = false;
    target.render_adaptive = false;
    target.render_adaptive_min = None;
    target
}

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

#[derive(Clone)]
struct Canvas {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
    /// Pixels a video record ever covered.
    video: Vec<bool>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, rgb: vec![0; w * h * 3], video: vec![false; w * h] }
    }

    /// Blit `src` rows of `bpp`-byte pixels, `sw` wide, onto (x, y) clipped to `w`×`h` of the record.
    #[allow(clippy::too_many_arguments)]
    fn blit(&mut self, x: usize, y: usize, w: usize, h: usize, src: &[u8], sw: usize, bpp: usize) {
        for row in 0..h {
            let cy = y + row;
            if cy >= self.h {
                break;
            }
            for col in 0..w {
                let cx = x + col;
                if cx >= self.w || col >= sw {
                    break;
                }
                let s = (row * sw + col) * bpp;
                if s + 3 > src.len() {
                    return;
                }
                let d = (cy * self.w + cx) * 3;
                self.rgb[d..d + 3].copy_from_slice(&src[s..s + 3]);
            }
        }
    }

    fn copy(&mut self, sx: usize, sy: usize, x: usize, y: usize, w: usize, h: usize) {
        let mut tmp = vec![0u8; w * h * 3];
        for row in 0..h {
            for col in 0..w {
                let (cx, cy) = (sx + col, sy + row);
                if cx < self.w && cy < self.h {
                    let s = (cy * self.w + cx) * 3;
                    tmp[(row * w + col) * 3..(row * w + col) * 3 + 3].copy_from_slice(&self.rgb[s..s + 3]);
                }
            }
        }
        self.blit(x, y, w, h, &tmp, w, 3);
    }

    fn mark_video(&mut self, x: usize, y: usize, w: usize, h: usize) {
        for cy in y..(y + h).min(self.h) {
            for cx in x..(x + w).min(self.w) {
                self.video[cy * self.w + cx] = true;
            }
        }
    }

    fn png(&self, path: &std::path::Path) {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, self.w as u32, self.h as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&self.rgb).unwrap();
        writer.finish().unwrap();
        std::fs::write(path, out).unwrap();
    }
}

/// One VP9 decoder per stream id.
struct Vp9 {
    ctx: vpx::vpx_codec_ctx_t,
}

impl Vp9 {
    fn new() -> Self {
        unsafe {
            let iface = vpx::vpx_codec_vp9_dx();
            let cfg = vpx::vpx_codec_dec_cfg_t { threads: 1, w: 0, h: 0 };
            let mut ctx: vpx::vpx_codec_ctx_t = std::mem::zeroed();
            let err = vpx::vpx_codec_dec_init_ver(&mut ctx, iface, &cfg, 0, vpx::VPX_DECODER_ABI_VERSION as c_int);
            assert_eq!(err, vpx::vpx_codec_err_t_VPX_CODEC_OK);
            Self { ctx }
        }
    }

    /// Decode one unit and return the last frame as RGB (`w`×`h` of the picture).
    fn decode(&mut self, data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
        unsafe {
            let err = vpx::vpx_codec_decode(&mut self.ctx, data.as_ptr(), data.len() as _, std::ptr::null_mut(), 0);
            if err != vpx::vpx_codec_err_t_VPX_CODEC_OK {
                println!("  vp9 decode error {err}");
                return None;
            }
            let mut iter: vpx::vpx_codec_iter_t = std::ptr::null();
            let mut last = None;
            loop {
                let img = vpx::vpx_codec_get_frame(&mut self.ctx, &mut iter);
                if img.is_null() {
                    break;
                }
                let img = &*img;
                let (w, h) = (img.d_w as usize, img.d_h as usize);
                let (xs, ys) = (img.x_chroma_shift as usize, img.y_chroma_shift as usize);
                let mut rgb = vec![0u8; w * h * 3];
                for y in 0..h {
                    for x in 0..w {
                        let yy = *img.planes[0].add(y * img.stride[0] as usize + x) as f32;
                        let u = *img.planes[1].add((y >> ys) * img.stride[1] as usize + (x >> xs)) as f32;
                        let v = *img.planes[2].add((y >> ys) * img.stride[2] as usize + (x >> xs)) as f32;
                        // BT.601 studio swing, which is what `crate::vp9` declares in
                        // the bitstream and what `crate::video` converted with. Reading
                        // it back as BT.709 tints every decoded pixel, and this probe
                        // counts pixels that differ.
                        let c = (yy - 16.0) * 1.164_383;
                        let d = u - 128.0;
                        let e = v - 128.0;
                        let px = &mut rgb[(y * w + x) * 3..(y * w + x) * 3 + 3];
                        px[0] = (c + 1.596_027 * e).clamp(0.0, 255.0) as u8;
                        px[1] = (c - 0.391_762 * d - 0.812_968 * e).clamp(0.0, 255.0) as u8;
                        px[2] = (c + 2.017_232 * d).clamp(0.0, 255.0) as u8;
                    }
                }
                last = Some((w, h, rgb));
            }
            last
        }
    }
}

impl Drop for Vp9 {
    fn drop(&mut self) {
        unsafe {
            vpx::vpx_codec_destroy(&mut self.ctx);
        }
    }
}

#[derive(Default)]
struct Model {
    canvas: Option<Canvas>,
    slots: HashMap<u16, (u8, u16, u16, Vec<u8>)>,
    decoders: HashMap<u8, Vp9>,
    tiles: u64,
    refs: u64,
    copies: u64,
    videos: u64,
    undecodable: u64,
    bytes: u64,
    coverage: Option<common::TileCoverage>,
    /// When each 64x64 cell was last carried by any record. A cell that
    /// disagrees with the repaint but was written a moment ago is the race
    /// between the last delivered frame and the refresh; one that disagrees
    /// and has not been written for seconds is stale.
    touched: HashMap<(u16, u16), Instant>,
    /// Which still format each cell was last delivered as, per phase. A cell
    /// `classify` sent as lossy WebP in one pass and lossless PNG in the other
    /// differs for that reason alone; only a cell encoded the same way twice
    /// can say anything about staleness.
    formats: [HashMap<(u16, u16), u8>; 2],
    phase: usize,
}

impl Model {
    fn decode_still(format: u8, payload: &[u8]) -> Option<(usize, usize, Vec<u8>, usize)> {
        match format {
            1 => {
                let decoder = png::Decoder::new(std::io::Cursor::new(payload));
                let mut reader = decoder.read_info().ok()?;
                let mut buf = vec![0; reader.output_buffer_size()?];
                let info = reader.next_frame(&mut buf).ok()?;
                let bpp = match info.color_type {
                    png::ColorType::Rgb => 3,
                    png::ColorType::Rgba => 4,
                    other => {
                        println!("  png color type {other:?}");
                        return None;
                    }
                };
                buf.truncate(info.buffer_size());
                Some((info.width as usize, info.height as usize, buf, bpp))
            }
            3 => {
                let image = webp::Decoder::new(payload).decode()?;
                let bpp = if image.is_alpha() { 4 } else { 3 };
                Some((image.width() as usize, image.height() as usize, image.to_vec(), bpp))
            }
            other => {
                println!("  unexpected tile format {other}");
                None
            }
        }
    }

    /// Every cell `rect` covers, written now.
    fn touch(&mut self, x: u16, y: u16, w: u16, h: u16, format: Option<u8>) {
        let now = Instant::now();
        for cy in (y / 64)..(y.saturating_add(h)).div_ceil(64) {
            for cx in (x / 64)..(x.saturating_add(w)).div_ceil(64) {
                self.touched.insert((cx, cy), now);
                if let Some(format) = format {
                    self.formats[self.phase].insert((cx, cy), format);
                }
            }
        }
    }

    fn paint_still(&mut self, x: u16, y: u16, w: u16, h: u16, format: u8, payload: &[u8]) {
        let Some((pw, _ph, pixels, bpp)) = Self::decode_still(format, payload) else {
            self.undecodable += 1;
            return;
        };
        if let Some(canvas) = &mut self.canvas {
            canvas.blit(x as usize, y as usize, w as usize, h as usize, &pixels, pw, bpp);
        }
        self.touch(x, y, w, h, Some(format));
        if let Some(coverage) = &mut self.coverage {
            coverage.add((x, y, w, h));
        }
    }

    fn frame(&mut self, frame: &[u8]) {
        self.bytes += frame.len() as u64;
        assert_eq!(frame[0], batch::FRAME_KIND);
        let count = u16::from_le_bytes([frame[2], frame[3]]);
        let mut at = batch::HEADER_LEN;
        let mut seen = 0;
        while at < frame.len() {
            let le = |o: usize| u16::from_le_bytes([frame[at + o], frame[at + o + 1]]);
            match frame[at] {
                batch::OP_TILE_REF => {
                    let (slot, x, y) = (le(1), le(3), le(5));
                    self.refs += 1;
                    let (format, w, h, payload) = self.slots.get(&slot).cloned().expect("reference to an empty slot");
                    self.paint_still(x, y, w, h, format, &payload);
                    at += batch::TILE_REF_LEN;
                }
                batch::OP_TILE => {
                    let len = u32::from_le_bytes([frame[at + 12], frame[at + 13], frame[at + 14], frame[at + 15]]) as usize;
                    let format = frame[at + 1];
                    let (slot, x, y, w, h) = (le(2), le(4), le(6), le(8), le(10));
                    let start = at + batch::TILE_HEADER_LEN;
                    let payload = &frame[start..start + len];
                    self.tiles += 1;
                    if slot != batch::NO_SLOT {
                        self.slots.insert(slot, (format, w, h, payload.to_vec()));
                    }
                    self.paint_still(x, y, w, h, format, payload);
                    at = start + len;
                }
                batch::OP_COPY => {
                    let (sx, sy, x, y, w, h) = (le(1), le(3), le(5), le(7), le(9), le(11));
                    self.copies += 1;
                    if let Some(canvas) = &mut self.canvas {
                        canvas.copy(sx as usize, sy as usize, x as usize, y as usize, w as usize, h as usize);
                    }
                    self.touch(x, y, w, h, None);
                    if let Some(coverage) = &mut self.coverage {
                        coverage.add((x, y, w, h));
                    }
                    at += batch::COPY_LEN;
                }
                batch::OP_VIDEO => {
                    let stream = frame[at + 1];
                    let len = u32::from_le_bytes([frame[at + 11], frame[at + 12], frame[at + 13], frame[at + 14]]) as usize;
                    let (x, y, w, h) = (le(3), le(5), le(7), le(9));
                    let start = at + batch::VIDEO_HEADER_LEN;
                    let data = &frame[start..start + len];
                    self.videos += 1;
                    let decoder = self.decoders.entry(stream).or_insert_with(Vp9::new);
                    if let (Some((pw, _ph, rgb)), Some(canvas)) =
                        (decoder.decode(data), &mut self.canvas)
                    {
                        canvas.blit(x as usize, y as usize, w as usize, h as usize, &rgb, pw, 3);
                        canvas.mark_video(x as usize, y as usize, w as usize, h as usize);
                    }
                    self.touch(x, y, w, h, None);
                    if let Some(coverage) = &mut self.coverage {
                        coverage.add((x, y, w, h));
                    }
                    at = start + len;
                }
                op => panic!("unknown record op {op}"),
            }
            seen += 1;
        }
        assert_eq!(seen, count);
    }
}

/// Compare two canvases and write a diff picture; report counts inside and outside the video mask.
fn compare(before: &Canvas, after: &Canvas, dump: Option<&std::path::Path>) {
    let (w, h) = (before.w, before.h);
    let mut diff = Canvas::new(w, h);
    let (mut out_any, mut out_big, mut in_any, mut in_big) = (0u64, 0u64, 0u64, 0u64);
    let mut cells: HashMap<(usize, usize), u64> = HashMap::new();
    for i in 0..w * h {
        let a = &before.rgb[i * 3..i * 3 + 3];
        let b = &after.rgb[i * 3..i * 3 + 3];
        let d = (0..3).map(|c| (a[c] as i16 - b[c] as i16).unsigned_abs()).max().unwrap();
        let video = before.video[i] || after.video[i];
        if d > 0 {
            if video {
                in_any += 1;
            } else {
                out_any += 1;
            }
        }
        if d > 32 {
            if video {
                in_big += 1;
                diff.rgb[i * 3 + 1] = 128;
            } else {
                out_big += 1;
                diff.rgb[i * 3] = 255;
                diff.rgb[i * 3 + 1] = 255;
                diff.rgb[i * 3 + 2] = 255;
                *cells.entry(((i % w) / 64, (i / w) / 64)).or_default() += 1;
            }
        } else if video {
            diff.rgb[i * 3 + 2] = 48;
        }
    }
    println!(
        "  outside video: {out_any} px differ, {out_big} by >32; inside video: {in_any} differ, {in_big} by >32"
    );
    let mut worst: Vec<_> = cells.into_iter().collect();
    worst.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for ((cx, cy), n) in worst.iter().take(24) {
        println!("    cell ({},{}) at {}x{}: {n} px stale", cx, cy, cx * 64, cy * 64);
    }
    if let Some(dir) = dump {
        diff.png(&dir.join("diff.png"));
    }
}

/// A cell that disagrees with the repaint is only stale if nothing was sent for
/// it in a while: a cell written moments before the refresh disagrees because the
/// desktop moved in between, which is the measurement's own shadow and not a bug.
/// [`SETTLED`] is where one stops being the other.
const SETTLED: Duration = Duration::from_secs(3);

/// How far apart two pixels must be to count as a difference rather than the
/// lossy encoder rounding the same colour twice.
const APART: i32 = 32;

/// Cells that disagree with the repaint, cut by how long they had gone unwritten.
#[allow(clippy::too_many_arguments)]
fn report_stale(
    before: &Canvas,
    after: &Canvas,
    touched: &HashMap<(u16, u16), Instant>,
    formats: &[HashMap<(u16, u16), u8>; 2],
    refresh: Instant,
    units: u64,
    tiles: u64,
    refs: u64,
    kib: u64,
) {
    let mut recoded = 0usize;
    let mut swaps: HashMap<(Option<u8>, Option<u8>), usize> = HashMap::new();
    let (cols, rows) = (before.w.div_ceil(64), before.h.div_ceil(64));
    let mut settled = Vec::new();
    let mut fresh = 0usize;
    for cy in 0..rows {
        for cx in 0..cols {
            let (x0, y0) = (cx * 64, cy * 64);
            let (x1, y1) = ((x0 + 64).min(before.w), (y0 + 64).min(before.h));
            let mut differs = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    let at = (y * before.w + x) * 3;
                    if (0..3).any(|c| {
                        (i32::from(before.rgb[at + c]) - i32::from(after.rgb[at + c])).abs() > APART
                    }) {
                        differs += 1;
                    }
                }
            }
            if differs == 0 {
                continue;
            }
            let age = touched
                .get(&(cx as u16, cy as u16))
                .map_or(Duration::MAX, |seen| refresh.saturating_duration_since(*seen));
            let key = (cx as u16, cy as u16);
            if formats[0].get(&key) != formats[1].get(&key) {
                recoded += 1;
                *swaps.entry((formats[0].get(&key).copied(), formats[1].get(&key).copied())).or_insert(0usize) += 1;
                continue;
            }
            if age >= SETTLED {
                settled.push((x0, y0, differs, age));
            } else {
                fresh += 1;
            }
        }
    }
    let name = |f: Option<u8>| match f {
        Some(1) => "png".to_string(),
        Some(3) => "webp".to_string(),
        Some(other) => format!("format {other}"),
        None => "never sent as a still".to_string(),
    };
    let mut swaps: Vec<_> = swaps.into_iter().collect();
    swaps.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for ((from, to), count) in &swaps {
        println!("    {count} cells went from {} live to {} on the repaint", name(*from), name(*to));
    }
    settled.sort_by_key(|(_, _, differs, _)| std::cmp::Reverse(*differs));
    println!(
        "  RESULT arm={} units={} tiles={} refs={} kib={} recoded={} lossy_to_lossless={} stale={}",
        std::env::var("REMOTEX_STALE_ARM").unwrap_or_else(|_| "unnamed".into()),
        units,
        tiles,
        refs,
        kib,
        recoded,
        swaps
            .iter()
            .filter(|((from, to), _)| *from == Some(3) && *to == Some(1))
            .map(|(_, count)| *count)
            .sum::<usize>(),
        settled.len(),
    );
    // A run the remote spent still proves nothing about a path that only exists
    // while something moves, and comparing one against a run that had a video in it
    // is how a measurement lies. Said outright rather than left to be inferred from
    // the counts.
    if units < 200 {
        println!(
            "  WARNING: only {units} video units arrived — nothing was really playing on the \
             remote, so this run is not comparable with one that was"
        );
    }
    println!(
        "  {} cells disagree after settling for {SETTLED:?}; {fresh} more were still being written, \
         {recoded} were encoded differently in the two passes",
        settled.len()
    );
    for (x, y, differs, age) in settled.iter().take(24) {
        let age = if *age == Duration::MAX { "never written".to_string() } else { format!("{:.1}s", age.as_secs_f32()) };
        println!("    cell at {x},{y}: {differs} px differ, last written {age} before the refresh");
    }
}

#[tokio::test]
#[ignore]
async fn a_repaint_matches_what_was_delivered() {
    common::init_logging();
    let name = std::env::var(TARGET_ENV).expect("set REMOTEX_UAT_TARGET");
    let run = Duration::from_secs(std::env::var(RUN_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(25));
    let dump = std::env::var(DUMP_ENV).ok().map(std::path::PathBuf::from);
    if let Some(dir) = &dump {
        std::fs::create_dir_all(dir).unwrap();
    }
    let addr = spawn_app(uat_target(&name)).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = common::connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, &name).await;

    let mut model = Model::default();
    let mut phase = 0; // 0 = running, 1 = repaint after refresh
    let mut before: Option<Canvas> = None;
    let mut touched_before: HashMap<(u16, u16), Instant> = HashMap::new();
    let (mut units_before, mut tiles_before, mut refs_before, mut kib_before) = (0u64, 0u64, 0u64, 0u64);
    let mut deadline = Instant::now() + run;
    let mut first_batch: Option<Instant> = None;
    let mut refresh_at: Option<Instant> = None;
    let snap = Duration::from_secs(std::env::var("REMOTEX_STALE_SNAP_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(5));
    let mut last_snap = Instant::now();
    let mut snaps = 0;

    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(msg)) => msg.expect("websocket receive"),
            Ok(None) => panic!("websocket closed"),
            Err(_) => {
                if phase == 0 {
                    // Snapshot and ask for the repaint.
                    let canvas = model.canvas.clone().expect("a desktop was announced");
                    println!(
                        "  before refresh: {} tiles, {} refs, {} copies, {} video units, {} undecodable, \
                         {} streams, {} KiB on the wire",
                        model.tiles, model.refs, model.copies, model.videos, model.undecodable,
                        model.decoders.len(), model.bytes / 1024
                    );
                    if let Some(dir) = &dump {
                        canvas.png(&dir.join("before.png"));
                    }
                    model.coverage = Some(common::TileCoverage::new(canvas.w as u32, canvas.h as u32));
                    before = Some(canvas);
                    (units_before, tiles_before, refs_before, kib_before) =
                        (model.videos, model.tiles, model.refs, model.bytes / 1024);
                    touched_before = model.touched.clone();
                    model.phase = 1;
                    ws.send(Message::Text(r#"{"type":"refresh"}"#.into())).await.unwrap();
                    refresh_at = Some(Instant::now());
                    phase = 1;
                    deadline = Instant::now() + Duration::from_secs(40);
                    continue;
                }
                panic!(
                    "the repaint never covered the desktop: {} px covered",
                    model.coverage.as_ref().map_or(0, |c| c.covered())
                );
            }
        };
        match msg {
            Message::Text(text) => {
                assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                let control: serde_json::Value = serde_json::from_str(&text).unwrap();
                match control["type"].as_str().unwrap_or("") {
                    "resize" => {
                        let w = control["w"].as_u64().unwrap() as usize;
                        let h = control["h"].as_u64().unwrap() as usize;
                        println!("  resize {w}x{h}");
                        model.canvas = Some(Canvas::new(w, h));
                    }
                    "videoFormat" | "videoEnd" => println!("  {text}"),
                    _ => {}
                }
            }
            Message::Binary(frame) => {
                first_batch.get_or_insert_with(Instant::now);
                if phase == 0 && last_snap.elapsed() >= snap {
                    last_snap = Instant::now();
                    if let (Some(dir), Some(canvas)) = (&dump, &model.canvas) {
                        canvas.png(&dir.join(format!("snap-{snaps:02}.png")));
                        snaps += 1;
                    }
                }
                let sequence = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
                model.frame(&frame);
                ws.send(Message::Text(
                    format!(r#"{{"type":"paintAck","sequence":{sequence},"queuedMs":0,"drawMs":1}}"#).into(),
                ))
                .await
                .unwrap();
                if phase == 1 && model.coverage.as_ref().is_some_and(|c| c.is_complete()) {
                    break;
                }
            }
            _ => {}
        }
    }
    let after = model.canvas.clone().unwrap();
    println!(
        "  repaint covered the desktop in {:.1}s; {} tiles, {} refs, {} copies, {} video units in all",
        refresh_at.unwrap().elapsed().as_secs_f32(),
        model.tiles,
        model.refs,
        model.copies,
        model.videos
    );
    if let Some(dir) = &dump {
        after.png(&dir.join("after.png"));
    }
    let before = before.as_ref().unwrap();
    compare(before, &after, dump.as_deref());
    report_stale(
        before,
        &after,
        &touched_before,
        &model.formats,
        refresh_at.unwrap(),
        units_before,
        tiles_before,
        refs_before,
        kib_before,
    );
    ws.close(None).await.ok();
}
