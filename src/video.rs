//! What the video stream shares with the rest of the gateway: the framebuffer copy it
//! reads, the picture limits it encodes within, and the colour conversion in front of
//! it. [`crate::vp9`] knows only how to encode a picture; when a round is taken is
//! [`crate::encode`]'s business.
//!
//! Two things about the shape follow from the rest of the gateway rather than from
//! any codec:
//!
//! - **A frame may never be dropped.** [`crate::shadow::Shadow`] records source pixels
//!   as delivered the moment it accepts them, so a frame the encoder skipped is
//!   permanently wrong pixels — nothing re-sends it. Hence an empty bitstream leaves
//!   the caller's dirty flag alone instead of clearing it.
//! - **The stream is fed rectangles, not frames.** Damage arrives as rectangles, and
//!   VNC's can only be cropped out of the one it just decoded, so the [`Mirror`] holds
//!   the whole framebuffer and the stream encodes it when the engine says a frame has
//!   ended.
//!
//! A detach drops frames outright (`SessionManager::pump`), so a client coming back has
//! missed part of the stream and cannot decode from where it left off. It recovers
//! because every attach injects a repaint, which is one of the moments the stream's
//! keyframe is forced.

use crate::config::Chroma;
use crate::shadow::Rect;

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
/// 3840×2400 and 2400×3840 are both legal, and 3840×3840 is not. A `w <= 3840 &&
/// h <= 2400` test would read almost the same and would refuse a portrait desktop the
/// encoder is perfectly happy with.
///
/// libvpx has no ceiling this near — 4K is only VP9 level 5.0 of 6.2 — so this is the
/// gateway's own line: past 4K a software realtime encode of a desktop stops being
/// realtime. 4K is the *panel*, in
/// either shape: 3840×2160 is a 16:9 one and 3840×2400 the 16:10 one a 1920×1200
/// laptop at 2x is, 11% more pixels and one VP9 level up (6.0, which every browser's
/// decoder takes). 5K is past the line.
pub const MAX_LONG_SIDE: u16 = 3840;
/// See [`MAX_LONG_SIDE`].
pub const MAX_SHORT_SIDE: u16 = 2400;

/// Whether a picture of this many pixels is one a stream will encode — the test
/// [`check_picture`] refuses on, for a caller deciding what to ask a remote for.
pub fn within_ceiling((w, h): (u32, u32)) -> bool {
    let (long, short) = (w.max(h), w.min(h));
    long <= u32::from(MAX_LONG_SIDE) && short <= u32::from(MAX_SHORT_SIDE)
}

/// `pixels` held under the ceiling, the way High Performance holds a virtual
/// display under the Mac's: each axis clamped, the orientation kept, so a
/// landscape desktop is held to 3840×2400 and a portrait one to 2400×3840.
///
/// This is what lets a resizing engine ask the remote for a desktop the stream
/// will take instead of ending the session over the one it was about to get: a
/// 5120×2880 screen, or a 2560×1440 one at 2x, opens and resizes to a 3840×2400
/// desktop shown at 100% with the rest of the window left bare. Not a scale —
/// the gateway never shrinks a picture, every pixel the remote draws reaches the
/// browser as drawn — so the remote is the one asked for less. Per axis and not
/// by aspect, as the Mac's ceiling is: it keeps the most of the window.
pub fn fit_ceiling((w, h): (u32, u32)) -> (u32, u32) {
    let (long, short) = (u32::from(MAX_LONG_SIDE), u32::from(MAX_SHORT_SIDE));
    if w >= h {
        (w.min(long), h.min(short))
    } else {
        (w.min(short), h.min(long))
    }
}

/// One encoded frame: a complete access unit, and whether a decoder that has seen
/// nothing before it can start here.
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// The whole framebuffer as packed RGB888, and the copy the stream reads.
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

    /// `rect`'s pixels as packed RGB888, into a buffer the caller reuses — for the
    /// tests asserting what landed where. Bounded by [`Self::coded`], so it can read
    /// the padding too.
    #[cfg(test)]
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
    /// [`crate::encode`]'s round.
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

    /// The whole coded picture as one packed RGB888 slice — what the stream encodes.
    pub fn picture(&self) -> &[u8] {
        &self.rgb
    }

    /// Fill the at-most-one padding column and row from their neighbours.
    ///
    /// Only an odd-sized desktop has any. Called before every encode, because a blit
    /// can overwrite the edge the pad repeats.
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

