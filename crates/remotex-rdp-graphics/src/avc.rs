//! The H.264 the graphics pipeline draws video with, put back into colour.
//!
//! A Windows host that finds the client able to take H.264 does not switch the
//! desktop to it. It goes on drawing text and windows with ClearCodec and
//! Progressive, and hands the parts that move like video — a player, a game, a
//! scrolling page — to AVC420 in the same frames, each surface with an H.264 stream
//! of its own whose picture is the whole surface. The metablock in front of every
//! access unit ([`crate::proto::avc`]) says which rectangles of that picture are to
//! be shown; the rest is whatever the encoder left there.
//!
//! # Who decodes
//!
//! Not this crate. It has no H.264 decoder and is not to grow one: the access
//! units are decoded by whoever composes the pipeline, which is the page, with the
//! browser's own decoder. So the work is in three steps, and the caller drives
//! them for each run of commands:
//!
//! 1. [`scan`] the run for its access units, in the order the compositor will
//!    reach them, and for the surfaces whose streams end in it.
//! 2. Decode each unit, each surface's through a decoder of its own, and supply
//!    the picture under the unit's number
//!    ([`crate::Compositor::supply`]) — the samples the unit's rectangles show, as
//!    the decoder laid them out ([`Picture`]).
//! 3. Compose the run. A command that carries H.264 takes its units' pictures and
//!    paints its rectangles from them; one whose picture was not supplied leaves
//!    its rectangles as they were, as a codec payload that does not decode does.
//!
//! # What is done here
//!
//! AVC420 is the plain case: the masked rectangles converted from full-range
//! BT.709 YUV to RGB, the conversion [MS-RDPEGFX] 3.3.8.3.1 names — done here from
//! the samples rather than left to whatever drew the decoder's picture, because a
//! browser labels this stream's colour differently from one decoder to the next.
//! AVC444 is the same picture at full chroma resolution, carried as two YUV420
//! pictures through the one decoder: a *luma* view holding the luma and a quarter
//! of the chroma, averaged, and a *chroma* view whose three planes hold the other
//! three quarters, packed one of two ways ([`Layout`]). The luma view is kept
//! after it is shown, because the chroma view for a rectangle may come in a later
//! frame and combines with the last luma view that drew it. Putting the views back
//! together is [MS-RDPEGFX] 3.3.8.3.2 and 3.3.8.3.3, including the filter that
//! recovers the averaged samples from their three neighbours.
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use anyhow::{Context as _, Result, bail, ensure};
use rayon::prelude::*;

use crate::proto::avc::{Avc420, Region, access_unit, avc420, avc444};
use crate::proto::gfx::{self, Message, Rect16};
use crate::proto::wire::Malformed;

/// How a YUV444 picture is packed into its two YUV420 views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// AVC444: the chroma view is laid out macroblock by macroblock, the odd rows of
    /// U and V in its luma plane and the odd columns of their even rows in its
    /// chroma planes. [MS-RDPEGFX] 3.3.8.3.2.
    V1,
    /// AVC444v2: the chroma view is laid out over the whole picture, the odd columns
    /// of U and V in the left and right halves of its luma plane and the odd rows of
    /// their even columns in the halves of its chroma planes. [MS-RDPEGFX] 3.3.8.3.3.
    V2,
}

/// The difference from which the recovered chroma sample is used in place of the
/// averaged one — [\[MS-RDPEGFX\] 3.3.8.3.2]'s cutoff: the average is kept only
/// where the reverse changes it by less.
///
/// [\[MS-RDPEGFX\] 3.3.8.3.2]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/8131c1bc-1af8-4907-a05a-f72f4581160f
const FILTER_CUTOFF: i32 = 30;

/// Fewest pixels a rectangle is converted on more than one thread for.
const PARALLEL_FROM: usize = 1 << 16;

/// Where the painted rectangles go: one rectangle of the surface, its rows in the
/// surface's own `RGBX32`, and the bytes between the starts of consecutive rows.
pub(crate) type Paint<'a> = &'a mut dyn FnMut(Rect16, &[u8], usize);

/// Whether a codec is one of the H.264 ones.
pub(crate) fn carries(codec: u16) -> bool {
    matches!(codec, gfx::CODEC_AVC420 | gfx::CODEC_AVC444 | gfx::CODEC_AVC444_V2)
}

/// One command's H.264: the views it carries, each an access unit behind its mask.
/// An AVC420 stream is a luma view alone, and its picture is the whole of it.
pub(crate) struct Stream<'a> {
    /// How the two views pack a YUV444 picture; `None` for AVC420.
    layout: Option<Layout>,
    luma: Option<Avc420<'a>>,
    chroma: Option<Avc420<'a>>,
}

impl<'a> Stream<'a> {
    /// Read a command's payload in one of the codecs [`carries`] names.
    pub(crate) fn read(codec: u16, data: &'a [u8]) -> Result<Self, Malformed> {
        if codec == gfx::CODEC_AVC420 {
            return Ok(Self { layout: None, luma: Some(avc420(data)?), chroma: None });
        }
        let layout = if codec == gfx::CODEC_AVC444 { Layout::V1 } else { Layout::V2 };
        let views = avc444(data)?;
        Ok(Self { layout: Some(layout), luma: views.luma, chroma: views.chroma })
    }

    /// Its access units in the order they go through the decoder: the luma view's,
    /// then the chroma view's. This order is the units' numbering, for [`scan`]
    /// and for the compositor alike.
    fn views(&self) -> impl Iterator<Item = &Avc420<'a>> {
        self.luma.iter().chain(self.chroma.iter())
    }

    /// How many access units it carries.
    pub(crate) fn units(&self) -> u32 {
        self.views().count() as u32
    }

    /// Where the luma and the chroma view's units fall, counted from the stream's
    /// first.
    pub(crate) fn numbers(&self) -> (Option<u32>, Option<u32>) {
        let luma = self.luma.as_ref().map(|_| 0);
        let chroma = self.chroma.as_ref().map(|_| self.units() - 1);
        (luma, chroma)
    }
}

/// How much of a unit's picture its rectangles show, which is how much of it has
/// to be supplied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// None of it: the unit's mask is empty. It is still part of its stream, to be
    /// decoded in its turn.
    Nothing,
    /// The rectangle that bounds its mask, in picture pixels.
    Part(Rect16),
    /// All of it: a view of a YUV444 picture, whose samples are packed across the
    /// whole picture.
    Whole,
}

