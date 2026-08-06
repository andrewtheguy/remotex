//! What the video path shares with the rest of the gateway: the framebuffer copy the
//! streams read, the rectangle one of them may encode, and the colour conversion in
//! front of it.
//!
//! Two render dials arrive at a stream and they differ only in how many rectangles
//! they ask for. `render_type = "video"` asks for one covering the whole desktop.
//! `render_motion_subtype` asks for one per coalesced moving region, with the still
//! codecs carrying everything else. Which rectangles, and when they start and stop, is
//! [`crate::regions`]' business; [`crate::vp9`] knows only how to encode one.
//!
//! Three things about the shape follow from the rest of the gateway rather than from
//! any codec:
//!
//! - **A frame may never be dropped.** [`crate::tiles::Shadow`] records source pixels
//!   as delivered the moment it accepts them, so a frame an encoder skipped is
//!   permanently wrong pixels — nothing re-sends it. Hence an empty bitstream leaves
//!   the caller's dirty flag alone instead of clearing it.
//! - **A stream is fed rectangles, not frames.** Damage arrives as rectangles, and
//!   VNC's can only be cropped out of the one it just decoded, so the [`Mirror`] holds
//!   the whole framebuffer and every stream encodes a crop of it when the engine says a
//!   frame has ended. One mirror rather than one per stream is also what lets a stream
//!   start in the middle of a session: its region's pixels are already there, whether
//!   or not they have changed recently.
//! - **The mirror is the newest truth, so a cleanup needs no stash.** Everything
//!   blitted is exact source, moving or not, so a region that settles is re-sent crisp
//!   by cropping it out of here — never from a remembered frame that has since been
//!   overtaken. See [`crate::regions`].
//!
//! A detach drops frames outright (`SessionManager::pump`), so a client coming back has
//! missed part of every stream and cannot decode from where it left off. It recovers
//! because every attach injects a repaint, which is one of the moments a stream's
//! keyframe is forced.

use crate::tiles::Rect;

/// The 1–100 quality dial, coarsest first.
///
/// The dial is the gateway's own scale rather than a quantizer: [`crate::vp9`] maps it
/// onto its 0–63, and that mapping is the only place the two numbers meet. The
/// congestion loop in [`crate::encode`] walks *this* scale, so a future codec is a new
/// mapping rather than a new loop.
pub const QUALITY_MIN: u8 = 1;
/// See [`QUALITY_MIN`].
pub const QUALITY_MAX: u8 = 100;

/// The longest side a stream will encode, and the longest the *other* side may then
/// be.
///
/// Checked either way up, because the limit is on the picture and not on width:
/// 3840×2160 and 2160×3840 are both legal, and 3840×3840 is not. A `w <= 3840 &&
/// h <= 2160` test would read almost the same and would refuse a portrait desktop the
/// encoder is perfectly happy with.
///
/// libvpx has no ceiling this near — 4K is only VP9 level 5.0 of 6.2 — so this is the
/// gateway's own line: past 4K a software realtime encode of a desktop stops being
/// realtime, and the still paths carry such a desktop better.
const MAX_LONG_SIDE: u16 = 3840;
/// See [`MAX_LONG_SIDE`].
const MAX_SHORT_SIDE: u16 = 2160;

/// One encoded frame: a complete access unit, and whether a decoder that has seen
/// nothing before it can start here.
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// A `render_motion_debug` outline drawn round the picture a stream encodes.
///
/// Painted on the crop, never on the [`Mirror`]: the mirror is what
/// [`crate::tiles::Shadow`] has already recorded as delivered and what a cleanup crops,
/// so a mark left there would be restored as though it were content and nothing would
/// ever take it off again. On the crop it lives exactly as long as the stream does, and
/// the cleanup that follows erases it.
#[derive(Clone, Copy)]
pub struct Mark {
    pub colour: [u8; 3],
    /// Border thickness in pixels. Owned by the caller because the still path draws the
    /// same outline round its own pieces, and one thickness should mean one thickness.
    pub px: u16,
}

/// The whole framebuffer as packed RGB888, and the one copy every stream reads.
///
/// Held at the desktop size rounded up to even sides. Only an encoder ever sees those
/// extra pixels — a client is told the true size and crops — and they are filled from
/// their neighbours rather than left black, because a hard black edge beside content is
/// a strong feature an encoder would pay for in every frame.
#[derive(Clone)]
pub struct Mirror {
    /// The desktop as the client knows it, and as a record header reports it.
    size: (u16, u16),
    /// The picture actually held: [`Self::size`] rounded up to even sides.
    coded: (u16, u16),
    rgb: Vec<u8>,
}

