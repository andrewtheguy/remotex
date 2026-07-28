//! What the client has already been shown, and what of a damage rectangle is
//! therefore worth sending.
//!
//! # Why this is a shadow copy and not a grid of hashes
//!
//! The plan this change follows called for snapping damage outward onto a fixed
//! [`CELL_W`]×[`CELL_H`] grid and hashing each cell, on the premise that IronRDP
//! reports damage as a coarse bounding box. **That premise is wrong for RDP, and
//! measuring it is the only reason we know.** Over a 240-position mouse sweep
//! across a 1280×800 xrdp desktop, the engine reported 310 damage rectangles with
//! a *median area of 1295 pixels* — and 92% of them were smaller than a single
//! 320×64 cell. Snapping those outward and letting the gate pay for it cost
//! **8.9× more bytes** than sending the rectangles as they came (1,030,637 against
//! 115,751). A grid is the right unit for damage that arrives coarse — which is
//! exactly what the macOS agent's ScreenCaptureKit does, 15 to 21 full-width
//! strips of a 3200-pixel desktop per frame — and the wrong unit here.
//!
//! So the unit stays the damage rectangle, and the question becomes precise
//! instead of approximate: keep a copy of the pixels the client was last sent, and
//! answer *exactly* which part of a rectangle differs from it.
//!
//! That is strictly better than a hash gate, for three reasons and not only the
//! obvious one:
//!
//! - **It trims.** A rectangle that is mostly unchanged is sent as the sub-rectangle
//!   that changed, not as a cell-aligned approximation of it and not whole. A hash
//!   can only ever answer yes or no.
//! - **There is no aliasing hazard.** A hash memo keyed by rectangle is unsafe as
//!   soon as rectangles overlap partially, which is the normal case here: send a
//!   200×100 rect, then a small rect inside it, and the big rect's remembered hash
//!   now describes a region the client no longer has. Suppressing on that hash
//!   leaves stale pixels. A shadow copy cannot drift, because it *is* the record of
//!   every byte sent.
//! - **No collisions.** A 64-bit collision in a hash gate is not a retry, it is a
//!   region left stale until something else happens to repaint it.
//!
//! The cost is one `w * h * 3` buffer — 3 MB at 1280×800, 25 MB at 4K — and a
//! `memcmp` per damage rectangle, which for the median rectangle above is under a
//! microsecond. Only a session with a client attached has one, and there is only
//! ever one session.
//!
//! # Forgetting is a correctness requirement
//!
//! The session layer drops frames while no browser is attached
//! ([`crate::session`]), so a shadow kept across a detach would claim the *next*
//! client has pixels it has never seen, leaving regions permanently blank.
//! `Refresh` is injected on every attach and after a takeover, so clearing there
//! covers detach, reattach and eviction with one rule.
//!
//! Cleared means *black*, which is not a placeholder: a client that has just been
//! told to resize shows black, and one that was not resized keeps pixels the
//! shadow now under-claims. Under-claiming only costs bytes. Over-claiming would
//! cost correctness, and nothing here can do it.

use crate::protocol::CELL_H;

/// A rectangle of the framebuffer, in pixels, with **inclusive** edges.
///
/// Inclusive because that is how IronRDP reports a region, and converting once at
/// the boundary beats converting at every use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl Rect {
    /// A rectangle from a position and a size, as RFB reports one.
    ///
    /// `None` for an empty rectangle: it covers no pixels, and an inclusive `right`
    /// for a zero width cannot be expressed at all.
    pub fn from_size(x: u16, y: u16, w: u16, h: u16) -> Option<Self> {
        if w == 0 || h == 0 {
            return None;
        }
        Some(Self {
            left: x,
            top: y,
            right: x.saturating_add(w - 1),
            bottom: y.saturating_add(h - 1),
        })
    }

    pub fn w(&self) -> u16 {
        self.right - self.left + 1
    }

    pub fn h(&self) -> u16 {
        self.bottom - self.top + 1
    }

    /// This rectangle split into pieces at most [`CELL_H`] rows tall, top down.
    ///
    /// Payloads have to stay bounded — one PNG of a whole 4K desktop is neither a
    /// useful unit of progress nor a comfortable WebSocket frame — and a client
    /// draws the pieces exactly as it draws any other tiles.
    pub fn bands(&self) -> impl Iterator<Item = Rect> + '_ {
        (self.top..=self.bottom)
            .step_by(usize::from(CELL_H))
            .map(move |top| Rect {
                left: self.left,
                top,
                right: self.right,
                bottom: self.bottom.min(top.saturating_add(CELL_H - 1)),
            })
    }
}