/// Refuse a coded picture the encoder will not take.
///
/// The coded picture is the mirror's: the desktop grown to even sides. 4:2:0
/// subsamples chroma 2×2, so [`Yuv`] needs even sides there — and 4:4:4, which would
/// not, is held to the same ones: one geometry, not two. VP9 itself does not need
/// them either; the mirror's padding already supplies the column or row an odd
/// desktop is short of, and an odd-width chroma plane would be a second path to be
/// wrong in.
///
/// That cannot be a config-time refusal — only the remote knows its own size, and it
/// may change mid-session — so the message has to carry the whole explanation to
/// wherever it surfaces.
pub fn check_picture((w, h): (u16, u16)) -> anyhow::Result<()> {
    anyhow::ensure!(
        within_ceiling((u32::from(w), u32::from(h))),
        "a video stream will not encode a {w}x{h} picture: one is refused with a long \
         side over {MAX_LONG_SIDE} or a short side over {MAX_SHORT_SIDE}. Only the \
         remote knows its own size, so check-config cannot catch this — give this \
         target a remote that can be asked for a smaller desktop: with resize = true \
         the gateway holds every size it asks for under this ceiling"
    );
    Ok(())
}

/// How many threads the encoder gets: half the machine, capped. The stream has one
/// picture and nothing to overlap with, so its parallelism has to come from inside
/// the picture — VP9's row-based multithreading, set by the caller alongside this
/// count. The engine's read loop and the socket still need somewhere to run.
pub fn threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get() / 2).clamp(1, 4)
}

/// One picture as planar YUV, and the RGB→YUV conversion in front of the encoder.
///
/// Scalar integer BT.601 studio-swing arithmetic, owned here rather than a library's.
/// The chroma planes are one sample per pixel or one per 2×2 group averaged, as
/// [`Chroma`] says — the tight `(w, w, w)` I444 or `(w, w/2, w/2)` I420 layout libvpx
/// wraps without copying. The conversion's cost is measured separately in the encoder
/// bench, because if it ever dominates a release encode, *that* is the number that
/// would justify libyuv.
pub struct Yuv {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    size: (usize, usize),
    chroma: Chroma,
}

/// BT.601 studio-swing chroma for one colour, whether that colour is a pixel's own or
/// a 2×2 group's average. The arithmetic never leaves i16: the largest coefficient
/// sum is 112 × 255.
fn chroma_of(r: i16, g: i16, b: i16) -> (u8, u8) {
    let u = (((-38 * r - 74 * g + 112 * b) >> 8) + 128) as u8;
    let v = (((112 * r - 94 * g - 18 * b) >> 8) + 128) as u8;
    (u, v)
}

impl Yuv {
    /// A buffer for a `w`×`h` picture, both even.
    ///
    /// Reused across frames so a 1080p conversion is not a 3 MB allocation apiece.
    /// Even because the mirror is held at even sides, which is what keeps the 4:2:0
    /// chroma rows below made of whole 2×2 groups.
    pub fn new(w: u16, h: u16, chroma: Chroma) -> Self {
        let size = (usize::from(w), usize::from(h));
        let samples = match chroma {
            Chroma::Subsampled => size.0 * size.1 / 4,
            Chroma::Full => size.0 * size.1,
        };
        Self { y: vec![0; size.0 * size.1], u: vec![0; samples], v: vec![0; samples], size, chroma }
    }