impl Mirror {
    /// A mirror for a `w`×`h` desktop. ~6 MB at 1080p.
    pub fn new(w: u16, h: u16) -> anyhow::Result<Self> {
        anyhow::ensure!(w > 0 && h > 0, "a video mirror cannot hold a {w}x{h} desktop");
        // Saturating rather than wrapping: a 65535-wide desktop is not real, but
        // wrapping to 0 here would hand an encoder a zero-sized picture, and the point
        // of this constructor is that nothing invalid gets that far.
        let coded = (w.saturating_add(w % 2), h.saturating_add(h % 2));
        let (cw, ch) = (usize::from(coded.0), usize::from(coded.1));
        Ok(Self { size: (w, h), coded, rgb: vec![0; cw * ch * 3] })
    }

    /// The desktop this mirror is for.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// The picture actually held: up to one pixel wider and one taller than
    /// [`Self::size`].
    pub fn coded(&self) -> (u16, u16) {
        self.coded
    }

    /// The whole desktop as a rectangle — the region a `video` target streams.
    pub fn rect(&self) -> Rect {
        Rect::from_size(0, 0, self.size.0, self.size.1).expect("a desktop with pixels")
    }

    /// Copy `rgb` — packed RGB888 for `rect` — into the mirror.
    ///
    /// A rectangle outside the desktop is refused rather than clipped: it means this
    /// mirror and the engine disagree about how big the framebuffer is, and taking part
    /// of the update would put that disagreement on the screen instead of in the log.
    pub fn blit(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        let (w, h) = (usize::from(rect.w()), usize::from(rect.h()));
        anyhow::ensure!(
            rgb.len() == w * h * 3,
            "a video blit is {} bytes, expected {} for {w}x{h} RGB",
            rgb.len(),
            w * h * 3
        );
        anyhow::ensure!(
            rect.right < self.size.0 && rect.bottom < self.size.1,
            "a video blit of {w}x{h} at ({},{}) falls outside a {}x{} desktop",
            rect.left,
            rect.top,
            self.size.0,
            self.size.1
        );

        let stride = usize::from(self.coded.0) * 3;
        for row in 0..h {
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 3;
            self.rgb[at..at + w * 3].copy_from_slice(&rgb[row * w * 3..(row + 1) * w * 3]);
        }
        Ok(())
    }

    /// `rect`'s pixels as packed RGB888, into a buffer the caller reuses.
    ///
    /// `rect` may reach into the padding — that is exactly what a stream at the
    /// desktop's odd edge does — so this is bounded by [`Self::coded`] rather than by
    /// [`Self::size`]. Out of those bounds it refuses: the alternative is an index
    /// panic, and this binary aborts on one.
    pub fn crop_into(&self, rect: Rect, out: &mut Vec<u8>) -> anyhow::Result<()> {
        anyhow::ensure!(
            rect.right < self.coded.0 && rect.bottom < self.coded.1,
            "a video crop of {}x{} at ({},{}) falls outside a {}x{} mirror",
            rect.w(),
            rect.h(),
            rect.left,
            rect.top,
            self.coded.0,
            self.coded.1
        );
        let stride = usize::from(self.coded.0) * 3;
        let (w, h) = (usize::from(rect.w()), usize::from(rect.h()));
        out.clear();
        out.reserve(w * h * 3);
        for row in 0..h {
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 3;
            out.extend_from_slice(&self.rgb[at..at + w * 3]);
        }
        Ok(())
    }

    /// Copy `rect`'s pixels from `src` — the double-buffer sync in
    /// [`crate::regions::Regions::take_round`].
    ///
    /// The two mirrors are twins by construction (the spare is a clone of the
    /// current one), and every staged rect went through [`Self::blit`]'s bounds
    /// check, so a mismatch here is a bug in that bookkeeping rather than a state a
    /// session can reach — asserted in debug, clamped to a no-op in release, where
    /// the worst outcome is a stale region the next damage repaints.
    pub fn adopt(&mut self, src: &Mirror, rect: Rect) {
        debug_assert_eq!(self.coded, src.coded, "a mirror adopted from a differently sized twin");
        if self.coded != src.coded || rect.right >= self.coded.0 || rect.bottom >= self.coded.1 {
            return;
        }
        let stride = usize::from(self.coded.0) * 3;
        let (w, h) = (usize::from(rect.w()), usize::from(rect.h()));
        for row in 0..h {
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 3;
            self.rgb[at..at + w * 3].copy_from_slice(&src.rgb[at..at + w * 3]);
        }
    }

