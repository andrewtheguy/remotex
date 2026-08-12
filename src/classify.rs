//! The picture classifier behind `render_subtype = "classify"`: per tile, is
//! this photographic content that JPEG compresses well, or flat UI and text
//! that PNG keeps small *and* sharp?
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

/// Pixels below which a tile is never worth a JPEG: at this size the payload
/// saving cannot repay JPEG's own header and table overhead, and small tiles
/// are overwhelmingly cursors, carets and widget slivers — exactly the sharp
/// content the lossy arm mistreats.
const MIN_PHOTO_PIXELS: usize = 4096;

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

/// Whether a `w`×`h` tile of packed RGB888 reads as photographic — the tile
/// JPEG should carry. `false` is the safe answer and every malformed or
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
    for px in rgb.chunks_exact(3) {
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

    /// Below the size floor nothing is photographic, however smooth.
    #[test]
    fn a_small_tile_is_never_photographic() {
        let (w, h) = (32u16, 32u16);
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
}
