//! Exact shadow of pixels sent to the client. Damage is trimmed to the changed
//! sub-rectangle without grid expansion, hash collisions, or overlap aliasing.
//! The shadow is cleared before a repaint for a new attachment so it never
//! claims that client has pixels it did not receive.

use crate::protocol::{CELL_H, CELL_W};

/// A rectangle of the framebuffer, in pixels, with **inclusive** edges.
///
/// Inclusive because that is how RFB reports a rectangle, and converting once at
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

    pub fn contains(&self, other: &Self) -> bool {
        self.left <= other.left
            && self.top <= other.top
            && self.right >= other.right
            && self.bottom >= other.bottom
    }

    /// The pixels these two rectangles have in common, or `None` where they have
    /// none. Touching edges do count: `right`/`bottom` are inclusive.
    pub fn intersect(&self, other: &Self) -> Option<Self> {
        let (left, top) = (self.left.max(other.left), self.top.max(other.top));
        let (right, bottom) = (self.right.min(other.right), self.bottom.min(other.bottom));
        (left <= right && top <= bottom).then_some(Self { left, top, right, bottom })
    }

    /// This rectangle split into pieces at most [`CELL_H`] rows tall, top down.
    ///
    /// Payloads have to stay bounded — one payload for a whole 4K desktop is neither a
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

    /// This rectangle cut at the [`CELL_W`]×[`CELL_H`] grid lines, left to right
    /// within each row of cells, top down. Clipped to itself, never snapped
    /// outward — a piece covers only pixels this rectangle already covered.
    ///
    /// Both axes are cut, and the vertical one is not redundant with
    /// [`Self::bands`]: bands are anchored to the rectangle's own `top`, so a
    /// band starting at y=37 straddles the grid line at y=64 and has to be cut in
    /// two here. The point of the cut is that no piece straddles a line, which is
    /// what makes [`Self::cell_key`] answerable for every piece.
    pub fn cells(&self) -> impl Iterator<Item = Rect> + '_ {
        let start = |v: u16, step: u16| v - v % step;
        (start(self.top, CELL_H)..=self.bottom)
            .step_by(usize::from(CELL_H))
            .flat_map(move |row| {
                let top = self.top.max(row);
                let bottom = self.bottom.min(row.saturating_add(CELL_H - 1));
                (start(self.left, CELL_W)..=self.right)
                    .step_by(usize::from(CELL_W))
                    .map(move |col| Rect {
                        left: self.left.max(col),
                        top,
                        right: self.right.min(col.saturating_add(CELL_W - 1)),
                        bottom,
                    })
            })
    }

    /// The grid cell this rectangle lies in, as `(column, row)`.
    ///
    /// Meaningful for a piece [`Self::cells`] produced, which by construction lies
    /// wholly inside one cell however far from its corner it starts. A rectangle
    /// that straddles a grid line answers for the cell its top-left is in, which
    /// is not wrong so much as not a question worth asking.
    pub fn cell_key(&self) -> (u16, u16) {
        (self.left / CELL_W, self.top / CELL_H)
    }
}

/// What an accepted rectangle owes the client: one box round everything that
/// differs, and which grid cells inside it actually differ.
///
/// The two are not the same thing, and the difference is the whole reason this is
/// a struct. `rect` is a *bounding box*, so a change in two corners sweeps up
/// everything between them. Sending those extra pixels is harmless — they are the
/// right pixels, only redundant — but *counting* them as change is not, because
/// that is what puts a still sidebar in motion because a video is playing beside
/// it. Anything deciding how hard a region is working reads `cells`; anything
/// deciding what to put on the wire reads `rect`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    /// Bounding box of every differing pixel.
    pub rect: Rect,
    /// Grid cells ([`Rect::cell_key`]) holding at least one differing pixel,
    /// sorted and deduplicated. Always a subset of the cells `rect` covers, and
    /// never empty when `rect` is present — unless the shadow was told nothing
    /// reads them ([`Shadow::classify_cells`]), in which case it is always empty.
    pub cells: Vec<(u16, u16)>,
}

impl Changed {
    /// Whether `key` is one of the cells that actually changed.
    pub fn has(&self, key: (u16, u16)) -> bool {
        self.cells.binary_search(&key).is_ok()
    }
}