    /// The whole coded picture as one packed RGB888 slice, when `rect` *is* the
    /// coded picture — the `Policy::Whole` stream, whose crop was a full-framebuffer
    /// copy producing byte-for-byte what this buffer already holds. `None` for any
    /// smaller rectangle, whose rows are not contiguous here.
    pub fn whole(&self, rect: Rect) -> Option<&[u8]> {
        (rect.left == 0 && rect.top == 0 && (rect.w(), rect.h()) == self.coded)
            .then_some(&self.rgb)
    }

    /// Fill the at-most-one padding column and row from their neighbours.
    ///
    /// Only an odd-sized desktop has any. Called once per encode round rather than per
    /// stream: at most one stream can reach the pad on each axis, and finding out which
    /// is more work than repeating a column copy.
    pub fn pad_edges(&mut self) {
        let stride = usize::from(self.coded.0) * 3;
        if self.coded.0 != self.size.0 {
            let last = usize::from(self.size.0 - 1) * 3;
            for row in 0..usize::from(self.size.1) {
                let at = row * stride + last;
                self.rgb.copy_within(at..at + 3, at + 3);
            }
        }
        if self.coded.1 != self.size.1 {
            let last = usize::from(self.size.1 - 1) * stride;
            self.rgb.copy_within(last..last + stride, last + stride);
        }
    }
}

/// The rectangle a stream over `rect` actually encodes: `rect` grown to even sides.
///
/// **The evenness of the coded rectangle is a theorem, not a hope, and this is where it
/// is checked.** I420 subsamples chroma 2×2, so [`I420`] needs even sides. A region is
/// a union of whole grid cells
/// clipped to the desktop, and `CELL_W`/`CELL_H` are both even, so a region's origin is
/// always even and its size is odd only where its right or bottom edge is the desktop's
/// own and the desktop is odd there — in which case the mirror's own padding is exactly
/// the column or row needed. So the coded rectangle is the region grown right and down,
/// it never leaves the mirror, and it never contains a pixel that was invented. A
/// whole-desktop `video` stream is the same statement with the region set to the
/// desktop.
///
/// VP9 itself does not need even sides, and it is held to them anyway. The mirror's
/// padding, the conversion's whole-pixel chroma groups and the geometry theorem in
/// [`crate::regions`] are one argument, and an odd-width chroma plane would be a second
/// path to be wrong in — for no gain, since the pad column is already there and costs
/// one column of repeated pixels.
///
/// Fails for a picture the encoder will not take. That cannot be a config-time refusal —
/// only the remote knows its own size, and it may change mid-session — so the message
/// has to carry the whole explanation to wherever it surfaces.
pub fn coded_rect(rect: Rect, mirror: (u16, u16)) -> anyhow::Result<Rect> {
    // Checked rather than plain arithmetic, though a rectangle ending at the last
    // representable pixel needs a desktop wider than `u16` can describe: this binary
    // aborts on an arithmetic overflow, and the point of this function is that nothing
    // invalid gets as far as an abort.
    let (right, bottom) = rect
        .right
        .checked_add(rect.w() % 2)
        .zip(rect.bottom.checked_add(rect.h() % 2))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "a video stream over {}x{} at ({},{}) cannot be grown to even sides \
                 without leaving the framebuffer's coordinate space",
                rect.w(),
                rect.h(),
                rect.left,
                rect.top
            )
        })?;
    let coded = Rect { left: rect.left, top: rect.top, right, bottom };
    anyhow::ensure!(
        coded.right < mirror.0 && coded.bottom < mirror.1,
        "a video stream over {}x{} at ({},{}) needs even sides, and growing it to \
         {}x{} leaves a {}x{} mirror — a region is a union of grid cells, so this \
         is a region that was not",
        rect.w(),
        rect.h(),
        rect.left,
        rect.top,
        coded.w(),
        coded.h(),
        mirror.0,
        mirror.1
    );
    let (long, short) = (coded.w().max(coded.h()), coded.w().min(coded.h()));
    anyhow::ensure!(
        long <= MAX_LONG_SIDE && short <= MAX_SHORT_SIDE,
        "a video stream will not encode a {}x{} picture: one is refused with a long \
         side over {MAX_LONG_SIDE} or a short side over {MAX_SHORT_SIDE}. Only the \
         remote knows its own size, so check-config cannot catch this — give this \
         target a still render_type (\"fixed-quality\" with render_subtype = \
         \"webp\"), or ask the remote for a smaller desktop",
        rect.w(),
        rect.h()
    );
    Ok(coded)
}

