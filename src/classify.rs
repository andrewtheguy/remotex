//! The picture classifier behind `render_subtype = "classify"`: per tile, is
//! this photographic content that a lossy still compresses well, or flat UI and
//! text that PNG keeps small *and* sharp?
//!
//! Which lossy still that is — JPEG or WebP, `render_classify_lossy` — is not a
//! question for this module. It answers what the pixels are; the encoder the
//! answer is carried out by is the operator's, and every threshold below was
//! measured against JPEG, which of the two is the harder to break even on.
//!
//! The question is answered from the pixels alone, per tile, on the encode
//! worker — nothing upstream carries state for it, so two tiles of one frame
//! can answer differently and a window dragged across the screen re-answers
//! wherever it lands. PNG is the safe verdict everywhere: a photo sent
//! lossless costs bytes, while text sent lossy costs legibility for as long
//! as the region stays still, and JPEG's ringing around glyph edges is the
//! artifact this subtype exists to avoid.
//!
//! Two measurements, in the order they are cheap to refuse on:
//!
//! 1. **Palette size.** Flat UI lives in a few colours however large the
//!    region; a photograph's colour count grows with its area. A tile that
//!    stays at or under [`FLAT_COLORS`] distinct colours is UI, full stop —
//!    this is the test Tight encoders have used to pick palette encodings
//!    over JPEG for two decades.
//! 2. **How its neighbours differ.** Past the palette gate the tile is
//!    colourful, but antialiased text and gradient-heavy chrome are colourful
//!    too. What separates a photograph is *locality*: adjacent pixels differ a
//!    little, almost everywhere. Text is the opposite — flat runs, then a
//!    glyph edge crossed in one or two steps of large delta. So transitions
//!    are counted into soft (small, nonzero) and hard (large), and only a
//!    tile whose soft transitions outnumber hard ones [`SOFT_PER_HARD`]-fold
//!    reads as photographic.

/// Pixels below which a tile is never offered to the lossy encode: the break-even
/// for JPEG's own header and table bytes, weighed rather than assumed. One floor
/// serves both lossy stills because JPEG is the one with a break-even to find —
/// measured on the same ladder, WebP's container costs so little that it is a tenth
/// of the PNG at every size on it, 16×16 included, so a floor set where JPEG turns
/// over is simply well clear of where WebP does.
///
/// [`tests::weigh_the_size_floor`] carries all three encodings across the size ladder. A
/// tile this classifier admits costs a third of its PNG at 32×32 and half at 24×24,
/// while at 16×16 the ~640 bytes of tables win and JPEG loses outright; real tiles
/// agree, a 9×10 caret being 261 bytes as PNG against 683 as JPEG. Below 257 pixels
/// the palette gate could not admit anything anyway, wanting more colours than the
/// tile has pixels, so a floor under that would be inert.
///
/// Pixels rather than points, because table bytes are bytes and do not care how much
/// screen a pixel covers. A floor in pixels therefore lapses as density rises, and
/// the value here is what that lapse taught. It was 4096 while it doubled as a guard
/// against sending small sharp furniture lossy — but measured on a wlshare desktop
/// driven beside the gateway, a 60×24-point gradient chip repainting on its own was
/// refused 117 times out of 117 at 1×, for 378 KB of PNG, and admitted 118 out of
/// 118 at 2×, for 145 KB of JPEG. The guard was doing nothing at the density where
/// furniture is largest and charging for it at the density where it is smallest. The
/// sharp content it was meant to catch is refused by the transition test, which is
/// the measurement that actually looks for edges. That chip had to be built to raise
/// the question at all: across three 2× sessions of scrolled photographs, terminal
/// glyphs and chrome, streams and cleanups live, no admitted tile fell under a
/// point-scaled floor, real damage boxes being wide.
///
/// Nothing here follows `CELL_POINTS`. That the old value equalled a 64×64 cell
/// exactly was a coincidence of two unrelated choices; this one sits where the bytes
/// turn over.
const MIN_PHOTO_PIXELS: usize = 1024;

/// Distinct colours at or below which a tile reads as flat UI outright.
/// One byte's worth: the palette a Tight encoder would have indexed.
const FLAT_COLORS: usize = 256;

/// Per-channel neighbour delta at or below which a transition is a gradient
/// step rather than an edge. Photographs and smooth gradients live under it;
/// a glyph edge, even antialiased, crosses far more per step.
const SOFT_DELTA: u8 = 24;