/// One access unit found in a run of commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unit {
    /// The surface whose stream it belongs to.
    pub surface: u16,
    /// Where it lies in the commands, Annex B: `start..end`.
    pub start: usize,
    pub end: usize,
    /// Whether a decoder can start at it.
    pub key: bool,
    /// The profile, constraint flags and level its parameter sets name, when it
    /// carries them.
    pub profile: Option<[u8; 3]>,
    pub window: Window,
}

/// What [`scan`] finds, in command order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scanned {
    /// An access unit. Its number, for [`crate::Compositor::supply`], is how many
    /// units the run has before it.
    Unit(Unit),
    /// A surface was created or deleted, so the stream that was drawn into that
    /// number is over and its decoder done with. Reported for every surface, since
    /// which of them had a stream is the caller's to know.
    Gone { surface: u16 },
}

/// Find the H.264 access units in a run of commands, and the surfaces whose
/// streams end in it.
///
/// An error is a command that does not decode, which [`crate::Compositor::compose`]
/// refuses in the same words. A payload that does not decode is no error here
/// either: its units are not reported, and composing it paints nothing.
pub fn scan(commands: &[u8]) -> Result<Vec<Scanned>, Malformed> {
    let mut found = Vec::new();
    for message in gfx::messages(commands) {
        match message? {
            Message::CreateSurface { surface, .. } | Message::DeleteSurface { surface } => {
                found.push(Scanned::Gone { surface });
            }
            Message::WireToSurface1 { surface, codec, data, .. } if carries(codec) => {
                let Ok(stream) = Stream::read(codec, data) else { continue };
                for view in stream.views() {
                    let headers = access_unit(view.bitstream);
                    // The view's bytes are a slice of the commands'.
                    let start = view.bitstream.as_ptr() as usize - commands.as_ptr() as usize;
                    let window = match (stream.layout, bounds(&view.regions)) {
                        (_, None) => Window::Nothing,
                        (None, Some(rect)) => Window::Part(rect),
                        (Some(_), Some(_)) => Window::Whole,
                    };
                    found.push(Scanned::Unit(Unit {
                        surface,
                        start,
                        end: start + view.bitstream.len(),
                        key: headers.key,
                        profile: headers.profile,
                        window,
                    }));
                }
            }
            _ => {}
        }
    }
    Ok(found)
}

/// The rectangle that bounds a mask; `None` for an empty one.
fn bounds(regions: &[Region]) -> Option<Rect16> {
    regions.iter().map(|region| region.rect).reduce(|a, b| Rect16 {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    })
}

/// Where a plane's samples are in a picture's bytes: its first, and the bytes
/// between the starts of consecutive rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    pub offset: usize,
    pub stride: usize,
}

/// How a decoder laid a picture's samples out. Both are YUV 4:2:0, eight bits a
/// sample; which one a decoder hands back is its own choice, a software decoder's
/// usually the first and a hardware one's the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Samples {
    /// Three planes: Y, then U and V at half size each way.
    I420 { y: Plane, u: Plane, v: Plane },
    /// Two planes: Y, then U and V interleaved, a pair for every 2x2 of luma.
    Nv12 { y: Plane, uv: Plane },
}

/// A decoded picture, or the part of one a unit's rectangles show
/// ([`Window`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    /// The samples, laid out as `samples` says.
    pub data: Vec<u8>,
    pub samples: Samples,
    /// Where the part held sits in the picture, in luma pixels: an even corner,
    /// since a chroma sample covers two pixels each way.
    pub left: u32,
    pub top: u32,
    /// The size of the part held.
    pub width: u32,
    pub height: u32,
}

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

    fn pixels(&self) -> usize {
        self.width() * self.height()
    }

    /// The region rectangle, kept to the surface and to the part of the picture
    /// held; `None` when nothing of it is inside both.
    fn clip(rect: Rect16, surface: (u32, u32), held: Area) -> Option<Self> {
        let left = usize::from(rect.left).max(held.left);
        let top = usize::from(rect.top).max(held.top);
        let right = usize::from(rect.right).min(surface.0 as usize).min(held.right);
        let bottom = usize::from(rect.bottom).min(surface.1 as usize).min(held.bottom);
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

/// Three planes, borrowed from wherever they are: a supplied picture or a copy
/// kept here. Coordinates are the picture's; `held` is the part the planes cover.
/// The chroma planes are half size each way, their samples `chroma_step` bytes
/// apart: one where each has a plane of its own, two where they are interleaved.
#[derive(Clone, Copy)]
struct View<'a> {
    held: Area,
    y: &'a [u8],
    y_stride: usize,
    u: &'a [u8],
    v: &'a [u8],
    chroma_stride: usize,
    chroma_step: usize,
}

impl<'a> View<'a> {
    /// A supplied picture's planes, once every sample of the part it says it holds
    /// is found to be inside its bytes.
    fn of(picture: &'a Picture) -> Result<Self> {
        let (left, top) = (picture.left as usize, picture.top as usize);
        let (width, height) = (picture.width as usize, picture.height as usize);
        ensure!(width > 0 && height > 0, "a picture with no samples");
        ensure!(left.is_multiple_of(2) && top.is_multiple_of(2), "a picture cut at an odd corner, {left},{top}");
        let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
        // The plane from its first sample on, with its last row's last byte inside.
        let plane = |plane: Plane, rows: usize, row_bytes: usize| {
            let end = (rows - 1)
                .checked_mul(plane.stride)
                .and_then(|n| n.checked_add(row_bytes))
                .and_then(|n| n.checked_add(plane.offset));
            match end {
                Some(end) if end <= picture.data.len() && plane.stride >= row_bytes => {
                    Ok(&picture.data[plane.offset..])
                }
                _ => bail!(
                    "a plane of {rows} rows of {row_bytes} bytes, {} apart from byte {}, in a picture of {} bytes",
                    plane.stride,
                    plane.offset,
                    picture.data.len()
                ),
            }
        };
        let held = Area { left, top, right: left + width, bottom: top + height };
        match picture.samples {
            Samples::I420 { y, u, v } => {
                ensure!(u.stride == v.stride, "chroma planes {} and {} bytes a row", u.stride, v.stride);
                Ok(Self {
                    held,
                    y: plane(y, height, width)?,
                    y_stride: y.stride,
                    u: plane(u, chroma_height, chroma_width)?,
                    v: plane(v, chroma_height, chroma_width)?,
                    chroma_stride: u.stride,
                    chroma_step: 1,
                })
            }
            Samples::Nv12 { y, uv } => {
                let pairs = plane(uv, chroma_height, chroma_width * 2)?;
                Ok(Self {
                    held,
                    y: plane(y, height, width)?,
                    y_stride: y.stride,
                    u: pairs,
                    v: &pairs[1..],
                    chroma_stride: uv.stride,
                    chroma_step: 2,
                })
            }
        }
    }