#[cfg(test)]
impl Shadow {
    /// [`Self::accept`]'s bounding box alone, for the tests that are about the box.
    fn accept_rect(&mut self, rect: Rect, rgb: &[u8]) -> Option<Rect> {
        self.accept(rect, rgb).map(|c| c.rect)
    }
}

/// The pixels the client was last sent, as packed RGB888, and which of them are
/// actually known.
///
/// The `known` half is not bookkeeping, it is the difference between right and
/// wrong. A cleared shadow cannot be treated as *black*, however tempting: both
/// clients deliberately keep their pixels when a `resize` repeats the size they
/// already have (`FramebufferRenderer.resize`, `handleResize`), so a browser
/// reattaching to an unchanged desktop still shows the previous session's picture.
/// A shadow claiming black would then withhold every region that is *now* black —
/// a window that closed while nobody was attached would stay on screen forever.
///
/// So a cleared pixel is not black, it is unknown, and unknown differs from
/// everything.
pub struct Shadow {
    /// Which engine this belongs to, for the log line only.
    engine: &'static str,
    w: u16,
    h: u16,
    pixels: Vec<u8>,
    /// One flag per pixel: whether `pixels` says anything about it. `[bool]`
    /// rather than a bitset because scanning it for a `false` is a `memchr`.
    known: Vec<bool>,
    /// How many of `known` are false. The steady state is zero — everything on
    /// screen has been seen — and zero is what lets [`Self::accept`] skip the
    /// per-row scan of the flags entirely.
    unknown: usize,
    /// Whether [`Self::accept`] classifies which grid cells differ. Only a motion
    /// strategy reads [`Changed::cells`]; on every other plan the classification
    /// was per-cell `memcmp` work computed to be discarded.
    classify: bool,
    /// Rectangles examined, and those that turned out to hold nothing new.
    examined: u64,
    unchanged: u64,
    /// Pixels the trim kept out of rectangles that did hold something new, so the
    /// saving from *shrinking* an update can be told apart from the saving from
    /// dropping one.
    trimmed: u64,
}

impl Shadow {
    /// A shadow for a `w`×`h` framebuffer, knowing nothing.
    pub fn new(engine: &'static str, w: u16, h: u16) -> Self {
        Self {
            engine,
            w,
            h,
            pixels: vec![0; usize::from(w) * usize::from(h) * 3],
            known: vec![false; usize::from(w) * usize::from(h)],
            unknown: usize::from(w) * usize::from(h),
            classify: true,
            examined: 0,
            unchanged: 0,
            trimmed: 0,
        }
    }

    /// Say whether anything will read [`Changed::cells`]. Only a motion strategy
    /// does (`TileSink::wants_cells`); with this off, `accept` skips the per-cell
    /// classification and returns `cells` empty.
    pub fn classify_cells(&mut self, on: bool) {
        self.classify = on;
    }

    pub fn size(&self) -> (u16, u16) {
        (self.w, self.h)
    }

    /// Adopt a new framebuffer size, keeping the tally but nothing else.
    pub fn resize(&mut self, w: u16, h: u16) {
        let counts = (self.examined, self.unchanged, self.trimmed);
        let classify = self.classify;
        *self = Self::new(self.engine, w, h);
        (self.examined, self.unchanged, self.trimmed) = counts;
        self.classify = classify;
    }

