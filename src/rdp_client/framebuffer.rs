//! The Rust-owned copy of the remote desktop.

use std::sync::{Mutex, MutexGuard};

use log::warn;

/// A rectangle of the desktop, in pixels from the top left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// One complete frame, in `RGBX32`: four bytes per pixel, red first, the fourth
/// byte unused.
///
/// That byte order is the decoder's own — IronRDP's `RgbA32` image stores R,G,B in
/// memory order — so a paint is a row copy with no swizzle, and a consumer that
/// encodes finds the channels in the order every encoder wants.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row. Equal to `width * 4` — the buffer is packed — but read it
    /// rather than recomputing it, so that a future stride does not silently
    /// corrupt a caller.
    pub stride: usize,
    pub pixels: Vec<u8>,
}

impl Frame {
    /// The rows of one rectangle, top to bottom, each already narrowed to the
    /// rectangle's width.
    ///
    /// The shape a damage-driven encoder wants, and the reason it exists here
    /// rather than in every caller: getting `stride` into the arithmetic by hand is
    /// the classic way to produce a picture that is *nearly* right — sheared by a
    /// few pixels a row — and that is a bug people stare at for an hour.
    ///
    /// Returns nothing for a rectangle that is not wholly inside the frame. A
    /// damage rectangle from the session always is, but a resize the caller has not
    /// processed yet could make one stale, and an empty iterator is a better answer
    /// to that than a panic in an encoder.
    pub fn rows(&self, rect: Rect) -> impl Iterator<Item = &[u8]> {
        let fits = rect.x.saturating_add(rect.width) <= self.width
            && rect.y.saturating_add(rect.height) <= self.height;
        let (start, count) = if fits { (rect.y as usize, rect.height as usize) } else { (0, 0) };
        let left = rect.x as usize * 4;
        let right = left + rect.width as usize * 4;
        (start..start + count).map(move |row| {
            let base = row * self.stride;
            &self.pixels[base + left..base + right]
        })
    }
}

/// The framebuffer the session thread writes and the caller reads.
///
/// Locked for the duration of one damaged-rectangle copy on the writing side, and
/// for the duration of whatever the caller does inside [`Framebuffer::with`] on the
/// reading side. So a caller that spends a long time in there is holding up the
/// session thread's next paint — the right trade for a headless encoder, where the
/// alternative is double-buffering a multi-megabyte image to save a copy nobody was
/// waiting on.
///
/// It is a *copy* of what the decoders hold, not the decoders' own buffer. That
/// costs one memcpy of each damaged region — negligible beside encoding it — and
/// buys the property that matters: a reader holding [`Frame`] cannot be looking at
/// memory a resize is reallocating underneath it, and the decoders never wait on
/// the lock for longer than that copy.
pub struct Framebuffer {
    frame: Mutex<Frame>,
}

impl Framebuffer {
    pub(super) fn new() -> Self {
        Self { frame: Mutex::new(Frame { width: 0, height: 0, stride: 0, pixels: Vec::new() }) }
    }

    /// Read the frame. The lock is held for the duration of `f`.
    pub fn with<R>(&self, f: impl FnOnce(&Frame) -> R) -> R {
        f(&self.lock())
    }

    /// A poisoned lock is not a reason to fail here.
    ///
    /// Poisoning means a panic happened while some *other* thread held this lock,
    /// and the only thing under it is a pixel buffer: there are no invariants across
    /// fields to have been left half-updated, so the worst case is a frame with a
    /// stale rectangle in it. Propagating the poison instead would turn one panic in
    /// a caller's `with` closure into a session that can never paint again.
    fn lock(&self) -> MutexGuard<'_, Frame> {
        self.frame.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Resize and clear. Called on connect and on every desktop resize.
    pub(super) fn resize(&self, width: u32, height: u32) {
        let mut frame = self.lock();
        frame.width = width;
        frame.height = height;
        frame.stride = width as usize * 4;
        frame.pixels.clear();
        let bytes = frame.stride * height as usize;
        frame.pixels.resize(bytes, 0);
    }

