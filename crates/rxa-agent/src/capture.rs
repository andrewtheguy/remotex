//! ScreenCaptureKit capture: native dirty rects in, packed-RGB tiles out.
//!
//! ## Why dirty rects are load-bearing
//!
//! ScreenCaptureKit hands us the regions that actually changed, in the
//! `SCStreamFrameInfo.dirtyRects` attachment on each `CMSampleBuffer`. Encoding
//! only those is the difference between a usable Retina session and one that
//! pegs a core diffing full frames. This is the single most important API
//! detail in the whole agent, and it is why `screencapturekit` was chosen over
//! `objc2-screen-capture-kit`: it exposes the attachment directly.
//!
//! ## The seam
//!
//! Everything above this module deals only in [`RawTile`] — a rectangle plus
//! packed RGB888 — via the [`FrameSink`] trait. Swapping the binding crate, or
//! moving to full-frame diffing if a future macOS stops reporting dirty rects,
//! is contained here.
//!
//! ## Two bugs this module exists to not have
//!
//! - **Stride.** `CVPixelBuffer::bytes_per_row` is *not* `width * 4`; macOS
//!   pads rows for alignment. Every read goes row by row at the reported
//!   stride. Assuming `width * 4` produces a picture that shears progressively
//!   further right as it goes down — the classic ScreenCaptureKit bug.
//! - **Backing scale.** A Retina display is captured at pixel dimensions that
//!   differ from the point dimensions `CGEventPost` wants. We capture at full
//!   pixel size for fidelity and report the measured scale so
//!   [`crate::input`] can divide by it. The scale is *measured* from the
//!   surface rather than assumed, because assuming it is how clicks end up in
//!   the wrong place.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use log::{debug, info, warn};
use screencapturekit::cm::SCFrameStatus;
use screencapturekit::stream::delegate_trait::ErrorHandler;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;

/// Dirty rectangles taller than this are split into strips, so a full-screen
/// repaint becomes many small messages the browser can start painting
/// immediately rather than one enormous one. Mirrors the gateway's
/// `protocol::STRIP_ROWS`, for the same reason.
const STRIP_ROWS: u16 = 64;

/// A rectangle in captured-surface pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// One tile's worth of changed screen: a rectangle plus packed RGB888.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTile {
    pub rect: Rect,
    /// `w * h * 3` bytes, row-major, no padding.
    pub rgb: Vec<u8>,
}

/// Where captured tiles go. Implemented by the session pipeline.
///
/// Called from ScreenCaptureKit's dispatch queue — an arbitrary thread, possibly
/// several concurrently — so implementations must be cheap and must not block.
pub trait FrameSink: Send + Sync + 'static {
    /// One frame's changed tiles. Empty vectors are never sent.
    fn tiles(&self, tiles: Vec<RawTile>);
    /// The captured surface size changed (display reconfigured, mode switch).
    fn resized(&self, w: u16, h: u16);
    /// The stream cannot continue. The session decides whether to restart it.
    fn failed(&self, message: String);
}

/// What the agent needs to know about a display before it starts streaming:
/// the size to announce in `Hello`, and the two numbers input conversion needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    /// Captured surface size in pixels.
    pub width: u16,
    pub height: u16,
    /// Captured pixels per display point — 2.0 on a Retina panel. Input
    /// coordinates are divided by this (see [`crate::input`]).
    pub scale: f64,
    /// The display's origin in the global point coordinate space, which
    /// `CGEventPost` addresses. Non-zero for a secondary display.
    pub origin: (f64, f64),
}

/// Measure a display without starting a stream.
///
/// `Hello` has to carry the screen size before the gateway sends `Attach`, but
/// the stream itself should not start until it does (battery, CPU) — so the
/// geometry is probed separately. This still needs the Screen Recording grant,
/// which makes it the natural place to discover a missing one.
pub fn probe(display: usize) -> anyhow::Result<Geometry> {
    let content = SCShareableContent::get()
        .map_err(|e| anyhow::anyhow!("cannot list shareable content: {e}"))?;
    let displays = content.displays();
    anyhow::ensure!(!displays.is_empty(), "no displays available to capture");
    Ok(geometry(pick(&displays, display)))
}

/// A running capture stream. Dropping this stops the capture.
pub struct Capture {
    stream: SCStream,
    shared: Arc<Shared>,
    /// The geometry the stream was configured for.
    pub geometry: Geometry,
}