    /// Forget everything: the client is about to be repainted from scratch.
    ///
    /// Reports first, because this is one of the two moments the tally stops being
    /// current — the other is the end of the session.
    pub fn forget(&mut self) {
        self.report();
        // The pixels are left alone; only the claim to know them is dropped. That
        // is all the difference between them, and it saves touching 25 MB.
        self.known.fill(false);
        self.unknown = self.known.len();
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
    pub fn accept(&mut self, rect: Rect, rgb: &[u8]) -> Option<Changed> {
        self.examined += 1;
        let w = usize::from(rect.w());
        let h = usize::from(rect.h());
        if rect.right >= self.w || rect.bottom >= self.h || rgb.len() != w * h * 3 {
            // Nothing here can be compared, so nothing can be ruled out: every cell
            // it touches counts as changed.
            let mut cells: Vec<(u16, u16)> = if self.classify {
                rect.cells().map(|c| c.cell_key()).collect()
            } else {
                Vec::new()
            };
            cells.sort_unstable();
            cells.dedup();
            return Some(Changed { rect, cells });
        }

        let row_bytes = w * 3;
        // Column bounds are tracked in bytes and converted once, so a row that
        // differs in one channel of one pixel still narrows to that pixel.
        let mut first_row = usize::MAX;
        let mut last_row = 0usize;
        let mut first_byte = usize::MAX;
        let mut last_byte = 0usize;
        let mut cells: Vec<(u16, u16)> = Vec::new();

        for r in 0..h {
            let y = rect.top + r as u16;
            let src = &rgb[r * row_bytes..(r + 1) * row_bytes];
            // Whole-slice equality first: it is a `memcmp`, and on an update that
            // changed nothing — which is most of them — no row is scanned byte by
            // byte.
            let differs = (src != self.row(rect.left, y, row_bytes))
                .then(|| differing_bytes(src, self.row(rect.left, y, row_bytes)));
            // An unknown pixel differs by definition, even where its bytes match.
            // The counter is what makes the steady state — everything known — skip
            // the flag scan outright instead of walking `w` flags per row to learn
            // nothing.
            let unknown = (self.unknown > 0)
                .then(|| self.first_unknown(rect.left, y, w))
                .flatten()
                .map(|lo| (lo * 3, self.last_unknown(rect.left, y, w).unwrap_or(lo) * 3 + 2));

            let (lo, hi) = match (differs, unknown) {
                (None, None) => continue,
                (Some(bytes), None) | (None, Some(bytes)) => bytes,
                (Some(a), Some(b)) => (a.0.min(b.0), a.1.max(b.1)),
            };
            first_row = first_row.min(r);
            last_row = r;
            first_byte = first_byte.min(lo);
            last_byte = last_byte.max(hi);

            // Which grid cells of this row actually hold a differing pixel, as
            // against which ones the row's span merely reaches over. One `memcmp`
            // per cell across a span already known to differ, so the extra work
            // only happens on rows that changed at all — and it is what stops a
            // change in two corners of a row from reading as a change everywhere
            // between them. Skipped entirely when nothing reads the answer.
            if !self.classify {
                continue;
            }
            let mine = self.row(rect.left, y, row_bytes);
            let (row_cell, left, cw) = (y / CELL_H, u32::from(rect.left), u32::from(CELL_W));
            let (span_lo, span_hi) = (left + (lo / 3) as u32, left + (hi / 3) as u32);
            let mut col = span_lo / cw;
            while col * cw <= span_hi {
                let from = ((col * cw).max(span_lo) - left) as usize * 3;
                let to = ((col * cw + cw - 1).min(span_hi) - left) as usize * 3 + 3;
                // An unknown pixel differs whatever its bytes say, so a cell the
                // unknown span reaches cannot be ruled out by comparison.
                let blind = unknown.is_some_and(|(ulo, uhi)| from <= uhi && to > ulo);
                if blind || src[from..to] != mine[from..to] {
                    cells.push((col as u16, row_cell));
                }
                col += 1;
            }
        }

        if first_row == usize::MAX {
            self.unchanged += 1;
            return None;
        }

        // Copy the changed rows only. Rows outside them are identical *and* known
        // already, so copying them would be work for no effect.
        for r in first_row..=last_row {
            let src = &rgb[r * row_bytes..(r + 1) * row_bytes];
            let y = rect.top + r as u16;
            self.row_mut(rect.left, y, row_bytes).copy_from_slice(src);
            let at = usize::from(y) * usize::from(self.w) + usize::from(rect.left);
            if self.unknown > 0 {
                self.unknown -= self.known[at..at + w].iter().filter(|known| !**known).count();
            }
            self.known[at..at + w].fill(true);
        }

        let changed = Rect {
            left: rect.left + (first_byte / 3) as u16,
            top: rect.top + first_row as u16,
            right: rect.left + (last_byte / 3) as u16,
            bottom: rect.top + last_row as u16,
        };
        self.trimmed += (usize::from(rect.w()) * usize::from(rect.h())
            - usize::from(changed.w()) * usize::from(changed.h())) as u64;
        cells.sort_unstable();
        cells.dedup();
        Some(Changed { rect: changed, cells })
    }

    /// The pixels of `rect` as packed RGB888, or `None` when any of them is
    /// unknown.
    ///
    /// CopyRect names a region of the framebuffer instead of carrying pixels, and a
    /// client here is sent tiles rather than draw commands, so the source has to be
    /// read back out of the only copy this side keeps. A region the shadow never
    /// learned cannot be reproduced, and inventing one would leave wrong pixels on
    /// screen for as long as nothing else changed there — suppressed by this very
    /// shadow on every later update. So the caller is told to ask for a repaint
    /// rather than handed a guess.
    ///
    /// The pixels are copied out before the caller writes the destination, so a
    /// source overlapping its destination still copies the original.
    pub fn copy_out(&self, rect: Rect) -> Option<Vec<u8>> {
        if rect.right >= self.w || rect.bottom >= self.h {
            return None;
        }
        let w = usize::from(rect.w());
        let mut out = Vec::with_capacity(w * usize::from(rect.h()) * 3);
        for y in rect.top..=rect.bottom {
            let at = usize::from(y) * usize::from(self.w) + usize::from(rect.left);
            // The `memchr` the `known` field is a `[bool]` for.
            if self.known[at..at + w].contains(&false) {
                return None;
            }
            out.extend_from_slice(self.row(rect.left, y, w * 3));
        }
        Some(out)
    }

    /// Move `src`'s pixels to `dst` inside the shadow, as CopyRect moves them on
    /// the remote — and as a `COPY` record is about to move them on the client's
    /// canvas.
    ///
    /// `None` when the move cannot be made, which the caller answers with a repaint
    /// rather than a guess: a source [`Self::copy_out`] refuses, or a destination
    /// off the framebuffer. `Some(false)` when the destination already held those
    /// exact pixels, which is the same dedup [`Self::accept`] does and worth doing
    /// here for the same reason — a copy nothing can see is a record and a canvas
    /// blit for nothing.
    ///
    /// The destination is checked here rather than left to `accept`, which answers
    /// a rectangle it does not cover with "all of it changed" and records nothing.
    /// That is right for pixels that arrived — sending a tile that was not needed
    /// beats withholding one that was — and wrong for this, where the answer is a
    /// `COPY` record: the client would be told to move pixels while this side kept
    /// no account of it, and the two would disagree until something else repainted
    /// that region.
    ///
    /// The source is read whole before anything is written, so a copy overlapping
    /// its own source moves the original pixels. Both ends of the link do that;
    /// this is only the third copy of them agreeing.
    pub fn copy_within(&mut self, src: Rect, dst: Rect) -> Option<bool> {
        if src.w() != dst.w() || src.h() != dst.h() || dst.right >= self.w || dst.bottom >= self.h {
            return None;
        }
        let pixels = self.copy_out(src)?;
        Some(self.accept(dst, &pixels).is_some())
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

    /// The first unknown pixel of `w` starting at `x` in row `y`, as an offset from
    /// `x`. `None` when every one of them is known, which is the steady state.
    fn first_unknown(&self, x: u16, y: u16, w: usize) -> Option<usize> {
        let at = usize::from(y) * usize::from(self.w) + usize::from(x);
        self.known[at..at + w].iter().position(|known| !known)
    }

    fn last_unknown(&self, x: u16, y: u16, w: usize) -> Option<usize> {
        let at = usize::from(y) * usize::from(self.w) + usize::from(x);
        self.known[at..at + w]
            .iter()
            .rposition(|known| !known)
    }
}

/// Copy `sub` out of `src`, which holds `rect`'s pixels as packed RGB888.
///
/// `sub` must be inside `rect`; `out` is left empty if it is not, which shows up
/// as a payload-length error at the encoder rather than as wrong pixels.
pub fn crop(src: &[u8], rect: Rect, sub: Rect, out: &mut Vec<u8>) {
    out.clear();
    if sub.left < rect.left
        || sub.top < rect.top
        || sub.right > rect.right
        || sub.bottom > rect.bottom
        || src.len() != usize::from(rect.w()) * usize::from(rect.h()) * 3
    {
        return;
    }
    let stride = usize::from(rect.w()) * 3;
    let row_bytes = usize::from(sub.w()) * 3;
    let left = usize::from(sub.left - rect.left) * 3;
    out.reserve(row_bytes * usize::from(sub.h()));
    for r in 0..usize::from(sub.h()) {
        let at = (usize::from(sub.top - rect.top) + r) * stride + left;
        out.extend_from_slice(&src[at..at + row_bytes]);
    }
}

/// The first and last byte index at which two **equal-length** rows differ.
///
/// Only called for rows already known to differ, so there is always an answer.
/// Saturating instead of unwrapping anyway: the caller is one line away today, and
/// a panic here would take a whole session down over an update that could simply
/// have been sent whole. The length precondition is held to the same standard —
/// asserted in debug, answered with "the whole row differs" in release — because
/// the remainder scans below index `b` at offsets derived from `a`, and two
/// different widths would otherwise be the one input that panics.
///
/// Word-compared eight bytes at a time from both ends — the byte-at-a-time scans
/// this replaces ran the full width of every changed row, and the reverse one
/// defeated autovectorization outright.
fn differing_bytes(a: &[u8], b: &[u8]) -> (usize, usize) {
    debug_assert_eq!(a.len(), b.len(), "differing_bytes compares two rows of one width");
    if a.len() != b.len() {
        return (0, a.len().saturating_sub(1));
    }
    const WORD: usize = 8;
    let word = |chunk: &[u8]| u64::from_ne_bytes(chunk.try_into().expect("an 8-byte chunk"));

    let mut first = a.len();
    for (i, (x, y)) in a.chunks_exact(WORD).zip(b.chunks_exact(WORD)).enumerate() {
        if word(x) != word(y) {
            first = i * WORD + x.iter().zip(y).position(|(p, q)| p != q).unwrap_or(0);
            break;
        }
    }
    if first == a.len() {
        let head = a.len() - a.len() % WORD;
        first = a[head..]
            .iter()
            .zip(&b[head..])
            .position(|(p, q)| p != q)
            .map_or(0, |i| head + i);
    }

    let mut last = None;
    for (i, (x, y)) in a.rchunks_exact(WORD).zip(b.rchunks_exact(WORD)).enumerate() {
        if word(x) != word(y) {
            let end = a.len() - i * WORD;
            let back = x.iter().rev().zip(y.iter().rev()).position(|(p, q)| p != q).unwrap_or(0);
            last = Some(end - 1 - back);
            break;
        }
    }
    let last = last.unwrap_or_else(|| {
        let tail = a.len() % WORD;
        a[..tail]
            .iter()
            .rev()
            .zip(b[..tail].iter().rev())
            .position(|(p, q)| p != q)
            .map_or_else(|| a.len().saturating_sub(1), |back| tail - 1 - back)
    });
    (first, last)
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

    /// What CopyRect reads back, and the one thing it must refuse.
    #[test]
    fn copy_out_returns_known_pixels_and_nothing_else() {
        let mut shadow = Shadow::new("test", 8, 4);
        let painted = rect(2, 1, 5, 2);
        shadow.accept_rect(painted, &solid(painted, 9));

        assert_eq!(shadow.copy_out(painted), Some(solid(painted, 9)));
        // One row up is a region nothing has painted, so it cannot be reproduced.
        assert_eq!(shadow.copy_out(rect(2, 0, 5, 1)), None);
        // Neither can one that leaves the framebuffer.
        assert_eq!(shadow.copy_out(rect(6, 1, 9, 2)), None);
    }

    /// `forget` drops the claim to know the pixels, so a CopyRect right after a
    /// browser refresh has nothing to read and asks for a repaint instead.
    #[test]
    fn copy_out_refuses_everything_a_forget_has_disclaimed() {
        let mut shadow = Shadow::new("test", 8, 4);
        let r = rect(0, 0, 7, 3);
        shadow.accept_rect(r, &solid(r, 9));
        assert!(shadow.copy_out(r).is_some());
        shadow.forget();
        assert_eq!(shadow.copy_out(r), None);
    }

    #[test]
    fn the_first_send_of_anything_but_black_is_new() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(0, 0, 15, 15);
        assert_eq!(shadow.accept_rect(r, &solid(r, 9)), Some(r));
    }

    /// Black on a fresh shadow *is* new. This is the case that makes `known`
    /// necessary rather than tidy: a client keeps its pixels when a `resize`
    /// repeats the size it already has, so a shadow that assumed black would
    /// withhold every region that is now black — a window closed while nobody was
    /// attached would stay on screen for the rest of the session.
    #[test]
    fn black_on_a_fresh_shadow_is_still_sent() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(0, 0, 15, 15);
        assert_eq!(shadow.accept_rect(r, &solid(r, 0)), Some(r));
        assert_eq!(shadow.accept_rect(r, &solid(r, 0)), None, "but only once");
    }