/// The pixels the client was last sent, as packed RGB888.
pub struct Shadow {
    /// Which engine this belongs to, for the log line only.
    engine: &'static str,
    w: u16,
    h: u16,
    pixels: Vec<u8>,
    /// Rectangles examined, and those that turned out to hold nothing new.
    examined: u64,
    unchanged: u64,
    /// Pixels the trim kept out of rectangles that did hold something new, so the
    /// saving from *shrinking* an update can be told apart from the saving from
    /// dropping one.
    trimmed: u64,
}

impl Shadow {
    /// A shadow for a `w`×`h` framebuffer, holding black.
    pub fn new(engine: &'static str, w: u16, h: u16) -> Self {
        Self {
            engine,
            w,
            h,
            pixels: vec![0; usize::from(w) * usize::from(h) * 3],
            examined: 0,
            unchanged: 0,
            trimmed: 0,
        }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.w, self.h)
    }

    /// Adopt a new framebuffer size, keeping the tally but nothing else.
    pub fn resize(&mut self, w: u16, h: u16) {
        let counts = (self.examined, self.unchanged, self.trimmed);
        *self = Self::new(self.engine, w, h);
        (self.examined, self.unchanged, self.trimmed) = counts;
    }

    /// Forget everything: the client is about to be repainted from scratch.
    ///
    /// Reports first, because this is one of the two moments the tally stops being
    /// current — the other is the end of the session.
    pub fn forget(&mut self) {
        self.report();
        self.pixels.fill(0);
        self.examined = 0;
        self.unchanged = 0;
        self.trimmed = 0;
    }

    /// The part of `rect` that differs from what the client has, recording `rgb`
    /// as what it will have.
    ///
    /// `rgb` is `rect`'s pixels, packed RGB888 with no padding. Returns `None` when
    /// nothing in `rect` changed — the common case for an over-reported update, and
    /// the whole reason this exists.
    ///
    /// A rectangle the shadow does not cover is returned unchanged and recorded
    /// nowhere: sending a tile that did not need sending wastes bytes, where
    /// suppressing one that was needed leaves the client showing pixels the remote
    /// no longer has.
    pub fn accept(&mut self, rect: Rect, rgb: &[u8]) -> Option<Rect> {
        self.examined += 1;
        let w = usize::from(rect.w());
        let h = usize::from(rect.h());
        if rect.right >= self.w || rect.bottom >= self.h || rgb.len() != w * h * 3 {
            return Some(rect);
        }

        let row_bytes = w * 3;
        // Column bounds are tracked in bytes and converted once, so a row that
        // differs in one channel of one pixel still narrows to that pixel.
        let mut first_row = usize::MAX;
        let mut last_row = 0usize;
        let mut first_byte = usize::MAX;
        let mut last_byte = 0usize;

        for r in 0..h {
            let src = &rgb[r * row_bytes..(r + 1) * row_bytes];
            let dst = self.row(rect.left, rect.top + r as u16, row_bytes);
            // Whole-slice equality first: it is a `memcmp`, and on an update that
            // changed nothing — which is most of them — no row is ever scanned
            // byte by byte.
            if src == dst {
                continue;
            }
            let (lo, hi) = differing_bytes(src, dst);
            first_row = first_row.min(r);
            last_row = r;
            first_byte = first_byte.min(lo);
            last_byte = last_byte.max(hi);
        }

        if first_row == usize::MAX {
            self.unchanged += 1;
            return None;
        }

        // Copy the changed rows only. Rows outside them are identical already, so
        // copying them would be work for no effect.
        for r in first_row..=last_row {
            let src = &rgb[r * row_bytes..(r + 1) * row_bytes];
            let dst = self.row_mut(rect.left, rect.top + r as u16, row_bytes);
            dst.copy_from_slice(src);
        }

        let changed = Rect {
            left: rect.left + (first_byte / 3) as u16,
            top: rect.top + first_row as u16,
            right: rect.left + (last_byte / 3) as u16,
            bottom: rect.top + last_row as u16,
        };
        self.trimmed += (usize::from(rect.w()) * usize::from(rect.h())
            - usize::from(changed.w()) * usize::from(changed.h())) as u64;
        Some(changed)
    }

    /// Log what comparing against the client's own pixels has saved, if anything.
    pub fn report(&self) {
        if self.examined > 0 {
            log::info!(
                "{}: {} of {} update(s) held nothing new; \
                 {} pixel(s) trimmed off the rest",
                self.engine,
                self.unchanged,
                self.examined,
                self.trimmed
            );
        }
    }

    fn offset(&self, x: u16, y: u16) -> usize {
        (usize::from(y) * usize::from(self.w) + usize::from(x)) * 3
    }

    fn row(&self, x: u16, y: u16, len: usize) -> &[u8] {
        let at = self.offset(x, y);
        &self.pixels[at..at + len]
    }

    fn row_mut(&mut self, x: u16, y: u16, len: usize) -> &mut [u8] {
        let at = self.offset(x, y);
        &mut self.pixels[at..at + len]
    }
}

