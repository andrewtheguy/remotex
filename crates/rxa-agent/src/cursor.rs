//! The pointer shape, read from `NSCursor` and sent as an RGBA PNG.
//!
//! The capture stream runs with `showsCursor = false` (see [`crate::capture`]),
//! so the pointer is never composited into the framebuffer. It travels as a
//! shape instead, straight into the gateway's existing `ServerMsg::Cursor` path
//! and from there to the frontend's `paintCursor` — which was built for VNC
//! against macOS Screen Sharing and needs no changes at all.
//!
//! `NSBitmapImageRep` will hand us PNG bytes directly, which is exactly the
//! protocol's format, so there is no pixel wrangling and no second encoder here.
//!
//! ## Hotspot scaling
//!
//! `NSCursor.hotSpot` is in **points**; the exported PNG is in **pixels**. On a
//! Retina display a 16pt cursor exports as 32px and its hotspot has to be
//! doubled to match, or the pointer is drawn offset by half its own size. The
//! scale is derived from the image itself (pixel width over point width) rather
//! than assumed, because a cursor's representations do not always match the
//! display's scale.

use log::debug;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSCursor};
use objc2_foundation::NSDictionary;
use rxa_proto::msg::CursorImage;

/// Read the current system cursor as a PNG plus its hotspot in image pixels.
///
/// `scale` is the *capture's* pixels-per-point, not the main display's: the
/// browser draws this image onto a canvas measured in captured pixels, so a
/// cursor sized for the wrong display comes out too big or too small.
///
/// Returns `None` when the shape cannot be read — a missing shape is reported to
/// the browser as "no cursor" rather than failing the session, since a session
/// without a pointer is degraded but perfectly usable.
pub fn current(scale: f64) -> Option<CursorImage> {
    // Every AppKit call here returns autoreleased objects. Without a pool this
    // runs on a timer for the life of the agent and leaks steadily.
    // `currentSystemCursor` is deprecated in favour of either capturing the
    // cursor into the framebuffer via `showsCursor`, or `currentCursor`. Neither
    // applies here: compositing the pointer is exactly what this design avoids
    // (the browser draws it, so it tracks the mouse at local latency), and
    // `currentCursor` returns *this process's* cursor, which for a background
    // agent with no windows is meaningless. There is no replacement for "what
    // shape is the system pointer right now", so the deprecated call stands.
    #[allow(deprecated)]
    autoreleasepool(|_| unsafe {
        let cursor = NSCursor::currentSystemCursor()?;
        let image = cursor.image();

        // Points, as AppKit reports them.
        let point_size = image.size();
        let hotspot = cursor.hotSpot();

        // Pick the representation closest to the size the browser will draw at:
        // the cursor's point size times the display's backing scale, because the
        // canvas it lands on is in captured *pixels*.
        //
        // Not simply "the largest representation" — a system cursor can carry a
        // vector-backed rep at an arbitrary resolution. On this Mac the I-beam
        // reports a 14x20 point size with a 280x400 rep available, and taking
        // the largest produced a cursor 20x too big whose hotspot was scaled by
        // 20 to match, putting the pointer's anchor far off the image.
        let target_w = point_size.width * scale;
        let reps = image.representations();
        let mut best: Option<objc2::rc::Retained<NSBitmapImageRep>> = None;
        for i in 0..reps.count() {
            let rep = reps.objectAtIndex(i);
            if let Ok(bitmap) = rep.downcast::<NSBitmapImageRep>() {
                if bitmap.pixelsWide() <= 0 || bitmap.pixelsHigh() <= 0 {
                    continue;
                }
                let closer = best.as_ref().is_none_or(|b| {
                    (bitmap.pixelsWide() as f64 - target_w).abs()
                        < (b.pixelsWide() as f64 - target_w).abs()
                });
                if closer {
                    best = Some(bitmap);
                }
            }
        }
        let bitmap = best?;

        let pixels_wide = bitmap.pixelsWide();
        let pixels_high = bitmap.pixelsHigh();
        if pixels_wide <= 0 || pixels_high <= 0 {
            debug!("cursor: representation has no pixels");
            return None;
        }

        let png = bitmap.representationUsingType_properties(
            NSBitmapImageFileType::PNG,
            &NSDictionary::new(),
        )?;
        let png = png.to_vec();
        if png.is_empty() {
            return None;
        }

        // Points -> pixels for the hotspot. Guard the division: a zero point
        // size would otherwise produce NaN and a nonsense hotspot.
        let scale_x = pixels_per_point(pixels_wide as f64, point_size.width);
        let scale_y = pixels_per_point(pixels_high as f64, point_size.height);

        Some(CursorImage {
            w: clamp_u16(pixels_wide as f64),
            h: clamp_u16(pixels_high as f64),
            hx: clamp_u16(hotspot.x * scale_x),
            hy: clamp_u16(hotspot.y * scale_y),
            png,
        })
    })
}

