//! The H.264 the graphics pipeline draws video with, decoded and put back into
//! colour.
//!
//! A Windows host that finds the client able to take H.264 does not switch the
//! desktop to it. It goes on drawing text and windows with ClearCodec and
//! Progressive, and hands the parts that move like video — a player, a game, a
//! scrolling page — to the AVC codecs in the same frames, each surface with an
//! H.264 stream of its own whose picture is the surface rounded up to macroblocks.
//! The metablock in front of every access unit ([`super::proto::avc`]) says which
//! rectangles of that picture are to be shown; the rest is whatever the encoder
//! left there.
//!
//! [`Avc`] is one surface's decoder — Cisco's OpenH264, built from source — and
//! the working space around it. AVC420 is the plain case: decode, convert the
//! masked rectangles from full-range BT.709 YUV to RGB, and hand them to the
//! surface. AVC444 is the same picture at full chroma resolution, carried as two
//! YUV420 pictures through the one decoder: a *luma* view holding the luma and a
//! quarter of the chroma, averaged, and a *chroma* view whose three planes hold
//! the other three quarters, packed one of two ways ([`Layout`]). The luma view is
//! kept after it is shown, because the chroma view for a rectangle may come in a
//! later frame and combines with the last luma view that drew it. Putting the
//! views back together is [MS-RDPEGFX] 3.3.8.3.2 and 3.3.8.3.3, including the
//! filter that recovers the averaged samples from their three neighbours.
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use anyhow::{Context as _, Result, bail, ensure};
use openh264::OpenH264API;
use openh264::decoder::{DecodedYUV, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use yuv::{YuvPlanarImage, YuvRange, YuvStandardMatrix};

use super::proto::avc::{Avc420, Avc444, Region};
use super::proto::gfx::Rect16;

/// How a YUV444 picture is packed into its two YUV420 views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layout {
    /// AVC444: the chroma view is laid out macroblock by macroblock, the odd rows of
    /// U and V in its luma plane and the odd columns of their even rows in its
    /// chroma planes. [MS-RDPEGFX] 3.3.8.3.2.
    V1,
    /// AVC444v2: the chroma view is laid out over the whole picture, the odd columns
    /// of U and V in the left and right halves of its luma plane and the odd rows of
    /// their even columns in the halves of its chroma planes. [MS-RDPEGFX] 3.3.8.3.3.
    V2,
}

/// The difference past which the recovered chroma sample is used in place of the
/// averaged one — [MS-RDPEGFX] 3.3.8.3.2's cutoff.
const FILTER_CUTOFF: i32 = 30;

/// Where the painted rectangles go: one rectangle of the surface, its rows of packed
/// RGB, and the bytes between the starts of consecutive rows.
pub(super) type Paint<'a> = &'a mut dyn FnMut(Rect16, &[u8], usize);

/// A rectangle in picture pixels, right and bottom exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
}

impl Area {
    fn width(&self) -> usize {
        self.right - self.left
    }

    fn height(&self) -> usize {
        self.bottom - self.top
    }

    /// The region rectangle, kept to the surface and to the picture; `None` when
    /// nothing of it is inside both.
    fn clip(rect: Rect16, surface: (u32, u32), picture: (usize, usize)) -> Option<Self> {
        let right = usize::from(rect.right).min(surface.0 as usize).min(picture.0);
        let bottom = usize::from(rect.bottom).min(surface.1 as usize).min(picture.1);
        let (left, top) = (usize::from(rect.left), usize::from(rect.top));
        (left < right && top < bottom).then_some(Self { left, top, right, bottom })
    }

    /// This rectangle widened to a multiple of `unit` on every side, within the
    /// picture.
    fn aligned(&self, unit: usize, picture: (usize, usize)) -> Self {
        Self {
            left: self.left / unit * unit,
            top: self.top / unit * unit,
            right: self.right.div_ceil(unit).saturating_mul(unit).min(picture.0),
            bottom: self.bottom.div_ceil(unit).saturating_mul(unit).min(picture.1),
        }
    }

    fn rect16(&self) -> Rect16 {
        // Clipped to a surface whose sides came off the wire as u16.
        let side = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        Rect16 { left: side(self.left), top: side(self.top), right: side(self.right), bottom: side(self.bottom) }
    }
}

/// Three planes, borrowed from wherever they are: the decoder's own buffers or a
/// copy kept here. Strides are in bytes; the chroma planes are half size each way.
#[derive(Clone, Copy)]
struct View<'a> {
    width: usize,
    height: usize,
    y: &'a [u8],
    y_stride: usize,
    u: &'a [u8],
    u_stride: usize,
    v: &'a [u8],
    v_stride: usize,
}

