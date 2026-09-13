//! The Rust-owned copy of the remote desktop.

use std::sync::{Mutex, MutexGuard};

use log::warn;

use super::proto::bitmap::MAX_DESKTOP_BYTES;

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

    /// Whether the two share any pixel.
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }

    /// The pixels this rectangle holds, wide enough that a desktop-sized union
    /// cannot overflow the arithmetic [`stage`] does with it.
    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// The smallest rectangle holding both.
    pub fn union(self, other: Rect) -> Rect {
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Rect { x, y, width: right - x, height: bottom - y }
    }

    /// The part of this rectangle inside `bounds`, or `None` when nothing is.
    pub fn clipped(self, bounds: Rect) -> Option<Rect> {
        let x = self.x.max(bounds.x);
        let y = self.y.max(bounds.y);
        let right = (self.x.saturating_add(self.width)).min(bounds.x.saturating_add(bounds.width));
        let bottom = (self.y.saturating_add(self.height)).min(bounds.y.saturating_add(bounds.height));
        (right > x && bottom > y).then(|| Rect { x, y, width: right - x, height: bottom - y })
    }
}

/// A desktop size the server named, refused before anything is allocated for it —
/// see [`MAX_DESKTOP_BYTES`]. Both a real desktop's size and an absurd one are legal
/// on the wire, so the difference is made here.
pub(super) fn affordable(width: u32, height: u32) -> anyhow::Result<()> {
    let bytes = usize::try_from(width)
        .ok()
        .zip(usize::try_from(height).ok())
        .and_then(|(width, height)| width.checked_mul(height))
        .and_then(|pixels| pixels.checked_mul(4));
    match bytes {
        Some(bytes) if bytes <= MAX_DESKTOP_BYTES => Ok(()),
        _ => Err(anyhow::anyhow!(
            "the server asked for a {width}x{height} desktop, which is more than the {} MiB \
             this client will hold",
            MAX_DESKTOP_BYTES >> 20
        )),
    }
}

/// One complete frame, in `RGBX32`: four bytes per pixel, red first, the fourth
/// byte unused.
///
/// That byte order is the decoder's own — [`super::proto::bitmap`] writes R, G, B in
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