/// How many threads a stream's encoder gets: several for the one stream that covers
/// the whole mirror, one for a region stream.
///
/// Region streams are disjoint and encode in parallel with each other on the same
/// round, which is where the cores go — a second level of threading inside each
/// would oversubscribe them. `Policy::Whole` has exactly one stream and nothing to
/// overlap with, so its parallelism has to come from inside the picture: VP9's
/// row-based multithreading, set by the caller alongside this count. Half the
/// machine, capped: the engine's read loop, the socket and the tile workers still
/// need somewhere to run.
pub fn threads_for(coded: Rect, mirror: (u16, u16)) -> usize {
    let whole = coded.left == 0
        && coded.top == 0
        && (coded.w(), coded.h()) == mirror;
    if !whole {
        return 1;
    }
    std::thread::available_parallelism().map_or(1, |n| n.get() / 2).clamp(1, 4)
}

/// One picture as I420, and the RGB→YUV conversion in front of the encoder.
///
/// Scalar integer BT.601 studio-swing arithmetic with 2×2-averaged chroma, owned here
/// rather than a library's. The planes are the tight `(w, w/2, w/2)` I420
/// layout libvpx wraps without copying. The conversion's cost is measured separately in
/// the encoder bench, because if it ever dominates a release encode, *that* is the
/// number that would justify libyuv.
pub struct I420 {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    size: (usize, usize),
}

impl I420 {
    /// A buffer for a `w`×`h` picture, both even.
    ///
    /// Reused across frames so a 1080p conversion is not a 3 MB allocation apiece.
    /// Evenness is [`coded_rect`]'s theorem, and it is what keeps the chroma rows
    /// below made of whole 2×2 groups.
    pub fn new(w: u16, h: u16) -> Self {
        let size = (usize::from(w), usize::from(h));
        Self {
            y: vec![0; size.0 * size.1],
            u: vec![0; size.0 * size.1 / 4],
            v: vec![0; size.0 * size.1 / 4],
            size,
        }
    }

    /// Convert `rgb` — packed RGB888 for exactly this buffer's picture — in place.
    ///
    /// The length is checked rather than trusted, because everything after the check
    /// indexes by the picture size and this binary aborts on an out-of-bounds panic.
    pub fn read_rgb(&mut self, rgb: &[u8]) -> anyhow::Result<()> {
        let (w, h) = self.size;
        anyhow::ensure!(
            rgb.len() == w * h * 3,
            "a video crop came back {} bytes for a {w}x{h} picture",
            rgb.len(),
        );
        for (pix, y) in rgb.chunks_exact(3).zip(self.y.iter_mut()) {
            *y = (((66 * u32::from(pix[0]) + 129 * u32::from(pix[1]) + 25 * u32::from(pix[2]))
                >> 8)
                + 16) as u8;
        }
        // Chroma is one sample per 2×2 pixel group, from the group's average — the
        // arithmetic never leaves i16: the largest coefficient sum is 112 × 255.
        let half = w / 2;
        let rows0 = rgb.chunks_exact(w * 3).step_by(2);
        let rows1 = rgb.chunks_exact(w * 3).skip(1).step_by(2);
        let u_rows = self.u.chunks_exact_mut(half);
        let v_rows = self.v.chunks_exact_mut(half);
        for (((row0, row1), u_row), v_row) in rows0.zip(rows1).zip(u_rows).zip(v_rows) {
            for (((pix0, pix1), u), v) in
                row0.chunks_exact(6).zip(row1.chunks_exact(6)).zip(u_row).zip(v_row)
            {
                let r = (i16::from(pix0[0]) + i16::from(pix0[3]) + i16::from(pix1[0]) + i16::from(pix1[3]) + 2) / 4;
                let g = (i16::from(pix0[1]) + i16::from(pix0[4]) + i16::from(pix1[1]) + i16::from(pix1[4]) + 2) / 4;
                let b = (i16::from(pix0[2]) + i16::from(pix0[5]) + i16::from(pix1[2]) + i16::from(pix1[5]) + 2) / 4;
                *u = (((-38 * r - 74 * g + 112 * b) >> 8) + 128) as u8;
                *v = (((112 * r - 94 * g - 18 * b) >> 8) + 128) as u8;
            }
        }
        Ok(())
    }