impl<'a> View<'a> {
    fn of(picture: &'a DecodedYUV<'_>) -> Self {
        let (width, height) = picture.dimensions();
        let (y_stride, u_stride, v_stride) = picture.strides();
        Self { width, height, y: picture.y(), y_stride, u: picture.u(), u_stride, v: picture.v(), v_stride }
    }

    fn of_planes(planes: &'a Planes) -> Self {
        let chroma = planes.width.div_ceil(2);
        Self {
            width: planes.width,
            height: planes.height,
            y: &planes.y,
            y_stride: planes.width,
            u: &planes.u,
            u_stride: chroma,
            v: &planes.v,
            v_stride: chroma,
        }
    }

    fn y_at(&self, x: usize, y: usize) -> u8 {
        self.y[y * self.y_stride + x]
    }

    fn u_at(&self, x: usize, y: usize) -> u8 {
        self.u[y * self.u_stride + x]
    }

    fn v_at(&self, x: usize, y: usize) -> u8 {
        self.v[y * self.v_stride + x]
    }

    /// The luma plane from `area`'s corner on, for a converter that takes a stride.
    fn y_from(&self, area: Area) -> &'a [u8] {
        &self.y[area.top * self.y_stride + area.left..]
    }
}

/// A picture of this module's own: the luma views' rectangles, each as the last
/// luma view that carried it left it, kept for the chroma views that combine with
/// them. [MS-RDPEGFX] 3.3.8.3.2 has a chroma rectangle combine with "the last
/// corresponding rectangle in a luma subframe", so a luma view updates only the
/// rectangles in its mask: the picture outside them is whatever its encoder left
/// there, and taking it would overwrite rectangles a later chroma view still
/// needs.
struct Planes {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl Planes {
    fn empty() -> Self {
        Self { width: 0, height: 0, y: Vec::new(), u: Vec::new(), v: Vec::new() }
    }

    /// Sized to the picture. A picture of another size starts over, black.
    fn fit(&mut self, width: usize, height: usize) {
        if (self.width, self.height) != (width, height) {
            let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
            self.width = width;
            self.height = height;
            self.y = vec![0; width * height];
            self.u = vec![0; cw * ch];
            self.v = vec![0; cw * ch];
        }
    }

    /// Take one rectangle of the view, with the chroma samples that enclose it.
    fn keep(&mut self, view: &View<'_>, area: Area) {
        let cw = self.width.div_ceil(2);
        for y in area.top..area.bottom {
            let row = &view.y[y * view.y_stride..][area.left..area.right];
            self.y[y * self.width..][area.left..area.right].copy_from_slice(row);
        }
        for y in area.top / 2..area.bottom.div_ceil(2) {
            let (left, right) = (area.left / 2, area.right.div_ceil(2));
            self.u[y * cw..][left..right].copy_from_slice(&view.u[y * view.u_stride..][left..right]);
            self.v[y * cw..][left..right].copy_from_slice(&view.v[y * view.v_stride..][left..right]);
        }
    }
}

/// One surface's H.264 decoder, the last luma view it showed, and the working
/// space the conversions use.
pub(super) struct Avc {
    decoder: Decoder,
    luma: Option<Planes>,
    /// The full-resolution chroma of one block of macroblocks, put together from
    /// the two views.
    u444: Vec<u8>,
    v444: Vec<u8>,
    /// The RGB rows of one block, on their way to the surface.
    rgb: Vec<u8>,
}

impl Avc {
    pub(super) fn new() -> Result<Self> {
        // Every access unit is decoded as it arrives and its picture taken at once.
        // OpenH264 holds a decoded picture back for reordering — measured against a
        // Windows host's Main-profile stream, whose first unit came out as no
        // picture and every later one as the picture before it — so each decode
        // flushes the picture its unit made. A screen stream has nothing to
        // reorder.
        let config = DecoderConfig::new().flush_after_decode(Flush::Flush);
        let decoder =
            Decoder::with_api_config(OpenH264API::from_source(), config).context("creating an H.264 decoder")?;
        Ok(Self { decoder, luma: None, u444: Vec::new(), v444: Vec::new(), rgb: Vec::new() })
    }

    /// An AVC420 stream: decode its access unit and paint its rectangles.
    ///
    /// An error is the whole stream's — its rectangles are not painted — and the
    /// decoder keeps whatever state the access unit left it in, as any H.264
    /// decoder does until the next keyframe.
    pub(super) fn draw_420(&mut self, stream: &Avc420<'_>, surface: (u32, u32), paint: Paint<'_>) -> Result<()> {
        let picture = decode(&mut self.decoder, stream.bitstream)?;
        let view = View::of(&picture);
        for region in &stream.regions {
            if let Some(area) = Area::clip(region.rect, surface, (view.width, view.height)) {
                convert_420(&view, area, &mut self.rgb, paint)?;
            }
        }
        Ok(())
    }