    /// Which sampling this buffer holds, and so which libvpx image format wraps it.
    pub fn chroma(&self) -> Chroma {
        self.chroma
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
        for (pix, y) in rgb.as_chunks::<3>().0.iter().zip(self.y.iter_mut()) {
            *y = (((66 * u32::from(pix[0]) + 129 * u32::from(pix[1]) + 25 * u32::from(pix[2]))
                >> 8)
                + 16) as u8;
        }
        match self.chroma {
            Chroma::Full => {
                for (pix, (u, v)) in
                    rgb.as_chunks::<3>().0.iter().zip(self.u.iter_mut().zip(self.v.iter_mut()))
                {
                    (*u, *v) = chroma_of(i16::from(pix[0]), i16::from(pix[1]), i16::from(pix[2]));
                }
            }
            Chroma::Subsampled => {
                // One sample per 2×2 pixel group, from the group's average.
                let half = w / 2;
                let rows0 = rgb.chunks_exact(w * 3).step_by(2);
                let rows1 = rgb.chunks_exact(w * 3).skip(1).step_by(2);
                let u_rows = self.u.chunks_exact_mut(half);
                let v_rows = self.v.chunks_exact_mut(half);
                for (((row0, row1), u_row), v_row) in rows0.zip(rows1).zip(u_rows).zip(v_rows) {
                    for (((pix0, pix1), u), v) in
                        row0.as_chunks::<6>().0.iter().zip(row1.as_chunks::<6>().0).zip(u_row).zip(v_row)
                    {
                        let r = (i16::from(pix0[0]) + i16::from(pix0[3]) + i16::from(pix1[0]) + i16::from(pix1[3]) + 2) / 4;
                        let g = (i16::from(pix0[1]) + i16::from(pix0[4]) + i16::from(pix1[1]) + i16::from(pix1[4]) + 2) / 4;
                        let b = (i16::from(pix0[2]) + i16::from(pix0[5]) + i16::from(pix1[2]) + i16::from(pix1[5]) + 2) / 4;
                        (*u, *v) = chroma_of(r, g, b);
                    }
                }
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
        let chroma = match self.chroma {
            Chroma::Subsampled => self.size.0 / 2,
            Chroma::Full => self.size.0,
        };
        (self.size.0, chroma, chroma)
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
    /// `Q_FINEST`, `CPU_USED` and thread count, and what 4:4:4 costs over 4:2:0.
    ///
    /// `#[ignore]`d because it takes a minute and prints rather than asserts: the numbers
    /// are the output, and a threshold on them would be a test of this machine.
    ///
    /// **`--release`, and that is not the usual boilerplate — a debug run measures the
    /// wrong thing and says so convincingly.** The encoder is C and is optimized
    /// whatever this profile is: libvpx is compiled `-O3` once, into the archive
    /// `libvpx-prebuilt` publishes, and nothing a consumer does can touch it. The
    /// conversion is *Rust* — [`Yuv::read_rgb`] — so it is compiled with **this**
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
        let chromas = [Chroma::Subsampled, Chroma::Full];

        println!(
            "\n| size      | chroma | quality | KB total | KB keyframe | µs/frame encode \
             | µs/frame convert | kbit/s at 30fps |"
        );
        println!(
            "|-----------|--------|---------|----------|-------------|-----------------\
             |------------------|-----------------|"
        );
        for (w, h) in sizes {
            for (chroma, quality) in
                chromas.iter().flat_map(|c| qualities.iter().map(move |q| (*c, *q)))
            {
                let mut mirror = Mirror::new(w, h).expect("a mirror");
                let mut stream =
                    crate::vp9::Stream::new(mirror.coded(), quality, chroma)
                        .expect("a stream");
                let mut total = 0usize;
                let mut keyframe_bytes = 0usize;
                let mut encode = std::time::Duration::ZERO;
                for frame in 0..FRAMES {
                    mirror
                        .blit(rect(0, 0, w, h), &screen(w, h, frame))
                        .expect("a full-screen blit");
                    let started = std::time::Instant::now();
                    let unit = stream
                        .encode(&mirror)
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
                let mut yuv = Yuv::new(mirror.coded().0, mirror.coded().1, chroma);
                let crop = mirror.picture().to_vec();
                let started = std::time::Instant::now();
                for _ in 0..FRAMES {
                    yuv.read_rgb(&crop).expect("its own picture");
                }
                let convert = started.elapsed();

                let bits = total as f64 * 8.0;
                println!(
                    "| {:9} | {:6} | {:7} | {:8} | {:11} | {:15} | {:16} | {:15.0} |",
                    format!("{w}x{h}"),
                    chroma.name(),
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
            .expect("a blit");
        let mut out = Vec::new();
        mirror.crop_into(rect(320, 64, 320, 64), &mut out).expect("a crop of that rectangle");
        assert_eq!(out, flat(320, 64, [9, 8, 7]));
        // Its neighbour is untouched, which is the same statement from the other side.
        mirror.crop_into(rect(0, 64, 320, 64), &mut out).expect("a crop of the rectangle beside it");
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
    }

    #[test]
    fn a_picture_too_large_is_refused_by_name() {
        // Straight to `check_picture` rather than through a 44 MB allocation: a mirror
        // will hold whatever it is asked to, and the refusal being tested is the
        // encoder's.
        let Err(refused) = check_picture((5120, 2880)) else {
            panic!("a 5K picture was accepted");
        };
        let message = format!("{refused:#}");
        assert!(message.contains("5120x2880"), "the message does not say what was asked for");
        assert!(message.contains("3840"), "the message does not say what the limit is");
        assert!(message.contains("resize"), "the message does not say what to do instead");
        // Both 4K panels are pictures: the 16:9 one and the 16:10 one a 1920×1200 laptop
        // is at 2x.
        assert!(check_picture((3840, 2160)).is_ok(), "16:9 4K was refused");
        assert!(check_picture((3840, 2400)).is_ok(), "16:10 4K was refused");
        // The limit is on the picture, not on width: turning a legal desktop on its side
        // does not make it illegal.
        assert!(
            check_picture((2400, 3840)).is_ok(),
            "a portrait 4K desktop was refused"
        );
        assert!(Mirror::new(0, 1080).is_err(), "a desktop with no pixels was accepted");
    }

    /// The size a resizing engine asks for instead: each axis held, the
    /// orientation kept, and anything already inside the ceiling untouched.
    #[test]
    fn a_desktop_is_held_under_the_ceiling_per_axis() {
        assert_eq!(fit_ceiling((5120, 2880)), (3840, 2400), "a 5K screen");
        assert_eq!(fit_ceiling((2880, 5120)), (2400, 3840), "the same screen on its side");
        assert_eq!(fit_ceiling((1920, 1200)), (1920, 1200), "a desktop the stream takes");
        assert_eq!(fit_ceiling((3840, 2160)), (3840, 2160), "16:9 4K is under the ceiling");
        assert_eq!(fit_ceiling((3840, 2400)), (3840, 2400), "the ceiling itself");
        for size in [(5120, 2880), (2880, 5120), (4000, 4000), (3840, 2401)] {
            assert!(!within_ceiling(size), "{size:?} is over the ceiling");
            let held = fit_ceiling(size);
            assert!(within_ceiling(held), "{size:?} held to {held:?} is still over it");
            assert!(
                check_picture((held.0 as u16, held.1 as u16)).is_ok(),
                "the encoder refuses the {held:?} the engine was told to ask for"
            );
        }
    }

    #[test]
    fn a_conversion_refuses_a_crop_that_is_not_its_picture() {
        let mut i420 = Yuv::new(64, 32, Chroma::Subsampled);
        i420.read_rgb(&flat(64, 32, [10, 20, 30])).expect("its own picture");
        assert!(
            i420.read_rgb(&flat(64, 31, [10, 20, 30])).is_err(),
            "a mis-sized crop would have indexed out of the planes"
        );
        // I420: full-size luma, quarter-size chroma, both tight.
        let (y, u, v) = i420.planes();
        assert_eq!((y.len(), u.len(), v.len()), (64 * 32, 32 * 16, 32 * 16));
        assert_eq!(i420.strides(), (64, 32, 32));
        // I444: three full-size planes.
        let mut i444 = Yuv::new(64, 32, Chroma::Full);
        i444.read_rgb(&flat(64, 32, [10, 20, 30])).expect("its own picture");
        assert!(i444.read_rgb(&flat(64, 31, [10, 20, 30])).is_err());
        let (y, u, v) = i444.planes();
        assert_eq!((y.len(), u.len(), v.len()), (64 * 32, 64 * 32, 64 * 32));
        assert_eq!(i444.strides(), (64, 64, 64));
    }

    /// The conversion's arithmetic, at the points BT.601 studio swing pins exactly:
    /// black and white land on 16 and 235, every grey is chroma-neutral at 128, and
    /// a saturated red is the strongest V a swing this size has. The tolerance is one
    /// code value, which is the rounding the integer coefficients are allowed.
    #[test]
    fn the_conversion_is_bt601_studio_swing() {
        let mut i420 = Yuv::new(2, 2, Chroma::Subsampled);
        let mut i444 = Yuv::new(2, 2, Chroma::Full);
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
            for yuv in [&mut i420, &mut i444] {
                yuv.read_rgb(&flat(2, 2, colour)).expect("a 2x2 picture");
                let (y, u, v) = yuv.planes();
                close(y[0], y_want, name);
                close(u[0], u_want, name);
                close(v[0], v_want, name);
            }
        }
        // At 4:2:0 the chroma sample is the 2×2 average, not the top-left pixel: a
        // checkerboard of full red and full blue meets in the middle. At 4:4:4 each
        // pixel keeps its own.
        let mut quad = Vec::new();
        quad.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
        quad.extend_from_slice(&[0, 0, 255, 255, 0, 0]);
        i420.read_rgb(&quad).expect("a 2x2 picture");
        let (_, u, v) = i420.planes();
        close(u[0], 165, "checkerboard U");
        close(v[0], 175, "checkerboard V");
        i444.read_rgb(&quad).expect("a 2x2 picture");
        let (_, u, v) = i444.planes();
        close(u[0], 90, "red pixel U");
        close(v[0], 240, "red pixel V");
        close(u[1], 240, "blue pixel U");
        close(v[1], 110, "blue pixel V");
    }
}