/// State shared with the capture callback.
struct Shared {
    sink: Arc<dyn FrameSink>,
    /// Set to make the next frame report the whole surface as dirty. Used for
    /// the first frame after `Attach`, for `Refresh`, and to recover from a
    /// dropped backlog. Shared with the session — see [`Capture::start`].
    full_repaint: Arc<AtomicBool>,
    /// Last surface size seen, so a change is reported once rather than per
    /// frame.
    last_size: Mutex<(u16, u16)>,
}

impl Capture {
    /// Start capturing `display` (an index into the shareable-display list).
    ///
    /// `full_repaint` is shared with the caller: setting it makes the next frame
    /// report the whole surface as dirty. The session sets it on `Refresh`, and
    /// the sink sets it when it has to drop a frame — which is how falling
    /// behind degrades into a later, coarser repaint instead of a backlog.
    ///
    /// It starts set, so the first frame is a complete picture and a freshly
    /// attached browser does not wait for the screen to change.
    pub fn start(
        display: usize,
        sink: Arc<dyn FrameSink>,
        full_repaint: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        // Fails with "user declined TCC" when Screen Recording is not granted;
        // the caller turns that into an AgentMsg::Error for the browser.
        let content = SCShareableContent::get()
            .map_err(|e| anyhow::anyhow!("cannot list shareable content: {e}"))?;
        let displays = content.displays();
        anyhow::ensure!(!displays.is_empty(), "no displays available to capture");
        let display = pick(&displays, display);
        let geometry = geometry(display);
        info!(
            "capture: display {} — {}x{} pixels at {}x, origin ({}, {})",
            display.display_id(),
            geometry.width,
            geometry.height,
            geometry.scale,
            geometry.origin.0,
            geometry.origin.1
        );

        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();

        full_repaint.store(true, Ordering::Relaxed);
        let config = SCStreamConfiguration::new()
            .with_width(u32::from(geometry.width))
            .with_height(u32::from(geometry.height))
            .with_pixel_format(PixelFormat::BGRA)
            // The pointer is sent as a shape instead (see crate::cursor), so it
            // must not be burned into the framebuffer.
            .with_shows_cursor(false)
            // Cap the frame rate: past ~30fps the encoder is the bottleneck and
            // the extra frames only add latency.
            .with_minimum_frame_interval(&CMTime::new(1, 30))
            // Shallow: a deep queue buys buffering we do not want. Falling
            // behind should coalesce into a later, coarser repaint.
            .with_queue_depth(3);

        let shared = Arc::new(Shared {
            sink,
            full_repaint,
            last_size: Mutex::new((geometry.width, geometry.height)),
        });

        // The delegate is how ScreenCaptureKit reports an *unexpected* stop —
        // the display going away, the Screen Recording grant being revoked mid
        // session, the system tearing the stream down. Without it those look
        // identical to "the screen stopped changing", and the session would sit
        // there painting nothing forever.
        let delegate_shared = Arc::clone(&shared);
        let mut stream = SCStream::new_with_delegate(
            &filter,
            &config,
            ErrorHandler::new(move |error| {
                delegate_shared.sink.failed(format!("capture stream stopped: {error}"));
            }),
        );
        stream.add_output_handler(Handler(Arc::clone(&shared)), SCStreamOutputType::Screen);
        stream
            .start_capture()
            .map_err(|e| anyhow::anyhow!("cannot start the capture stream: {e}"))?;

        Ok(Self {
            stream,
            shared,
            geometry,
        })
    }

    /// Make the next frame report the whole surface as dirty.
    pub fn request_full_repaint(&self) {
        self.shared.full_repaint.store(true, Ordering::Relaxed);
    }

    /// Stop the stream. Also happens on drop; this exists so a session can stop
    /// capturing (battery, CPU) while keeping the object around.
    pub fn stop(&mut self) {
        if let Err(e) = self.stream.stop_capture() {
            debug!("capture: stop_capture: {e}");
        }
    }
}

/// Pick a display by index, falling back to the main one rather than failing —
/// a stale `display = 2` in the config should degrade to a working session.
fn pick(displays: &[SCDisplay], index: usize) -> &SCDisplay {
    displays.get(index).unwrap_or_else(|| {
        warn!("capture: display index {index} out of range; using the main display");
        &displays[0]
    })
}