    /// The three planes, for the codec's image to point at.
    pub fn planes(&self) -> (&[u8], &[u8], &[u8]) {
        (&self.y, &self.u, &self.v)
    }

    /// The three planes' strides, in the same order as [`Self::planes`].
    pub fn strides(&self) -> (usize, usize, usize) {
        (self.size.0, self.size.0 / 2, self.size.0 / 2)
    }
}

/// Paint `mark`'s border round a `w`×`h` packed-RGB888 picture, in place.
pub fn outline(rgb: &mut [u8], (w, h): (usize, usize), mark: Mark) {
    let t = usize::from(mark.px);
    let mut paint = |row: usize, from: usize, to: usize| {
        for x in from..to {
            rgb[(row * w + x) * 3..][..3].copy_from_slice(&mark.colour);
        }
    };
    for y in 0..h {
        if y < t || y + t >= h {
            paint(y, 0, w);
        } else {
            paint(y, 0, t.min(w));
            paint(y, w.saturating_sub(t), w);
        }
    }
}

#[cfg(test)]
impl Mirror {
    /// The pixel at `(x, y)`, for the tests about blitting and padding.
    pub(crate) fn pixel(&self, x: u16, y: u16) -> [u8; 3] {
        let at = (usize::from(y) * usize::from(self.coded.0) + usize::from(x)) * 3;
        [self.rgb[at], self.rgb[at + 1], self.rgb[at + 2]]
    }