/// The first and last byte index at which two equal-length rows differ.
///
/// Only called for rows already known to differ, so there is always an answer.
fn differing_bytes(a: &[u8], b: &[u8]) -> (usize, usize) {
    let first = a
        .iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .expect("rows differ");
    let back = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .position(|(x, y)| x != y)
        .expect("rows differ");
    (first, a.len() - 1 - back)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Packed RGB for a solid-colour rectangle.
    fn solid(rect: Rect, value: u8) -> Vec<u8> {
        vec![value; usize::from(rect.w()) * usize::from(rect.h()) * 3]
    }

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn a_zero_sized_update_covers_nothing() {
        assert_eq!(Rect::from_size(4, 4, 0, 10), None);
        assert_eq!(Rect::from_size(4, 4, 10, 0), None);
        assert_eq!(Rect::from_size(4, 4, 1, 1), Some(rect(4, 4, 4, 4)));
    }

    #[test]
    fn the_first_send_of_anything_but_black_is_new() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(0, 0, 15, 15);
        assert_eq!(shadow.accept(r, &solid(r, 9)), Some(r));
    }

    /// Black on a fresh shadow is not new, and that is deliberate: a client that
    /// was just told to resize is showing black already.
    #[test]
    fn black_on_a_fresh_shadow_is_not_sent() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(0, 0, 15, 15);
        assert_eq!(shadow.accept(r, &solid(r, 0)), None);
    }

    #[test]
    fn an_unchanged_repeat_is_dropped_whole() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(8, 8, 23, 23);
        assert_eq!(shadow.accept(r, &solid(r, 5)), Some(r));
        assert_eq!(shadow.accept(r, &solid(r, 5)), None);
        assert_eq!(shadow.accept(r, &solid(r, 5)), None);
    }

    /// The point of comparing pixels rather than hashing them: an over-reported
    /// rectangle is sent as the part that actually moved.
    #[test]
    fn an_over_reported_rectangle_is_trimmed_to_what_changed() {
        let mut shadow = Shadow::new("test", 200, 200);
        let whole = rect(0, 0, 199, 199);
        shadow.accept(whole, &solid(whole, 1));

        // Change one pixel at (100, 50) inside a rectangle covering the screen.
        let mut pixels = solid(whole, 1);
        let at = (50 * 200 + 100) * 3;
        pixels[at..at + 3].copy_from_slice(&[9, 9, 9]);

        assert_eq!(shadow.accept(whole, &pixels), Some(rect(100, 50, 100, 50)));
    }

    #[test]
    fn a_trim_keeps_every_changed_pixel_inside_it() {
        let mut shadow = Shadow::new("test", 100, 100);
        let whole = rect(0, 0, 99, 99);
        shadow.accept(whole, &solid(whole, 1));

        let mut pixels = solid(whole, 1);
        for (x, y) in [(10u16, 20u16), (60, 25), (33, 70)] {
            let at = (usize::from(y) * 100 + usize::from(x)) * 3;
            pixels[at..at + 3].copy_from_slice(&[7, 7, 7]);
        }

        let changed = shadow.accept(whole, &pixels).expect("something changed");
        assert_eq!(changed, rect(10, 20, 60, 70));
    }

    /// A trim must record the pixels it *sent*, not the pixels it was given, or the
    /// next comparison lies about what the client has. Both halves are checked here
    /// because the copy skips untouched rows: a shadow that skipped a *changed* row
    /// would go on suppressing it forever.
    #[test]
    fn what_the_trim_sent_is_what_the_shadow_remembers() {
        let mut shadow = Shadow::new("test", 64, 64);
        let whole = rect(0, 0, 63, 63);
        shadow.accept(whole, &solid(whole, 1));

        let mut pixels = solid(whole, 1);
        let at = (32 * 64 + 32) * 3;
        pixels[at..at + 3].copy_from_slice(&[4, 4, 4]);
        assert_eq!(shadow.accept(whole, &pixels), Some(rect(32, 32, 32, 32)));

        // The same pixels again: nothing new, so the changed row was recorded.
        assert_eq!(shadow.accept(whole, &pixels), None);
        // And back to the original: a change again, not a phantom match.
        assert_eq!(
            shadow.accept(whole, &solid(whole, 1)),
            Some(rect(32, 32, 32, 32))
        );
    }

    /// Two rectangles overlapping partially is the case a hash memo keyed by
    /// rectangle gets wrong, and the reason this holds pixels instead.
    #[test]
    fn a_small_send_inside_a_larger_one_does_not_strand_it() {
        let mut shadow = Shadow::new("test", 64, 64);
        let big = rect(0, 0, 31, 31);
        let small = rect(4, 4, 7, 7);
        shadow.accept(big, &solid(big, 1));

        // Paint the small rect a different colour.
        assert_eq!(shadow.accept(small, &solid(small, 2)), Some(small));

        // Now the big rect is reported again with its *original* pixels. A memo
        // keyed by rectangle would still hold the first hash and suppress this,
        // leaving the small patch on screen forever. The changed area is exactly
        // the small patch.
        assert_eq!(shadow.accept(big, &solid(big, 1)), Some(small));
    }

    #[test]
    fn forgetting_sends_everything_again() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(0, 0, 31, 31);
        assert_eq!(shadow.accept(r, &solid(r, 3)), Some(r));
        assert_eq!(shadow.accept(r, &solid(r, 3)), None);

        shadow.forget();

        assert_eq!(
            shadow.accept(r, &solid(r, 3)),
            Some(r),
            "a repaint must not be suppressed"
        );
    }

    #[test]
    fn a_resize_forgets_and_regrids() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(0, 0, 31, 31);
        shadow.accept(r, &solid(r, 3));

        shadow.resize(128, 96);

        assert_eq!(shadow.size(), (128, 96));
        assert_eq!(shadow.accept(r, &solid(r, 3)), Some(r));
        let far = rect(96, 64, 127, 95);
        assert_eq!(shadow.accept(far, &solid(far, 3)), Some(far));
    }

    /// A rectangle the shadow does not cover is sent, not suppressed and not
    /// remembered. Wasting bytes is recoverable; a stale region is not.
    #[test]
    fn a_rectangle_outside_the_shadow_is_always_sent() {
        let mut shadow = Shadow::new("test", 32, 32);
        let outside = rect(16, 16, 47, 47);
        assert_eq!(shadow.accept(outside, &solid(outside, 1)), Some(outside));
        assert_eq!(shadow.accept(outside, &solid(outside, 1)), Some(outside));
    }

    /// A payload that does not match its rectangle is sent rather than trusted
    /// into a comparison that would read the wrong rows.
    #[test]
    fn a_mismatched_payload_length_is_not_compared() {
        let mut shadow = Shadow::new("test", 32, 32);
        let r = rect(0, 0, 15, 15);
        assert_eq!(shadow.accept(r, &[1, 2, 3]), Some(r));
    }

    #[test]
    fn bands_split_tall_rectangles_and_leave_short_ones_alone() {
        let short = rect(10, 10, 20, 20);
        assert_eq!(short.bands().collect::<Vec<_>>(), vec![short]);

        let tall = rect(0, 0, 99, CELL_H * 2);
        let bands: Vec<_> = tall.bands().collect();
        assert_eq!(bands.len(), 3, "{bands:?}");
        assert_eq!(bands[0], rect(0, 0, 99, CELL_H - 1));
        assert_eq!(bands[1], rect(0, CELL_H, 99, CELL_H * 2 - 1));
        assert_eq!(bands[2], rect(0, CELL_H * 2, 99, CELL_H * 2));
        // No gaps, no overlap, and the whole rectangle is covered.
        assert_eq!(bands.iter().map(|b| usize::from(b.h())).sum::<usize>(), usize::from(tall.h()));
    }
}