/// Measure a display: `SCDisplay` reports points, and a Retina panel captures at
/// a multiple of that. We ask for the full pixel size so the browser gets the
/// panel's real detail, and carry the scale so input can be converted back.
fn geometry(display: &SCDisplay) -> Geometry {
    let scale = backing_scale(display);
    let frame = display.frame();
    Geometry {
        width: clamp_u16(((display.width() as f64) * scale).round() as u32),
        height: clamp_u16(((display.height() as f64) * scale).round() as u32),
        scale,
        origin: (frame.origin.x, frame.origin.y),
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Captured pixels per display point.
///
/// `SCDisplay` exposes points only, so the scale comes from the CoreGraphics
/// display mode: pixel width over point width. A `1.0` fallback is safe — it
/// just means a non-Retina capture.
fn backing_scale(display: &SCDisplay) -> f64 {
    use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode};

    let Some(mode) = CGDisplayCopyDisplayMode(display.display_id()) else {
        return 1.0;
    };
    let pixel_w = CGDisplayMode::pixel_width(Some(&mode)) as f64;
    let point_w = CGDisplayMode::width(Some(&mode)) as f64;
    if point_w > 0.0 && pixel_w > 0.0 {
        pixel_w / point_w
    } else {
        1.0
    }
}

struct Handler(Arc<Shared>);

impl SCStreamOutputTrait for Handler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _: SCStreamOutputType) {
        // The callback runs on ScreenCaptureKit's dispatch queue. Everything
        // here is bounded work — extract pixels, hand them off — and never
        // blocks on the network or on an encoder.
        if let Err(e) = self.handle(&sample) {
            debug!("capture: dropped a frame: {e}");
        }
    }
}

impl Handler {
    fn handle(&self, sample: &CMSampleBuffer) -> anyhow::Result<()> {
        // `Idle` means "nothing changed and there is no new IOSurface" — the
        // common case on a still screen. Anything but Complete has no pixels
        // worth reading.
        match sample.frame_status() {
            Some(SCFrameStatus::Complete) => {}
            other => {
                anyhow::bail!("frame status {other:?}");
            }
        }

        let buffer = sample
            .image_buffer()
            .ok_or_else(|| anyhow::anyhow!("frame carries no image buffer"))?;
        let guard = buffer
            .lock(CVPixelBufferLockFlags::READ_ONLY)
            .map_err(|e| anyhow::anyhow!("cannot lock the pixel buffer: {e}"))?;

        let stride = guard.bytes_per_row();
        let surface_w = clamp_u16(guard.width() as u32);
        let surface_h = clamp_u16(guard.height() as u32);
        let pixels = guard.as_slice();

        // A mode switch changes the surface under us; tell the session so it can
        // re-announce the size before any tile with new coordinates arrives.
        {
            let mut last = self.0.last_size.lock().unwrap();
            if *last != (surface_w, surface_h) {
                *last = (surface_w, surface_h);
                self.0.full_repaint.store(true, Ordering::Relaxed);
                self.0.sink.resized(surface_w, surface_h);
            }
        }

        // `swap` so a concurrent callback doesn't also do the full frame.
        let full = self.0.full_repaint.swap(false, Ordering::Relaxed);
        let dirty: Vec<Rect> = if full {
            vec![Rect {
                x: 0,
                y: 0,
                w: surface_w,
                h: surface_h,
            }]
        } else {
            let Some(rects) = sample.dirty_rects() else {
                // No attachment at all — not something we can work around, so
                // fall back to a full frame rather than painting nothing.
                debug!("capture: no dirtyRects attachment; repainting in full");
                self.0.full_repaint.store(true, Ordering::Relaxed);
                return Ok(());
            };
            rects
                .iter()
                .filter_map(|r| clamp_rect(r, surface_w, surface_h))
                .collect()
        };
        if dirty.is_empty() {
            return Ok(());
        }

        let mut tiles = Vec::new();
        for rect in dirty {
            for strip in split_strips(rect) {
                tiles.push(RawTile {
                    rect: strip,
                    rgb: extract_rgb(pixels, stride, strip),
                });
            }
        }
        if !tiles.is_empty() {
            self.0.sink.tiles(tiles);
        }
        Ok(())
    }
}