    /// An AVC444 or AVC444v2 stream: decode whichever views it carries, through the
    /// one decoder in the order they arrive, and paint their rectangles — a luma
    /// view's at half chroma, as [MS-RDPEGFX] 3.3.8.3.2 has it, unless a chroma
    /// view for the same rectangles is in the same stream and paints them whole.
    pub(super) fn draw_444(
        &mut self,
        stream: &Avc444<'_>,
        layout: Layout,
        surface: (u32, u32),
        paint: Paint<'_>,
    ) -> Result<()> {
        if let Some(luma) = &stream.luma {
            let picture = decode(&mut self.decoder, luma.bitstream)?;
            let view = View::of(&picture);
            let size = (view.width, view.height);
            let kept = self.luma.get_or_insert_with(Planes::empty);
            kept.fit(view.width, view.height);
            let painted_whole = stream.chroma.as_ref().is_some_and(|chroma| same_mask(&chroma.regions, &luma.regions));
            for region in &luma.regions {
                let Some(area) = Area::clip(region.rect, surface, size) else { continue };
                kept.keep(&view, area);
                if !painted_whole {
                    convert_420(&view, area, &mut self.rgb, paint)?;
                }
            }
        }
        if let Some(chroma) = &stream.chroma {
            let Some(kept) = &self.luma else {
                bail!("a chroma view arrived before any luma view to combine it with");
            };
            let picture = decode(&mut self.decoder, chroma.bitstream)?;
            let aux = View::of(&picture);
            ensure!(
                (aux.width, aux.height) == (kept.width, kept.height),
                "the chroma view is {}x{} and the luma view {}x{}",
                aux.width,
                aux.height,
                kept.width,
                kept.height
            );
            // The chroma view's tables index whole macroblocks, which [MS-RDPEGFX]
            // 2.2.4.4 has the picture made of.
            ensure!(
                aux.width.is_multiple_of(16) && aux.height.is_multiple_of(16),
                "the picture is {}x{}, which is not whole macroblocks",
                aux.width,
                aux.height
            );
            let main = View::of_planes(kept);
            let picture = (main.width, main.height);
            for region in &chroma.regions {
                let Some(area) = Area::clip(region.rect, surface, picture) else { continue };
                // Whole macroblocks, then the mask: the chroma view is laid out by
                // macroblock, and the filter reads across every 2x2.
                let block = area.aligned(16, picture);
                combine(&main, &aux, layout, block, &mut self.u444, &mut self.v444);
                convert_444(&main, block, &self.u444, &self.v444, area, &mut self.rgb, paint)?;
            }
        }
        Ok(())
    }
}

/// One access unit in, one picture out. No picture is a fault: a screen stream's
/// every access unit carries a slice, and a decoder that returns nothing for one
/// has dropped it. The fault names the unit's NAL types, which is what tells a
/// unit the decoder refused from one that carried no slice.
fn decode<'d>(decoder: &'d mut Decoder, bitstream: &[u8]) -> Result<DecodedYUV<'d>> {
    match decoder.decode(bitstream).with_context(|| format!("decoding an H.264 access unit of {}", describe(bitstream)))? {
        Some(picture) => Ok(picture),
        None => bail!("an H.264 access unit of {} decoded to no picture", describe(bitstream)),
    }
}

/// An access unit as a log line sees it: its length and the type of each NAL unit
/// in it, by Annex B start code.
fn describe(bitstream: &[u8]) -> String {
    let types: Vec<u8> = openh264::nal_units(bitstream)
        .filter_map(|nal| nal.iter().skip_while(|b| **b == 0).nth(1).map(|header| header & 0x1F))
        .collect();
    format!("{} bytes, NAL types {types:?}", bitstream.len())
}

/// Whether two region masks name the same rectangles.
fn same_mask(a: &[Region], b: &[Region]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.rect == b.rect)
}

/// Convert `area` of a YUV420 view to RGB and paint it. The conversion covers the
/// area widened to whole chroma samples; the paint is the area itself.
fn convert_420(view: &View<'_>, area: Area, rgb: &mut Vec<u8>, paint: Paint<'_>) -> Result<()> {
    let block = area.aligned(2, (view.width, view.height));
    let stride = block.width() * 3;
    rgb.clear();
    rgb.resize(stride * block.height(), 0);
    let image = YuvPlanarImage {
        y_plane: view.y_from(block),
        y_stride: view.y_stride as u32,
        u_plane: &view.u[block.top / 2 * view.u_stride + block.left / 2..],
        u_stride: view.u_stride as u32,
        v_plane: &view.v[block.top / 2 * view.v_stride + block.left / 2..],
        v_stride: view.v_stride as u32,
        width: block.width() as u32,
        height: block.height() as u32,
    };
    yuv::yuv420_to_rgb(&image, rgb, stride as u32, YuvRange::Full, YuvStandardMatrix::Bt709)
        .context("converting a YUV420 rectangle to RGB")?;
    paint(area.rect16(), &rgb[(area.top - block.top) * stride + (area.left - block.left) * 3..], stride);
    Ok(())
}