    pub(crate) fn len(&self) -> usize {
        self.rgb.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle from a position and a size, which is what most of these want.
    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    /// Synthetic screen content: a light panel with text-like runs, and one window being
    /// dragged across the dark side of the desktop.
    ///
    /// Screen content rather than a gradient or noise, and moving rather than still,
    /// because both encoders are tuned for the first and neither is measurable on the
    /// second: identical frames cost nearly nothing at any quality, and noise costs the
    /// same at every quality. This is the same picture `crates/libvpx-e2e` draws, in RGB
    /// rather than I420, so a number from one is comparable with a number from the other.
    fn screen(w: u16, h: u16, frame: u32) -> Vec<u8> {
        let (w, h) = (usize::from(w), usize::from(h));
        let mut rgb = vec![24u8; w * h * 3];
        let put = |rgb: &mut [u8], row: usize, from: usize, to: usize, shade: u8| {
            let to = to.min(w);
            if from < to {
                rgb[(row * w + from) * 3..(row * w + to) * 3].fill(shade);
            }
        };
        for row in 0..h {
            put(&mut rgb, row, w * 2 / 3, w, 200);
        }
        for line in 0..(h / 20) {
            let row = 24 + line * 20;
            if row + 9 >= h {
                break;
            }
            for glyph in 0..14 {
                let x = w * 2 / 3 + 8 + glyph * 12;
                for dy in 0..9 {
                    put(&mut rgb, row + dy, x, x + 7, 32);
                }
            }
        }
        let x0 = 8 + (frame as usize * 9) % (w / 2);
        let y0 = 40 + (frame as usize * 5) % (h / 2);
        for dy in 0..120 {
            let row = y0 + dy;
            if row >= h {
                break;
            }
            put(&mut rgb, row, x0, x0 + 160, if dy < 24 { 96 } else { 144 });
        }
        rgb
    }

    /// What the encoder costs on desktop pixels — the measurement that settles VP9's
    /// `Q_FINEST`, `CPU_USED` and thread count.
    ///
    /// `#[ignore]`d because it takes a minute and prints rather than asserts: the numbers
    /// are the output, and a threshold on them would be a test of this machine.
    ///
    /// **`--release`, and that is not the usual boilerplate — a debug run measures the
    /// wrong thing and says so convincingly.** The encoder is C and is optimized
    /// whatever this profile is: libvpx is compiled `-O3` once, into the archive
    /// `libvpx-prebuilt` publishes, and nothing a consumer does can touch it. The
    /// conversion is *Rust* — [`I420::read_rgb`] — so it is compiled with **this**
    /// crate's profile, and at
    /// `opt-level = 0` it is per-pixel arithmetic with bounds checks and no vectorization.
    /// Measured: 29.1 ms/frame at 1280×800 in debug against 0.44 in release, a 66× swing
    /// that makes the conversion look like 90% of the encode and sends the reader off to
    /// replace it with libyuv for nothing.
    ///
    /// ```sh
    /// cargo test --release --lib video::tests::measure_the_encoder -- --ignored --nocapture
    /// ```
    ///
    /// The conversion column is still measured separately, for the case the paragraph
    /// above rules out today: if it ever
    /// does dominate a release encode, it is the thing to replace — and nothing else here
    /// would say so. Sweeping VP9's speed and thread settings means editing the constants
    /// at the top of `src/vp9.rs` and running this again; they are compile-time on
    /// purpose, since a deployment has no business setting them.
    #[test]
    #[ignore = "manual: measures the encoder and prints a table"]
    fn measure_the_encoder() {
        const FRAMES: u32 = 60;
        let sizes = [(1280u16, 800u16), (1920, 1080)];
        let qualities = [20u8, 40, 60, 80];

        println!(
            "\n| size      | quality | KB total | KB keyframe | µs/frame encode \
             | µs/frame convert | kbit/s at 30fps |"
        );
        println!(
            "|-----------|---------|----------|-------------|-----------------\
             |------------------|-----------------|"
        );
        for (w, h) in sizes {
            for quality in qualities {
                let mut mirror = Mirror::new(w, h).expect("a mirror");
                let mut stream = crate::vp9::Stream::new(mirror.rect(), mirror.coded(), quality)
                    .expect("a stream");
                let mut total = 0usize;
                let mut keyframe_bytes = 0usize;
                let mut encode = std::time::Duration::ZERO;
                for frame in 0..FRAMES {
                    mirror
                        .blit(mirror.rect(), &screen(w, h, frame))
                        .expect("a full-screen blit");
                    let started = std::time::Instant::now();
                    let unit = stream
                        .encode(&mirror, None)
                        .expect("an encode")
                        .expect("an access unit");
                    encode += started.elapsed();
                    total += unit.data.len();
                    if unit.keyframe {
                        keyframe_bytes += unit.data.len();
                    }
                }

                // The conversion on its own, over the same pixels: it is inside the
                // encode timing above, and this is what says how much of it it was.
                let mut yuv = I420::new(mirror.coded().0, mirror.coded().1);
                let mut crop = Vec::new();
                mirror.crop_into(mirror.rect(), &mut crop).expect("a crop");
                let started = std::time::Instant::now();
                for _ in 0..FRAMES {
                    yuv.read_rgb(&crop).expect("its own picture");
                }
                let convert = started.elapsed();

                let bits = total as f64 * 8.0;
                println!(
                    "| {:9} | {:7} | {:8} | {:11} | {:15} | {:16} | {:15.0} |",
                    format!("{w}x{h}"),
                    quality,
                    total / 1024,
                    keyframe_bytes / 1024,
                    encode.as_micros() / u128::from(FRAMES),
                    convert.as_micros() / u128::from(FRAMES),
                    bits / (f64::from(FRAMES) / 30.0) / 1000.0,
                );
            }
        }
        println!(
            "\n  {FRAMES} frames of synthetic screen content per row, one stream over the \
             whole desktop.\n  A row's keyframe column is its first frame; everything else \
             is inter-frame."
        );
        // Said in the output rather than only in the doc comment, because the numbers are
        // what gets pasted into a commit message or a doc, and a debug row's conversion
        // column is off by two orders of magnitude — see this test's doc comment.
        if cfg!(debug_assertions) {
            println!(
                "  ** debug build: the conversion column is Rust at opt-level 0 and is \
                 ~66x\n     slower than what ships. Re-run with --release before believing \
                 it.**\n"
            );
        } else {
            println!("  Release build, so the conversion column is the one that ships.\n");
        }
    }

    /// `w`×`h` of one colour, which makes "did these pixels land here" readable a byte
    /// at a time.
    fn flat(w: u16, h: u16, colour: [u8; 3]) -> Vec<u8> {
        colour
            .iter()
            .copied()
            .cycle()
            .take(usize::from(w) * usize::from(h) * 3)
            .collect()
    }

    #[test]
    fn a_blit_lands_where_the_rectangle_says() {
        let mut mirror = Mirror::new(640, 480).expect("a mirror");
        mirror
            .blit(rect(33, 41, 17, 9), &flat(17, 9, [200, 100, 50]))
            .expect("a blit inside the desktop");
        assert_eq!(mirror.pixel(33, 41), [200, 100, 50]);
        assert_eq!(mirror.pixel(49, 49), [200, 100, 50]);
        // One pixel outside each edge is still the black the mirror started as.
        assert_eq!(mirror.pixel(32, 41), [0, 0, 0]);
        assert_eq!(mirror.pixel(50, 41), [0, 0, 0]);
        assert_eq!(mirror.pixel(33, 40), [0, 0, 0]);
        assert_eq!(mirror.pixel(33, 50), [0, 0, 0]);
    }

    #[test]
    fn a_blit_outside_the_desktop_is_refused_rather_than_clipped() {
        let mut mirror = Mirror::new(640, 480).expect("a mirror");
        let too_far = mirror.blit(rect(630, 470, 20, 20), &flat(20, 20, [1, 2, 3]));
        assert!(too_far.is_err(), "a rectangle off the edge of the desktop was accepted");
        let wrong_size = mirror.blit(rect(0, 0, 16, 16), &flat(16, 15, [1, 2, 3]));
        assert!(wrong_size.is_err(), "pixels that are not the rectangle's were accepted");
    }

    #[test]
    fn a_crop_reads_the_rectangle_it_names() {
        let mut mirror = Mirror::new(640, 480).expect("a mirror");
        mirror
            .blit(rect(320, 64, 320, 64), &flat(320, 64, [9, 8, 7]))
            .expect("a cell-sized blit");
        let mut out = Vec::new();
        mirror.crop_into(rect(320, 64, 320, 64), &mut out).expect("a crop of that cell");
        assert_eq!(out, flat(320, 64, [9, 8, 7]));
        // Its neighbour is untouched, which is the same statement from the other side.
        mirror.crop_into(rect(0, 64, 320, 64), &mut out).expect("a crop of the cell beside it");
        assert_eq!(out, flat(320, 64, [0, 0, 0]));
        // A rectangle the mirror does not hold is refused rather than indexed.
        assert!(mirror.crop_into(rect(600, 400, 64, 128), &mut out).is_err());
    }

    #[test]
    fn the_pad_repeats_the_edge_rather_than_leaving_it_black() {
        let mut mirror = Mirror::new(1919, 1079).expect("an odd-sized mirror");
        mirror
            .blit(rect(0, 0, 1919, 1079), &flat(1919, 1079, [255, 255, 255]))
            .expect("a full-screen blit");
        mirror.pad_edges();
        assert_eq!(mirror.pixel(1919, 0), [255, 255, 255], "the pad column is a black seam");
        assert_eq!(mirror.pixel(0, 1079), [255, 255, 255], "the pad row is a black seam");
        assert_eq!(mirror.pixel(1919, 1079), [255, 255, 255], "the pad corner is black");
    }

    #[test]
    fn an_odd_desktop_is_held_at_even_sides_and_still_reports_its_true_size() {
        let mirror = Mirror::new(1919, 1079).expect("a mirror");
        assert_eq!(mirror.size(), (1919, 1079), "a client is told the real desktop");
        assert_eq!(mirror.coded(), (1920, 1080), "an encoder is given even sides");
        assert_eq!(mirror.len(), 1920 * 1080 * 3);
        assert_eq!(mirror.rect(), rect(0, 0, 1919, 1079), "a record header carries the true size");
    }

    /// The geometry theorem [`coded_rect`] rests on, from both sides: an interior region
    /// is even because the cell grid is, and a region at an odd desktop's edge finds the
    /// pad already there.
    #[test]
    fn a_region_at_the_desktops_odd_edge_grows_into_the_mirrors_pad() {
        let mirror = Mirror::new(1919, 1079).expect("an odd-sized mirror");
        // The bottom-right region of a 1919x1079 desktop: cells from (320*5, 64*16) to
        // the edge, so 319 wide and 55 tall — odd on both axes, and the only way a
        // region can be.
        let edge = Rect { left: 1600, top: 1024, right: 1918, bottom: 1078 };
        assert_eq!((edge.w() % 2, edge.h() % 2), (1, 1), "this test wants the odd case");
        let grown = coded_rect(edge, mirror.coded())
            .expect("a region at the edge of an odd desktop had nowhere to grow");
        assert_eq!((grown.w() % 2, grown.h() % 2), (0, 0), "it was not grown to even sides");
        // An interior region is even without help, whatever cells it covers.
        let inside = Rect { left: 320, top: 64, right: 959, bottom: 191 };
        assert_eq!((inside.w() % 2, inside.h() % 2), (0, 0));
        assert_eq!(coded_rect(inside, mirror.coded()).expect("an interior region"), inside);
        // The rule from both sides, at the very edge: a single-pixel-wide rectangle one
        // column short of the mirror's width grows into the padding and is fine, and the
        // same rectangle one column further right has nowhere to grow and is refused
        // rather than reaching an encoder's assertion.
        let ragged = Rect { left: 1918, top: 0, right: 1918, bottom: 63 };
        assert!(
            coded_rect(ragged, (1920, 1080)).is_ok(),
            "growing right is inside a padded mirror"
        );
        let past = Rect { left: 1919, top: 0, right: 1919, bottom: 63 };
        assert!(
            coded_rect(past, (1920, 1080)).is_err(),
            "a region with nowhere left to grow was accepted"
        );
    }

    #[test]
    fn a_picture_too_large_is_refused_by_name() {
        // Straight to `coded_rect` with the mirror's size rather than through a 44 MB
        // allocation: a mirror will hold whatever it is asked to, and the refusal being
        // tested is the encoders'.
        let Err(refused) = coded_rect(rect(0, 0, 5120, 2880), (5120, 2880)) else {
            panic!("a 5K picture was accepted");
        };
        let message = format!("{refused:#}");
        assert!(message.contains("5120x2880"), "the message does not say what was asked for");
        assert!(message.contains("3840"), "the message does not say what the limit is");
        assert!(message.contains("webp"), "the message does not say what to do instead");
        // The limit is on the picture, not on width: turning a legal desktop on its side
        // does not make it illegal.
        assert!(
            coded_rect(rect(0, 0, 2160, 3840), (2160, 3840)).is_ok(),
            "a portrait 4K desktop was refused"
        );
        assert!(Mirror::new(0, 1080).is_err(), "a desktop with no pixels was accepted");
    }

    #[test]
    fn a_conversion_refuses_a_crop_that_is_not_its_picture() {
        let mut i420 = I420::new(64, 32);
        i420.read_rgb(&flat(64, 32, [10, 20, 30])).expect("its own picture");
        assert!(
            i420.read_rgb(&flat(64, 31, [10, 20, 30])).is_err(),
            "a mis-sized crop would have indexed out of the planes"
        );
        // I420: full-size luma, quarter-size chroma, both tight.
        let (y, u, v) = i420.planes();
        assert_eq!((y.len(), u.len(), v.len()), (64 * 32, 32 * 16, 32 * 16));
        assert_eq!(i420.strides(), (64, 32, 32));
    }

    /// The conversion's arithmetic, at the points BT.601 studio swing pins exactly:
    /// black and white land on 16 and 235, every grey is chroma-neutral at 128, and
    /// a saturated red is the strongest V a swing this size has. The tolerance is one
    /// code value, which is the rounding the integer coefficients are allowed.
    #[test]
    fn the_conversion_is_bt601_studio_swing() {
        let mut i420 = I420::new(2, 2);
        let close = |got: u8, want: u8, what: &str| {
            assert!(got.abs_diff(want) <= 1, "{what}: got {got}, wanted {want}");
        };
        for (colour, y_want, u_want, v_want, name) in [
            ([0u8, 0, 0], 16u8, 128u8, 128u8, "black"),
            ([255, 255, 255], 235, 128, 128, "white"),
            ([128, 128, 128], 126, 128, 128, "mid grey"),
            ([255, 0, 0], 81, 90, 240, "red"),
            ([0, 0, 255], 41, 240, 110, "blue"),
        ] {
            i420.read_rgb(&flat(2, 2, colour)).expect("a 2x2 picture");
            let (y, u, v) = i420.planes();
            close(y[0], y_want, name);
            close(u[0], u_want, name);
            close(v[0], v_want, name);
        }
        // The chroma sample is the 2×2 average, not the top-left pixel: a checkerboard
        // of full red and full blue meets in the middle.
        let mut quad = Vec::new();
        quad.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
        quad.extend_from_slice(&[0, 0, 255, 255, 0, 0]);
        i420.read_rgb(&quad).expect("a 2x2 picture");
        let (_, u, v) = i420.planes();
        close(u[0], 165, "checkerboard U");
        close(v[0], 175, "checkerboard V");
    }
}