/// The latest pointer shape, polled on the main thread and read by sessions.
///
/// AppKit is main-thread-only, so `NSCursor` cannot be touched from a tokio
/// worker. The main thread polls into this cache instead (see `main.rs`) and
/// sessions only ever read it — which also gives the caching the plan asks for:
/// a browser attaching later gets the current pointer immediately, rather than
/// no pointer until the shape happens to change.
#[derive(Default)]
pub struct Tracker {
    inner: std::sync::Mutex<Cached>,
    /// Pixels per point of the running capture, as f64 bits. Written by the
    /// session when a stream starts, read by the main thread's poll. Defaults
    /// to 1.0 so a poll before any session still produces a sane shape.
    scale_bits: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct Cached {
    shape: Option<CursorImage>,
    /// Bumped on every change, so a session can tell "unchanged" from "changed
    /// to something that happens to look similar" with one integer compare
    /// instead of a PNG comparison per tick.
    generation: u64,
}

/// A generation no session has seen, so the first check always sends.
pub const UNSEEN: u64 = 0;

impl Tracker {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::default(),
            scale_bits: std::sync::atomic::AtomicU64::new(1.0f64.to_bits()),
        }
    }

    /// Tell the tracker what the running capture's scale is.
    ///
    /// Called when a stream starts. Without it the pointer would be sized for
    /// whatever the *main* display happens to be, which is wrong whenever the
    /// shared display is not the main one or has a different backing scale.
    pub fn set_scale(&self, scale: f64) {
        if scale > 0.0 {
            self.scale_bits
                .store(scale.to_bits(), std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Re-read the system cursor and record it if it changed.
    ///
    /// **Main thread only.**
    pub fn poll(&self) {
        let scale = f64::from_bits(self.scale_bits.load(std::sync::atomic::Ordering::Relaxed));
        let shape = current(scale);
        let mut cached = self.inner.lock().unwrap();
        if cached.shape != shape {
            cached.shape = shape;
            cached.generation += 1;
        }
    }

    /// The current shape, if it is newer than `seen`. Returns the generation to
    /// remember alongside it.
    pub fn changed_since(&self, seen: u64) -> Option<(u64, Option<CursorImage>)> {
        let cached = self.inner.lock().unwrap();
        (cached.generation != seen).then(|| (cached.generation, cached.shape.clone()))
    }
}

/// Pixels per point for one axis, falling back to 1.0 for a degenerate size.
fn pixels_per_point(pixels: f64, points: f64) -> f64 {
    if points > 0.0 && pixels > 0.0 {
        pixels / points
    } else {
        1.0
    }
}

/// Round to the nearest pixel and clamp into the protocol's `u16`.
fn clamp_u16(v: f64) -> u16 {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    v.round().min(f64::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_is_pixels_per_point() {
        assert_eq!(pixels_per_point(32.0, 16.0), 2.0);
        assert_eq!(pixels_per_point(16.0, 16.0), 1.0);
    }

    // A degenerate image size must not turn the hotspot into NaN.
    #[test]
    fn a_degenerate_size_falls_back_to_one_to_one() {
        assert_eq!(pixels_per_point(32.0, 0.0), 1.0);
        assert_eq!(pixels_per_point(0.0, 16.0), 1.0);
        assert_eq!(pixels_per_point(32.0, -1.0), 1.0);
    }

    #[test]
    fn hotspot_scaling_doubles_on_retina() {
        // A 16pt cursor exported at 32px: a hotspot at (4, 3) points is at
        // (8, 6) pixels. Getting this wrong offsets the pointer by half its own
        // size, which is very visible.
        let s = pixels_per_point(32.0, 16.0);
        assert_eq!(clamp_u16(4.0 * s), 8);
        assert_eq!(clamp_u16(3.0 * s), 6);
    }

    #[test]
    fn clamping_rejects_nonsense_and_rounds() {
        assert_eq!(clamp_u16(0.0), 0);
        assert_eq!(clamp_u16(-5.0), 0);
        // Non-finite values are nonsense rather than "very large", so they
        // collapse to 0 — a hotspot of 0,0 is wrong but harmless, whereas
        // u16::MAX would put the pointer's anchor off the far edge of itself.
        assert_eq!(clamp_u16(f64::NAN), 0);
        assert_eq!(clamp_u16(f64::INFINITY), 0);
        assert_eq!(clamp_u16(f64::NEG_INFINITY), 0);
        assert_eq!(clamp_u16(3.4), 3);
        assert_eq!(clamp_u16(3.6), 4);
        assert_eq!(clamp_u16(70_000.0), u16::MAX);
    }

    #[test]
    fn a_fresh_tracker_reports_a_change_so_the_first_check_always_sends() {
        let tracker = Tracker::new();
        // Nothing polled yet: generation 0 == UNSEEN, so no spurious send.
        assert!(tracker.changed_since(UNSEEN).is_none());

        // After a poll the generation moves only if the shape differs from the
        // default (None) — in a headless session it may legitimately stay None.
        tracker.poll();
        let after = tracker.changed_since(UNSEEN);
        let generation = match after {
            Some((generation, _)) => generation,
            None => {
                eprintln!("no system cursor available (headless session); skipping");
                return;
            }
        };
        assert_eq!(generation, 1);
        // A session that has seen this generation is told nothing further.
        assert!(tracker.changed_since(generation).is_none());
        // An unchanged cursor does not bump the generation again.
        tracker.poll();
        assert!(tracker.changed_since(generation).is_none());
    }

    // Requires a GUI session, so it only asserts self-consistency rather than a
    // particular shape: whatever comes back must be a real PNG whose hotspot is
    // inside its own bounds. A pointer drawn from an out-of-bounds hotspot lands
    // nowhere near the cursor.
    #[test]
    fn the_live_system_cursor_is_a_coherent_png() {
        let Some(shape) = current(1.0) else {
            eprintln!("no system cursor available (headless session); skipping");
            return;
        };
        assert_eq!(&shape.png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(shape.w > 0 && shape.h > 0);
        assert!(
            shape.hx <= shape.w && shape.hy <= shape.h,
            "hotspot ({}, {}) outside a {}x{} cursor",
            shape.hx,
            shape.hy,
            shape.w,
            shape.h
        );
    }
}