    fn of_planes(planes: &'a Planes) -> Self {
        Self {
            held: Area { left: 0, top: 0, right: planes.width, bottom: planes.height },
            y: &planes.y,
            y_stride: planes.width,
            u: &planes.u,
            v: &planes.v,
            chroma_stride: planes.width.div_ceil(2),
            chroma_step: 1,
        }
    }

    /// The picture's size, of a view that holds all of it.
    fn size(&self) -> (usize, usize) {
        (self.held.right, self.held.bottom)
    }

    /// One row of luma, from `left` to `right`, both inside the part held.
    fn y_row(&self, y: usize, left: usize, right: usize) -> &'a [u8] {
        let row = (y - self.held.top) * self.y_stride;
        &self.y[row + left - self.held.left..row + right - self.held.left]
    }

    /// Where in the chroma planes the sample over luma pixel `x`, `y` is.
    fn chroma_at(&self, x: usize, y: usize) -> usize {
        (y / 2 - self.held.top / 2) * self.chroma_stride + (x / 2 - self.held.left / 2) * self.chroma_step
    }

    /// A luma sample by its place in the plane, if the plane has it.
    fn y_at(&self, x: usize, y: usize) -> Option<u8> {
        (x < self.held.right && y < self.held.bottom).then(|| self.y[y * self.y_stride + x])
    }

    /// A chroma sample by its place in its plane, if the plane has it.
    fn u_at(&self, x: usize, y: usize) -> Option<u8> {
        self.in_chroma(x, y).then(|| self.u[y * self.chroma_stride + x * self.chroma_step])
    }

    fn v_at(&self, x: usize, y: usize) -> Option<u8> {
        self.in_chroma(x, y).then(|| self.v[y * self.chroma_stride + x * self.chroma_step])
    }

    fn in_chroma(&self, x: usize, y: usize) -> bool {
        x < self.held.right.div_ceil(2) && y < self.held.bottom.div_ceil(2)
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

    /// Take one rectangle of a view of the same size, with the chroma samples that
    /// enclose it.
    fn keep(&mut self, view: &View<'_>, area: Area) {
        let cw = self.width.div_ceil(2);
        for y in area.top..area.bottom {
            self.y[y * self.width..][area.left..area.right].copy_from_slice(view.y_row(y, area.left, area.right));
        }
        for y in (area.top / 2..area.bottom.div_ceil(2)).map(|row| row * 2) {
            for x in (area.left / 2..area.right.div_ceil(2)).map(|column| column * 2) {
                let at = view.chroma_at(x, y);
                self.u[y / 2 * cw + x / 2] = view.u[at];
                self.v[y / 2 * cw + x / 2] = view.v[at];
            }
        }
    }
}

/// What one surface's H.264 stream leaves here between its units: the last luma
/// view it showed, and the working space the conversions use.
#[derive(Default)]
pub(crate) struct Avc {
    luma: Option<Planes>,
    /// The full-resolution chroma of one block of macroblocks, put together from
    /// the two views.
    u444: Vec<u8>,
    v444: Vec<u8>,
    /// The rows of one rectangle, on their way to the surface.
    rows: Vec<u8>,
}

impl Avc {
    /// Paint a stream's rectangles from its units' pictures, the luma view's and
    /// the chroma view's.
    ///
    /// An error is the whole stream's — its rectangles are not painted, or not all
    /// of them.
    pub(crate) fn draw(
        &mut self,
        stream: &Stream<'_>,
        luma: Option<&Picture>,
        chroma: Option<&Picture>,
        surface: (u32, u32),
        paint: Paint<'_>,
    ) -> Result<()> {
        match stream.layout {
            None => self.draw_420(stream, luma, surface, paint),
            Some(layout) => self.draw_444(stream, layout, luma, chroma, surface, paint),
        }
    }

    /// An AVC420 stream: its rectangles, straight from its picture.
    fn draw_420(
        &mut self,
        stream: &Stream<'_>,
        picture: Option<&Picture>,
        surface: (u32, u32),
        paint: Paint<'_>,
    ) -> Result<()> {
        let Some((avc, view)) = shown(stream.luma.as_ref(), picture)? else { return Ok(()) };
        for region in &avc.regions {
            if let Some(area) = Area::clip(region.rect, surface, view.held) {
                convert_420(&view, area, &mut self.rows, paint);
            }
        }
        Ok(())
    }

    /// An AVC444 or AVC444v2 stream: whichever views it carries, their rectangles
    /// painted — a luma view's at half chroma, as [MS-RDPEGFX] 3.3.8.3.2 has it,
    /// unless a chroma view for the same rectangles is in the same stream and
    /// paints them whole.
    fn draw_444(
        &mut self,
        stream: &Stream<'_>,
        layout: Layout,
        luma: Option<&Picture>,
        chroma: Option<&Picture>,
        surface: (u32, u32),
        paint: Paint<'_>,
    ) -> Result<()> {
        if let Some((avc, view)) = shown(stream.luma.as_ref(), luma)? {
            let (width, height) = whole(&view)?;
            let kept = self.luma.get_or_insert_with(Planes::empty);
            kept.fit(width, height);
            let painted_whole = stream.chroma.as_ref().is_some_and(|chroma| same_mask(&chroma.regions, &avc.regions));
            for region in &avc.regions {
                let Some(area) = Area::clip(region.rect, surface, view.held) else { continue };
                kept.keep(&view, area);
                if !painted_whole {
                    convert_420(&view, area, &mut self.rows, paint);
                }
            }
        }
        if let Some((avc, aux)) = shown(stream.chroma.as_ref(), chroma)? {
            let Some(kept) = &self.luma else {
                bail!("a chroma view arrived before any luma view to combine it with");
            };
            let picture = whole(&aux)?;
            ensure!(
                picture == (kept.width, kept.height),
                "the chroma view is {}x{} and the luma view {}x{}",
                picture.0,
                picture.1,
                kept.width,
                kept.height
            );
            let main = View::of_planes(kept);
            for region in &avc.regions {
                let Some(area) = Area::clip(region.rect, surface, main.held) else { continue };
                // Whole macroblocks, then the mask: the chroma view is laid out by
                // macroblock, and the filter reads across every 2x2.
                let block = area.aligned(16, picture);
                combine(&main, &aux, layout, block, &mut self.u444, &mut self.v444);
                convert_444(&main, block, &self.u444, &self.v444, area, &mut self.rows, paint);
            }
        }
        Ok(())
    }
}