/// Fold `rect` into the damage already staged, keeping the list to `cap`
/// rectangles.
///
/// A rectangle overlapping one already there is unioned into it. Past the cap the
/// list is kept bounded by merging `rect` into whichever rectangle wastes the
/// fewest pixels — the neighbouring tile of the same damaged block, almost always,
/// which unions with no waste at all.
///
/// **Not a bounding box over everything**, which is what this used to do and what
/// the graphics pipeline cannot afford. EGFX reports a frame as disjoint 64x64
/// tiles, so a box round a video in one corner and a clock in the other is most of
/// the desktop; the gateway then cuts that box into full-width bands, and
/// `render_subtype = "classify"` judges each band as one tile. Text swept in beside
/// a moving picture reads as photographic and goes out lossy — and, being
/// unchanged from then on, is never sent again. Merging the cheapest pair keeps the
/// list bounded *and* the damage the shape the host drew it.
pub(super) fn stage(pending: &mut Vec<Rect>, rect: Rect, cap: usize) {
    debug_assert!(cap > 0, "a cap of zero has nowhere to put a rectangle");
    if let Some(waiting) = pending.iter_mut().find(|waiting| waiting.overlaps(&rect)) {
        *waiting = waiting.union(rect);
        return;
    }
    if pending.len() < cap {
        pending.push(rect);
        return;
    }
    // Disjoint from every one of them — the overlap case returned above — so the
    // waste of a merge is what the union adds beyond the two rectangles themselves,
    // and a union of two disjoint rectangles can never be smaller than their sum.
    let Some(pick) = pending
        .iter()
        .enumerate()
        .min_by_key(|(_, waiting)| waiting.union(rect).area() - waiting.area() - rect.area())
        .map(|(at, _)| at)
    else {
        pending.push(rect);
        return;
    };
    pending[pick] = pending[pick].union(rect);
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

    /// Copy one decoded rectangle in, from a buffer holding nothing but that
    /// rectangle: `rect.width * rect.height` pixels, top row first, packed.
    ///
    /// That is the shape a bitmap update decodes to, one rectangle at a time, so
    /// nothing between the decoder and here has to know the desktop's own stride.
    ///
    /// `false`, with the reason logged, for a rectangle that does not fit the frame
    /// or a source that is not the size it claims: the first can only mean the two
    /// disagree about the desktop's size, which is a missed resize, and clamping
    /// would paint a sheared image and hide it.
    pub(super) fn blit(&self, src: &[u8], rect: Rect) -> bool {
        let bytes = rect.width as usize * 4;
        if src.len() != bytes * rect.height as usize {
            warn!(
                "rdp: dropping a {}x{}+{}+{} paint carrying {} bytes",
                rect.width,
                rect.height,
                rect.x,
                rect.y,
                src.len()
            );
            return false;
        }
        self.blit_from(src, bytes, (0, 0), rect)
    }

    /// Copy `rect` in out of a larger picture — a graphics pipeline surface — whose
    /// rows are `src_stride` bytes apart, taking the pixels from `origin` in it:
    /// the framebuffer's `(rect.x, rect.y)` is the source's `(origin.0, origin.1)`.
    ///
    /// `false`, with the reason logged, for a rectangle that does not fit either
    /// side, for the reason [`Framebuffer::blit`] gives.
    pub(super) fn blit_from(
        &self,
        src: &[u8],
        src_stride: usize,
        origin: (usize, usize),
        rect: Rect,
    ) -> bool {
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
            let from = (origin.1 + row) * src_stride + origin.0 * 4;
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
        let src = vec![0xAB; 8];
        assert!(fb.blit(&src, Rect { x: 1, y: 2, width: 2, height: 1 }));

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
        assert!(!fb.blit(&src, Rect { x: 0, y: 0, width: 4, height: 4 }));
        fb.with(|frame| assert!(frame.pixels.iter().all(|b| *b == 0)));
    }

    /// A source that is not the size its own rectangle asks for stops the copy
    /// rather than panicking on the slice — or, worse, shearing the picture.
    #[test]
    fn a_source_that_is_not_its_rectangles_size_is_not_a_panic() {
        let fb = Framebuffer::new();
        fb.resize(4, 4);
        // Two rows of four pixels, from a buffer that holds one row.
        assert!(!fb.blit(&[0; 16], Rect { x: 0, y: 0, width: 4, height: 2 }));
        assert!(!fb.blit(&[0; 48], Rect { x: 0, y: 0, width: 4, height: 2 }));
    }

    /// A surface is wider than the rectangle taken out of it, so row `n` of the
    /// source is `n * stride` in — not `n * width * 4` — and starts at the origin.
    #[test]
    fn a_blit_from_a_larger_picture_reads_the_source_at_its_own_stride() {
        let fb = Framebuffer::new();
        fb.resize(3, 2);
        // A 4x3 surface whose pixel (x, y) is the byte pattern [x, y, 0, 0].
        let mut surface = Vec::new();
        for y in 0..3_u8 {
            for x in 0..4_u8 {
                surface.extend_from_slice(&[x, y, 0, 0]);
            }
        }
        // Its (1, 1)..(3, 3) lands at the framebuffer's (1, 0).
        assert!(fb.blit_from(&surface, 16, (1, 1), Rect { x: 1, y: 0, width: 2, height: 2 }));
        fb.with(|frame| {
            let rows: Vec<_> = frame.rows(Rect { x: 1, y: 0, width: 2, height: 2 }).collect();
            assert_eq!(rows[0], &[1, 1, 0, 0, 2, 1, 0, 0]);
            assert_eq!(rows[1], &[1, 2, 0, 0, 2, 2, 0, 0]);
        });
        // And a source that runs out is not a panic.
        assert!(!fb.blit_from(&surface[..40], 16, (1, 1), Rect { x: 1, y: 0, width: 2, height: 2 }));
    }

    #[test]
    fn rectangles_overlap_union_and_clip_by_their_edges() {
        let a = Rect { x: 0, y: 0, width: 10, height: 10 };
        let b = Rect { x: 5, y: 5, width: 10, height: 10 };
        let c = Rect { x: 10, y: 0, width: 1, height: 1 };
        assert!(a.overlaps(&b));
        assert!(!a.overlaps(&c), "an edge in common is not an overlap");
        assert_eq!(a.union(b), Rect { x: 0, y: 0, width: 15, height: 15 });
        assert_eq!(b.clipped(a), Some(Rect { x: 5, y: 5, width: 5, height: 5 }));
        assert_eq!(c.clipped(a), None);
        let huge = Rect { x: u32::MAX - 1, y: 0, width: 4, height: 1 };
        assert_eq!(huge.clipped(a), None, "the arithmetic does not wrap");
    }

    /// A server names the desktop, and this client allocates a framebuffer from what
    /// it says. Both numbers are legal on the wire well past any real screen.
    #[test]
    fn a_desktop_too_large_to_hold_is_refused_rather_than_allocated() {
        affordable(1920, 1080).expect("an ordinary desktop");
        affordable(15360, 4320).expect("two 8K monitors side by side is still real");
        affordable(0, 0).expect("a desktop with no pixels costs nothing");

        // The largest desktop a negotiation can name, and past what this client
        // will hold.
        let err = affordable(32766, 32766).expect_err("4 GiB");
        assert!(format!("{err}").contains("32766x32766"), "{err}");
        affordable(65535, 65535).expect_err("17 GB");
        affordable(u32::MAX, u32::MAX).expect_err("the arithmetic itself must not wrap");
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
