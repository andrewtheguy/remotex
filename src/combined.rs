//! A Standard Mac's Combined Display, composed here: every screen at once, as Apple's
//! viewer shows them ([`crate::config::StandardView::Combined`]).
//!
//! The Mac sends that view as one framebuffer holding each screen's own pixels, so
//! over screens of different densities a 1x screen beside a 2x one arrives as 1280 +
//! 2880 pixels across, and no one density presents it. The page's mosaic
//! (`frontend/src/mosaic.ts`) draws each screen at its points, and so does this, by
//! the same arithmetic: where the browser's density is no finer than any screen's,
//! every screen is resampled down to it here ([`Composition`]), so a 2x screen shown
//! on a 1x browser is carried at the pixels the browser shows rather than at four
//! times them, and the page is sent one plain desktop at that density. A screen
//! less dense than the browser is never enlarged here: such a view is sent as the
//! Mac's pixels with its mosaic, which the page draws its screens up from.
//!
//! The resampling is per region, as the page's is per screen, and per changed
//! rectangle, as the tiles are coded: a changed rectangle of the Mac's framebuffer is
//! kept in a copy of it ([`Compositor`]) and the composed pixels it touches are
//! averaged from that copy, each the mean of the box of the Mac's pixels it covers,
//! which is what the page's medium interpolation comes to at the 2:1 Combined
//! Display has. The gaps between unequal screens are filled with the colour the
//! page leaves them.
//!
//! The picture ceiling counts Combined Display by its screens, not by the bounds
//! they span: the sum of the pixels the page is sent of each, as composed here or as
//! the Mac sent them, against the ceiling's area ([`crate::video::MAX_PIXELS`]).
//! A 300×300-point 1x screen beside a 200×200-point 2x one is 300×300 + 200×200
//! pixels to a 1x browser and 300×300 + 400×400 to a 2x one.

use crate::protocol::MosaicRegion;
use crate::shadow::Rect;
use crate::video::Mirror;

/// What fills the composed view between unequal screens: the page's `GAP`
/// (`frontend/src/desktopPainterWorker.ts`).
pub const GAP: [u8; 3] = [26, 26, 26];

/// One screen's place in a composition: its pixels in the Mac's framebuffer, and
/// where they are drawn in the composed view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Draw {
    src: Area,
    dst: Area,
}

/// A rectangle by its corner and its size, as the mosaic names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// The Combined Display at one density, composed here: what the page is told its
/// desktop is, and how each of the Mac's screens is drawn into it.
#[derive(Debug, Clone, PartialEq)]
pub struct Composition {
    size: (u16, u16),
    scale: f32,
    draws: Vec<Draw>,
}

impl Composition {
    /// The view `regions` compose to at `density`, the browser's, where it is
    /// composed here: only where no screen is less dense than the browser, so a
    /// screen is never enlarged here. `None` otherwise, where the page composes
    /// it from the Mac's pixels, counted as the Mac sent them.
    pub fn of(regions: &[MosaicRegion], density: f32) -> Option<Self> {
        let draws = draws(regions, density);
        let downscales = draws.iter().all(|d| d.dst.w <= d.src.w && d.dst.h <= d.src.h);
        if draws.is_empty() || !downscales {
            return None;
        }
        Some(Self { size: extent(&draws), scale: density, draws })
    }

    /// The composed desktop, in its own pixels.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Its pixels per point: the browser's density it was composed at.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The pixels of its screens, the gaps between them aside: what the picture
    /// ceiling counts it by ([`crate::video::within_area`]).
    pub fn pixels(&self) -> u64 {
        self.draws.iter().map(|d| u64::from(d.dst.w) * u64::from(d.dst.h)).sum()
    }