/// How many soft transitions it takes to outweigh one hard edge. Photographs
/// carry edges too — a horizon, a window frame in a photo — so the demand is
/// dominance rather than absence: gradient-like change nearly everywhere,
/// sharp change rarely.
const SOFT_PER_HARD: u64 = 4;

/// Whether a `w`×`h` tile of packed RGB888 reads as photographic — the tile the
/// lossy still should carry. `false` is the safe answer and every malformed or
/// borderline input gets it: the caller then encodes PNG, which is never
/// wrong, only bigger.
pub fn photographic(w: u16, h: u16, rgb: &[u8]) -> bool {
    let (w, h) = (usize::from(w), usize::from(h));
    if w * h < MIN_PHOTO_PIXELS || rgb.len() != w * h * 3 {
        return false;
    }
    if !colorful(rgb) {
        return false;
    }
    let (mut soft, mut hard) = (0u64, 0u64);
    for row in rgb.chunks_exact(w * 3) {
        for pair in row.windows(6).step_by(3) {
            let delta = pair[..3]
                .iter()
                .zip(&pair[3..])
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            if delta == 0 {
                // A flat run says nothing about which kind of picture this is;
                // both kinds have them, and counting them would let a mostly
                // flat tile drown out its own edges.
            } else if delta <= SOFT_DELTA {
                soft += 1;
            } else {
                hard += 1;
            }
        }
    }
    soft > hard.saturating_mul(SOFT_PER_HARD)
}