/// Convert `area` of the picture to RGB from the luma view's Y and the block's
/// combined chroma, and paint it.
fn convert_444(
    main: &View<'_>,
    block: Area,
    u444: &[u8],
    v444: &[u8],
    area: Area,
    rgb: &mut Vec<u8>,
    paint: Paint<'_>,
) -> Result<()> {
    let stride = block.width() * 3;
    rgb.clear();
    rgb.resize(stride * block.height(), 0);
    let image = YuvPlanarImage {
        y_plane: main.y_from(block),
        y_stride: main.y_stride as u32,
        u_plane: u444,
        u_stride: block.width() as u32,
        v_plane: v444,
        v_stride: block.width() as u32,
        width: block.width() as u32,
        height: block.height() as u32,
    };
    yuv::yuv444_to_rgb(&image, rgb, stride as u32, YuvRange::Full, YuvStandardMatrix::Bt709)
        .context("converting a YUV444 rectangle to RGB")?;
    paint(area.rect16(), &rgb[(area.top - block.top) * stride + (area.left - block.left) * 3..], stride);
    Ok(())
}

/// Put the two views' chroma back together over `block`, into `u444` and `v444`
/// at the block's width.
///
/// Every sample but the even rows' even columns is carried whole in the chroma
/// view, where `layout` says; those come first. The even-even samples are the
/// luma view's chroma, which the host averaged over each 2x2, and come second,
/// recovered from the average and the three neighbours just written.
fn combine(main: &View<'_>, aux: &View<'_>, layout: Layout, block: Area, u444: &mut Vec<u8>, v444: &mut Vec<u8>) {
    let width = block.width();
    u444.clear();
    u444.resize(width * block.height(), 0);
    v444.clear();
    v444.resize(width * block.height(), 0);
    // V2 puts V's samples in the right half of each chroma-view plane.
    let (half, quarter) = (aux.width / 2, aux.width / 4);
    for y in block.top..block.bottom {
        let row = (y - block.top) * width;
        for x in block.left..block.right {
            let (u, v) = match layout {
                Layout::V1 if y & 1 == 1 => {
                    // B4 and B5: the odd rows of U then of V, eight of each per
                    // macroblock, in the chroma view's luma plane.
                    let base = y & !15;
                    let r = base + ((y & 15) >> 1);
                    (aux.y_at(x, r), aux.y_at(x, r + 8))
                }
                Layout::V1 if x & 1 == 1 => {
                    // B6 and B7: the odd columns of the even rows, in its chroma planes.
                    (aux.u_at(x >> 1, y >> 1), aux.v_at(x >> 1, y >> 1))
                }
                Layout::V2 if x & 1 == 1 => {
                    // B4 and B5: the odd columns, every row, in the left and right
                    // halves of the chroma view's luma plane.
                    (aux.y_at(x >> 1, y), aux.y_at(half + (x >> 1), y))
                }
                Layout::V2 if y & 1 == 1 => {
                    // B6 to B9: the odd rows of the even columns, columns 4n in its U
                    // plane and 4n+2 in its V plane, U's samples in each plane's left
                    // half and V's in its right.
                    let (col, r) = (x >> 2, y >> 1);
                    if x & 2 == 0 {
                        (aux.u_at(col, r), aux.u_at(quarter + col, r))
                    } else {
                        (aux.v_at(col, r), aux.v_at(quarter + col, r))
                    }
                }
                _ => continue,
            };
            u444[row + x - block.left] = u;
            v444[row + x - block.left] = v;
        }
    }
    for y in (block.top..block.bottom).step_by(2) {
        let row = (y - block.top) * width;
        for x in (block.left..block.right).step_by(2) {
            let at = row + x - block.left;
            let (u, v) = (main.u_at(x >> 1, y >> 1), main.v_at(x >> 1, y >> 1));
            let whole = x + 1 < block.right && y + 1 < block.bottom;
            u444[at] = if whole { unfilter(u, u444[at + 1], u444[at + width], u444[at + width + 1]) } else { u };
            v444[at] = if whole { unfilter(v, v444[at + 1], v444[at + width], v444[at + width + 1]) } else { v };
        }
    }
}