    /// A position on the composed view as the Mac addresses it: a pixel of its
    /// combined framebuffer, or `None` between screens. As the page's mosaic and
    /// Apple's viewer hit-test them: the screens in order, each widened by one
    /// pixel on its far edges.
    pub fn to_framebuffer(&self, x: i32, y: i32) -> Option<(i32, i32)> {
        let d = self.draws.iter().find(|d| {
            let (dx, dy) = (d.dst.x as i32, d.dst.y as i32);
            d.dst.w > 0
                && d.dst.h > 0
                && (dx..=dx + d.dst.w as i32).contains(&x)
                && (dy..=dy + d.dst.h as i32).contains(&y)
        })?;
        let map = |at: i32, dst: u32, dst_len: u32, src: u32, src_len: u32| {
            let mapped = f64::from(src) + f64::from(at - dst as i32) * f64::from(src_len) / f64::from(dst_len);
            (mapped.round() as i32).clamp(src as i32, (src + src_len) as i32 - 1)
        };
        Some((map(x, d.dst.x, d.dst.w, d.src.x, d.src.w), map(y, d.dst.y, d.dst.h, d.src.y, d.src.h)))
    }

    /// The parts of the composed view no screen covers, which the page is sent as
    /// [`GAP`].
    pub fn gaps(&self) -> Vec<Rect> {
        let mut xs: Vec<u32> = vec![0, u32::from(self.size.0)];
        let mut ys: Vec<u32> = vec![0, u32::from(self.size.1)];
        for d in &self.draws {
            xs.extend([d.dst.x, d.dst.x + d.dst.w]);
            ys.extend([d.dst.y, d.dst.y + d.dst.h]);
        }
        for edges in [&mut xs, &mut ys] {
            edges.sort_unstable();
            edges.dedup();
        }
        let covered = |x: u32, y: u32| {
            self.draws.iter().any(|d| (d.dst.x..d.dst.x + d.dst.w).contains(&x) && (d.dst.y..d.dst.y + d.dst.h).contains(&y))
        };
        let mut gaps = Vec::new();
        for row in ys.windows(2) {
            for col in xs.windows(2) {
                if col[0] < col[1] && row[0] < row[1] && !covered(col[0], row[0]) {
                    let size = |from: u32, to: u32| u16::try_from(to - from).unwrap_or(u16::MAX);
                    gaps.extend(Rect::from_size(
                        col[0] as u16,
                        row[0] as u16,
                        size(col[0], col[1]),
                        size(row[0], row[1]),
                    ));
                }
            }
        }
        gaps
    }
}

/// Every region at its points at `density`, as `mosaicView` places them: edges
/// from rounded corners, so neighbours share their boundary pixel.
fn draws(regions: &[MosaicRegion], density: f32) -> Vec<Draw> {
    let scale = if density.is_finite() && density > 0.0 { f64::from(density) } else { 1.0 };
    let at = |points: u32| (f64::from(points) * scale).round() as u32;
    regions
        .iter()
        .map(|region| {
            let (points, pixels) = (region.points, region.pixels);
            let (x, y) = (at(u32::from(points.x)), at(u32::from(points.y)));
            Draw {
                src: Area { x: pixels.x.into(), y: pixels.y.into(), w: pixels.w.into(), h: pixels.h.into() },
                dst: Area {
                    x,
                    y,
                    w: at(u32::from(points.x) + u32::from(points.w)) - x,
                    h: at(u32::from(points.y) + u32::from(points.h)) - y,
                },
            }
        })
        .collect()
}

/// The view `draws` span, at least a pixel each way.
fn extent(draws: &[Draw]) -> (u16, u16) {
    let side = |end: fn(&Draw) -> u32| u16::try_from(draws.iter().map(end).max().unwrap_or(1).max(1)).unwrap_or(u16::MAX);
    (side(|d| d.dst.x + d.dst.w), side(|d| d.dst.y + d.dst.h))
}

/// A composition with the Mac's framebuffer it is drawn from: the copy each changed
/// rectangle lands in, so a composed pixel that covers more of the Mac's than the
/// rectangle brought is averaged from all of them.
pub struct Compositor {
    composition: Composition,
    source: Mirror,
}

impl Compositor {
    /// A compositor for `composition` of a `framebuffer`-sized combined view.
    pub fn new(composition: Composition, framebuffer: (u16, u16)) -> anyhow::Result<Self> {
        Ok(Self { composition, source: Mirror::new(framebuffer.0, framebuffer.1)? })
    }

    /// Whether this compositor draws `composition` of a `framebuffer`-sized view.
    pub fn draws(&self, composition: &Composition, framebuffer: (u16, u16)) -> bool {
        self.composition == *composition && self.source.size() == framebuffer
    }

