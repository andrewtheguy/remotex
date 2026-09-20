//! The picture classifier behind `render_subtype = "classify"`: per tile, is
//! this photographic content that WebP compresses well, or flat UI and
//! text that PNG keeps small *and* sharp?
//!
//! It answers what the pixels are, not what they cost: the two thresholds
//! below were measured when JPEG was the lossy still beside WebP, and JPEG is
//! the harder of the two to break even on, so both are a conservative reading
//! for the encoder that remains. A third stood in front of them — a minimum
//! tile size, placed where JPEG's tables broke even — and does not survive the
//! same question. WebP has no tables, and on two recorded Windows sessions
//! ([`tests::weigh_a_tape_by_tile_size`]) every tile the floor alone refused was
//! one WebP would have sent for a tenth of its PNG. It is gone, and the palette
//! gate is the size gate: a tile cannot hold more colours than it has pixels.
//!
//! The question is answered from the pixels alone, per tile, on the encode
//! worker — nothing upstream carries state for it, so two tiles of one frame
//! can answer differently and a window dragged across the screen re-answers
//! wherever it lands. PNG is the safe verdict everywhere: a photo sent
//! lossless costs bytes, while text sent lossy costs legibility for as long
//! as the region stays still, and the softening of glyph edges is the
//! artifact this subtype exists to avoid.
//!
//! Two measurements, in the order they are cheap to refuse on:
//!
//! 1. **Palette size.** Flat UI lives in a few colours however large the
//!    region; a photograph's colour count grows with its area. A tile that
//!    stays at or under [`FLAT_COLORS`] distinct colours is UI, full stop —
//!    this is the test Tight encoders have used to pick palette encodings
//!    over a lossy still for two decades.
//! 2. **How its neighbours differ.** Past the palette gate the tile is
//!    colourful, but antialiased text and gradient-heavy chrome are colourful
//!    too. What separates a photograph is *locality*: adjacent pixels differ a
//!    little, almost everywhere. Text is the opposite — flat runs, then a
//!    glyph edge crossed in one or two steps of large delta. So transitions
//!    are counted into soft (small, nonzero) and hard (large), and only a
//!    tile whose soft transitions outnumber hard ones [`SOFT_PER_HARD`]-fold
//!    reads as photographic.

/// Distinct colours at or below which a tile reads as flat UI outright.
/// One byte's worth: the palette a Tight encoder would have indexed.
///
/// It is the size gate as well, and the only one. A tile holds at most one
/// colour per pixel, so nothing at or under this many pixels can pass here
/// however photographic it looks — a 16×16 cell is refused on arithmetic. A
/// separate pixel floor stood in front of this for as long as JPEG's tables
/// gave it a break-even to sit at; with WebP alone it refused tiles worth
/// sending and nothing worth refusing, and it is gone.
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
    if rgb.len() != w * h * 3 {
        return false;
    }
    colorful(rgb) && gradientish(w, rgb)
}