    /// The same hazard through the front door: a region goes black while the shadow
    /// has been forgotten, and must still be sent.
    #[test]
    fn a_region_that_went_black_while_forgotten_is_sent() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(0, 0, 31, 31);
        shadow.accept_rect(r, &solid(r, 200));

        shadow.forget();

        assert_eq!(shadow.accept_rect(r, &solid(r, 0)), Some(r));
    }

    /// A rectangle partly covering a forgotten region: the unknown pixels widen the
    /// trim even where their bytes happen to match, or they would never be sent.
    #[test]
    fn unknown_pixels_widen_the_trim() {
        let mut shadow = Shadow::new("test", 64, 64);
        let whole = rect(0, 0, 63, 63);
        shadow.accept_rect(whole, &solid(whole, 5));
        assert_eq!(shadow.accept_rect(whole, &solid(whole, 5)), None);

        shadow.forget();

        // Identical pixels, but nothing is known any more, so all of it is sent.
        assert_eq!(shadow.accept_rect(whole, &solid(whole, 5)), Some(whole));
    }

    /// The bounding box reaches across a change in two corners; the cell list does
    /// not. This is the distinction the whole struct exists for: everything inside
    /// the box gets *sent*, but only what actually differs gets *counted* as
    /// change, and counting the rest is what puts a still sidebar into motion
    /// because a video is playing at the other end of the same screen.
    #[test]
    fn changed_cells_do_not_span_a_bounding_box() {
        let mut shadow = Shadow::new("test", CELL_W * 4, CELL_H * 2);
        let whole = rect(0, 0, CELL_W * 4 - 1, CELL_H * 2 - 1);
        shadow.accept_rect(whole, &solid(whole, 1));

        // Two pixels change: one in the top-left cell, one in the bottom-right.
        let mut pixels = solid(whole, 1);
        let at = |x: usize, y: usize| (y * usize::from(CELL_W) * 4 + x) * 3;
        pixels[at(4, 4)] = 2;
        let far = at(usize::from(CELL_W) * 3 + 4, usize::from(CELL_H) + 4);
        pixels[far] = 2;

        let changed = shadow.accept(whole, &pixels).expect("something changed");
        assert!(
            changed.rect.contains(&rect(4, 4, CELL_W * 3 + 4, CELL_H + 4)),
            "the box has to reach both, or the far pixel is never sent: {:?}",
            changed.rect
        );
        assert_eq!(
            changed.cells,
            vec![(0, 0), (3, 1)],
            "the cells in between were counted as changed"
        );
        assert!(changed.has((0, 0)) && changed.has((3, 1)) && !changed.has((1, 0)));
    }

    /// Every cell of a region genuinely repainted end to end is reported, so the
    /// narrowing above cannot be hiding real change.
    #[test]
    fn a_wholly_repainted_region_reports_every_cell() {
        let mut shadow = Shadow::new("test", CELL_W * 2, CELL_H * 2);
        let whole = rect(0, 0, CELL_W * 2 - 1, CELL_H * 2 - 1);

        // The first acceptance is all-unknown, which differs by definition.
        let first = shadow.accept(whole, &solid(whole, 1)).expect("nothing was known");
        assert_eq!(first.cells, vec![(0, 0), (0, 1), (1, 0), (1, 1)]);

        // And again by comparison, with everything known and everything different.
        let second = shadow.accept(whole, &solid(whole, 2)).expect("every pixel changed");
        assert_eq!(second.cells, first.cells);
        assert_eq!(second.rect, whole);
    }

    #[test]
    fn an_unchanged_repeat_is_dropped_whole() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(8, 8, 23, 23);
        assert_eq!(shadow.accept_rect(r, &solid(r, 5)), Some(r));
        assert_eq!(shadow.accept_rect(r, &solid(r, 5)), None);
        assert_eq!(shadow.accept_rect(r, &solid(r, 5)), None);
    }

    /// The point of comparing pixels rather than hashing them: an over-reported
    /// rectangle is sent as the part that actually moved.
    #[test]
    fn an_over_reported_rectangle_is_trimmed_to_what_changed() {
        let mut shadow = Shadow::new("test", 200, 200);
        let whole = rect(0, 0, 199, 199);
        shadow.accept_rect(whole, &solid(whole, 1));

        // Change one pixel at (100, 50) inside a rectangle covering the screen.
        let mut pixels = solid(whole, 1);
        let at = (50 * 200 + 100) * 3;
        pixels[at..at + 3].copy_from_slice(&[9, 9, 9]);

        assert_eq!(shadow.accept_rect(whole, &pixels), Some(rect(100, 50, 100, 50)));
    }

    #[test]
    fn a_trim_keeps_every_changed_pixel_inside_it() {
        let mut shadow = Shadow::new("test", 100, 100);
        let whole = rect(0, 0, 99, 99);
        shadow.accept_rect(whole, &solid(whole, 1));

        let mut pixels = solid(whole, 1);
        for (x, y) in [(10u16, 20u16), (60, 25), (33, 70)] {
            let at = (usize::from(y) * 100 + usize::from(x)) * 3;
            pixels[at..at + 3].copy_from_slice(&[7, 7, 7]);
        }

        let changed = shadow.accept_rect(whole, &pixels).expect("something changed");
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
        shadow.accept_rect(whole, &solid(whole, 1));

        let mut pixels = solid(whole, 1);
        let at = (32 * 64 + 32) * 3;
        pixels[at..at + 3].copy_from_slice(&[4, 4, 4]);
        assert_eq!(shadow.accept_rect(whole, &pixels), Some(rect(32, 32, 32, 32)));

        // The same pixels again: nothing new, so the changed row was recorded.
        assert_eq!(shadow.accept_rect(whole, &pixels), None);
        // And back to the original: a change again, not a phantom match.
        assert_eq!(
            shadow.accept_rect(whole, &solid(whole, 1)),
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
        shadow.accept_rect(big, &solid(big, 1));

        // Paint the small rect a different colour.
        assert_eq!(shadow.accept_rect(small, &solid(small, 2)), Some(small));

        // Now the big rect is reported again with its *original* pixels. A memo
        // keyed by rectangle would still hold the first hash and suppress this,
        // leaving the small patch on screen forever. The changed area is exactly
        // the small patch.
        assert_eq!(shadow.accept_rect(big, &solid(big, 1)), Some(small));
    }

    #[test]
    fn forgetting_sends_everything_again() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(0, 0, 31, 31);
        assert_eq!(shadow.accept_rect(r, &solid(r, 3)), Some(r));
        assert_eq!(shadow.accept_rect(r, &solid(r, 3)), None);

        shadow.forget();

        assert_eq!(
            shadow.accept_rect(r, &solid(r, 3)),
            Some(r),
            "a repaint must not be suppressed"
        );
    }

    #[test]
    fn a_resize_forgets_and_regrids() {
        let mut shadow = Shadow::new("test", 64, 64);
        let r = rect(0, 0, 31, 31);
        shadow.accept_rect(r, &solid(r, 3));

        shadow.resize(128, 96);

        assert_eq!(shadow.size(), (128, 96));
        assert_eq!(shadow.accept_rect(r, &solid(r, 3)), Some(r));
        let far = rect(96, 64, 127, 95);
        assert_eq!(shadow.accept_rect(far, &solid(far, 3)), Some(far));
    }

    /// A rectangle the shadow does not cover is sent, not suppressed and not
    /// remembered. Wasting bytes is recoverable; a stale region is not.
    #[test]
    fn a_rectangle_outside_the_shadow_is_always_sent() {
        let mut shadow = Shadow::new("test", 32, 32);
        let outside = rect(16, 16, 47, 47);
        assert_eq!(shadow.accept_rect(outside, &solid(outside, 1)), Some(outside));
        assert_eq!(shadow.accept_rect(outside, &solid(outside, 1)), Some(outside));
    }

    /// A payload that does not match its rectangle is sent rather than trusted
    /// into a comparison that would read the wrong rows.
    #[test]
    fn a_mismatched_payload_length_is_not_compared() {
        let mut shadow = Shadow::new("test", 32, 32);
        let r = rect(0, 0, 15, 15);
        assert_eq!(shadow.accept_rect(r, &[1, 2, 3]), Some(r));
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

    #[test]
    fn an_intersection_is_the_pixels_two_rectangles_share() {
        let a = rect(10, 10, 29, 29);
        assert_eq!(a.intersect(&a), Some(a), "a rectangle does not intersect itself");
        assert_eq!(
            a.intersect(&rect(20, 20, 39, 39)),
            Some(rect(20, 20, 29, 29)),
            "an overlapping corner"
        );
        assert_eq!(
            a.intersect(&rect(0, 0, 100, 100)),
            Some(a),
            "a rectangle wholly inside another is the whole of the overlap"
        );
        // right/bottom are inclusive, so 29 and 30 are neighbours, not overlaps.
        assert_eq!(a.intersect(&rect(29, 10, 40, 29)), Some(rect(29, 10, 29, 29)), "one column");
        assert_eq!(a.intersect(&rect(30, 10, 40, 29)), None, "touching edges are not an overlap");
        assert_eq!(a.intersect(&rect(0, 0, 5, 5)), None, "disjoint");
    }

    /// Every pixel of the source lands in exactly one piece, and in no piece
    /// twice. Asserted by counting rather than by comparing rectangles, so it
    /// holds for any shape rather than the one that happened to be written down.
    #[test]
    fn cells_cover_a_rectangle_exactly_once() {
        for source in [
            rect(0, 0, 0, 0),
            rect(37, 41, 900, 200),
            rect(319, 63, 320, 64),
            rect(1000, 500, 1279, 799),
            rect(0, 0, CELL_W * 3 - 1, CELL_H * 3 - 1),
        ] {
            let mut seen = std::collections::HashSet::new();
            for cell in source.cells() {
                assert!(source.contains(&cell), "{cell:?} escaped {source:?}");
                for y in cell.top..=cell.bottom {
                    for x in cell.left..=cell.right {
                        assert!(seen.insert((x, y)), "({x},{y}) covered twice in {source:?}");
                    }
                }
            }
            let area = usize::from(source.w()) * usize::from(source.h());
            assert_eq!(seen.len(), area, "{source:?} was not fully covered");
        }
    }

    /// The property `cell_key` rests on: a piece lies wholly within one cell, so
    /// its every pixel answers to the same key its top-left does.
    #[test]
    fn no_cell_straddles_a_grid_line() {
        let source = rect(37, 41, 900, 200);
        for cell in source.cells() {
            assert_eq!(
                (cell.left / CELL_W, cell.top / CELL_H),
                (cell.right / CELL_W, cell.bottom / CELL_H),
                "{cell:?} spans two cells"
            );
            assert_eq!(cell.cell_key(), (cell.left / CELL_W, cell.top / CELL_H));
        }
    }

    /// The identity the whole scheme wants: two protocols describing the same
    /// region with different rectangles still name the same cell. A `bands` piece
    /// anchored to its rectangle's top is the case that forces the vertical cut —
    /// without it, the band below would carry the row above's key.
    #[test]
    fn differently_shaped_damage_lands_on_the_same_key() {
        let wide = rect(600, 100, 700, 110);
        let tall = rect(650, 70, 660, 120);
        let keys = |r: Rect| r.cells().map(|c| c.cell_key()).collect::<Vec<_>>();
        assert_eq!(keys(wide), vec![(1, 1), (2, 1)]);
        assert_eq!(keys(tall), vec![(2, 1)]);

        // A band that straddles y = CELL_H is cut into both rows.
        let band = rect(0, CELL_H - 1, 99, CELL_H * 2 - 2);
        assert_eq!(keys(band), vec![(0, 0), (0, 1)]);
    }

    /// A rectangle inside one cell is one piece, unchanged — the case a still
    /// screen spends nearly all its time in.
    #[test]
    fn a_rectangle_inside_one_cell_is_left_alone() {
        let small = rect(330, 70, 350, 90);
        assert_eq!(small.cells().collect::<Vec<_>>(), vec![small]);
        assert_eq!(small.cell_key(), (1, 1));
    }

    /// The pieces arrive in the order the engines emit tiles, so a client paints
    /// a split band the way it paints an unsplit one.
    #[test]
    fn cells_arrive_left_to_right_then_top_down() {
        let source = rect(300, 60, 650, 130);
        assert_eq!(
            source.cells().collect::<Vec<_>>(),
            vec![
                rect(300, 60, 319, 63),
                rect(320, 60, 639, 63),
                rect(640, 60, 650, 63),
                rect(300, 64, 319, 127),
                rect(320, 64, 639, 127),
                rect(640, 64, 650, 127),
                rect(300, 128, 319, 130),
                rect(320, 128, 639, 130),
                rect(640, 128, 650, 130),
            ]
        );
    }
}