    /// Take a changed rectangle of the Mac's framebuffer, `rgb` packed RGB888 for
    /// `rect`, and return what of the composed view it changes: a rectangle per
    /// screen it falls on, with its composed pixels.
    pub fn compose(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<Vec<(Rect, Vec<u8>)>> {
        self.source.blit(rect, rgb)?;
        let (x0, y0) = (u32::from(rect.left), u32::from(rect.top));
        let (x1, y1) = (u32::from(rect.right) + 1, u32::from(rect.bottom) + 1);
        let stride = usize::from(self.source.size().0) * 3;
        let source = self.source.rgb();
        let mut out = Vec::new();
        for d in &self.composition.draws {
            let (src, dst) = (d.src, d.dst);
            let (sx0, sy0) = (x0.max(src.x), y0.max(src.y));
            let (sx1, sy1) = (x1.min(src.x + src.w), y1.min(src.y + src.h));
            if sx0 >= sx1 || sy0 >= sy1 || dst.w == 0 || dst.h == 0 {
                continue;
            }
            // The composed pixels whose boxes reach into the changed part.
            let down = |at: u32, from: u32, len: u32, to_len: u32| (at - from) * to_len / len;
            let up = |at: u32, from: u32, len: u32, to_len: u32| ((at - from) * to_len).div_ceil(len);
            let (dx0, dx1) = (down(sx0, src.x, src.w, dst.w), up(sx1, src.x, src.w, dst.w).min(dst.w));
            let (dy0, dy1) = (down(sy0, src.y, src.h, dst.h), up(sy1, src.y, src.h, dst.h).min(dst.h));
            if dx0 >= dx1 || dy0 >= dy1 {
                continue;
            }
            // The box of the Mac's pixels one composed pixel covers, along one side.
            let span = |i: u32, from: u32, len: u32, to_len: u32| {
                let start = from + i * len / to_len;
                let end = (from + (i + 1) * len / to_len).max(start + 1).min(from + len);
                (start, end)
            };
            let mut pixels = Vec::with_capacity(((dx1 - dx0) * (dy1 - dy0) * 3) as usize);
            for j in dy0..dy1 {
                let (ry0, ry1) = span(j, src.y, src.h, dst.h);
                for i in dx0..dx1 {
                    let (rx0, rx1) = span(i, src.x, src.w, dst.w);
                    let mut sum = [0u32; 3];
                    for sy in ry0..ry1 {
                        let row = sy as usize * stride;
                        for sx in rx0..rx1 {
                            let at = row + sx as usize * 3;
                            for (total, value) in sum.iter_mut().zip(&source[at..at + 3]) {
                                *total += u32::from(*value);
                            }
                        }
                    }
                    let count = (ry1 - ry0) * (rx1 - rx0);
                    pixels.extend(sum.map(|total| ((total + count / 2) / count) as u8));
                }
            }
            let area = Rect::from_size(
                (dst.x + dx0) as u16,
                (dst.y + dy0) as u16,
                (dx1 - dx0) as u16,
                (dy1 - dy0) as u16,
            );
            out.extend(area.map(|area| (area, pixels)));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MosaicRect;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> MosaicRect {
        MosaicRect { x, y, w, h }
    }

    /// A 1280×800 1x screen with a 1440×900-point 2x screen to its right, as the
    /// gateway's mosaic names them.
    fn mixed() -> Vec<MosaicRegion> {
        vec![
            MosaicRegion { pixels: rect(0, 0, 1280, 800), points: rect(0, 0, 1280, 800) },
            MosaicRegion { pixels: rect(1280, 0, 2880, 1800), points: rect(1280, 0, 1440, 900) },
        ]
    }

    #[test]
    fn a_1x_browser_has_the_2x_screen_composed_down_here() {
        let composition = Composition::of(&mixed(), 1.0).expect("composed here");
        assert_eq!(composition.size(), (2720, 900));
        assert_eq!(composition.scale(), 1.0);
        assert_eq!(composition.pixels(), 1280 * 800 + 1440 * 900, "the gap under the 1x screen aside");
    }

    #[test]
    fn a_2x_browser_is_sent_the_macs_pixels() {
        assert_eq!(Composition::of(&mixed(), 2.0), None, "the 1x screen is never enlarged here");
    }

    /// A 300×300-point 1x screen and a 200×200-point 2x one: 300×300 + 200×200
    /// pixels sent to a 1x browser.
    #[test]
    fn what_is_counted_is_what_each_screen_is_sent_at() {
        let regions = vec![
            MosaicRegion { pixels: rect(0, 0, 300, 300), points: rect(0, 0, 300, 300) },
            MosaicRegion { pixels: rect(300, 0, 400, 400), points: rect(300, 0, 200, 200) },
        ];
        assert_eq!(Composition::of(&regions, 1.0).unwrap().pixels(), 300 * 300 + 200 * 200);
    }

    #[test]
    fn the_gap_under_the_shorter_screen_is_filled() {
        let composition = Composition::of(&mixed(), 1.0).unwrap();
        let gaps = composition.gaps();
        assert_eq!(gaps, vec![Rect::from_size(0, 800, 1280, 100).unwrap()]);
    }

    #[test]
    fn a_composed_pixel_is_the_mean_of_the_macs_pixels_it_covers() {
        let regions = vec![MosaicRegion { pixels: rect(0, 0, 4, 2), points: rect(0, 0, 2, 1) }];
        let mut compositor = Compositor::new(Composition::of(&regions, 1.0).unwrap(), (4, 2)).unwrap();
        #[rustfmt::skip]
        let rgb = [
            0, 0, 0,   100, 100, 100,   10, 20, 30,   10, 20, 30,
            200, 200, 200,   100, 100, 100,   10, 20, 30,   50, 60, 70,
        ];
        let changed = compositor.compose(Rect::from_size(0, 0, 4, 2).unwrap(), &rgb).unwrap();
        assert_eq!(changed, vec![(Rect::from_size(0, 0, 2, 1).unwrap(), vec![100, 100, 100, 20, 30, 40])]);
    }

    #[test]
    fn a_change_to_one_mac_pixel_recomposes_the_box_it_is_in_from_its_neighbours() {
        let regions = vec![MosaicRegion { pixels: rect(0, 0, 4, 4), points: rect(0, 0, 2, 2) }];
        let mut compositor = Compositor::new(Composition::of(&regions, 1.0).unwrap(), (4, 4)).unwrap();
        compositor.compose(Rect::from_size(0, 0, 4, 4).unwrap(), &[40; 4 * 4 * 3]).unwrap();
        let changed = compositor.compose(Rect::from_size(3, 2, 1, 1).unwrap(), &[200, 200, 200]).unwrap();
        assert_eq!(changed, vec![(Rect::from_size(1, 1, 1, 1).unwrap(), vec![80, 80, 80])]);
    }

    #[test]
    fn a_1x_screen_is_copied_as_it_came() {
        let mut compositor = Compositor::new(Composition::of(&mixed(), 1.0).unwrap(), (4160, 1800)).unwrap();
        let changed = compositor.compose(Rect::from_size(10, 20, 2, 1).unwrap(), &[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(changed, vec![(Rect::from_size(10, 20, 2, 1).unwrap(), vec![1, 2, 3, 4, 5, 6])]);
    }

    #[test]
    fn a_rectangle_across_both_screens_changes_each_where_it_draws() {
        let mut compositor = Compositor::new(Composition::of(&mixed(), 1.0).unwrap(), (4160, 1800)).unwrap();
        let changed = compositor.compose(Rect::from_size(1278, 0, 4, 2).unwrap(), &[9; 4 * 2 * 3]).unwrap();
        let areas: Vec<Rect> = changed.into_iter().map(|(area, _)| area).collect();
        assert_eq!(areas, vec![Rect::from_size(1278, 0, 2, 2).unwrap(), Rect::from_size(1280, 0, 1, 1).unwrap()]);
    }

    #[test]
    fn a_composed_position_reaches_the_mac_in_its_screens_pixels() {
        let composition = Composition::of(&mixed(), 1.0).unwrap();
        assert_eq!(composition.to_framebuffer(100, 100), Some((100, 100)));
        assert_eq!(composition.to_framebuffer(1280 + 720, 450), Some((1280 + 1440, 900)));
        assert_eq!(composition.to_framebuffer(100, 850), None, "between the screens");
        assert_eq!(composition.to_framebuffer(2720, 900), Some((4159, 1799)), "the far edge, widened");
    }
}