/// Whether the tile holds more than [`FLAT_COLORS`] distinct colours, giving
/// up the count as soon as the answer is known.
fn colorful(rgb: &[u8]) -> bool {
    let mut seen = std::collections::HashSet::with_capacity(FLAT_COLORS + 1);
    for px in rgb.as_chunks::<3>().0 {
        if seen.insert(u32::from(px[0]) << 16 | u32::from(px[1]) << 8 | u32::from(px[2]))
            && seen.len() > FLAT_COLORS
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tile comfortably over [`MIN_PHOTO_PIXELS`].
    const W: u16 = 128;
    const H: u16 = 64;

    fn tile(pixel: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(usize::from(W) * usize::from(H) * 3);
        for y in 0..usize::from(H) {
            for x in 0..usize::from(W) {
                rgb.extend_from_slice(&pixel(x, y));
            }
        }
        rgb
    }

    #[test]
    fn a_solid_tile_is_not_photographic() {
        assert!(!photographic(W, H, &tile(|_, _| [200, 200, 200])));
    }

    /// Sharp two-colour text stays lossless however much of it there is.
    #[test]
    fn text_like_two_colour_content_is_not_photographic() {
        let rgb = tile(|x, y| if (x / 3 + y / 7) % 2 == 0 { [0, 0, 0] } else { [255; 3] });
        assert!(!photographic(W, H, &rgb));
    }

    /// A smooth two-axis gradient: thousands of colours, every transition
    /// gentle. This is the wallpaper PNG bloats on and JPEG was built for.
    #[test]
    fn a_smooth_gradient_is_photographic() {
        let rgb = tile(|x, y| [(x * 2) as u8, (y * 4) as u8, ((x + y) * 2) as u8]);
        assert!(photographic(W, H, &rgb));
    }

    /// A photo stand-in: smooth waves, locally continuous everywhere, with
    /// colour variety far past any palette.
    #[test]
    fn wave_shaded_content_is_photographic() {
        let rgb = tile(|x, y| {
            let (x, y) = (x as f32, y as f32);
            [
                (128.0 + 90.0 * (x / 11.0).sin() * (y / 7.0).cos()) as u8,
                (128.0 + 90.0 * (x / 17.0 + y / 13.0).sin()) as u8,
                (128.0 + 90.0 * (y / 9.0).sin()) as u8,
            ]
        });
        assert!(photographic(W, H, &rgb));
    }

    /// Colourful but hard-edged: a mosaic of flat swatches, the shape of a
    /// syntax-highlighted editor or a colour picker. Over the palette cap yet
    /// every transition is an edge, so it stays lossless.
    #[test]
    fn a_mosaic_of_flat_swatches_is_not_photographic() {
        let rgb = tile(|x, y| {
            let cell = (x / 4) * 31 + (y / 4) * 17;
            [(cell % 251) as u8, (cell % 241) as u8, (cell % 233) as u8]
        });
        assert!(!photographic(W, H, &rgb));
    }

    /// Below the size floor nothing is photographic, however smooth — and below it
    /// the palette gate could not pass a tile either, wanting more colours than
    /// this one has pixels.
    #[test]
    fn a_small_tile_is_never_photographic() {
        let (w, h) = (16u16, 16u16);
        let mut rgb = Vec::new();
        for y in 0..usize::from(h) {
            for x in 0..usize::from(w) {
                rgb.extend_from_slice(&[(x * 8) as u8, (y * 8) as u8, ((x + y) * 4) as u8]);
            }
        }
        assert!(!photographic(w, h, &rgb));
    }

    /// A malformed payload takes the safe verdict rather than a guess.
    #[test]
    fn a_mismatched_payload_is_not_photographic() {
        assert!(!photographic(W, H, &[0u8; 17]));
    }

    /// What the floor is protecting, weighed: for content the classifier admits,
    /// how each lossy still compares with the PNG it replaces at each size the grid
    /// hands the encoder. Both are here because the floor is one number for both,
    /// and it is JPEG's tables that set it: WebP breaks even far below the smallest
    /// size on this ladder, so the shared floor never costs it anything. Run with
    ///   cargo test --release --lib classify::tests::weigh_the_size_floor -- --ignored --nocapture
    #[test]
    #[ignore]
    fn weigh_the_size_floor() {
        use crate::protocol::{JpegSampling, Tile};

        // Smooth and detailed: the photograph the lossy arm exists for.
        let photo = |w: usize, h: usize| -> Vec<u8> {
            let mut rgb = Vec::with_capacity(w * h * 3);
            for y in 0..h {
                for x in 0..w {
                    let (fx, fy) = (x as f32, y as f32);
                    let n = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503) >> 3) as u8 % 13;
                    rgb.extend_from_slice(&[
                        (110.0 + 80.0 * (fx / 11.0).sin() * (fy / 7.0).cos()) as u8 + n,
                        (110.0 + 80.0 * (fx / 17.0 + fy / 13.0).sin()) as u8 + n,
                        (110.0 + 80.0 * (fy / 9.0).sin()) as u8 + n,
                    ]);
                }
            }
            rgb
        };
        // Smooth chrome: colourful enough to pass the palette gate, gradient
        // enough to pass the transition test, and the thing PNG carries well.
        let gradient = |w: usize, h: usize| -> Vec<u8> {
            let mut rgb = Vec::with_capacity(w * h * 3);
            for y in 0..h {
                for x in 0..w {
                    rgb.extend_from_slice(&[
                        (40 + x * 3 % 200) as u8,
                        (60 + y * 3 % 180) as u8,
                        (90 + (x + y) * 2 % 160) as u8,
                    ]);
                }
            }
            rgb
        };

        println!(
            "\n  {:>9} | {:>6} | {:>5} | {:>7} | {:>19} | {:>19} | {:>19}",
            "content",
            "size",
            "px",
            "png B",
            "jpeg 70 B (of png)",
            "jpeg 90 B (of png)",
            "webp 70 B (of png)"
        );
        for (name, make) in [("photo", &photo as &dyn Fn(usize, usize) -> Vec<u8>), ("gradient", &gradient)] {
            for (w, h) in [(16u16, 16u16), (24, 24), (32, 32), (48, 48), (64, 56), (64, 64), (128, 64), (128, 128), (320, 64)] {
                let rgb = make(usize::from(w), usize::from(h));
                let admitted = photographic(w, h, &rgb);
                let png = Tile::from_rgb(0, 0, w, h, &rgb).unwrap().data.len();
                let j70 = Tile::from_rgb_jpeg(0, 0, w, h, &rgb, 70, JpegSampling::for_quality(70))
                    .unwrap()
                    .data
                    .len();
                let j90 = Tile::from_rgb_jpeg(0, 0, w, h, &rgb, 90, JpegSampling::for_quality(90))
                    .unwrap()
                    .data
                    .len();
                let w70 = Tile::from_rgb_webp(0, 0, w, h, &rgb, 70).unwrap().data.len();
                println!(
                    "  {:>9} | {:>6} | {:>5} | {:>7} | {:>9} ({:>5.0}%) | {:>9} ({:>5.0}%) | \
                     {:>9} ({:>5.0}%){}",
                    name,
                    format!("{w}x{h}"),
                    usize::from(w) * usize::from(h),
                    png,
                    j70,
                    100.0 * j70 as f64 / png as f64,
                    j90,
                    100.0 * j90 as f64 / png as f64,
                    w70,
                    100.0 * w70 as f64 / png as f64,
                    if admitted { "" } else { "  [refused]" },
                );
            }
        }
        println!(
            "\n  [refused] is the classifier's own verdict, floor included: a row without it \
             is a tile that would go out at the target's lossy still today.\n"
        );
    }
}