/// The 2x2's top-left sample back from the host's average of the four and the
/// other three — taken when it differs from the average by more than the cutoff,
/// since past a quantized average the reverse can be further from the truth than
/// the average was. [MS-RDPEGFX] 3.3.8.3.2.
fn unfilter(average: u8, right: u8, below: u8, diagonal: u8) -> u8 {
    let average = i32::from(average);
    let reversed = average * 4 - i32::from(right) - i32::from(below) - i32::from(diagonal);
    if (average - reversed).abs() > FILTER_CUTOFF { reversed.clamp(0, 255) as u8 } else { average as u8 }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Pictures encoded as a host encodes them, for the tests here and the
    //! compositor's: OpenH264's own encoder, at a quality where a flat colour comes
    //! back within a few steps of itself.

    use openh264::OpenH264API;
    use openh264::encoder::{BitRate, Encoder, EncoderConfig, QpRange, RateControlMode};
    use openh264::formats::YUVBuffer;

    /// One H.264 stream, encoding the pictures it is given in turn, the first as a
    /// keyframe with its parameter sets inline.
    pub(crate) struct Stream(Encoder);

    impl Stream {
        pub(crate) fn new() -> Self {
            let config = EncoderConfig::new()
                .rate_control_mode(RateControlMode::Quality)
                .bitrate(BitRate::from_bps(50_000_000))
                .qp(QpRange::new(0, 4))
                .skip_frames(false);
            Self(Encoder::with_api_config(OpenH264API::from_source(), config).expect("an encoder"))
        }

        /// Encode one I420 picture, packed: Y then U then V.
        pub(crate) fn encode(&mut self, i420: Vec<u8>, width: usize, height: usize) -> Vec<u8> {
            let picture = YUVBuffer::from_vec(i420, width, height);
            self.0.encode(&picture).expect("an access unit").to_vec()
        }
    }

    /// A packed I420 picture of one colour.
    pub(crate) fn flat(width: usize, height: usize, yuv: [u8; 3]) -> Vec<u8> {
        let mut out = vec![yuv[0]; width * height];
        out.resize(width * height + width * height / 4, yuv[1]);
        out.resize(width * height + width * height / 2, yuv[2]);
        out
    }

    /// The RGB a full-range BT.709 YUV sample is, by [MS-RDPEGFX] 3.3.8.3.1's own
    /// integer form.
    pub(crate) fn rgb_of(yuv: [u8; 3]) -> [u8; 3] {
        let (y, u, v) = (i32::from(yuv[0]) * 256, i32::from(yuv[1]) - 128, i32::from(yuv[2]) - 128);
        let clamp = |n: i32| (n >> 8).clamp(0, 255) as u8;
        [clamp(y + 403 * v), clamp(y - 48 * u - 120 * v), clamp(y + 475 * u)]
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Stream, flat, rgb_of};
    use super::*;

    /// A YUV444 picture, and the two YUV420 views a host makes of it under either
    /// layout — the forward side of [MS-RDPEGFX] 3.3.8.3.2 and 3.3.8.3.3, written
    /// from the specification's tables so the combine is checked against something
    /// other than itself.
    struct Picture {
        width: usize,
        height: usize,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    }

    impl Picture {
        fn sample(&self, plane: &[u8], x: usize, y: usize) -> u8 {
            plane[y * self.width + x]
        }

        /// The luma view: Y whole, U and V averaged over each 2x2.
        fn luma_view(&self) -> Planes {
            let (cw, ch) = (self.width / 2, self.height / 2);
            let average = |plane: &[u8], x: usize, y: usize| {
                let sum: u32 = [(0, 0), (1, 0), (0, 1), (1, 1)]
                    .iter()
                    .map(|(dx, dy)| u32::from(self.sample(plane, 2 * x + dx, 2 * y + dy)))
                    .sum();
                (sum / 4) as u8
            };
            let mut u = vec![0; cw * ch];
            let mut v = vec![0; cw * ch];
            for y in 0..ch {
                for x in 0..cw {
                    u[y * cw + x] = average(&self.u, x, y);
                    v[y * cw + x] = average(&self.v, x, y);
                }
            }
            Planes { width: self.width, height: self.height, y: self.y.clone(), u, v }
        }

        /// The chroma view, under `layout`.
        fn chroma_view(&self, layout: Layout) -> Planes {
            let (w, h) = (self.width, self.height);
            let (cw, ch) = (w / 2, h / 2);
            let mut y = vec![0; w * h];
            let mut u = vec![0; cw * ch];
            let mut v = vec![0; cw * ch];
            match layout {
                Layout::V1 => {
                    for my in (0..h).step_by(16) {
                        for mx in (0..w).step_by(16) {
                            for yy in 0..8 {
                                for xx in 0..16 {
                                    y[(my + yy) * w + mx + xx] = self.sample(&self.u, mx + xx, my + 2 * yy + 1);
                                    y[(my + 8 + yy) * w + mx + xx] = self.sample(&self.v, mx + xx, my + 2 * yy + 1);
                                }
                            }
                            for yy in 0..8 {
                                for xx in 0..8 {
                                    u[(my / 2 + yy) * cw + mx / 2 + xx] = self.sample(&self.u, mx + 2 * xx + 1, my + 2 * yy);
                                    v[(my / 2 + yy) * cw + mx / 2 + xx] = self.sample(&self.v, mx + 2 * xx + 1, my + 2 * yy);
                                }
                            }
                        }
                    }
                }
                Layout::V2 => {
                    for yy in 0..h {
                        for xx in 0..w / 2 {
                            y[yy * w + xx] = self.sample(&self.u, 2 * xx + 1, yy);
                            y[yy * w + w / 2 + xx] = self.sample(&self.v, 2 * xx + 1, yy);
                        }
                    }
                    for yy in 0..h / 2 {
                        for xx in 0..w / 4 {
                            u[yy * cw + xx] = self.sample(&self.u, 4 * xx, 2 * yy + 1);
                            u[yy * cw + w / 4 + xx] = self.sample(&self.v, 4 * xx, 2 * yy + 1);
                            v[yy * cw + xx] = self.sample(&self.u, 4 * xx + 2, 2 * yy + 1);
                            v[yy * cw + w / 4 + xx] = self.sample(&self.v, 4 * xx + 2, 2 * yy + 1);
                        }
                    }
                }
            }
            Planes { width: w, height: h, y, u, v }
        }
    }

    /// A 32x32 picture whose chroma varies per pixel but whose every 2x2 averages
    /// exactly, so the combine has to land on the original sample for sample.
    fn textured() -> Picture {
        let (width, height) = (32, 32);
        let mut u = vec![0; width * height];
        let mut v = vec![0; width * height];
        for y in 0..height {
            for x in 0..width {
                // Each 2x2: [a, a+4, a+8, a+12], summing to 4a + 24, so the average
                // is a + 6 exactly and the reverse filter lands on a.
                let a = ((x / 2 + y / 2 * 16) % 60 * 3) as u8 + 40;
                let corner = ((x & 1) + 2 * (y & 1)) as u8 * 4;
                u[y * width + x] = a + corner;
                v[y * width + x] = 255 - a - corner;
            }
        }
        let y = (0..width * height).map(|i| (i % 251) as u8).collect();
        Picture { width, height, y, u, v }
    }

    fn combined(picture: &Picture, layout: Layout, block: Area) -> (Vec<u8>, Vec<u8>) {
        let (luma, chroma) = (picture.luma_view(), picture.chroma_view(layout));
        let (mut u444, mut v444) = (Vec::new(), Vec::new());
        combine(&View::of_planes(&luma), &View::of_planes(&chroma), layout, block, &mut u444, &mut v444);
        (u444, v444)
    }

    /// Both layouts put every chroma sample back where the picture had it — the
    /// carried ones by their tables, the averaged ones through the filter, whose
    /// reverse is beyond the cutoff by construction here.
    #[test]
    fn the_two_views_combine_back_into_the_picture_under_either_layout() {
        let picture = textured();
        let block = Area { left: 0, top: 0, right: 32, bottom: 32 };
        for layout in [Layout::V1, Layout::V2] {
            let (u444, v444) = combined(&picture, layout, block);
            // The averaged corner's reverse differs from the average by 6, under the
            // cutoff, so the average itself is what the specification keeps there.
            for y in 0..32 {
                for x in 0..32 {
                    let (eu, ev) = (picture.u[y * 32 + x], picture.v[y * 32 + x]);
                    let (eu, ev) = if x & 1 == 0 && y & 1 == 0 { (eu + 6, ev - 6) } else { (eu, ev) };
                    assert_eq!((u444[y * 32 + x], v444[y * 32 + x]), (eu, ev), "{layout:?} at {x},{y}");
                }
            }
        }
    }

    /// A block that is not the whole picture reads the views at the block's own
    /// coordinates: the second macroblock column and row of the picture.
    #[test]
    fn a_block_inside_the_picture_is_combined_in_place() {
        let picture = textured();
        let block = Area { left: 16, top: 16, right: 32, bottom: 32 };
        for layout in [Layout::V1, Layout::V2] {
            let (u444, _) = combined(&picture, layout, block);
            for y in 16..32 {
                for x in 16..32 {
                    let expect = picture.u[y * 32 + x] + if x & 1 == 0 && y & 1 == 0 { 6 } else { 0 };
                    assert_eq!(u444[(y - 16) * 16 + x - 16], expect, "{layout:?} at {x},{y}");
                }
            }
        }
    }

    /// The filter's two sides: a 2x2 whose corner is far from its average gets the
    /// corner back; one whose corner is near keeps the average.
    #[test]
    fn the_filter_recovers_a_corner_only_past_the_cutoff() {
        // [100, 200, 200, 200]: average 175, reverse 100, 75 apart.
        assert_eq!(unfilter(175, 200, 200, 200), 100);
        // [100, 110, 110, 112]: average 108, reverse 100, 8 apart.
        assert_eq!(unfilter(108, 110, 110, 112), 108);
        // The reverse is clamped when the neighbours overshoot it.
        assert_eq!(unfilter(10, 250, 250, 250), 0);
    }

    /// A clip keeps a rectangle to both the surface and the picture and drops one
    /// outside either; alignment widens within the picture.
    #[test]
    fn regions_are_clipped_then_aligned_within_the_picture() {
        let rect = Rect16 { left: 5, top: 3, right: 40, bottom: 60 };
        assert_eq!(Area::clip(rect, (32, 100), (48, 48)), Some(Area { left: 5, top: 3, right: 32, bottom: 48 }));
        assert_eq!(Area::clip(rect, (5, 100), (48, 48)), None);
        let area = Area { left: 5, top: 3, right: 31, bottom: 47 };
        assert_eq!(area.aligned(2, (48, 48)), Area { left: 4, top: 2, right: 32, bottom: 48 });
        assert_eq!(area.aligned(16, (48, 48)), Area { left: 0, top: 0, right: 32, bottom: 48 });
        assert_eq!(area.aligned(16, (36, 40)), Area { left: 0, top: 0, right: 32, bottom: 40 });
    }

    /// The rectangles a paint reports, with their rows.
    fn painted(calls: &mut Vec<(Rect16, Vec<[u8; 3]>)>) -> impl FnMut(Rect16, &[u8], usize) + '_ {
        move |rect, rgb, stride| {
            let width = usize::from(rect.width());
            let rows = (0..usize::from(rect.height()))
                .flat_map(|row| rgb[row * stride..row * stride + width * 3].as_chunks::<3>().0.iter().copied())
                .collect();
            calls.push((rect, rows));
        }
    }

    fn close(actual: [u8; 3], expected: [u8; 3]) -> bool {
        // The chroma filter quadruples the luma view's error at every recovered
        // sample, so a few steps of coding error — which differ between the
        // encoder's x86 and NEON paths — can be a dozen after it. The colours the
        // tests tell apart are over a hundred apart.
        actual.iter().zip(expected).all(|(a, e)| a.abs_diff(e) <= 16)
    }

    /// An AVC420 stream decodes and paints its masked rectangles, and only those, in
    /// the colour the picture carries.
    #[test]
    fn an_avc420_stream_paints_its_region_in_colour() {
        let (yuv, mut stream) = ([140, 90, 170], Stream::new());
        let unit = stream.encode(flat(32, 32, yuv), 32, 32);
        let mut avc = Avc::new().unwrap();
        let regions = vec![Region { rect: Rect16 { left: 3, top: 5, right: 20, bottom: 30 }, qp: 0, progressive: false, quality: 100 }];
        let mut calls = Vec::new();
        avc.draw_420(&Avc420 { regions, bitstream: &unit }, (32, 32), &mut painted(&mut calls)).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, Rect16 { left: 3, top: 5, right: 20, bottom: 30 });
        assert_eq!(calls[0].1.len(), 17 * 25);
        let expected = rgb_of(yuv);
        assert!(calls[0].1.iter().all(|px| close(*px, expected)), "{:?} vs {expected:?}", calls[0].1[0]);

        // A rectangle outside the surface paints nothing; garbage is a fault, not a
        // panic, and the decoder goes on.
        let regions = vec![Region { rect: Rect16 { left: 40, top: 0, right: 50, bottom: 8 }, qp: 0, progressive: false, quality: 100 }];
        let mut calls = Vec::new();
        avc.draw_420(&Avc420 { regions, bitstream: &unit }, (32, 32), &mut painted(&mut calls)).unwrap();
        assert!(calls.is_empty());
        let err = avc.draw_420(&Avc420 { regions: vec![], bitstream: &[0, 0, 1, 0x65, 9, 9] }, (32, 32), &mut painted(&mut calls))
            .unwrap_err();
        assert!(format!("{err:#}").contains("H.264"), "{err:#}");
    }

    /// Both views in one stream paint the rectangle at full chroma; a chroma view on
    /// its own later combines with the luma view kept from before; a chroma view
    /// before any luma view is refused.
    #[test]
    fn an_avc444_stream_combines_its_views_through_one_decoder() {
        // Chroma the luma view alone would get wrong: a 2x2 checker of two colours
        // averages to a third, and only the combined picture has the right one.
        let (a, b) = ([120u8, 60, 200], [120u8, 200, 60]);
        let mut picture = Picture { width: 32, height: 32, y: vec![120; 32 * 32], u: vec![0; 32 * 32], v: vec![0; 32 * 32] };
        for i in 0..32 * 32 {
            let (x, y) = (i % 32, i / 32);
            let c = if (x + y) % 2 == 0 { a } else { b };
            picture.u[i] = c[1];
            picture.v[i] = c[2];
        }
        let region = |rect| Region { rect, qp: 0, progressive: false, quality: 100 };
        let whole = Rect16 { left: 0, top: 0, right: 32, bottom: 32 };
        for layout in [Layout::V1, Layout::V2] {
            let (luma, chroma) = (picture.luma_view(), picture.chroma_view(layout));
            let (luma_i420, chroma_i420) = ([luma.y, luma.u, luma.v].concat(), [chroma.y, chroma.u, chroma.v].concat());
            let mut stream = Stream::new();
            let luma_unit = stream.encode(luma_i420.clone(), 32, 32);
            let chroma_unit = stream.encode(chroma_i420.clone(), 32, 32);

            let mut avc = Avc::new().unwrap();
            let mut calls = Vec::new();
            let err = avc
                .draw_444(
                    &Avc444 { luma: None, chroma: Some(Avc420 { regions: vec![region(whole)], bitstream: &chroma_unit }) },
                    layout,
                    (32, 32),
                    &mut painted(&mut calls),
                )
                .unwrap_err();
            assert!(format!("{err}").contains("before any luma view"), "{err}");

            let both = Avc444 {
                luma: Some(Avc420 { regions: vec![region(whole)], bitstream: &luma_unit }),
                chroma: Some(Avc420 { regions: vec![region(whole)], bitstream: &chroma_unit }),
            };
            avc.draw_444(&both, layout, (32, 32), &mut painted(&mut calls)).unwrap();
            assert_eq!(calls.len(), 1, "one paint for one mask on both views: {layout:?}");
            let (ra, rb) = (rgb_of(a), rgb_of(b));
            for (i, px) in calls[0].1.iter().enumerate() {
                let expected = if (i % 32 + i / 32) % 2 == 0 { ra } else { rb };
                assert!(close(*px, expected), "{layout:?} pixel {i}: {px:?} vs {expected:?}");
            }

            // A luma view alone paints its rectangles at half chroma — the average
            // of the checker — and is kept.
            // One stream, in the order the decoder will see it: the luma view, a
            // grey picture, then the chroma view.
            let grey = [200u8, 128, 128];
            let mut stream = Stream::new();
            let luma_unit = stream.encode(luma_i420.clone(), 32, 32);
            let grey_unit = stream.encode(flat(32, 32, grey), 32, 32);
            let chroma_unit = stream.encode(chroma_i420.clone(), 32, 32);
            let mut avc = Avc::new().unwrap();
            let mut calls = Vec::new();
            let alone = Avc444 { luma: Some(Avc420 { regions: vec![region(whole)], bitstream: &luma_unit }), chroma: None };
            avc.draw_444(&alone, layout, (32, 32), &mut painted(&mut calls)).unwrap();
            let mean = |p: u8, q: u8| ((u16::from(p) + u16::from(q)) / 2) as u8;
            let averaged = rgb_of([120, mean(a[1], b[1]), mean(a[2], b[2])]);
            assert!(calls[0].1.iter().all(|px| close(*px, averaged)), "{layout:?}: {:?} vs {averaged:?}", calls[0].1[0]);
            // A luma view for another rectangle in between: its picture is a
            // different one everywhere, but only its rectangle is kept.
            let elsewhere = Rect16 { left: 24, top: 0, right: 32, bottom: 32 };
            let between = Avc444 { luma: Some(Avc420 { regions: vec![region(elsewhere)], bitstream: &grey_unit }), chroma: None };
            avc.draw_444(&between, layout, (32, 32), &mut painted(&mut calls)).unwrap();
            assert_eq!(calls[1].0, elsewhere);
            assert!(calls[1].1.iter().all(|px| close(*px, rgb_of(grey))), "{layout:?}: {:?} vs {:?}", calls[1].1[0], rgb_of(grey));
            // Then the chroma view for part of the first rectangle, in a later
            // stream: it combines with the luma view that carried that rectangle,
            // not the grey one that came after.
            let part = Rect16 { left: 4, top: 4, right: 20, bottom: 24 };
            let later = Avc444 { luma: None, chroma: Some(Avc420 { regions: vec![region(part)], bitstream: &chroma_unit }) };
            avc.draw_444(&later, layout, (32, 32), &mut painted(&mut calls)).unwrap();
            assert_eq!(calls.len(), 3);
            assert_eq!(calls[2].0, part);
            for (i, px) in calls[2].1.iter().enumerate() {
                let (x, y) = (4 + i % 16, 4 + i / 16);
                let expected = if (x + y) % 2 == 0 { ra } else { rb };
                assert!(close(*px, expected), "{layout:?} later pixel {x},{y}: {px:?} vs {expected:?}");
            }
        }
    }
}