/// Clamp a CoreGraphics dirty rect into the captured surface, dropping it
/// entirely if it falls outside or is degenerate.
///
/// The rects arrive as `f64` in surface pixels. They are trusted to be roughly
/// right but not to be exactly in bounds — rounding at the edges of a scaled
/// capture can put them a pixel over, and a read past the end of the surface is
/// not a rounding error, it is a crash.
fn clamp_rect(rect: &screencapturekit::cg::CGRect, surface_w: u16, surface_h: u16) -> Option<Rect> {
    // Round outward so a partially-covered pixel is repainted rather than left
    // stale: a one-pixel seam of old content is very visible.
    let x0 = rect.origin.x.floor().max(0.0);
    let y0 = rect.origin.y.floor().max(0.0);
    let x1 = (rect.origin.x + rect.size.width).ceil().min(f64::from(surface_w));
    let y1 = (rect.origin.y + rect.size.height).ceil().min(f64::from(surface_h));
    if !(x1 > x0 && y1 > y0) {
        return None;
    }
    Some(Rect {
        x: x0 as u16,
        y: y0 as u16,
        w: (x1 - x0) as u16,
        h: (y1 - y0) as u16,
    })
}

/// Split a rectangle into strips at most [`STRIP_ROWS`] tall.
fn split_strips(rect: Rect) -> impl Iterator<Item = Rect> {
    (0..rect.h)
        .step_by(usize::from(STRIP_ROWS))
        .map(move |dy| Rect {
            x: rect.x,
            y: rect.y + dy,
            w: rect.w,
            h: STRIP_ROWS.min(rect.h - dy),
        })
}

/// Copy `rect` out of a BGRA surface as packed RGB888.
///
/// Reads row by row at `stride`, never `w * 4` — see the module docs. A row that
/// would run past the end of `pixels` is left black rather than panicking: a
/// short buffer is a bug elsewhere, and taking down the agent (which
/// `panic = "abort"` would do) is a worse outcome than one dark strip.
fn extract_rgb(pixels: &[u8], stride: usize, rect: Rect) -> Vec<u8> {
    let (w, h) = (usize::from(rect.w), usize::from(rect.h));
    let mut rgb = vec![0u8; w * h * 3];
    for row in 0..h {
        let src_start = (usize::from(rect.y) + row) * stride + usize::from(rect.x) * 4;
        let Some(src) = pixels.get(src_start..src_start + w * 4) else {
            debug!("capture: row {row} of {rect:?} is outside the surface; leaving it black");
            continue;
        };
        let dst = &mut rgb[row * w * 3..(row + 1) * w * 3];
        for (px, out) in src.chunks_exact(4).zip(dst.chunks_exact_mut(3)) {
            // BGRA -> RGB. The alpha channel is opaque for screen content and
            // the browser's canvas is opaque, so it is dropped.
            out[0] = px[2];
            out[1] = px[1];
            out[2] = px[0];
        }
    }
    rgb
}