/// A view that shows something, with its picture's planes: `None` for a view the
/// stream does not carry or whose mask is empty, and an error for one whose
/// picture was not supplied or does not hold what it says.
fn shown<'s, 'p>(
    view: Option<&'s Avc420<'_>>,
    picture: Option<&'p Picture>,
) -> Result<Option<(&'s Avc420<'s>, View<'p>)>> {
    let Some(view) = view.filter(|view| !view.regions.is_empty()) else { return Ok(None) };
    let picture = picture.context("no picture was supplied for its access unit")?;
    Ok(Some((view, View::of(picture)?)))
}

/// The size of the picture a view holds all of. A YUV444 picture's views are
/// packed across the whole of it, so a part is no use.
fn whole(view: &View<'_>) -> Result<(usize, usize)> {
    ensure!(
        view.held.left == 0 && view.held.top == 0,
        "a part of a picture from {},{} where the whole of it is needed",
        view.held.left,
        view.held.top
    );
    Ok(view.size())
}

/// Whether two region masks name the same rectangles.
fn same_mask(a: &[Region], b: &[Region]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.rect == b.rect)
}

/// The surface's pixel for one full-range BT.709 sample, by [MS-RDPEGFX]
/// 3.3.8.3.1's matrix in eight fractional bits.
fn rgbx(y: u8, u: u8, v: u8) -> [u8; 4] {
    let (y, u, v) = (i32::from(y) << 8, i32::from(u) - 128, i32::from(v) - 128);
    let clamp = |n: i32| (n >> 8).clamp(0, 255) as u8;
    [clamp(y + 403 * v), clamp(y - 48 * u - 120 * v), clamp(y + 475 * u), 0]
}

/// Fill `rows` with `area`'s pixels, a row at a time through `row`, on more than
/// one thread where the rectangle is large enough to be worth it.
fn fill(rows: &mut Vec<u8>, area: Area, row: impl Fn(usize, &mut [u8]) + Sync) {
    let stride = area.width() * 4;
    rows.clear();
    rows.resize(stride * area.height(), 0);
    if area.pixels() < PARALLEL_FROM {
        rows.chunks_exact_mut(stride).enumerate().for_each(|(at, out)| row(area.top + at, out));
    } else {
        crate::pool::install(|| {
            rows.par_chunks_exact_mut(stride).enumerate().for_each(|(at, out)| row(area.top + at, out));
        });
    }
}

/// Convert `area` of a YUV420 view to the surface's pixels and paint it, each
/// chroma sample shown under the 2x2 of luma it was coded for.
fn convert_420(view: &View<'_>, area: Area, rows: &mut Vec<u8>, paint: Paint<'_>) {
    fill(rows, area, |y, out| {
        let luma = view.y_row(y, area.left, area.right);
        for (at, (px, luma)) in out.as_chunks_mut::<4>().0.iter_mut().zip(luma).enumerate() {
            let chroma = view.chroma_at(area.left + at, y);
            *px = rgbx(*luma, view.u[chroma], view.v[chroma]);
        }
    });
    paint(area.rect16(), rows, area.width() * 4);
}

/// Convert `area` of the picture to the surface's pixels from the luma view's Y
/// and the block's combined chroma, and paint it.
fn convert_444(
    main: &View<'_>,
    block: Area,
    u444: &[u8],
    v444: &[u8],
    area: Area,
    rows: &mut Vec<u8>,
    paint: Paint<'_>,
) {
    fill(rows, area, |y, out| {
        let luma = main.y_row(y, area.left, area.right);
        let chroma = (y - block.top) * block.width() + area.left - block.left;
        let (u, v) = (&u444[chroma..chroma + area.width()], &v444[chroma..chroma + area.width()]);
        for (px, (luma, (u, v))) in out.as_chunks_mut::<4>().0.iter_mut().zip(luma.iter().zip(u.iter().zip(v))) {
            *px = rgbx(*luma, *u, *v);
        }
    });
    paint(area.rect16(), rows, area.width() * 4);
}

/// Put the two views' chroma back together over `block`, into `u444` and `v444`
/// at the block's width.
///
/// Every sample but the even rows' even columns is carried whole in the chroma
/// view, where `layout` says; those come first. The even-even samples are the
/// luma view's chroma, which the host averaged over each 2x2, and come second,
/// recovered from the average and the three neighbours just written. A sample the
/// chroma view would hold past its picture's edge — the last rows of a picture
/// that is not whole macroblocks — is left at the average.
fn combine(main: &View<'_>, aux: &View<'_>, layout: Layout, block: Area, u444: &mut Vec<u8>, v444: &mut Vec<u8>) {
    let width = block.width();
    u444.clear();
    u444.resize(width * block.height(), 0);
    v444.clear();
    v444.resize(width * block.height(), 0);
    // V2 puts V's samples in the right half of each chroma-view plane.
    let aux_width = aux.held.right;
    let (half, quarter) = (aux_width / 2, aux_width / 4);
    let averaged = |x: usize, y: usize| (main.u_at(x >> 1, y >> 1).unwrap_or(128), main.v_at(x >> 1, y >> 1).unwrap_or(128));
    for y in block.top..block.bottom {
        let row = (y - block.top) * width;
        for x in block.left..block.right {
            let carried = match layout {
                Layout::V1 if y & 1 == 1 => {
                    // B4 and B5: the odd rows of U then of V, eight of each per
                    // macroblock, in the chroma view's luma plane.
                    let r = (y & !15) + ((y & 15) >> 1);
                    aux.y_at(x, r).zip(aux.y_at(x, r + 8))
                }
                Layout::V1 if x & 1 == 1 => {
                    // B6 and B7: the odd columns of the even rows, in its chroma planes.
                    aux.u_at(x >> 1, y >> 1).zip(aux.v_at(x >> 1, y >> 1))
                }
                Layout::V2 if x & 1 == 1 => {
                    // B4 and B5: the odd columns, every row, in the left and right
                    // halves of the chroma view's luma plane.
                    aux.y_at(x >> 1, y).zip(aux.y_at(half + (x >> 1), y))
                }
                Layout::V2 if y & 1 == 1 => {
                    // B6 to B9: the odd rows of the even columns, columns 4n in its U
                    // plane and 4n+2 in its V plane, U's samples in each plane's left
                    // half and V's in its right.
                    let (col, r) = (x >> 2, y >> 1);
                    if x & 2 == 0 {
                        aux.u_at(col, r).zip(aux.u_at(quarter + col, r))
                    } else {
                        aux.v_at(col, r).zip(aux.v_at(quarter + col, r))
                    }
                }
                _ => continue,
            };
            let (u, v) = carried.unwrap_or_else(|| averaged(x, y));
            u444[row + x - block.left] = u;
            v444[row + x - block.left] = v;
        }
    }
    for y in (block.top..block.bottom).step_by(2) {
        let row = (y - block.top) * width;
        for x in (block.left..block.right).step_by(2) {
            let at = row + x - block.left;
            let (u, v) = averaged(x, y);
            let whole = x + 1 < block.right && y + 1 < block.bottom;
            u444[at] = if whole { unfilter(u, u444[at + 1], u444[at + width], u444[at + width + 1]) } else { u };
            v444[at] = if whole { unfilter(v, v444[at + 1], v444[at + width], v444[at + width + 1]) } else { v };
        }
    }
}