/// Whether horizontal neighbour deltas are dominated by gradient-sized steps —
/// the transition test, apart from the palette gate and the size floor that
/// [`photographic`] applies first. Its own function so a measurement can ask what
/// the floor is actually refusing; nothing else calls it alone.
fn gradientish(w: usize, rgb: &[u8]) -> bool {
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

    /// A tile with room for either kind of content's palette.
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
    /// gentle. This is the wallpaper PNG bloats on and a lossy still was built for.
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

    /// A tile smaller than the palette it would need is refused on arithmetic,
    /// however smooth it is: 256 pixels cannot carry more than [`FLAT_COLORS`]
    /// colours, and this is why no separate size floor is needed. One row more of
    /// the same gradient and the same content is admitted — the smallest tile that
    /// can be, and measurably smaller as WebP.
    #[test]
    fn a_small_tile_is_never_photographic() {
        let (w, h) = (16u16, 16u16);
        let row = |y: usize| -> Vec<u8> {
            (0..usize::from(w))
                .flat_map(|x| [(x * 8) as u8, (y * 8) as u8, ((x + y) * 4) as u8])
                .collect()
        };
        let mut rgb: Vec<u8> = (0..usize::from(h)).flat_map(row).collect();
        assert!(!photographic(w, h, &rgb));

        rgb.extend(row(usize::from(h)));
        assert!(photographic(w, h + 1, &rgb));
    }

    /// A malformed payload takes the safe verdict rather than a guess.
    #[test]
    fn a_mismatched_payload_is_not_photographic() {
        assert!(!photographic(W, H, &[0u8; 17]));
    }

    /// For content the classifier admits, how WebP compares with the PNG it
    /// replaces at each size the grid hands the encoder — synthetic content on a
    /// size ladder, where [`weigh_a_tape_by_tile_size`] asks the same question of a
    /// recorded desktop. Run with
    ///   cargo test --release --lib classify::tests::weigh_the_size_ladder -- --ignored --nocapture
    #[test]
    #[ignore]
    fn weigh_the_size_ladder() {
        use crate::protocol::Tile;

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
            "\n  {:>9} | {:>6} | {:>5} | {:>7} | {:>19} | {:>19}",
            "content", "size", "px", "png B", "webp 70 B (of png)", "webp 90 B (of png)"
        );
        for (name, make) in [("photo", &photo as &dyn Fn(usize, usize) -> Vec<u8>), ("gradient", &gradient)] {
            for (w, h) in [(16u16, 16u16), (24, 24), (32, 32), (48, 48), (64, 56), (64, 64), (128, 64), (128, 128), (320, 64)] {
                let rgb = make(usize::from(w), usize::from(h));
                let admitted = photographic(w, h, &rgb);
                let png = Tile::from_rgb(0, 0, w, h, &rgb).unwrap().data.len();
                let w70 = Tile::from_rgb_webp(0, 0, w, h, &rgb, 70).unwrap().data.len();
                let w90 = Tile::from_rgb_webp(0, 0, w, h, &rgb, 90).unwrap().data.len();
                println!(
                    "  {:>9} | {:>6} | {:>5} | {:>7} | {:>9} ({:>5.0}%) | {:>9} ({:>5.0}%){}",
                    name,
                    format!("{w}x{h}"),
                    usize::from(w) * usize::from(h),
                    png,
                    w70,
                    100.0 * w70 as f64 / png as f64,
                    w90,
                    100.0 * w90 as f64 / png as f64,
                    if admitted { "" } else { "  [refused]" },
                );
            }
        }
        println!(
            "\n  [refused] is the classifier's own verdict, floor included: a row without it \
             is a tile that would go out as WebP today.\n"
        );
    }

    /// What the classifier admits on a recorded desktop, by tile size, weighed as
    /// PNG against WebP: every piece a real session put on the wire, cut the way the
    /// tile path cuts it.
    ///
    /// This is the instrument that removed the pixel floor. That floor was the one
    /// threshold whose justification was a byte count rather than a picture, and the
    /// count had been taken against JPEG's tables; WebP has none to amortise. Two
    /// ten-second Windows RDP sessions at 1×, paging through a page of photographs,
    /// answered it. Nothing at all was admitted under 256 pixels — the palette gate
    /// cannot pass a tile with fewer colours than that, so a floor under it would be
    /// inert. Between 256 and the old floor of 1,024 it admitted 34 bands of 49 KB
    /// of PNG, 4.8 KB of them as WebP at quality 70, and — the same session read as
    /// single cells — 265 pieces of 393 KB, 37 KB as WebP; the second session found
    /// 78 and 170. Not one admitted piece, at any size in either session, came back
    /// larger as WebP. 1× because that is where a floor in pixels bites hardest and
    /// what this host renders at; it would not take 200%.
    ///
    /// Reads a damage tape ([`crate::tape`]), recorded by a `render_motion` session
    /// with the PNG base:
    ///
    /// ```text
    /// REMOTEX_MOTION_TAPE=tmp/floor.tape target/release/remotex serve -c tmp/test_uat.toml
    /// REMOTEX_MOTION_TAPE=tmp/floor.tape \
    ///   cargo test --release --lib classify::tests::weigh_a_tape_by_tile_size \
    ///   -- --ignored --nocapture
    /// ```
    ///
    /// Two populations, because the cut depends on what else is happening. **Bands**
    /// are what a still target sends: [`Rect::bands`] of the damage box, wide by
    /// construction. **Cells** are the smallest piece the motion path can send — one
    /// changed cell, alone between two live streams — and so the population the floor
    /// can actually reach. Both come out of the same records; they are two readings of
    /// one session, not two sessions, and their bytes must not be added together.
    #[test]
    #[ignore = "manual: weighs a damage tape's tiles by size, PNG against WebP"]
    fn weigh_a_tape_by_tile_size() {
        use crate::protocol::{Tile, TileGrid};
        use crate::tape::Record;
        use crate::tiles::Rect;

        let path =
            std::env::var_os(crate::tape::ENV).expect("REMOTEX_MOTION_TAPE names the tape to weigh");
        let (_, records) = crate::tape::read(&path).expect("a readable tape");

        /// Pixel counts a bucket ends at, the last one being everything above.
        /// 256 is where the palette gate can first answer at all and 1,024 is where
        /// the removed floor stood, so the second row is what it used to refuse.
        const BUCKETS: [usize; 4] = [256, 1024, 4096, usize::MAX];

        #[derive(Default, Clone, Copy)]
        struct Bucket {
            pieces: u64,
            admitted: u64,
            png: u64,
            webp70: u64,
            webp90: u64,
            /// Admitted pieces WebP at 70 did not shrink. The floor's whole case.
            webp_lost: u64,
        }

        let mut tally = [[Bucket::default(); BUCKETS.len()]; 2];
        let mut grid = TileGrid::ONE;
        let mut cut = false;
        for record in &records {
            match record {
                Record::Resize { scale, .. } => grid = TileGrid::at(*scale),
                Record::Cut { .. } => cut = true,
                Record::Frame { .. } => {}
                Record::Damage { rect, cells, rgb, .. } => {
                    let stride = usize::from(rect.w()) * 3;
                    let crop = |piece: Rect| -> Vec<u8> {
                        let x0 = usize::from(piece.left - rect.left) * 3;
                        let width = usize::from(piece.w()) * 3;
                        (piece.top..=piece.bottom)
                            .flat_map(|y| {
                                let row = usize::from(y - rect.top) * stride;
                                rgb[row + x0..row + x0 + width].iter().copied()
                            })
                            .collect()
                    };
                    let mut weigh = |population: usize, piece: Rect| {
                        let (w, h) = (piece.w(), piece.h());
                        let pixels = usize::from(w) * usize::from(h);
                        let bucket = BUCKETS.iter().position(|end| pixels < *end).unwrap_or(0);
                        let b = &mut tally[population][bucket];
                        b.pieces += 1;
                        let rgb = crop(piece);
                        if !(colorful(&rgb) && gradientish(usize::from(w), &rgb)) {
                            return;
                        }
                        let len = |tile: anyhow::Result<Tile>| tile.expect("an encode of a piece").data.len() as u64;
                        let png = len(Tile::from_rgb(0, 0, w, h, &rgb));
                        let webp70 = len(Tile::from_rgb_webp(0, 0, w, h, &rgb, 70));
                        b.admitted += 1;
                        b.png += png;
                        b.webp70 += webp70;
                        b.webp90 += len(Tile::from_rgb_webp(0, 0, w, h, &rgb, 90));
                        b.webp_lost += u64::from(webp70 >= png);
                    };
                    for band in rect.bands() {
                        weigh(0, band);
                        for cell in band.cells(grid) {
                            if cells.contains(&cell.cell_key(grid)) {
                                weigh(1, cell);
                            }
                        }
                    }
                }
            }
        }

        if cut {
            println!("\n  ** the recorder fell behind and cut the tape: this is its head, not the session. **");
        }
        if cfg!(debug_assertions) {
            println!("\n  ** debug build: re-run with --release before believing the bytes. **");
        }
        for (population, name) in [(0, "bands (a still target's cut)"), (1, "cells (the motion path's smallest)")] {
            println!(
                "\n  {name}\n  {:>13} | {:>7} | {:>9} | {:>9} | {:>19} | {:>19} | {:>10}",
                "pixels", "pieces", "admitted", "png B", "webp 70 B (of png)", "webp 90 B (of png)", "webp loses"
            );
            let mut from = 0;
            for (bucket, end) in BUCKETS.iter().enumerate() {
                let b = tally[population][bucket];
                let pct = |v: u64| if b.png == 0 { 0.0 } else { 100.0 * v as f64 / b.png as f64 };
                let range = if *end == usize::MAX {
                    format!("{from}+")
                } else {
                    format!("{from}..{end}")
                };
                println!(
                    "  {:>13} | {:>7} | {:>9} | {:>9} | {:>9} ({:>5.0}%) | {:>9} ({:>5.0}%) | {:>10}{}",
                    range, b.pieces, b.admitted, b.png, b.webp70, pct(b.webp70), b.webp90, pct(b.webp90), b.webp_lost,
                    if *end <= 1024 { "  [the removed floor refused these]" } else { "" },
                );
                from = *end;
            }
        }
        println!(
            "\n  \"admitted\" is the palette gate and the transition test — every gate there is. \
             \"webp loses\" counts the admitted pieces WebP at 70 did not shrink, and a row of it \
             is the measurement that would put a size floor back.\n"
        );
    }
}