    /// Copy `rect` out of a buffer laid out like the desktop itself — the decoded
    /// image, whose pixel (x, y) sits at `y * stride + x * 4`.
    ///
    /// `false`, with the reason logged, for a rectangle that does not fit either
    /// side: that can only mean the two disagree about the desktop's size, which is
    /// a missed resize, and clamping would paint a sheared image and hide it.
    pub(super) fn blit(&self, src: &[u8], src_stride: usize, rect: Rect) -> bool {
        let mut frame = self.lock();
        if rect.x.saturating_add(rect.width) > frame.width
            || rect.y.saturating_add(rect.height) > frame.height
        {
            warn!(
                "rdp: dropping a {}x{}+{}+{} paint that does not fit a {}x{} framebuffer — a \
                 resize was missed",
                rect.width, rect.height, rect.x, rect.y, frame.width, frame.height
            );
            return false;
        }
        let bytes = rect.width as usize * 4;
        let left = rect.x as usize * 4;
        let stride = frame.stride;
        for row in 0..rect.height as usize {
            let from = (rect.y as usize + row) * src_stride + left;
            let Some(src) = src.get(from..from + bytes) else {
                warn!(
                    "rdp: dropping the rest of a {}x{}+{}+{} paint whose pixels ran out at row \
                     {row}",
                    rect.width, rect.height, rect.x, rect.y
                );
                return false;
            };
            let to = (rect.y as usize + row) * stride + left;
            frame.pixels[to..to + bytes].copy_from_slice(src);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blit_lands_where_the_rectangle_says() {
        let fb = Framebuffer::new();
        fb.resize(4, 4);
        let src = vec![0xAB; 4 * 4 * 4];
        assert!(fb.blit(&src, 16, Rect { x: 1, y: 2, width: 2, height: 1 }));

        fb.with(|frame| {
            // Row 2, columns 1 and 2 — and nothing else.
            for (index, byte) in frame.pixels.iter().enumerate() {
                let expected = if (36..44).contains(&index) { 0xAB } else { 0 };
                assert_eq!(*byte, expected, "byte {index}");
            }
        });
    }

    /// The failure this guards is a *silent* one: a stale rectangle against a
    /// resized frame would otherwise read past the end of a row and shear the
    /// picture.
    #[test]
    fn a_rectangle_that_does_not_fit_is_dropped_rather_than_clamped() {
        let fb = Framebuffer::new();
        fb.resize(2, 2);
        let src = vec![0xFF; 4 * 4 * 4];
        assert!(!fb.blit(&src, 16, Rect { x: 0, y: 0, width: 4, height: 4 }));
        fb.with(|frame| assert!(frame.pixels.iter().all(|b| *b == 0)));
    }

    /// A source shorter than the rectangle it claims to cover stops the copy rather
    /// than panicking on the slice.
    #[test]
    fn a_source_too_short_for_its_rectangle_is_not_a_panic() {
        let fb = Framebuffer::new();
        fb.resize(4, 4);
        // Two rows of a 16-byte stride, from a buffer that holds one.
        assert!(!fb.blit(&[0; 16], 16, Rect { x: 0, y: 0, width: 4, height: 2 }));
    }

    #[test]
    fn rows_narrows_to_the_rectangle() {
        let fb = Framebuffer::new();
        fb.resize(3, 2);
        fb.with(|frame| {
            let rows: Vec<_> = frame.rows(Rect { x: 1, y: 0, width: 2, height: 2 }).collect();
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|row| row.len() == 8));
            // And a rectangle hanging off the edge yields nothing rather than panicking.
            assert_eq!(frame.rows(Rect { x: 2, y: 0, width: 2, height: 1 }).count(), 0);
        });
    }
}