/// The 2x2's top-left sample back from the host's average of the four and the
/// other three — taken when it differs from the average by the cutoff or more,
/// since past a quantized average the reverse can be further from the truth than
/// the average was. [MS-RDPEGFX] 3.3.8.3.2.
fn unfilter(average: u8, right: u8, below: u8, diagonal: u8) -> u8 {
    let average = i32::from(average);
    let reversed = average * 4 - i32::from(right) - i32::from(below) - i32::from(diagonal);
    if (average - reversed).abs() >= FILTER_CUTOFF { reversed.clamp(0, 255) as u8 } else { average as u8 }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Pictures as a decoder hands them back, for the tests here and the
    //! compositor's.

    use super::{Picture, Plane, Samples};

    /// A whole picture of one colour, three planes packed tight: Y then U then V.
    pub(crate) fn flat(width: u32, height: u32, yuv: [u8; 3]) -> Picture {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut data = vec![yuv[0]; w * h];
        data.resize(w * h + cw * ch, yuv[1]);
        data.resize(w * h + 2 * cw * ch, yuv[2]);
        packed(data, 0, 0, width, height)
    }

    /// The part of a picture from `left`, `top`, its three planes packed tight in
    /// `data`.
    pub(crate) fn packed(data: Vec<u8>, left: u32, top: u32, width: u32, height: u32) -> Picture {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let samples = Samples::I420 {
            y: Plane { offset: 0, stride: w },
            u: Plane { offset: w * h, stride: cw },
            v: Plane { offset: w * h + cw * ch, stride: cw },
        };
        Picture { data, samples, left, top, width, height }
    }

    /// The pixel a full-range BT.709 YUV sample is, by [MS-RDPEGFX] 3.3.8.3.1's
    /// matrix in floating point: within a step of the integer form.
    pub(crate) fn rgb_of(yuv: [u8; 3]) -> [u8; 3] {
        let (y, u, v) = (f64::from(yuv[0]), f64::from(yuv[1]) - 128.0, f64::from(yuv[2]) - 128.0);
        let clamp = |n: f64| n.clamp(0.0, 255.0) as u8;
        [clamp(y + 1.5748 * v), clamp(y - 0.1873 * u - 0.4681 * v), clamp(y + 1.8556 * u)]
    }

    /// Whether a painted pixel is the colour expected, to the step the integer
    /// conversion may be off by.
    pub(crate) fn close(actual: &[u8], expected: [u8; 3]) -> bool {
        actual.iter().zip(expected).all(|(a, e)| a.abs_diff(e) <= 1)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{close, flat, packed, rgb_of};
    use super::*;
    use crate::proto::avc::tests::{metablock, wrap444};
    use crate::proto::gfx::{CMD_CREATE_SURFACE, CMD_DELETE_SURFACE, CMD_WIRE_TO_SURFACE_1, PIXEL_XRGB_8888, pdu};
    use crate::proto::wire::Writer;

    /// A YUV444 picture, and the two YUV420 views a host makes of it under either
    /// layout — the forward side of [MS-RDPEGFX] 3.3.8.3.2 and 3.3.8.3.3, written
    /// from the specification's tables so the combine is checked against something
    /// other than itself.
    struct Original {
        width: usize,
        height: usize,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    }

    impl Original {
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

    /// A view as a decoder would hand it back: the whole picture, packed tight.
    fn decoded(planes: &Planes) -> Picture {
        packed([&planes.y[..], &planes.u, &planes.v].concat(), 0, 0, planes.width as u32, planes.height as u32)
    }

    /// A 32x32 picture whose chroma varies per pixel but whose every 2x2 averages
    /// exactly, so the combine has to land on the original sample for sample.
    fn textured() -> Original {
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
        Original { width, height, y, u, v }
    }

    fn combined(picture: &Original, layout: Layout, block: Area) -> (Vec<u8>, Vec<u8>) {
        let (luma, chroma) = (picture.luma_view(), picture.chroma_view(layout));
        let (mut u444, mut v444) = (Vec::new(), Vec::new());
        combine(&View::of_planes(&luma), &View::of_planes(&chroma), layout, block, &mut u444, &mut v444);
        (u444, v444)
    }

    /// Both layouts put every chroma sample back where the picture had it — the
    /// carried ones by their tables, the averaged ones through the filter, whose
    /// reverse is under the cutoff by construction here.
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
    fn the_filter_recovers_a_corner_only_from_the_cutoff_up() {
        // [100, 200, 200, 200]: average 175, reverse 100, 75 apart.
        assert_eq!(unfilter(175, 200, 200, 200), 100);
        // [100, 110, 110, 112]: average 108, reverse 100, 8 apart.
        assert_eq!(unfilter(108, 110, 110, 112), 108);
        // The cutoff itself is the reverse's: [80, 120, 120, 120], average 110,
        // reverse 80, 30 apart; and [81, 120, 120, 119], 29 apart, keeps the average.
        assert_eq!(unfilter(110, 120, 120, 120), 80);
        assert_eq!(unfilter(110, 120, 120, 119), 110);
        // The reverse is clamped when the neighbours overshoot it.
        assert_eq!(unfilter(10, 250, 250, 250), 0);
    }

    /// A clip keeps a rectangle to the surface and to the part of the picture held,
    /// and drops one outside either; alignment widens within the picture.
    #[test]
    fn regions_are_clipped_then_aligned_within_the_picture() {
        let rect = Rect16 { left: 5, top: 3, right: 40, bottom: 60 };
        let held = |left, top, right, bottom| Area { left, top, right, bottom };
        assert_eq!(Area::clip(rect, (32, 100), held(0, 0, 48, 48)), Some(Area { left: 5, top: 3, right: 32, bottom: 48 }));
        assert_eq!(Area::clip(rect, (5, 100), held(0, 0, 48, 48)), None);
        assert_eq!(Area::clip(rect, (64, 64), held(8, 4, 20, 30)), Some(Area { left: 8, top: 4, right: 20, bottom: 30 }));
        assert_eq!(Area::clip(rect, (64, 64), held(40, 0, 48, 48)), None);
        let area = Area { left: 5, top: 3, right: 31, bottom: 47 };
        assert_eq!(area.aligned(16, (48, 48)), Area { left: 0, top: 0, right: 32, bottom: 48 });
        assert_eq!(area.aligned(16, (36, 40)), Area { left: 0, top: 0, right: 32, bottom: 40 });
    }

    /// The rectangles a paint reports, with their pixels.
    fn painted(calls: &mut Vec<(Rect16, Vec<[u8; 4]>)>) -> impl FnMut(Rect16, &[u8], usize) + '_ {
        move |rect, rows, stride| {
            let width = usize::from(rect.width());
            let pixels = (0..usize::from(rect.height()))
                .flat_map(|row| rows[row * stride..row * stride + width * 4].as_chunks::<4>().0.iter().copied())
                .collect();
            calls.push((rect, pixels));
        }
    }

    fn region(rect: (u16, u16, u16, u16)) -> Region {
        let rect = Rect16 { left: rect.0, top: rect.1, right: rect.2, bottom: rect.3 };
        Region { rect, qp: 0, progressive: false, quality: 100 }
    }

    fn stream_420(regions: Vec<Region>) -> Stream<'static> {
        Stream { layout: None, luma: Some(Avc420 { regions, bitstream: &[] }), chroma: None }
    }

    /// The matrix, at its corners: grey stays grey, and each colour difference
    /// moves the channels the specification says it moves.
    #[test]
    fn a_sample_becomes_the_pixel_the_specification_names() {
        assert_eq!(rgbx(0, 128, 128), [0, 0, 0, 0]);
        assert_eq!(rgbx(255, 128, 128), [255, 255, 255, 0]);
        assert_eq!(rgbx(90, 128, 128), [90, 90, 90, 0]);
        for yuv in [[140, 90, 170], [60, 200, 100], [200, 30, 240], [16, 128, 240], [235, 240, 16]] {
            let px = rgbx(yuv[0], yuv[1], yuv[2]);
            assert!(close(&px[..3], rgb_of(yuv)), "{yuv:?}: {px:?} vs {:?}", rgb_of(yuv));
        }
    }

    /// An AVC420 stream paints its masked rectangles, and only those, in the colour
    /// the picture carries.
    #[test]
    fn an_avc420_stream_paints_its_region_in_colour() {
        let yuv = [140, 90, 170];
        let picture = flat(32, 32, yuv);
        let mut avc = Avc::default();
        let mut calls = Vec::new();
        avc.draw(&stream_420(vec![region((3, 5, 20, 30))]), Some(&picture), None, (32, 32), &mut painted(&mut calls))
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, Rect16 { left: 3, top: 5, right: 20, bottom: 30 });
        assert_eq!(calls[0].1.len(), 17 * 25);
        assert!(calls[0].1.iter().all(|px| close(&px[..3], rgb_of(yuv)) && px[3] == 0), "{:?}", calls[0].1[0]);

        // A rectangle outside the surface paints nothing, an empty mask needs no
        // picture, and a mask with no picture is a fault.
        let mut calls = Vec::new();
        avc.draw(&stream_420(vec![region((40, 0, 50, 8))]), Some(&picture), None, (32, 32), &mut painted(&mut calls))
            .unwrap();
        avc.draw(&stream_420(vec![]), None, None, (32, 32), &mut painted(&mut calls)).unwrap();
        assert!(calls.is_empty());
        let err = avc
            .draw(&stream_420(vec![region((0, 0, 8, 8))]), None, None, (32, 32), &mut painted(&mut calls))
            .unwrap_err();
        assert!(format!("{err:#}").contains("no picture"), "{err:#}");
    }

    /// A picture whose every sample says where it is: luma by its column and row,
    /// chroma by its own.
    fn graded(width: usize, height: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let (cw, ch) = (width / 2, height / 2);
        let y = (0..width * height).map(|i| (i % width * 3 + i / width * 5) as u8).collect();
        let u = (0..cw * ch).map(|i| 40 + (i % cw * 7) as u8).collect();
        let v = (0..cw * ch).map(|i| 200 - (i / cw * 9) as u8).collect();
        (y, u, v)
    }

    /// The part of a picture a unit's window names is all that is supplied, and a
    /// decoder lays it out its own way — planes apart or chroma interleaved, rows
    /// wider than their samples. Each paints what the whole picture, packed tight,
    /// paints.
    #[test]
    fn a_part_of_a_picture_paints_as_the_whole_does_in_either_layout() {
        let (width, height) = (32usize, 24usize);
        let (y, u, v) = graded(width, height);
        let whole = packed([&y[..], &u, &v].concat(), 0, 0, width as u32, height as u32);
        let mask = vec![region((9, 7, 27, 20))];
        let paint_with = |picture: &Picture| {
            let mut calls = Vec::new();
            Avc::default()
                .draw(&stream_420(mask.clone()), Some(picture), None, (32, 24), &mut painted(&mut calls))
                .unwrap();
            calls
        };
        let expected = paint_with(&whole);
        assert_eq!(expected[0].1.len(), 18 * 13);
        // The first pixel is the sample at 9,7 with the chroma over it, at 4,3.
        let first = rgbx(y[7 * width + 9], u[3 * 16 + 4], v[3 * 16 + 4]);
        assert_eq!(expected[0].1[0], first);

        // The window: the mask widened to whole chroma samples, 8,6 to 28,20.
        let (left, top, w, h) = (8usize, 6usize, 20usize, 14usize);
        let cut = |plane: &[u8], stride: usize, left: usize, top: usize, w: usize, h: usize| -> Vec<Vec<u8>> {
            (top..top + h).map(|row| plane[row * stride + left..row * stride + left + w].to_vec()).collect()
        };
        let (ys, us, vs) = (cut(&y, 32, left, top, w, h), cut(&u, 16, 4, 3, 10, 7), cut(&v, 16, 4, 3, 10, 7));

        // Three planes, each row padded past its samples and the planes apart.
        let mut data = vec![0xEE; 5];
        let pad = |data: &mut Vec<u8>, rows: &[Vec<u8>], stride: usize| {
            let offset = data.len();
            for row in rows {
                data.extend_from_slice(row);
                data.resize(data.len() + stride - row.len(), 0xEE);
            }
            Plane { offset, stride }
        };
        let samples = Samples::I420 { y: pad(&mut data, &ys, 24), u: pad(&mut data, &us, 13), v: pad(&mut data, &vs, 13) };
        let planar = Picture { data, samples, left: 8, top: 6, width: 20, height: 14 };
        assert_eq!(paint_with(&planar), expected);

        // Two planes, the chroma interleaved.
        let mut data = Vec::new();
        let y_plane = pad(&mut data, &ys, 20);
        let pairs: Vec<Vec<u8>> =
            us.iter().zip(&vs).map(|(u, v)| u.iter().zip(v).flat_map(|(u, v)| [*u, *v]).collect()).collect();
        let samples = Samples::Nv12 { y: y_plane, uv: pad(&mut data, &pairs, 22) };
        let interleaved = Picture { data, samples, left: 8, top: 6, width: 20, height: 14 };
        assert_eq!(paint_with(&interleaved), expected);

        // A part that does not reach the mask's edge paints what it holds of it.
        let short = Picture { height: 12, ..planar.clone() };
        let calls = paint_with(&short);
        assert_eq!(calls[0].0, Rect16 { left: 9, top: 7, right: 27, bottom: 18 });
        assert_eq!(calls[0].1[..], expected[0].1[..18 * 11]);
    }

    /// A picture that says it holds more than its bytes do is refused before a
    /// sample is read, as is one cut at an odd corner.
    #[test]
    fn a_picture_that_does_not_hold_what_it_says_is_refused() {
        let good = flat(16, 16, [1, 2, 3]);
        let draw = |picture: &Picture| {
            Avc::default().draw(&stream_420(vec![region((0, 0, 16, 16))]), Some(picture), None, (16, 16), &mut |_, _, _| {})
        };
        draw(&good).unwrap();
        let mut short = good.clone();
        short.data.pop();
        assert!(format!("{:#}", draw(&short).unwrap_err()).contains("a plane of"));
        let tall = Picture { height: 18, ..good.clone() };
        assert!(draw(&tall).is_err());
        let odd = Picture { left: 1, ..good.clone() };
        assert!(format!("{:#}", draw(&odd).unwrap_err()).contains("odd corner"));
        let narrow = Picture { samples: Samples::Nv12 { y: Plane { offset: 0, stride: 8 }, uv: Plane { offset: 256, stride: 16 } }, ..good.clone() };
        assert!(draw(&narrow).is_err(), "a stride shorter than a row");
        let empty = Picture { width: 0, ..good };
        assert!(draw(&empty).is_err());
    }

    /// Both views in one stream paint the rectangle at full chroma; a chroma view on
    /// its own later combines with the luma view kept from before; a chroma view
    /// before any luma view is refused.
    #[test]
    fn an_avc444_stream_combines_its_views() {
        // Chroma the luma view alone would get wrong: a 2x2 checker of two colours
        // averages to a third, and only the combined picture has the right one.
        let (a, b) = ([120u8, 60, 200], [120u8, 200, 60]);
        let mut picture = Original { width: 32, height: 32, y: vec![120; 32 * 32], u: vec![0; 32 * 32], v: vec![0; 32 * 32] };
        for i in 0..32 * 32 {
            let (x, y) = (i % 32, i / 32);
            let c = if (x + y) % 2 == 0 { a } else { b };
            picture.u[i] = c[1];
            picture.v[i] = c[2];
        }
        let whole = (0, 0, 32, 32);
        let view = |rect| Some(Avc420 { regions: vec![region(rect)], bitstream: &[][..] });
        for layout in [Layout::V1, Layout::V2] {
            let (luma, chroma) = (decoded(&picture.luma_view()), decoded(&picture.chroma_view(layout)));
            let stream = |luma, chroma| Stream { layout: Some(layout), luma, chroma };

            let mut avc = Avc::default();
            let mut calls = Vec::new();
            let err = avc
                .draw(&stream(None, view(whole)), None, Some(&chroma), (32, 32), &mut painted(&mut calls))
                .unwrap_err();
            assert!(format!("{err}").contains("before any luma view"), "{err}");

            avc.draw(&stream(view(whole), view(whole)), Some(&luma), Some(&chroma), (32, 32), &mut painted(&mut calls))
                .unwrap();
            assert_eq!(calls.len(), 1, "one paint for one mask on both views: {layout:?}");
            let (ra, rb) = (rgbx(a[0], a[1], a[2]), rgbx(b[0], b[1], b[2]));
            for (i, px) in calls[0].1.iter().enumerate() {
                let expected = if (i % 32 + i / 32) % 2 == 0 { ra } else { rb };
                assert_eq!(*px, expected, "{layout:?} pixel {i}");
            }

            // A luma view alone paints its rectangles at half chroma — the average
            // of the checker — and is kept.
            let mut avc = Avc::default();
            let mut calls = Vec::new();
            avc.draw(&stream(view(whole), None), Some(&luma), None, (32, 32), &mut painted(&mut calls)).unwrap();
            let mean = |p: u8, q: u8| ((u16::from(p) + u16::from(q)) / 2) as u8;
            let averaged = rgbx(120, mean(a[1], b[1]), mean(a[2], b[2]));
            assert!(calls[0].1.iter().all(|px| *px == averaged), "{layout:?}: {:?} vs {averaged:?}", calls[0].1[0]);
            // A luma view for another rectangle in between: its picture is a
            // different one everywhere, but only its rectangle is kept.
            let grey = [200u8, 128, 128];
            let elsewhere = (24, 0, 32, 32);
            avc.draw(&stream(view(elsewhere), None), Some(&flat(32, 32, grey)), None, (32, 32), &mut painted(&mut calls))
                .unwrap();
            assert_eq!(calls[1].0, region(elsewhere).rect);
            assert!(calls[1].1.iter().all(|px| *px == [200, 200, 200, 0]), "{layout:?}: {:?}", calls[1].1[0]);
            // Then the chroma view for part of the first rectangle, in a later
            // stream: it combines with the luma view that carried that rectangle,
            // not the grey one that came after.
            let part = (4, 4, 20, 24);
            avc.draw(&stream(None, view(part)), None, Some(&chroma), (32, 32), &mut painted(&mut calls)).unwrap();
            assert_eq!(calls.len(), 3);
            assert_eq!(calls[2].0, region(part).rect);
            for (i, px) in calls[2].1.iter().enumerate() {
                let (x, y) = (4 + i % 16, 4 + i / 16);
                let expected = if (x + y) % 2 == 0 { ra } else { rb };
                assert_eq!(*px, expected, "{layout:?} later pixel {x},{y}");
            }

            // A part of a picture is no use to a view packed across the whole of it.
            let cut = Picture { left: 16, ..luma.clone() };
            let err = Avc::default()
                .draw(&stream(view(whole), None), Some(&cut), None, (32, 32), &mut painted(&mut calls))
                .unwrap_err();
            assert!(format!("{err}").contains("the whole of it"), "{err}");
        }
    }

    /// A picture that is not whole macroblocks: the first layout's chroma view would
    /// hold the last rows' samples past its edge, and they are left at the average
    /// rather than read from there.
    #[test]
    fn a_picture_that_is_not_whole_macroblocks_combines_without_reading_past_it() {
        let luma = Planes { width: 32, height: 24, y: vec![90; 32 * 24], u: vec![70; 16 * 12], v: vec![180; 16 * 12] };
        let chroma = Planes { width: 32, height: 24, y: vec![70; 32 * 24], u: vec![70; 16 * 12], v: vec![180; 16 * 12] };
        let block = Area { left: 0, top: 0, right: 32, bottom: 24 };
        for layout in [Layout::V1, Layout::V2] {
            let (mut u444, mut v444) = (Vec::new(), Vec::new());
            combine(&View::of_planes(&luma), &View::of_planes(&chroma), layout, block, &mut u444, &mut v444);
            // Rows 16 to 23 are half a macroblock: under the first layout V's odd
            // rows would be in rows 24 to 27 of the chroma view's luma plane.
            assert_eq!((u444[23 * 32 + 5], v444[23 * 32 + 5]), if layout == Layout::V1 { (70, 180) } else { (70, 70) });
        }
    }

    fn wire(surface: u16, codec: u16, data: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16_le(surface);
        w.u16_le(codec);
        w.u8(PIXEL_XRGB_8888);
        for side in [0u16, 0, 64, 64] {
            w.u16_le(side);
        }
        w.u32_le(u32::try_from(data.len()).unwrap());
        w.bytes(data);
        pdu(CMD_WIRE_TO_SURFACE_1, &w.finish())
    }

    /// A scan names every access unit in the order the compositor reaches them,
    /// with where its bytes are, whether a decoder can start at it, and how much of
    /// its picture is shown — and the surfaces whose streams are over.
    #[test]
    fn a_scan_finds_the_units_and_the_surfaces_that_are_gone() {
        let key = [0, 0, 0, 1, 0x67, 0x4D, 0x40, 0x20, 0, 0, 0, 1, 0x65, 0x88];
        let delta = [0, 0, 0, 1, 0x41, 0x9A];
        let masked = |rects: &[(u16, u16, u16, u16)], unit: &[u8]| {
            let specs: Vec<_> = rects.iter().map(|rect| (*rect, 20, 100)).collect();
            [metablock(&specs), unit.to_vec()].concat()
        };
        let create = {
            let mut w = Writer::new();
            w.u16_le(3);
            w.u16_le(64);
            w.u16_le(64);
            w.u8(PIXEL_XRGB_8888);
            pdu(CMD_CREATE_SURFACE, &w.finish())
        };
        let commands = [
            create,
            wire(3, gfx::CODEC_AVC420, &masked(&[(8, 4, 20, 12), (30, 2, 40, 9)], &key)),
            // Not H.264, and H.264 that does not read: neither is a unit.
            wire(3, gfx::CODEC_UNCOMPRESSED, &[0; 16]),
            wire(3, gfx::CODEC_AVC420, &[9, 0, 0, 0]),
            wire(5, gfx::CODEC_AVC444_V2, &wrap444(Some(&masked(&[(0, 0, 16, 16)], &delta)), Some(&masked(&[], &delta)))),
            pdu(CMD_DELETE_SURFACE, &5u16.to_le_bytes()),
        ]
        .concat();
        let found = scan(&commands).unwrap();
        let unit = |at: usize| match found[at] {
            Scanned::Unit(unit) => unit,
            other => panic!("{other:?}"),
        };
        assert_eq!(found.len(), 5);
        assert_eq!(found[0], Scanned::Gone { surface: 3 });
        let first = unit(1);
        assert_eq!((first.surface, first.key, first.profile), (3, true, Some([0x4D, 0x40, 0x20])));
        assert_eq!(first.window, Window::Part(Rect16 { left: 8, top: 2, right: 40, bottom: 12 }));
        assert_eq!(&commands[first.start..first.end], &key);
        let (luma, chroma) = (unit(2), unit(3));
        assert_eq!((luma.surface, luma.key, luma.profile, luma.window), (5, false, None, Window::Whole));
        assert_eq!(&commands[luma.start..luma.end], &delta);
        assert_eq!(chroma.window, Window::Nothing, "an empty mask shows nothing, and is still decoded");
        assert_eq!(&commands[chroma.start..chroma.end], &delta);
        assert!(chroma.start > luma.end);
        assert_eq!(found[4], Scanned::Gone { surface: 5 });

        // A command that does not decode is refused, as composing it would be.
        assert!(scan(&commands[..commands.len() - 1]).is_err());
    }
}