fn clamp_u16(v: u32) -> u16 {
    v.min(u32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn strips_tile_a_tall_rect_without_gaps_or_overlap() {
        let strips: Vec<Rect> = split_strips(rect(10, 20, 100, 150)).collect();
        assert_eq!(strips.len(), 3);
        assert_eq!(strips[0], rect(10, 20, 100, 64));
        assert_eq!(strips[1], rect(10, 84, 100, 64));
        // The last strip is short, not padded past the rect.
        assert_eq!(strips[2], rect(10, 148, 100, 22));
        // Contiguous: each strip starts where the previous ended.
        for pair in strips.windows(2) {
            assert_eq!(pair[1].y, pair[0].y + pair[0].h);
        }
        assert_eq!(
            strips.iter().map(|s| u32::from(s.h)).sum::<u32>(),
            150,
            "the strips must cover the rect exactly"
        );
    }

    #[test]
    fn a_short_rect_is_one_strip() {
        let strips: Vec<Rect> = split_strips(rect(0, 0, 8, 1)).collect();
        assert_eq!(strips, vec![rect(0, 0, 8, 1)]);
        // Exactly one strip tall stays one strip.
        let strips: Vec<Rect> = split_strips(rect(0, 0, 8, 64)).collect();
        assert_eq!(strips, vec![rect(0, 0, 8, 64)]);
        // One pixel more is two.
        assert_eq!(split_strips(rect(0, 0, 8, 65)).count(), 2);
    }

    // The stride bug this module exists to avoid: rows are `stride` apart, not
    // `width * 4`. With padding present, assuming width*4 shears the image.
    #[test]
    fn extract_reads_rows_at_the_reported_stride() {
        // 2x2 image with 4 bytes of row padding: stride 12, not 8.
        let stride = 12;
        let mut surface = vec![0u8; stride * 2];
        // Row 0: two pixels, BGRA.
        surface[0..4].copy_from_slice(&[1, 2, 3, 255]); // B=1 G=2 R=3
        surface[4..8].copy_from_slice(&[4, 5, 6, 255]);
        surface[8..12].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // padding
        // Row 1.
        surface[12..16].copy_from_slice(&[7, 8, 9, 255]);
        surface[16..20].copy_from_slice(&[10, 11, 12, 255]);
        surface[20..24].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // padding

        let rgb = extract_rgb(&surface, stride, rect(0, 0, 2, 2));
        // BGRA -> RGB, and no padding byte anywhere in the output.
        assert_eq!(rgb, vec![3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
    }

    #[test]
    fn extract_honours_the_rect_offset() {
        let stride = 16; // 4 pixels wide, no padding
        let mut surface = vec![0u8; stride * 3];
        for y in 0..3usize {
            for x in 0..4usize {
                let i = y * stride + x * 4;
                // Encode the coordinate in the pixel: B=x, G=y, R=0xF0.
                surface[i..i + 4].copy_from_slice(&[x as u8, y as u8, 0xF0, 255]);
            }
        }
        // The bottom-right 2x2.
        let rgb = extract_rgb(&surface, stride, rect(2, 1, 2, 2));
        assert_eq!(
            rgb,
            vec![
                0xF0, 1, 2, 0xF0, 1, 3, // row y=1, x=2..3
                0xF0, 2, 2, 0xF0, 2, 3, // row y=2
            ]
        );
    }

    // A read past the end must degrade to black, not abort the agent —
    // `panic = "abort"` in a dispatch-queue callback kills the process.
    #[test]
    fn extract_leaves_out_of_bounds_rows_black_instead_of_panicking() {
        let stride = 8;
        let surface = vec![0xFFu8; stride]; // one row only
        let rgb = extract_rgb(&surface, stride, rect(0, 0, 2, 3));
        assert_eq!(rgb.len(), 2 * 3 * 3);
        // Row 0 came through; rows 1 and 2 are black.
        assert_eq!(&rgb[..6], &[0xFF; 6]);
        assert_eq!(&rgb[6..], &[0u8; 12]);
    }

    fn cg_rect(x: f64, y: f64, w: f64, h: f64) -> screencapturekit::cg::CGRect {
        screencapturekit::cg::CGRect {
            origin: screencapturekit::cg::CGPoint { x, y },
            size: screencapturekit::cg::CGSize {
                width: w,
                height: h,
            },
        }
    }

    #[test]
    fn dirty_rects_are_clamped_into_the_surface() {
        // Wholly inside: unchanged.
        assert_eq!(
            clamp_rect(&cg_rect(10.0, 20.0, 30.0, 40.0), 100, 100),
            Some(rect(10, 20, 30, 40))
        );
        // Overhanging the right and bottom edges: trimmed, not dropped.
        assert_eq!(
            clamp_rect(&cg_rect(90.0, 90.0, 30.0, 30.0), 100, 100),
            Some(rect(90, 90, 10, 10))
        );
        // Negative origin: trimmed to 0.
        assert_eq!(
            clamp_rect(&cg_rect(-5.0, -5.0, 20.0, 20.0), 100, 100),
            Some(rect(0, 0, 15, 15))
        );
    }

    #[test]
    fn degenerate_and_offscreen_dirty_rects_are_dropped() {
        assert_eq!(clamp_rect(&cg_rect(0.0, 0.0, 0.0, 10.0), 100, 100), None);
        assert_eq!(clamp_rect(&cg_rect(0.0, 0.0, 10.0, 0.0), 100, 100), None);
        // Entirely past the right edge.
        assert_eq!(clamp_rect(&cg_rect(100.0, 0.0, 10.0, 10.0), 100, 100), None);
        assert_eq!(clamp_rect(&cg_rect(-20.0, 0.0, 10.0, 10.0), 100, 100), None);
    }

    // Fractional rects come out of a scaled capture; rounding inward would
    // leave a one-pixel seam of stale content, which is very visible.
    #[test]
    fn fractional_dirty_rects_round_outward() {
        assert_eq!(
            clamp_rect(&cg_rect(10.4, 20.6, 30.2, 40.1), 100, 100),
            Some(rect(10, 20, 31, 41))
        );
    }
}
