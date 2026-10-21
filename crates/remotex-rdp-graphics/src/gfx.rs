//! The graphics pipeline's picture: surfaces, and how they reach the framebuffer.
//!
//! Under MS-RDPEGFX the server does not paint the desktop. It paints *surfaces* —
//! off-screen pictures it creates and sizes — and maps them onto the output at an
//! origin; the desktop is what the mapped surfaces show. Drawing arrives between a
//! StartFrame and an EndFrame, and only at the EndFrame is the output meant to
//! change, so this module keeps each surface's own pixels and the rectangles of it
//! that have changed since the last frame, and at the EndFrame copies those
//! rectangles of every mapped surface into the [`Framebuffer`]. That is the shape
//! FreeRDP's `libfreerdp/gdi/gfx.c` has, for the same reason: a surface command is
//! not yet a change to the screen.
//!
//! # What is decoded here
//!
//! Uncompressed rectangles and the planar codec of [`planar`] — the same codec a
//! bitmap update uses, the rows the right way up this time; ClearCodec and
//! RemoteFX Progressive. H.264 is not among them: a host sends it only to a session
//! whose capability advertise takes it, which is a passed pipeline's
//! ([`gfx::caps_advertise`]), and its access units are decoded by whoever composes
//! that pipeline and painted here from the pictures they supply
//! ([`Graphics::supply`], [`crate::avc`]). Every other codec a server names is
//! counted and left unpainted, with the first sighting of each said once in the
//! log: that count is what decides which decoder is written next, and a codec this
//! client cannot read is a hole in the picture rather than the end of the session.
//!
//! # What the page's GPU shows
//!
//! A page that composes a passed pipeline may leave an H.264 picture on its GPU
//! and draw the masked rectangles from there ([`crate::avc`]), since copying the
//! samples out of a hardware decoder costs more than the decode. Those rectangles
//! are then not in the surface's pixels: the surface records them as shown from
//! the GPU, present copies everything else, and the page draws them after
//! ([`GpuPaint`]). A command that writes over one takes it back, its pixels the
//! surface's again. A command that reads one — a copy, a cache entry, a ClearCodec
//! rectangle drawn over it — needs its pixels here, so before a run is composed
//! [`Graphics::plan`] names what the run reads of them, and the page reads those
//! back off its GPU into the surface ([`Graphics::restore`]). The same plan says
//! which of the run's pictures may be shown so: one whose frame ends in the run,
//! on a surface mapped wholly inside the output with no other over it, and that
//! nothing later in the run reads.
//!
//! [`Graphics::receive`] is the whole of it: one channel PDU in, and out come the
//! things the session has to act on that the framebuffer cannot show — the output
//! being redefined, the rectangles painted, and each frame's end.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result};
use log::{debug, info, warn};

use crate::avc::{self, Avc, Picture};
use crate::fidelity::Fidelity;
use crate::framebuffer::{Framebuffer, Rect, affordable, stage};
use crate::proto::bitmap::MAX_DESKTOP_BYTES;
use crate::proto::gfx::{self, Message, Point16, Rect16};
use crate::proto::wire::Malformed;
use crate::proto::{clear, planar, progressive, zgfx};

/// Most rectangles a surface holds as changed before two of them are merged to
/// make room — coarser, never longer. See [`stage`] for which two.
const DAMAGE_CAP: usize = 64;

/// Most rectangles a surface has shown from the GPU before the next run's plan
/// has them all read back. A command that writes over part of one leaves up to
/// four pieces of it, and a unit whose mask overlaps the last one's cuts it up;
/// pieces that line up are joined again ([`join_shown`]), so a desktop drawn all
/// in H.264 holds a few, and this is for a pattern that never lines up.
const GPU_CAP: usize = 512;

/// Most bytes the cache slots hold together. The caps this client advertises promise
/// the host a 16 MiB cache (`SMALL_CACHE`), so a host that keeps its word never comes
/// near this; one that does not is refused the entry rather than the process's memory.
const CACHE_BUDGET: usize = 64 << 20;

/// Something the pipeline did that the session has to act on, in the order it
/// happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// The host confirmed the pipeline: everything it draws from here on comes
    /// inside a frame. Reported before the first paint, which is what a consumer
    /// that paces frames itself needs to know before it receives one.
    Confirmed,
    /// The output was laid out again: its size, and where each monitor is in it,
    /// the primary first — one, the whole of it, when the host's definitions do
    /// not make it up ([`gfx::arrangement`]). The framebuffer has already been
    /// resized and cleared; the caller is what tells the world.
    Reset { width: u32, height: u32, monitors: Vec<gfx::Placed> },
    /// This rectangle of the framebuffer was painted.
    Paint(Rect),
    /// The server finished frame `id`, and this client has finished `decoded` in all.
    Frame { id: u32, decoded: u32 },
    /// One message off the channel for whoever composes the desktop in this
    /// client's place ([`Graphics::passing`]): as the host sent it, still in its
    /// bulk compression, which whoever composes it unwraps with a history of its
    /// own ([`crate::Compositor::unwrap`]). Every message is passed, in the order
    /// they came, since each is compressed against all the ones before it.
    /// `frames` are the frames its commands end, in order: each is owed an
    /// acknowledgement once whoever composes the message has.
    Passed { wrapped: Vec<u8>, frames: Vec<u32> },
    /// A passed pipeline drew with something it had not drawn with before: what it
    /// has drawn with so far, all of it ([`Fidelity`]). Ahead of the run that
    /// carries that draw.
    Drawn(Fidelity),
}

/// A rectangle of a surface whose pixels are a picture the page shows from its
/// GPU, and where the output shows it: the rectangle, placed where the surface was
/// mapped at the EndFrame that showed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuArea {
    pub surface: u16,
    /// In the surface's pixels.
    pub rect: Rect,
    /// In the output's.
    pub output: Rect,
}

/// One rectangle of the output for the page to draw from a picture it shows from
/// its GPU: `output` takes the pixels of access unit `unit`'s picture from
/// `source`, its top left, on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuPaint {
    /// The unit's number in the run, as [`avc::scan`] counts them.
    pub unit: u32,
    pub output: Rect,
    pub source: (u32, u32),
}

/// What a run of commands asks of the page before it is composed
/// ([`Graphics::plan`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// For each access unit, by its number in the run, whether its picture may be
    /// shown from the GPU.
    pub gpu: Vec<bool>,
    /// What the run reads of pictures shown from the GPU, each to be read back into
    /// its surface first ([`Graphics::restore`]).
    pub restore: Vec<GpuArea>,
}

/// One rectangle of a surface shown from the GPU.
#[derive(Clone, Copy, Debug)]
struct OnGpu {
    rect: Rect,
    /// Where the surface's top left sat on the output at the EndFrame that showed
    /// it; `None` before then.
    at: Option<(u32, u32)>,
    /// The unit whose picture it is, by its number in the run being composed;
    /// `None` once that run is over and its picture drawn.
    unit: Option<u32>,
}

/// One surface: its pixels in `RGBX32`, where it shows on the output, and what has
/// changed in it since the last frame.
struct Surface {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    /// Where its top left sits on the output, once it has been mapped.
    mapped: Option<(u32, u32)>,
    /// Rectangles of the surface drawn into since the last EndFrame.
    invalid: Vec<Rect>,
    /// What its H.264 stream leaves between units, made on the first one: every
    /// surface a host draws H.264 into has a stream of its own.
    avc: Option<Box<Avc>>,
    /// Rectangles shown from the GPU, apart from one another, whose pixels here are
    /// not the picture.
    gpu: Vec<OnGpu>,
}

impl Surface {
    fn stride(&self) -> usize {
        self.width as usize * 4
    }

    fn bounds(&self) -> Rect {
        Rect { x: 0, y: 0, width: self.width, height: self.height }
    }

    /// Whether a command's rectangle lies inside this surface.
    fn holds(&self, rect: Rect16) -> bool {
        u32::from(rect.right) <= self.width && u32::from(rect.bottom) <= self.height
    }

    /// Write one rectangle of packed `BGRX32` rows, top row first, swizzling into
    /// the framebuffer's order on the way.
    fn write(&mut self, rect: Rect16, bgrx: &[u8]) {
        let width = usize::from(rect.width());
        let stride = self.stride();
        for (row, src) in bgrx.chunks_exact(width * 4).take(usize::from(rect.height())).enumerate() {
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 4;
            let dst = &mut self.pixels[at..at + width * 4];
            for (out, px) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
                *out = [px[2], px[1], px[0], 0];
            }
        }
        self.invalidate(to_rect(rect));
    }

    /// Whether an arbitrary rectangle lies inside this surface.
    fn contains(&self, x: u32, y: u32, width: u32, height: u32) -> bool {
        x.checked_add(width).is_some_and(|right| right <= self.width)
            && y.checked_add(height).is_some_and(|bottom| bottom <= self.height)
    }

    /// Fill a rectangle with one colour, `[r, g, b]` in the surface's own order.
    fn fill(&mut self, rect: Rect, rgb: [u8; 3]) {
        let stride = self.stride();
        let px = [rgb[0], rgb[1], rgb[2], 0];
        for row in 0..rect.height as usize {
            let at = (rect.y as usize + row) * stride + rect.x as usize * 4;
            for out in self.pixels[at..at + rect.width as usize * 4].as_chunks_mut::<4>().0 {
                *out = px;
            }
        }
        self.invalidate(rect);
    }

    /// Copy a rectangle of this surface out, its rows packed tight, top row first.
    fn copy_out(&self, rect: Rect, out: &mut Vec<u8>) {
        let stride = self.stride();
        let bytes = rect.width as usize * 4;
        out.clear();
        out.reserve(bytes * rect.height as usize);
        for row in 0..rect.height as usize {
            let at = (rect.y as usize + row) * stride + rect.x as usize * 4;
            out.extend_from_slice(&self.pixels[at..at + bytes]);
        }
    }

    /// Write packed rows — as [`Surface::copy_out`] produced them — into a
    /// rectangle, and record it as changed. The caller has checked the rectangle
    /// fits.
    fn copy_in(&mut self, x: u32, y: u32, width: u32, height: u32, packed: &[u8]) {
        let rect = Rect { x, y, width, height };
        self.copy_rows(rect, packed, width as usize * 4);
    }

    /// Write rows already in this surface's order — a Progressive tile's — that
    /// start `src_stride` bytes apart into a rectangle, and record it as changed.
    /// The caller has checked the rectangle fits.
    fn copy_rows(&mut self, rect: Rect, rows: &[u8], src_stride: usize) {
        let stride = self.stride();
        let bytes = rect.width as usize * 4;
        for row in 0..rect.height as usize {
            let src = &rows[row * src_stride..row * src_stride + bytes];
            let at = (rect.y as usize + row) * stride + rect.x as usize * 4;
            self.pixels[at..at + bytes].copy_from_slice(src);
        }
        self.invalidate(rect);
    }

    /// Record a rectangle drawn into — see [`stage`] for how the list is kept to
    /// [`DAMAGE_CAP`]. What the GPU showed there is no longer the picture.
    fn invalidate(&mut self, rect: Rect) {
        stage(&mut self.invalid, rect, DAMAGE_CAP);
        self.release(rect);
    }

    /// The pixels here are the picture over `rect` again.
    fn release(&mut self, rect: Rect) {
        if self.gpu.iter().any(|shown| shown.rect.overlaps(&rect)) {
            self.gpu = self
                .gpu
                .iter()
                .flat_map(|shown| shown.rect.minus(rect).map(move |piece| OnGpu { rect: piece, ..*shown }))
                .collect();
        }
    }

    /// Unit `unit`'s picture, shown from the GPU over `rect`.
    fn show_on_gpu(&mut self, rect: Rect, unit: u32) {
        self.release(rect);
        self.gpu.push(OnGpu { rect, at: None, unit: Some(unit) });
    }

    /// The rectangles shown from the GPU, those that line up joined.
    fn join_gpu(&mut self) {
        if self.gpu.len() > 1 {
            join_shown(&mut self.gpu);
        }
    }

    /// `rect` with what the GPU shows taken out of it.
    fn off_gpu(&self, rect: Rect) -> Vec<Rect> {
        let mut pieces = vec![rect];
        for shown in &self.gpu {
            if pieces.iter().any(|piece| piece.overlaps(&shown.rect)) {
                pieces = pieces.iter().flat_map(|piece| piece.minus(shown.rect)).collect();
            }
        }
        pieces
    }

    /// What it has shown from the GPU, as the output shows it.
    fn gpu_areas(&self, surface: u16) -> impl Iterator<Item = GpuArea> + '_ {
        self.gpu.iter().filter_map(move |shown| {
            let (x, y) = shown.at?;
            let output = Rect { x: x + shown.rect.x, y: y + shown.rect.y, ..shown.rect };
            Some(GpuArea { surface, rect: shown.rect, output })
        })
    }
}

/// A rectangle lifted off a surface and kept under a slot, to be stamped back down
/// on the output later — the scroll-and-repeat the desktop leans on most. Its pixels
/// are a surface's own packed `RGBX32`.
struct Cache {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// A `Rect16`, with its exclusive edges, as the framebuffer's [`Rect`].
fn to_rect(rect: Rect16) -> Rect {
    Rect {
        x: u32::from(rect.left),
        y: u32::from(rect.top),
        width: u32::from(rect.width()),
        height: u32::from(rect.height()),
    }
}

/// What the channel has carried, by command and by codec.
///
/// Kept for the life of the channel and said once at the end: it is the measurement
/// the next decoder is chosen from, and a session that painted nothing says here
/// which codec it was waiting on.
#[derive(Debug, Default)]
struct Tally {
    commands: BTreeMap<u16, u64>,
    codecs: BTreeMap<u16, u64>,
    /// Codecs and commands whose first sighting has been logged.
    announced: Vec<(&'static str, u16)>,
}

impl Tally {
    fn command(&mut self, command: u16) {
        *self.commands.entry(command).or_default() += 1;
    }

    fn codec(&mut self, codec: u16) {
        *self.codecs.entry(codec).or_default() += 1;
    }

    /// Say once that something arrived which nothing here acts on yet.
    fn unhandled(&mut self, kind: &'static str, id: u16, name: &'static str) {
        if self.announced.contains(&(kind, id)) {
            return;
        }
        self.announced.push((kind, id));
        info!(
            "rdp: graphics {kind} {name} ({id:#06x}) is not decoded yet; its regions are left \
             unpainted and counted"
        );
    }

    fn summary(&self) -> String {
        let commands: Vec<String> = self
            .commands
            .iter()
            .map(|(command, count)| format!("{} {count}", gfx::command_name(*command)))
            .collect();
        let codecs: Vec<String> = self
            .codecs
            .iter()
            .map(|(codec, count)| format!("{} {count}", gfx::codec_name(*codec)))
            .collect();
        format!("commands [{}], codecs [{}]", commands.join(", "), codecs.join(", "))
    }
}

/// The pipeline's state for one channel: the decompressor, the surfaces, and what
/// has been seen.
pub struct Graphics {
    zgfx: zgfx::Zgfx,
    /// One PDU decompressed, reused across PDUs.
    buffer: Vec<u8>,
    surfaces: BTreeMap<u16, Surface>,
    /// Rectangles lifted off surfaces, by slot, for the copies below.
    caches: BTreeMap<u16, Cache>,
    /// A rectangle copied out of one surface on its way into another; reused.
    scratch: Vec<u8>,
    /// The output's size, from the last ResetGraphics. Nothing is mapped before one.
    output: Option<(u32, u32)>,
    /// The frame the server has started and not ended.
    frame: Option<u32>,
    /// How many frames have been ended, which every acknowledgement reports.
    decoded: u32,
    tally: Tally,
    /// The planar codec's working space.
    planes: Vec<u8>,
    pixels: Vec<u8>,
    /// ClearCodec's state and caches, made on the first ClearCodec rectangle since a
    /// channel may never draw one.
    clear: Option<Box<clear::Clear>>,
    /// Progressive's per-surface tiles, made on the first Progressive PDU.
    progressive: Option<Box<progressive::Progressive>>,
    /// Whether the commands are passed on rather than composed here — see
    /// [`Self::passing`].
    pass: bool,
    /// The pictures supplied for the next run's H.264 access units, by the unit's
    /// number in the run ([`Self::supply`]).
    supplied: BTreeMap<u32, Picture>,
    /// The next run's access units whose pictures the page shows from its GPU
    /// ([`Self::show_on_gpu`]).
    on_gpu: BTreeSet<u32>,
    /// What the last run left the page to draw from those pictures.
    gpu_paints: Vec<GpuPaint>,
    /// How many access units the run being composed has reached.
    unit: u32,
    /// What a passed pipeline has drawn with.
    fidelity: Fidelity,
}

impl Default for Graphics {
    fn default() -> Self {
        Self::new()
    }
}

impl Graphics {
    pub fn new() -> Self {
        Self {
            zgfx: zgfx::Zgfx::new(),
            buffer: Vec::new(),
            surfaces: BTreeMap::new(),
            caches: BTreeMap::new(),
            scratch: Vec::new(),
            output: None,
            frame: None,
            decoded: 0,
            tally: Tally::default(),
            planes: Vec::new(),
            pixels: Vec::new(),
            clear: None,
            progressive: None,
            pass: false,
            supplied: BTreeMap::new(),
            on_gpu: BTreeSet::new(),
            gpu_paints: Vec::new(),
            unit: 0,
            fidelity: Fidelity::default(),
        }
    }

    /// A pipeline whose commands are composed somewhere else: each message handed
    /// on as the host sent it, still compressed ([`Update::Passed`]), and unwrapped
    /// here too, for the commands' headers. Nothing is decoded, no surface is kept
    /// and the framebuffer is never drawn into; what this end still reads is what
    /// the session itself answers — the confirmation, the output's size and each
    /// frame's end, which is owed an acknowledgement once whoever draws the frame
    /// has: a frame is not finished here, so none is counted here.
    pub fn passing() -> Self {
        let mut passing = Self::new();
        passing.pass = true;
        passing
    }

    /// The pipeline a channel opened again starts with: nothing held, composed or
    /// passed as this one is.
    pub fn fresh(&self) -> Self {
        let mut fresh = Self::new();
        fresh.pass = self.pass;
        fresh
    }

    /// One PDU off the channel: unwrapped, decoded, and acted on. The pixels go
    /// into `framebuffer` here; what comes back is what the caller has to do
    /// about them.
    ///
    /// A PDU that does not decode ends the session, as any other malformed PDU
    /// does. A *codec payload* that does not decode does not: it is one rectangle
    /// the server will draw again, and it is logged and counted instead.
    pub fn receive(&mut self, data: &[u8], framebuffer: &Framebuffer) -> Result<Vec<Update>> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let outcome = self.receive_into(data, &mut buffer, framebuffer);
        self.buffer = buffer;
        outcome
    }

    fn receive_into(
        &mut self,
        data: &[u8],
        buffer: &mut Vec<u8>,
        framebuffer: &Framebuffer,
    ) -> Result<Vec<Update>> {
        self.unwrap(data, buffer)?;
        if self.pass {
            return self.hand_on(data, buffer);
        }
        self.compose(buffer, framebuffer)
    }

    /// One message off the channel out of its bulk compression, into `commands`:
    /// against everything this pipeline has unwrapped before it, so every message
    /// comes through here, in order.
    pub(crate) fn unwrap(&mut self, data: &[u8], commands: &mut Vec<u8>) -> Result<()> {
        self.zgfx.decompress(data, commands).context("unwrapping a graphics pipeline PDU")
    }

    /// Act on a buffer of commands that is already unwrapped. The pictures supplied
    /// for it are its own: whichever it did not take are dropped with it.
    pub(crate) fn compose(&mut self, commands: &[u8], framebuffer: &Framebuffer) -> Result<Vec<Update>> {
        self.unit = 0;
        let outcome = self.compose_run(commands, framebuffer);
        self.supplied.clear();
        self.on_gpu.clear();
        self.gpu_paints = self.finish_gpu();
        outcome
    }

    /// What the run just composed showed from the GPU, still shown at its end, for
    /// the page to draw. A picture whose frame did not end in the run was never
    /// placed, and is dropped: [`Self::plan`] shows none such.
    fn finish_gpu(&mut self) -> Vec<GpuPaint> {
        let mut paints = Vec::new();
        let output = self.output.map(|(width, height)| Rect { x: 0, y: 0, width, height });
        for (id, surface) in &mut self.surfaces {
            surface.gpu.retain_mut(|shown| {
                let Some(unit) = shown.unit.take() else { return true };
                let (Some((x, y)), Some(output)) = (shown.at, output) else {
                    warn!("rdp: a picture shown from the GPU on graphics surface {id} was never placed on the output");
                    return false;
                };
                let placed = Rect { x: x + shown.rect.x, y: y + shown.rect.y, ..shown.rect };
                if let Some(inside) = placed.clipped(output) {
                    paints.push(GpuPaint { unit, output: inside, source: (inside.x - x, inside.y - y) });
                }
                true
            });
            // Drawn, the units' pieces are one picture's, and join across them.
            surface.join_gpu();
        }
        paints
    }

    /// What the last run composed left the page to draw from the pictures it shows
    /// from its GPU.
    pub(crate) fn take_gpu_paints(&mut self) -> Vec<GpuPaint> {
        std::mem::take(&mut self.gpu_paints)
    }

    /// Access unit `unit` of the run composed next is shown from the page's GPU
    /// rather than supplied: its rectangles are recorded as the GPU's, and drawn by
    /// the page from what the run's end says ([`GpuPaint`]). Only for a unit
    /// [`Self::plan`] says may be.
    pub(crate) fn show_on_gpu(&mut self, unit: u32) {
        self.on_gpu.insert(unit);
    }

    /// Every rectangle shown from the GPU, as the output shows it: what the page
    /// reads back to have the surfaces whole.
    pub(crate) fn gpu_areas(&self) -> Vec<GpuArea> {
        self.surfaces.iter().flat_map(|(id, surface)| surface.gpu_areas(*id)).collect()
    }

    /// The pixels of a rectangle shown from the GPU, read back off it: `rgbx` is
    /// `rect`'s rows, packed, top row first. They are the picture already, so
    /// nothing is presented.
    pub(crate) fn restore(&mut self, surface: u16, rect: Rect, rgbx: &[u8]) -> Result<()> {
        let found = self.surfaces.get_mut(&surface).with_context(|| format!("no graphics surface {surface}"))?;
        anyhow::ensure!(
            found.contains(rect.x, rect.y, rect.width, rect.height),
            "a {}x{}+{}+{} rectangle read back for a {}x{} graphics surface",
            rect.width,
            rect.height,
            rect.x,
            rect.y,
            found.width,
            found.height
        );
        let bytes = rect.width as usize * 4;
        anyhow::ensure!(
            rgbx.len() == bytes * rect.height as usize,
            "{} bytes read back for a {}x{} rectangle",
            rgbx.len(),
            rect.width,
            rect.height
        );
        let stride = found.stride();
        for (row, src) in rgbx.chunks_exact(bytes).enumerate() {
            let at = (rect.y as usize + row) * stride + rect.x as usize * 4;
            for (out, px) in found.pixels[at..at + bytes].as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
                *out = [px[0], px[1], px[2], 0];
            }
        }
        found.release(rect);
        Ok(())
    }

    /// What a run of commands, unwrapped, asks of the page before it is composed:
    /// which of its access units may be shown from the GPU, and what it reads of
    /// what the GPU shows already. Walks the commands as [`Self::compose`] will,
    /// with nothing decoded and nothing changed.
    ///
    /// A unit may be shown from the GPU where its command's luma view paints on its
    /// own ([`avc::Stream::alone`]), its stream has carried no chroma view, a frame
    /// ends after it in the run with its surface mapped wholly inside the output
    /// and no other mapped surface over it, and nothing after it in the run reads
    /// that surface. What reads a surface is a copy out of it, a cache entry taken
    /// from it, and a ClearCodec rectangle, which is drawn over what it holds.
    pub(crate) fn plan(&self, commands: &[u8]) -> Result<Plan, Malformed> {
        let mut surfaces: BTreeMap<u16, Seen> = self
            .surfaces
            .iter()
            .map(|(id, surface)| {
                let chroma = surface.avc.as_ref().is_some_and(|avc| avc.chroma_seen());
                (*id, Seen { size: (surface.width, surface.height), mapped: surface.mapped, chroma })
            })
            .collect();
        let mut output = self.output;
        let mut plan = Plan::default();
        // A surface that has shown too many pieces from the GPU is read back whole.
        for (id, surface) in &self.surfaces {
            if surface.gpu.len() > GPU_CAP {
                plan.restore.extend(surface.gpu_areas(*id));
            }
        }
        // The units that may be shown from the GPU so far, by number and surface:
        // those whose frame has not ended yet, and all of them.
        let mut unplaced: Vec<(usize, u16)> = Vec::new();
        let mut shown: Vec<(usize, u16)> = Vec::new();
        let read = |plan: &mut Plan, shown: &[(usize, u16)], surface: u16, rect: Rect| {
            for (unit, on) in shown {
                if *on == surface {
                    plan.gpu[*unit] = false;
                }
            }
            if let Some(held) = self.surfaces.get(&surface) {
                for area in held.gpu_areas(surface) {
                    if area.rect.overlaps(&rect) && !plan.restore.contains(&area) {
                        plan.restore.push(area);
                    }
                }
            }
        };
        for message in gfx::messages(commands) {
            match message? {
                Message::ResetGraphics { width, height, .. } => output = Some((width, height)),
                Message::CreateSurface { surface, width, height, .. } => {
                    let size = (u32::from(width), u32::from(height));
                    surfaces.insert(surface, Seen { size, mapped: None, chroma: false });
                }
                Message::DeleteSurface { surface } => {
                    surfaces.remove(&surface);
                }
                Message::MapSurfaceToOutput { surface, x, y } => {
                    if let Some(seen) = surfaces.get_mut(&surface) {
                        seen.mapped = Some((x, y));
                    }
                }
                Message::EndFrame { .. } => {
                    for (unit, surface) in unplaced.drain(..) {
                        if !placeable(&surfaces, output, surface) {
                            plan.gpu[unit] = false;
                        }
                    }
                }
                Message::WireToSurface1 { surface, codec, data, .. } if avc::carries(codec) => {
                    // As `avc::scan` counts them: a payload that does not read has no units.
                    let Ok(stream) = avc::Stream::read(codec, data) else { continue };
                    let seen = surfaces.get_mut(&surface);
                    let chroma_before = seen.as_ref().is_some_and(|seen| seen.chroma);
                    if stream.has_chroma() {
                        if let Some(seen) = seen {
                            seen.chroma = true;
                        }
                        // The luma views before it are what it combines with.
                        for (unit, on) in &shown {
                            if *on == surface {
                                plan.gpu[*unit] = false;
                            }
                        }
                    }
                    let alone = stream.alone().is_some_and(|regions| !regions.is_empty());
                    for at in 0..stream.units() {
                        let unit = plan.gpu.len();
                        let may = at == 0 && alone && !chroma_before;
                        plan.gpu.push(may);
                        if may {
                            unplaced.push((unit, surface));
                            shown.push((unit, surface));
                        }
                    }
                }
                Message::WireToSurface1 { surface, codec: gfx::CODEC_CLEARCODEC, rect, .. } => {
                    read(&mut plan, &shown, surface, to_rect(rect));
                }
                Message::SurfaceToSurface { src, rect, .. } => read(&mut plan, &shown, src, to_rect(rect)),
                Message::SurfaceToCache { surface, rect, .. } => read(&mut plan, &shown, surface, to_rect(rect)),
                _ => {}
            }
        }
        // A frame that does not end in the run is shown in a run whose pictures
        // these are not.
        for (unit, _) in unplaced {
            plan.gpu[unit] = false;
        }
        Ok(plan)
    }

    fn compose_run(&mut self, commands: &[u8], framebuffer: &Framebuffer) -> Result<Vec<Update>> {
        let mut updates = Vec::new();
        for message in gfx::messages(commands) {
            self.act(message?, framebuffer, &mut updates)?;
        }
        Ok(updates)
    }

    /// The decoded picture of one H.264 access unit in the run composed next,
    /// under the unit's number in that run as [`avc::scan`] counts them.
    pub(crate) fn supply(&mut self, unit: u32, picture: Picture) {
        self.supplied.insert(unit, picture);
    }

    /// A message for whoever composes it, `wrapped` as the host sent it, behind
    /// what the session has to say of its `commands`: the confirmation and a reset,
    /// which so reach the caller ahead of the message that carries them, and the
    /// draws' codecs, for [`Update::Drawn`]. The message is not cut: compressed, it
    /// has no boundary between its commands, so it names every frame it ends.
    fn hand_on(&mut self, wrapped: &[u8], commands: &[u8]) -> Result<Vec<Update>> {
        let mut updates = Vec::new();
        let mut frames = Vec::new();
        for command in gfx::commands(commands) {
            let command = command?;
            self.tally.command(command.id);
            if !matches!(
                command.id,
                gfx::CMD_CAPS_CONFIRM
                    | gfx::CMD_RESET_GRAPHICS
                    | gfx::CMD_END_FRAME
                    | gfx::CMD_WIRE_TO_SURFACE_1
                    | gfx::CMD_WIRE_TO_SURFACE_2
            ) {
                continue;
            }
            let read = gfx::messages(&commands[command.start..command.end]).next();
            match read.transpose()? {
                Some(Message::WireToSurface1 { codec, data, .. } | Message::WireToSurface2 { codec, data, .. }) => {
                    if self.fidelity.saw(codec, data) {
                        let names = self.fidelity.names();
                        info!(
                            "rdp: the host has drawn with {} so far, {}",
                            names.join(", "),
                            if self.fidelity.exact() { "exact" } else { "not exact" }
                        );
                        updates.push(Update::Drawn(self.fidelity.clone()));
                    }
                }
                Some(Message::CapsConfirm { version, flags }) => {
                    info!(
                        "rdp: the host confirmed graphics pipeline version {version:#010x}, flags {flags:#x}; \
                         its commands are passed on"
                    );
                    // A pipeline that starts over has drawn with nothing yet, so
                    // the codecs it draws with again are reported again.
                    self.fidelity = Fidelity::default();
                    updates.push(Update::Confirmed);
                }
                Some(Message::ResetGraphics { width, height, monitors }) => {
                    affordable(width, height)?;
                    let monitors = gfx::arrangement(width, height, &monitors);
                    debug!("rdp: graphics reset to {width}x{height}, over {monitors:?}");
                    self.output = Some((width, height));
                    updates.push(Update::Reset { width, height, monitors });
                }
                Some(Message::EndFrame { frame }) => frames.push(frame),
                _ => {}
            }
        }
        updates.push(Update::Passed { wrapped: wrapped.to_vec(), frames });
        Ok(updates)
    }

    fn act(&mut self, message: Message<'_>, framebuffer: &Framebuffer, updates: &mut Vec<Update>) -> Result<()> {
        match message {
            Message::CapsConfirm { version, flags } => {
                self.tally.command(gfx::CMD_CAPS_CONFIRM);
                info!("rdp: the host confirmed graphics pipeline version {version:#010x}, flags {flags:#x}");
                updates.push(Update::Confirmed);
            }
            Message::ResetGraphics { width, height, monitors } => {
                self.tally.command(gfx::CMD_RESET_GRAPHICS);
                affordable(width, height)?;
                let monitors = gfx::arrangement(width, height, &monitors);
                debug!("rdp: graphics reset to {width}x{height}, over {monitors:?}");
                framebuffer.resize(width, height);
                self.output = Some((width, height));
                // The surfaces stay, blank, until the server draws into them again;
                // FreeRDP keeps them too.
                for surface in self.surfaces.values_mut() {
                    surface.pixels.fill(0);
                    surface.invalid.clear();
                    surface.gpu.clear();
                }
                updates.push(Update::Reset { width, height, monitors });
            }
            Message::CreateSurface { surface, width, height, format } => {
                self.tally.command(gfx::CMD_CREATE_SURFACE);
                let bytes = usize::from(width) * usize::from(height) * 4;
                // One budget for every surface together: a host may create as many
                // as it likes, each small enough on its own, and the allocation that
                // fails is the process. A surface created under a number in use
                // replaces the old one, so that one's bytes come back first.
                let held: usize = self
                    .surfaces
                    .iter()
                    .filter(|(id, _)| **id != surface)
                    .map(|(_, held)| held.pixels.len())
                    .sum::<usize>()
                    + self.progressive.as_ref().map_or(0, |progressive| progressive.held_except(surface));
                // Summed with the overflow looked for: the largest surface the wire
                // can name is within a megabyte of what 32 bits count to.
                anyhow::ensure!(
                    held.checked_add(bytes).is_some_and(|total| total <= MAX_DESKTOP_BYTES),
                    "the host created a {width}x{height} graphics surface, which with the {} MiB \
                     of surfaces it already has is more than the {} MiB this client will hold",
                    held >> 20,
                    MAX_DESKTOP_BYTES >> 20
                );
                debug!("rdp: graphics surface {surface} created, {width}x{height}, format {format:#04x}");
                let created = Surface {
                    width: u32::from(width),
                    height: u32::from(height),
                    pixels: vec![0; bytes],
                    mapped: None,
                    invalid: Vec::new(),
                    avc: None,
                    gpu: Vec::new(),
                };
                // A server may create a surface under a number still in use; the
                // new one replaces the old.
                self.surfaces.insert(surface, created);
                if let Some(progressive) = &mut self.progressive {
                    progressive.forget(surface);
                }
            }
            Message::DeleteSurface { surface } => {
                self.tally.command(gfx::CMD_DELETE_SURFACE);
                if self.surfaces.remove(&surface).is_none() {
                    debug!("rdp: the host deleted graphics surface {surface}, which did not exist");
                }
                if let Some(progressive) = &mut self.progressive {
                    progressive.forget(surface);
                }
            }
            Message::MapSurfaceToOutput { surface, x, y } => {
                self.tally.command(gfx::CMD_MAP_SURFACE_TO_OUTPUT);
                match self.surfaces.get_mut(&surface) {
                    Some(found) => {
                        debug!("rdp: graphics surface {surface} mapped to the output at {x},{y}");
                        // Whatever was drawn into it before the map is still owed to
                        // the output at the EndFrame, so the changed rectangles stay;
                        // FreeRDP keeps them too.
                        found.mapped = Some((x, y));
                    }
                    None => warn!("rdp: the host mapped graphics surface {surface}, which does not exist"),
                }
            }
            Message::StartFrame { frame, .. } => {
                self.tally.command(gfx::CMD_START_FRAME);
                if let Some(open) = self.frame.replace(frame) {
                    debug!("rdp: graphics frame {frame} started inside frame {open}");
                }
                if let Some(progressive) = self.progressive.as_mut() {
                    progressive.start_frame();
                }
            }
            Message::EndFrame { frame } => {
                self.tally.command(gfx::CMD_END_FRAME);
                if self.frame.take() != Some(frame) {
                    debug!("rdp: graphics frame {frame} ended without its start");
                }
                self.present(framebuffer, updates);
                self.decoded = self.decoded.wrapping_add(1);
                updates.push(Update::Frame { id: frame, decoded: self.decoded });
            }
            Message::WireToSurface1 { surface, codec, format, rect, data } => {
                self.tally.command(gfx::CMD_WIRE_TO_SURFACE_1);
                self.tally.codec(codec);
                if avc::carries(codec) {
                    // The picture is the surface, masked; the command's rectangle
                    // says nothing the metablock does not.
                    self.draw_avc(surface, codec, data);
                } else {
                    self.draw(surface, codec, format, rect, data);
                }
            }
            Message::WireToSurface2 { surface, codec, format, data, .. } => {
                self.tally.command(gfx::CMD_WIRE_TO_SURFACE_2);
                self.tally.codec(codec);
                if codec == gfx::CODEC_CAPROGRESSIVE {
                    self.draw_progressive(surface, format, data);
                } else {
                    self.tally.unhandled("codec", codec, gfx::codec_name(codec));
                }
            }
            Message::SolidFill { surface, color, rects } => {
                self.tally.command(gfx::CMD_SOLID_FILL);
                self.solid_fill(surface, color, &rects);
            }
            Message::SurfaceToSurface { src, dst, rect, points } => {
                self.tally.command(gfx::CMD_SURFACE_TO_SURFACE);
                self.surface_to_surface(src, dst, rect, &points);
            }
            Message::SurfaceToCache { surface, slot, rect, .. } => {
                self.tally.command(gfx::CMD_SURFACE_TO_CACHE);
                self.surface_to_cache(surface, slot, rect);
            }
            Message::CacheToSurface { slot, surface, points } => {
                self.tally.command(gfx::CMD_CACHE_TO_SURFACE);
                self.cache_to_surface(slot, surface, &points);
            }
            Message::EvictCacheEntry { slot } => {
                self.tally.command(gfx::CMD_EVICT_CACHE_ENTRY);
                self.caches.remove(&slot);
            }
            Message::DeleteEncodingContext { .. } => {
                // Nothing to delete: the one codec with a context, Progressive,
                // keeps its state by surface, not by context, and the surface's
                // deletion drops it.
                self.tally.command(gfx::CMD_DELETE_ENCODING_CONTEXT);
            }
            Message::Other { command, length } => {
                self.tally.command(command);
                debug!("rdp: ignoring a {length}-byte graphics PDU of type {command:#06x}");
            }
        }
        Ok(())
    }

    /// One rectangle of pixels for a surface, in whichever codec the server chose.
    fn draw(&mut self, surface: u16, codec: u16, format: u8, rect: Rect16, data: &[u8]) {
        let Some(found) = self.surfaces.get_mut(&surface) else {
            warn!("rdp: the host drew into graphics surface {surface}, which does not exist");
            return;
        };
        if !found.holds(rect) {
            warn!(
                "rdp: dropping a {}x{}+{}+{} draw that does not fit a {}x{} graphics surface",
                rect.width(),
                rect.height(),
                rect.left,
                rect.top,
                found.width,
                found.height
            );
            return;
        }
        if format != gfx::PIXEL_XRGB_8888 && format != gfx::PIXEL_ARGB_8888 {
            warn!("rdp: dropping a graphics draw in pixel format {format:#04x}");
            return;
        }
        let (width, height) = (rect.width(), rect.height());
        let decoded: Result<&[u8], Malformed> = match codec {
            gfx::CODEC_UNCOMPRESSED => {
                let need = usize::from(width) * usize::from(height) * 4;
                match data.get(..need) {
                    Some(pixels) => Ok(pixels),
                    None => Err(Malformed::Short { what: "an uncompressed graphics rectangle", len: data.len(), at: 0, need }),
                }
            }
            gfx::CODEC_PLANAR => {
                planar::decompress(data, &mut self.planes, &mut self.pixels, width, height)
                    .map(|()| &self.pixels[..])
            }
            gfx::CODEC_CLEARCODEC => {
                // Onto the surface itself, over what it holds: the host encodes a
                // ClearCodec rectangle against that picture (see `clear`).
                let clear = self.clear.get_or_insert_with(|| Box::new(clear::Clear::new()));
                let (surface_width, surface_height) = (found.width as usize, found.height as usize);
                let mut canvas = clear::Canvas::new(&mut found.pixels, surface_width, surface_height);
                let outcome =
                    clear.decompress(data, &mut canvas, usize::from(rect.left), usize::from(rect.top), width, height);
                let spilled = canvas.painted();
                // Whatever the outcome: the layers before a fault are on the surface
                // already, and a band's columns can land beside the rectangle.
                found.invalidate(Rect {
                    x: u32::from(rect.left),
                    y: u32::from(rect.top),
                    width: u32::from(width),
                    height: u32::from(height),
                });
                if let Some((x, y, w, h)) = spilled {
                    found.invalidate(Rect { x: x as u32, y: y as u32, width: w as u32, height: h as u32 });
                }
                if let Err(e) = outcome {
                    warn!("rdp: leaving part of a {width}x{height} ClearCodec rectangle unpainted: {e}");
                }
                return;
            }
            other => {
                self.tally.unhandled("codec", other, gfx::codec_name(other));
                return;
            }
        };
        match decoded {
            Ok(bgrx) => found.write(rect, bgrx),
            // One rectangle the server will draw again; not the session.
            Err(e) => warn!("rdp: leaving a {width}x{height} {} rectangle unpainted: {e}", gfx::codec_name(codec)),
        }
    }

    /// An H.264 stream for a surface: its masked rectangles, painted from the
    /// pictures supplied for its access units.
    ///
    /// The units are counted before anything can turn the stream away, so that the
    /// numbers stay [`avc::scan`]'s whatever becomes of one stream. A stream whose
    /// picture was not supplied, or does not hold what it says, costs its
    /// rectangles and not the session.
    fn draw_avc(&mut self, surface: u16, codec: u16, data: &[u8]) {
        let stream = match avc::Stream::read(codec, data) {
            Ok(stream) => stream,
            Err(e) => {
                warn!("rdp: leaving a {} stream unpainted: {e}", gfx::codec_name(codec));
                return;
            }
        };
        let first = self.unit;
        self.unit += stream.units();
        let (luma, chroma) = stream.numbers();
        if let Some(unit) = luma.map(|at| first + at).filter(|unit| self.on_gpu.contains(unit)) {
            self.show_avc_on_gpu(surface, codec, &stream, unit);
            return;
        }
        let luma = luma.and_then(|at| self.supplied.remove(&(first + at)));
        let chroma = chroma.and_then(|at| self.supplied.remove(&(first + at)));
        let Some(found) = self.surfaces.get_mut(&surface) else {
            warn!("rdp: the host drew into graphics surface {surface}, which does not exist");
            return;
        };
        // Lifted out of the surface for the call, so the paints can borrow the
        // surface's pixels.
        let mut state = found.avc.take().unwrap_or_default();
        let size = (found.width, found.height);
        let outcome = state.draw(&stream, luma.as_ref(), chroma.as_ref(), size, &mut |rect, rows, stride| {
            found.copy_rows(to_rect(rect), rows, stride);
        });
        found.avc = Some(state);
        if let Err(e) = outcome {
            warn!("rdp: leaving a {} stream's rectangles unpainted: {e:#}", gfx::codec_name(codec));
        }
    }

    /// A stream's masked rectangles, shown from the GPU from unit `unit`'s picture.
    fn show_avc_on_gpu(&mut self, surface: u16, codec: u16, stream: &avc::Stream<'_>, unit: u32) {
        let Some(found) = self.surfaces.get_mut(&surface) else {
            warn!("rdp: the host drew into graphics surface {surface}, which does not exist");
            return;
        };
        let Some(regions) = stream.alone() else {
            warn!("rdp: leaving a {} stream unpainted: its views combine, and its luma view's picture was not supplied", gfx::codec_name(codec));
            return;
        };
        let bounds = found.bounds();
        let rects: Vec<Rect> = regions.iter().filter_map(|region| to_rect(region.rect).clipped(bounds)).collect();
        for rect in &rects {
            found.show_on_gpu(*rect, unit);
        }
        found.join_gpu();
        found.avc.get_or_insert_default().shown_on_gpu(stream, &rects);
    }

    /// A Progressive PDU for a surface: its blocks carry their own rectangles, in
    /// surface coordinates, and the decoder keeps the tiles between PDUs.
    fn draw_progressive(&mut self, surface: u16, format: u8, data: &[u8]) {
        // The tiles share the surfaces' budget: what the surfaces hold is not theirs.
        let surfaces: usize = self.surfaces.values().map(|held| held.pixels.len()).sum();
        let Some(found) = self.surfaces.get_mut(&surface) else {
            warn!("rdp: the host drew into graphics surface {surface}, which does not exist");
            return;
        };
        if format != gfx::PIXEL_XRGB_8888 && format != gfx::PIXEL_ARGB_8888 {
            warn!("rdp: dropping a graphics draw in pixel format {format:#04x}");
            return;
        }
        let progressive = self.progressive.get_or_insert_with(|| Box::new(progressive::Progressive::new()));
        let budget = MAX_DESKTOP_BYTES.saturating_sub(surfaces);
        let outcome = progressive.decompress(surface, found.width, found.height, data, budget, |rect, rows, stride| {
            found.copy_rows(to_rect(rect), rows, stride);
        });
        if let Err(e) = outcome {
            // The regions before the fault are painted; the host will draw the rest
            // again. Not the session.
            warn!("rdp: leaving part of a Progressive update to graphics surface {surface} unpainted: {e}");
        }
    }

    /// Fill rectangles of a surface with one colour. Each is clipped to the
    /// surface, as [MS-RDPEGFX] 3.3.5.4 has it; the fill colour's alpha is ignored.
    fn solid_fill(&mut self, surface: u16, color: [u8; 4], rects: &[Rect16]) {
        let Some(found) = self.surfaces.get_mut(&surface) else {
            warn!("rdp: the host filled graphics surface {surface}, which does not exist");
            return;
        };
        let rgb = [color[2], color[1], color[0]];
        let bounds = found.bounds();
        for rect in rects {
            if let Some(rect) = to_rect(*rect).clipped(bounds) {
                found.fill(rect, rgb);
            }
        }
    }

    /// Copy one rectangle of a surface to each of a list of points, on the same
    /// surface or another. The source rectangle is lifted out whole first, so a
    /// copy that overlaps itself — a scroll — is still correct.
    fn surface_to_surface(&mut self, src: u16, dst: u16, rect: Rect16, points: &[Point16]) {
        let Some(source) = self.surfaces.get(&src) else {
            warn!("rdp: the host copied from graphics surface {src}, which does not exist");
            return;
        };
        if !source.holds(rect) {
            warn!("rdp: dropping a copy of a rectangle that does not fit graphics surface {src}");
            return;
        }
        let (width, height) = (u32::from(rect.width()), u32::from(rect.height()));
        let mut scratch = std::mem::take(&mut self.scratch);
        source.copy_out(to_rect(rect), &mut scratch);
        let Some(target) = self.surfaces.get_mut(&dst) else {
            warn!("rdp: the host copied to graphics surface {dst}, which does not exist");
            self.scratch = scratch;
            return;
        };
        for point in points {
            let (x, y) = (u32::from(point.x), u32::from(point.y));
            if target.contains(x, y, width, height) {
                target.copy_in(x, y, width, height, &scratch);
            } else {
                warn!("rdp: dropping a copy to {x},{y} that does not fit graphics surface {dst}");
            }
        }
        self.scratch = scratch;
    }

    /// Lift one rectangle of a surface into a cache slot, replacing whatever the slot
    /// held.
    fn surface_to_cache(&mut self, surface: u16, slot: u16, rect: Rect16) {
        let Some(source) = self.surfaces.get(&surface) else {
            warn!("rdp: the host cached from graphics surface {surface}, which does not exist");
            return;
        };
        if !source.holds(rect) {
            warn!("rdp: dropping a cache of a rectangle that does not fit graphics surface {surface}");
            return;
        }
        let (width, height) = (u32::from(rect.width()), u32::from(rect.height()));
        // One budget for every slot together; the slot being replaced gives its
        // bytes back first.
        let bytes = width as usize * height as usize * 4;
        let held: usize = self.caches.iter().filter(|(s, _)| **s != slot).map(|(_, held)| held.pixels.len()).sum();
        if held + bytes > CACHE_BUDGET {
            warn!(
                "rdp: dropping a {width}x{height} cache entry for slot {slot}: with the {} MiB of \
                 entries already held it is more than the {} MiB this client will cache",
                held >> 20,
                CACHE_BUDGET >> 20
            );
            // The host thinks the slot holds the new rectangle now; the old one must
            // not stand in for it.
            self.caches.remove(&slot);
            return;
        }
        let mut pixels = Vec::new();
        source.copy_out(to_rect(rect), &mut pixels);
        self.caches.insert(slot, Cache { width, height, pixels });
    }

    /// Stamp a cached rectangle down onto a surface at each of a list of points.
    fn cache_to_surface(&mut self, slot: u16, surface: u16, points: &[Point16]) {
        let Some(cache) = self.caches.get(&slot) else {
            warn!("rdp: the host drew cache slot {slot}, which is empty");
            return;
        };
        let Some(target) = self.surfaces.get_mut(&surface) else {
            warn!("rdp: the host drew to graphics surface {surface}, which does not exist");
            return;
        };
        let (width, height) = (cache.width, cache.height);
        for point in points {
            let (x, y) = (u32::from(point.x), u32::from(point.y));
            if target.contains(x, y, width, height) {
                target.copy_in(x, y, width, height, &cache.pixels);
            } else {
                warn!("rdp: dropping a cache draw to {x},{y} that does not fit graphics surface {surface}");
            }
        }
    }

    /// The EndFrame: every changed rectangle of every mapped surface, onto the
    /// framebuffer.
    fn present(&mut self, framebuffer: &Framebuffer, updates: &mut Vec<Update>) {
        let Some((width, height)) = self.output else {
            // Nothing is on the output before a ResetGraphics has said its size.
            for surface in self.surfaces.values_mut() {
                surface.invalid.clear();
            }
            return;
        };
        let output = Rect { x: 0, y: 0, width, height };
        for surface in self.surfaces.values_mut() {
            let invalid = std::mem::take(&mut surface.invalid);
            let Some((ox, oy)) = surface.mapped else {
                continue;
            };
            // What the GPU shows is the page's to draw, and not here.
            for rect in invalid.into_iter().filter_map(|rect| rect.clipped(surface.bounds())) {
                for rect in surface.off_gpu(rect) {
                    let placed = Rect { x: ox.saturating_add(rect.x), y: oy.saturating_add(rect.y), ..rect };
                    let Some(placed) = placed.clipped(output) else { continue };
                    // The source pixel under the placed rectangle's corner.
                    let origin = ((placed.x - ox) as usize, (placed.y - oy) as usize);
                    if framebuffer.blit_from(&surface.pixels, surface.stride(), origin, placed) {
                        updates.push(Update::Paint(placed));
                    }
                }
            }
            for shown in surface.gpu.iter_mut().filter(|shown| shown.at.is_none()) {
                shown.at = Some((ox, oy));
            }
        }
    }
}

/// Join rectangles shown from the GPU that line up — the same rows and touching
/// side to side, or the same columns and touching end to end — where they are of
/// the same picture and the same place on the output, until none do.
fn join_shown(shown: &mut Vec<OnGpu>) {
    let key = |s: &OnGpu| (s.at, s.unit);
    loop {
        let before = shown.len();
        // Side by side.
        shown.sort_by_key(|s| (key(s), s.rect.y, s.rect.height, s.rect.x));
        let mut joined: Vec<OnGpu> = Vec::with_capacity(shown.len());
        for next in shown.drain(..) {
            match joined.last_mut() {
                Some(last)
                    if key(last) == key(&next)
                        && (last.rect.y, last.rect.height) == (next.rect.y, next.rect.height)
                        && last.rect.x + last.rect.width == next.rect.x =>
                {
                    last.rect.width += next.rect.width;
                }
                _ => joined.push(next),
            }
        }
        // End to end.
        joined.sort_by_key(|s| (key(s), s.rect.x, s.rect.width, s.rect.y));
        for next in joined.drain(..) {
            match shown.last_mut() {
                Some(last)
                    if key(last) == key(&next)
                        && (last.rect.x, last.rect.width) == (next.rect.x, next.rect.width)
                        && last.rect.y + last.rect.height == next.rect.y =>
                {
                    last.rect.height += next.rect.height;
                }
                _ => shown.push(next),
            }
        }
        if shown.len() == before {
            return;
        }
    }
}

/// A surface as [`Graphics::plan`] walks a run: its size, where it is mapped, and
/// whether its stream has carried a chroma view.
struct Seen {
    size: (u32, u32),
    mapped: Option<(u32, u32)>,
    chroma: bool,
}

impl Seen {
    /// Where it lies on the output, if it is mapped.
    fn placed(&self) -> Option<Rect> {
        let (x, y) = self.mapped?;
        Some(Rect { x, y, width: self.size.0, height: self.size.1 })
    }
}

/// Whether a picture shown from the GPU on `surface` can be placed on the output
/// and read back from where it lands: the surface is mapped, wholly inside the
/// output, and no other mapped surface overlaps it.
fn placeable(surfaces: &BTreeMap<u16, Seen>, output: Option<(u32, u32)>, surface: u16) -> bool {
    let Some((width, height)) = output else { return false };
    let output = Rect { x: 0, y: 0, width, height };
    let Some(placed) = surfaces.get(&surface).and_then(Seen::placed) else { return false };
    placed.clipped(output) == Some(placed)
        && surfaces.iter().all(|(id, other)| *id == surface || other.placed().is_none_or(|other| !other.overlaps(&placed)))
}

impl Drop for Graphics {
    /// The measurement, said once when the channel is done with.
    fn drop(&mut self) {
        if !self.tally.commands.is_empty() {
            let frames = self.tally.commands.get(&gfx::CMD_END_FRAME).copied().unwrap_or(0);
            info!("rdp: the graphics pipeline carried {frames} frames; {}", self.tally.summary());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::gfx::{
        CMD_CACHE_TO_SURFACE, CMD_CAPS_CONFIRM, CMD_CREATE_SURFACE, CMD_END_FRAME, CMD_MAP_SURFACE_TO_OUTPUT,
        CMD_RESET_GRAPHICS, CMD_SOLID_FILL, CMD_START_FRAME, CMD_SURFACE_TO_CACHE,
        CMD_SURFACE_TO_SURFACE, CMD_WIRE_TO_SURFACE_1, CMD_WIRE_TO_SURFACE_2, CODEC_CAPROGRESSIVE, CODEC_PLANAR,
        CODEC_UNCOMPRESSED, PIXEL_XRGB_8888, pdu,
    };
    use crate::proto::wire::Writer;

    fn reset(width: u32, height: u32) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32_le(width);
        w.u32_le(height);
        w.u32_le(1); // monitorCount
        for field in [0, 0, width - 1, height - 1, 1] {
            w.u32_le(field); // the one monitor, primary
        }
        w.zeros(340 - 8 - 12 - 20); // the PDU's padding
        pdu(CMD_RESET_GRAPHICS, &w.finish())
    }

    fn create(surface: u16, width: u16, height: u16) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16_le(surface);
        w.u16_le(width);
        w.u16_le(height);
        w.u8(PIXEL_XRGB_8888);
        pdu(CMD_CREATE_SURFACE, &w.finish())
    }

    fn map(surface: u16, x: u32, y: u32) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16_le(surface);
        w.u16_le(0);
        w.u32_le(x);
        w.u32_le(y);
        pdu(CMD_MAP_SURFACE_TO_OUTPUT, &w.finish())
    }

    fn start(frame: u32) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32_le(0);
        w.u32_le(frame);
        pdu(CMD_START_FRAME, &w.finish())
    }

    fn end(frame: u32) -> Vec<u8> {
        pdu(CMD_END_FRAME, &frame.to_le_bytes())
    }

    fn wire(surface: u16, codec: u16, rect: (u16, u16, u16, u16), data: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16_le(surface);
        w.u16_le(codec);
        w.u8(PIXEL_XRGB_8888);
        w.u16_le(rect.0);
        w.u16_le(rect.1);
        w.u16_le(rect.2);
        w.u16_le(rect.3);
        w.u32_le(u32::try_from(data.len()).unwrap());
        w.bytes(data);
        pdu(CMD_WIRE_TO_SURFACE_1, &w.finish())
    }

    fn wire2(surface: u16, codec: u16, data: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16_le(surface);
        w.u16_le(codec);
        w.u32_le(0); // codecContextId
        w.u8(PIXEL_XRGB_8888);
        w.u32_le(u32::try_from(data.len()).unwrap());
        w.bytes(data);
        pdu(CMD_WIRE_TO_SURFACE_2, &w.finish())
    }

    fn solidfill(surface: u16, bgra: [u8; 4], rects: &[(u16, u16, u16, u16)]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16_le(surface);
        w.bytes(&bgra);
        w.u16_le(u16::try_from(rects.len()).unwrap());
        for r in rects {
            w.u16_le(r.0);
            w.u16_le(r.1);
            w.u16_le(r.2);
            w.u16_le(r.3);
        }
        pdu(CMD_SOLID_FILL, &w.finish())
    }

    fn s2s(src: u16, dst: u16, rect: (u16, u16, u16, u16), points: &[(u16, u16)]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16_le(src);
        w.u16_le(dst);
        w.u16_le(rect.0);
        w.u16_le(rect.1);
        w.u16_le(rect.2);
        w.u16_le(rect.3);
        w.u16_le(u16::try_from(points.len()).unwrap());
        for p in points {
            w.u16_le(p.0);
            w.u16_le(p.1);
        }
        pdu(CMD_SURFACE_TO_SURFACE, &w.finish())
    }

    fn s2c(surface: u16, slot: u16, rect: (u16, u16, u16, u16)) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16_le(surface);
        w.u32_le(0); // cacheKey low
        w.u32_le(0); // cacheKey high
        w.u16_le(slot);
        w.u16_le(rect.0);
        w.u16_le(rect.1);
        w.u16_le(rect.2);
        w.u16_le(rect.3);
        pdu(CMD_SURFACE_TO_CACHE, &w.finish())
    }

    fn c2s(slot: u16, surface: u16, points: &[(u16, u16)]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16_le(slot);
        w.u16_le(surface);
        w.u16_le(u16::try_from(points.len()).unwrap());
        for p in points {
            w.u16_le(p.0);
            w.u16_le(p.1);
        }
        pdu(CMD_CACHE_TO_SURFACE, &w.finish())
    }

    /// Several PDUs in one channel PDU, wrapped the way a server that compressed
    /// nothing would wrap them.
    fn packet(pdus: &[Vec<u8>]) -> Vec<u8> {
        zgfx::wrap(&pdus.concat())
    }

    fn receive(graphics: &mut Graphics, framebuffer: &Framebuffer, pdus: &[Vec<u8>]) -> Vec<Update> {
        graphics.receive(&packet(pdus), framebuffer).expect("well-formed PDUs")
    }

    /// The whole shape of a first frame: a reset sizes the output, a surface is
    /// created and mapped, one rectangle is drawn, and the EndFrame is when it
    /// reaches the framebuffer — swizzled to the framebuffer's order.
    #[test]
    fn a_frame_reaches_the_framebuffer_at_its_end_and_not_before() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        let updates = receive(&mut graphics, &framebuffer, &[reset(4, 4), create(1, 4, 4), map(1, 0, 0)]);
        assert_eq!(updates, vec![Update::Reset { width: 4, height: 4, monitors: vec![gfx::Placed { x: 0, y: 0, w: 4, h: 4 }] }]);
        framebuffer.with(|frame| assert_eq!((frame.width, frame.height), (4, 4)));

        // Two BGRX pixels at (1, 2) and (2, 2).
        let pixels = [10, 20, 30, 0xFF, 11, 21, 31, 0xFF];
        let updates = receive(&mut graphics, &framebuffer, &[start(1), wire(1, CODEC_UNCOMPRESSED, (1, 2, 3, 3), &pixels)]);
        assert!(updates.is_empty(), "nothing shows before the EndFrame: {updates:?}");
        framebuffer.with(|frame| assert!(frame.pixels.iter().all(|b| *b == 0)));

        let updates = receive(&mut graphics, &framebuffer, &[end(1)]);
        let painted = Rect { x: 1, y: 2, width: 2, height: 1 };
        assert_eq!(updates, vec![Update::Paint(painted), Update::Frame { id: 1, decoded: 1 }]);
        framebuffer.with(|frame| {
            let row: Vec<_> = frame.rows(painted).collect();
            assert_eq!(row, vec![&[30, 20, 10, 0, 31, 21, 11, 0][..]]);
        });
    }

    /// A surface mapped away from the origin lands where it was mapped, and the
    /// part of it past the output's edge is clipped rather than dropped.
    #[test]
    fn a_mapped_surface_is_placed_at_its_origin_and_clipped_to_the_output() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(4, 2), create(7, 2, 2), map(7, 3, 1)]);
        // A 2x2 planar rectangle, three flat planes: red 1, green 2, blue 3.
        let planar = [0x20, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 0];
        let updates = receive(&mut graphics, &framebuffer, &[start(5), wire(7, CODEC_PLANAR, (0, 0, 2, 2), &planar), end(5)]);
        // Only the one pixel of it inside a 4x2 output at (3, 1).
        assert_eq!(updates, vec![
            Update::Paint(Rect { x: 3, y: 1, width: 1, height: 1 }),
            Update::Frame { id: 5, decoded: 1 },
        ]);
        framebuffer.with(|frame| {
            assert_eq!(&frame.pixels[(4 + 3) * 4..], &[1, 2, 3, 0]);
            assert!(frame.pixels[..(4 + 3) * 4].iter().all(|b| *b == 0));
        });
    }

    /// The measurement: a codec with no decoder is counted, its rectangle left as it
    /// was, and the frame still ends and is acknowledged.
    #[test]
    fn a_codec_with_no_decoder_leaves_the_rectangle_alone_and_the_frame_still_ends() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(4, 4), create(1, 4, 4), map(1, 0, 0)]);
        let updates = receive(&mut graphics, &framebuffer, &[
            start(2),
            wire(1, CODEC_CAPROGRESSIVE, (0, 0, 4, 4), &[0xAA; 16]),
            wire(1, CODEC_CAPROGRESSIVE, (0, 0, 2, 2), &[0xAA; 4]),
            end(2),
        ]);
        assert_eq!(updates, vec![Update::Frame { id: 2, decoded: 1 }]);
        assert_eq!(graphics.tally.codecs.get(&CODEC_CAPROGRESSIVE), Some(&2));
        assert_eq!(graphics.tally.commands.get(&CMD_WIRE_TO_SURFACE_1), Some(&2));
        assert!(graphics.tally.summary().contains("Progressive 2"), "{}", graphics.tally.summary());
    }

    /// A rectangle whose payload does not decode is a hole, not the end: the
    /// session goes on and the frame is still acknowledged.
    #[test]
    fn a_payload_that_does_not_decode_costs_the_rectangle_and_not_the_session() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(4, 4), create(1, 4, 4), map(1, 0, 0)]);
        let updates = receive(&mut graphics, &framebuffer, &[
            start(3),
            wire(1, CODEC_UNCOMPRESSED, (0, 0, 2, 2), &[0; 8]), // half the pixels
            wire(1, CODEC_PLANAR, (0, 0, 2, 2), &[0x21, 0]),    // a colour loss level
            wire(2, CODEC_UNCOMPRESSED, (0, 0, 1, 1), &[0; 4]), // no such surface
            wire(1, CODEC_UNCOMPRESSED, (3, 3, 5, 5), &[0; 16]), // past the surface
            end(3),
        ]);
        assert_eq!(updates, vec![Update::Frame { id: 3, decoded: 1 }]);
    }

    /// A PDU that does not decode is another matter: the buffer after it is at an
    /// offset nothing vouches for, so it ends the session.
    #[test]
    fn a_malformed_pdu_ends_the_session() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        let mut truncated = create(1, 4, 4);
        truncated.pop();
        assert!(graphics.receive(&zgfx::wrap(&truncated), &framebuffer).is_err());
        assert!(graphics.receive(&[0xE2], &framebuffer).is_err(), "and so does a bad wrapper");
    }

    /// A reset in the middle of a session resizes the framebuffer at once — before
    /// the paints that follow it in the same PDU — and blanks the surfaces.
    #[test]
    fn a_reset_resizes_before_the_paints_that_follow_it() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(2, 2), create(1, 2, 2), map(1, 0, 0)]);
        receive(&mut graphics, &framebuffer, &[start(1), wire(1, CODEC_UNCOMPRESSED, (0, 0, 2, 2), &[9; 16]), end(1)]);
        framebuffer.with(|frame| assert!(frame.pixels.iter().all(|b| *b == 9 || *b == 0)));

        let updates = receive(&mut graphics, &framebuffer, &[
            reset(4, 4),
            create(1, 4, 4),
            map(1, 0, 0),
            start(2),
            wire(1, CODEC_UNCOMPRESSED, (3, 3, 4, 4), &[5, 6, 7, 0]),
            end(2),
        ]);
        assert_eq!(updates, vec![
            Update::Reset { width: 4, height: 4, monitors: vec![gfx::Placed { x: 0, y: 0, w: 4, h: 4 }] },
            Update::Paint(Rect { x: 3, y: 3, width: 1, height: 1 }),
            Update::Frame { id: 2, decoded: 2 },
        ]);
        framebuffer.with(|frame| {
            assert_eq!((frame.width, frame.height), (4, 4));
            assert_eq!(&frame.pixels[60..], &[7, 6, 5, 0]);
            assert!(frame.pixels[..60].iter().all(|b| *b == 0), "the old picture is gone");
        });
    }

    /// A Progressive PDU on the second wire-to-surface command paints its tiles into
    /// the surface at the region's rectangles, and the framebuffer shows them at
    /// EndFrame; the tiles are kept, so a later PDU on the same surface finds them.
    #[test]
    fn a_progressive_pdu_paints_its_region_and_keeps_its_tiles() {
        use crate::proto::progressive::testing::{empty_upgrade, flat_tile, grey, pdu as progressive, region};
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(100, 70), create(1, 100, 70), map(1, 0, 0)]);
        let first = progressive(&[region(&[(60, 66, 100, 100)], 1, &[flat_tile(0xCCC6, 1, 1, 9)])]);
        let updates = receive(&mut graphics, &framebuffer, &[start(1), wire2(1, CODEC_CAPROGRESSIVE, &first), end(1)]);
        let painted = Rect { x: 64, y: 66, width: 36, height: 4 };
        assert_eq!(updates, vec![Update::Paint(painted), Update::Frame { id: 1, decoded: 1 }]);
        let expected = grey(9);
        framebuffer.with(|frame| {
            for row in frame.rows(painted) {
                for px in row.as_chunks::<4>().0 {
                    assert_eq!(px, &expected);
                }
            }
            // The pixel left of the region is untouched.
            assert_eq!(&frame.pixels[(66 * 100 + 63) * 4..(66 * 100 + 64) * 4], &[0, 0, 0, 0]);
        });
        // An upgrade with nothing to add finds the tile and repaints it, not a refusal.
        let again = progressive(&[region(&[(64, 64, 36, 6)], 1, &[empty_upgrade(1, 1)])]);
        let updates = receive(&mut graphics, &framebuffer, &[start(2), wire2(1, CODEC_CAPROGRESSIVE, &again), end(2)]);
        assert_eq!(updates, vec![Update::Paint(Rect { x: 64, y: 64, width: 36, height: 6 }), Update::Frame { id: 2, decoded: 2 }]);
    }

    /// A solid fill paints one colour, and a rectangle that pokes past the surface
    /// is clipped to it rather than dropped.
    #[test]
    fn a_solid_fill_paints_one_colour_clipped_to_the_surface() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(4, 4), create(1, 4, 4), map(1, 0, 0)]);
        // Fill R=10 G=20 B=30 (wire order B, G, R, A) over a rectangle past the edge.
        let updates = receive(&mut graphics, &framebuffer, &[
            start(1),
            solidfill(1, [30, 20, 10, 0xFF], &[(2, 2, 6, 6)]),
            end(1),
        ]);
        let painted = Rect { x: 2, y: 2, width: 2, height: 2 };
        assert_eq!(updates, vec![Update::Paint(painted), Update::Frame { id: 1, decoded: 1 }]);
        framebuffer.with(|frame| {
            for row in frame.rows(painted) {
                for px in row.as_chunks::<4>().0 {
                    assert_eq!(px, &[10, 20, 30, 0]);
                }
            }
        });
    }

    /// A rectangle lifted into a cache slot and stamped back down lands where the
    /// server points it, and survives the frame that cached it.
    #[test]
    fn a_rectangle_cached_and_stamped_back_lands_where_it_is_told() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(4, 4), create(1, 4, 4), map(1, 0, 0)]);
        receive(&mut graphics, &framebuffer, &[
            start(1),
            wire(1, CODEC_UNCOMPRESSED, (0, 0, 1, 1), &[30, 20, 10, 0xFF]),
            s2c(1, 5, (0, 0, 1, 1)),
            c2s(5, 1, &[(3, 3)]),
            end(1),
        ]);
        framebuffer.with(|frame| {
            assert_eq!(&frame.pixels[..4], &[10, 20, 30, 0]);
            assert_eq!(&frame.pixels[(3 * 4 + 3) * 4..], &[10, 20, 30, 0]);
        });
    }

    /// A copy of a rectangle over itself — a scroll — reads its whole source before
    /// it writes, so the overlap does not smear.
    #[test]
    fn a_scroll_copies_a_rectangle_over_itself_correctly() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(2, 2), create(1, 2, 2), map(1, 0, 0)]);
        receive(&mut graphics, &framebuffer, &[
            start(1),
            // The top row is one colour; copy it down onto the bottom row.
            wire(1, CODEC_UNCOMPRESSED, (0, 0, 2, 1), &[30, 20, 10, 0xFF, 30, 20, 10, 0xFF]),
            s2s(1, 1, (0, 0, 2, 1), &[(0, 1)]),
            end(1),
        ]);
        framebuffer.with(|frame| {
            assert_eq!(&frame.pixels[..4], &[10, 20, 30, 0]);
            assert_eq!(&frame.pixels[8..12], &[10, 20, 30, 0]);
        });
    }

    /// A cache draw from an empty slot, a copy from a missing surface, and a stamp
    /// that does not fit are each a dropped rectangle, not the end of the session.
    #[test]
    fn a_bad_copy_or_cache_costs_the_rectangle_and_not_the_session() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(4, 4), create(1, 4, 4), map(1, 0, 0)]);
        let updates = receive(&mut graphics, &framebuffer, &[
            start(1),
            c2s(9, 1, &[(0, 0)]),           // empty slot
            s2s(2, 1, (0, 0, 2, 2), &[(0, 0)]), // no such source surface
            wire(1, CODEC_UNCOMPRESSED, (0, 0, 2, 2), &[0; 16]),
            s2c(1, 3, (0, 0, 2, 2)),
            c2s(3, 1, &[(3, 3)]),            // 2x2 at (3,3) does not fit a 4x4
            end(1),
        ]);
        assert_eq!(updates.last(), Some(&Update::Frame { id: 1, decoded: 1 }));
    }

    /// A surface larger than any desktop this client holds is refused before it is
    /// allocated, and so is an output.
    #[test]
    fn an_absurd_surface_or_output_is_refused_before_it_is_allocated() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        let err = graphics.receive(&packet(&[create(1, 32766, 32766)]), &framebuffer).unwrap_err();
        assert!(format!("{err}").contains("32766x32766"), "{err}");
        // And beside a surface already held, where the two together are past what 32
        // bits count to.
        receive(&mut graphics, &framebuffer, &[create(1, 1024, 1024)]);
        let err = graphics.receive(&packet(&[create(2, 32766, 32766)]), &framebuffer).unwrap_err();
        assert!(format!("{err}").contains("32766x32766"), "{err}");
        let err = graphics.receive(&packet(&[reset(32766, 32766)]), &framebuffer).unwrap_err();
        assert!(format!("{err}").contains("32766x32766"), "{err}");
    }

    /// Every surface a host creates draws on one budget: a second one that takes the
    /// total past it is refused however small it is on its own, and a surface created
    /// again under its own number replaces rather than adds.
    #[test]
    fn surfaces_share_one_budget() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        // Exactly the budget, in one surface. Zeroed pages the kernel hands out
        // lazily, so this costs the test nothing but address space.
        receive(&mut graphics, &framebuffer, &[create(1, 16384, 8192)]);
        let err = graphics.receive(&packet(&[create(2, 1, 1)]), &framebuffer).unwrap_err();
        assert!(format!("{err}").contains("512 MiB of surfaces"), "{err}");
        receive(&mut graphics, &framebuffer, &[create(1, 16384, 8192)]);
        assert_eq!(graphics.surfaces.len(), 1);
    }

    /// The cache slots draw on one budget: an entry that would take them past it is
    /// dropped, and the slot it was meant for is emptied rather than left stale; an
    /// entry replacing a slot's own is not counted against it.
    #[test]
    fn cache_slots_share_one_budget() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        // A 4096x4096 surface is exactly the cache budget when lifted whole.
        receive(&mut graphics, &framebuffer, &[reset(4, 4), create(1, 4096, 4096), map(1, 0, 0)]);
        receive(&mut graphics, &framebuffer, &[s2c(1, 1, (0, 0, 4096, 4096))]);
        assert_eq!(graphics.caches.len(), 1);
        receive(&mut graphics, &framebuffer, &[s2c(1, 2, (0, 0, 1, 1))]);
        assert!(!graphics.caches.contains_key(&2), "the second entry was dropped");
        receive(&mut graphics, &framebuffer, &[s2c(1, 1, (0, 0, 4096, 4096))]);
        assert_eq!(graphics.caches.len(), 1, "the slot's own entry is replaced, not added to");
        // Now the slot holds a small one; the next big one has room.
        receive(&mut graphics, &framebuffer, &[s2c(1, 1, (0, 0, 1, 1)), s2c(1, 2, (0, 0, 4096, 4095))]);
        assert_eq!(graphics.caches.len(), 2);
    }

    /// A surface drawn into before it is mapped shows what was drawn at the EndFrame
    /// after the map, rather than waiting for the host to draw it again.
    #[test]
    fn what_was_drawn_before_the_map_is_presented_after_it() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(4, 4), create(1, 4, 4)]);
        let updates = receive(&mut graphics, &framebuffer, &[
            start(1),
            wire(1, CODEC_UNCOMPRESSED, (0, 0, 1, 1), &[30, 20, 10, 0xFF]),
            map(1, 0, 0),
            end(1),
        ]);
        assert_eq!(updates, vec![
            Update::Paint(Rect { x: 0, y: 0, width: 1, height: 1 }),
            Update::Frame { id: 1, decoded: 1 },
        ]);
    }

    /// The host's CapsConfirm is the moment the session learns that its frames will
    /// be marked, and it is reported ahead of anything drawn.
    #[test]
    fn a_caps_confirm_is_reported_before_the_first_paint() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        let mut w = Writer::new();
        w.u32_le(0x000A_0002); // version 10.2
        w.u32_le(4); // capsDataLength
        w.u32_le(0); // flags
        let confirm = pdu(CMD_CAPS_CONFIRM, &w.finish());
        let updates = receive(&mut graphics, &framebuffer, &[confirm, reset(4, 4)]);
        assert_eq!(updates, vec![Update::Confirmed, Update::Reset { width: 4, height: 4, monitors: vec![gfx::Placed { x: 0, y: 0, w: 4, h: 4 }] }]);
    }

    fn confirm() -> Vec<u8> {
        let mut w = Writer::new();
        w.u32_le(gfx::CAPVERSION_10);
        w.u32_le(4);
        w.u32_le(0x22);
        pdu(CMD_CAPS_CONFIRM, &w.finish())
    }

    /// A passing pipeline hands every message on as the host sent it, still
    /// compressed, and draws none of its commands: what it says for itself is the
    /// confirmation and the reset, ahead of the message that carries them, and the
    /// frames the message ends.
    #[test]
    fn a_passing_pipeline_hands_each_message_on_as_the_host_sent_it() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::passing();
        let opening = [confirm(), reset(4, 4), create(1, 4, 4), map(1, 0, 0)];
        let first = [start(1), wire(1, CODEC_UNCOMPRESSED, (0, 0, 1, 1), &[1, 2, 3, 0]), end(1)];
        let second = [start(2), solidfill(1, [9, 9, 9, 0], &[(0, 0, 2, 2)])];
        let pdus: Vec<Vec<u8>> = opening.iter().chain(&first).chain(&second).cloned().collect();

        let updates = receive(&mut graphics, &framebuffer, &pdus);
        let mut drawn = Fidelity::default();
        drawn.saw(CODEC_UNCOMPRESSED, &[]);
        assert_eq!(updates, vec![
            Update::Confirmed,
            Update::Reset { width: 4, height: 4, monitors: vec![gfx::Placed { x: 0, y: 0, w: 4, h: 4 }] },
            Update::Drawn(drawn),
            Update::Passed { wrapped: packet(&pdus), frames: vec![1] },
        ]);
        framebuffer.with(|frame| assert_eq!((frame.width, frame.height), (0, 0), "nothing is drawn here"));
        assert!(graphics.surfaces.is_empty() && graphics.clear.is_none(), "and nothing is kept");
    }

    /// A frame that ends in a later message is passed as it arrives, and the message
    /// that ends it is the only one that names it; one that ends two names both.
    #[test]
    fn a_passing_pipeline_names_the_frames_each_message_ends() {
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::passing();
        let head = [reset(4, 4), create(1, 4, 4), map(1, 0, 0), start(7)];
        let tail = [solidfill(1, [1, 2, 3, 0], &[(0, 0, 4, 4)]), end(7), start(8), end(8), start(9)];
        assert_eq!(receive(&mut graphics, &framebuffer, &head), vec![
            Update::Reset { width: 4, height: 4, monitors: vec![gfx::Placed { x: 0, y: 0, w: 4, h: 4 }] },
            Update::Passed { wrapped: packet(&head), frames: Vec::new() },
        ]);
        assert_eq!(receive(&mut graphics, &framebuffer, &tail), vec![Update::Passed {
            wrapped: packet(&tail),
            frames: vec![7, 8],
        }]);
    }

    /// The history a compositor unwraps against is the pipeline's: a message may
    /// match into the one before it, as a host's do.
    #[test]
    fn a_compositor_unwraps_each_message_against_the_ones_before_it() {
        use crate::Compositor;
        let mut compositor = Compositor::new();
        assert_eq!(compositor.unwrap(&zgfx::wrap(b"ab")).unwrap(), b"ab");
        // A compressed segment of one match, distance 2 and length 6: see
        // `a_match_longer_than_its_distance_repeats_itself` in proto/zgfx.rs.
        let bits = (0b10_0010_0010_1010_u16 << 2).to_be_bytes();
        let matched = [0xE0, 0x24, bits[0], bits[1], 2];
        assert_eq!(compositor.unwrap(&matched).unwrap(), b"ababab");
        assert_ne!(
            Compositor::new().unwrap(&matched).unwrap(),
            b"ababab",
            "a compositor of its own has nothing of the other's history to match into"
        );
    }

    /// What is passed is what a compositor composes: the same commands, handed on
    /// and then composed, make the picture the host's own packets make — and the
    /// compositor says what it painted and when the output changed size.
    #[test]
    fn a_compositor_reports_what_a_run_did_to_the_picture() {
        use crate::{Composed, Compositor};
        let mut compositor = Compositor::new();
        let opening = [reset(4, 4), create(1, 4, 4), map(1, 0, 0)].concat();
        assert_eq!(
            compositor.compose(&opening).unwrap(),
            Composed { resized: Some((4, 4)), painted: Vec::new(), frames: 0, gpu: Vec::new() }
        );
        let frame = [start(1), solidfill(1, [10, 20, 30, 0], &[(1, 1, 3, 2)]), end(1)].concat();
        assert_eq!(
            compositor.compose(&frame).unwrap(),
            Composed { resized: None, painted: vec![Rect { x: 1, y: 1, width: 2, height: 1 }], frames: 1, gpu: Vec::new() }
        );
        compositor.framebuffer().with(|picture| {
            let at = (picture.width as usize + 1) * 4;
            assert_eq!(picture.pixels[at..at + 3], [30, 20, 10]);
        });
        // A command that does not decode is the end of what can be trusted.
        assert!(compositor.compose(&[0x09, 0, 0, 0, 0xFF, 0, 0, 0]).is_err());
    }

    #[test]
    fn what_is_passed_composes_to_the_picture_the_packets_make() {
        let pdus = [
            confirm(),
            reset(4, 4),
            create(1, 4, 4),
            map(1, 0, 0),
            start(1),
            solidfill(1, [10, 20, 30, 0], &[(0, 0, 4, 4)]),
            s2c(1, 3, (0, 0, 2, 2)),
            wire(1, CODEC_UNCOMPRESSED, (1, 1, 2, 2), &[1, 2, 3, 0]),
            c2s(3, 1, &[(2, 2)]),
            end(1),
        ];
        let direct = Framebuffer::new();
        receive(&mut Graphics::new(), &direct, &pdus);

        let mut passing = Graphics::passing();
        let relayed = Framebuffer::new();
        let mut composer = Graphics::new();
        let mut compositor = crate::Compositor::new();
        for update in receive(&mut passing, &Framebuffer::new(), &pdus) {
            if let Update::Passed { wrapped, .. } = update {
                let commands = compositor.unwrap(&wrapped).expect("a well-formed message");
                composer.compose(&commands, &relayed).expect("well-formed commands");
            }
        }
        let pixels = |framebuffer: &Framebuffer| framebuffer.with(|frame| (frame.width, frame.height, frame.pixels.clone()));
        assert_eq!(pixels(&relayed), pixels(&direct));
        assert_eq!(pixels(&direct).0, 4, "a picture was made at all");
    }

    /// A channel the host closes and opens again starts a pipeline of the same kind.
    #[test]
    fn a_fresh_pipeline_keeps_whether_it_passes() {
        assert!(Graphics::passing().fresh().pass);
        assert!(!Graphics::new().fresh().pass);
    }

    /// H.264 is painted from the pictures supplied for a run's access units, each
    /// under its number in the run — which counts every unit that reads, whatever
    /// becomes of its stream — and a unit with no picture leaves its rectangles as
    /// they were.
    #[test]
    fn h264_is_painted_from_the_pictures_supplied_for_its_units() {
        use crate::avc::testing::{close, flat, rgb_of};
        use crate::avc::{Scanned, scan};
        use crate::proto::avc::tests::{metablock, wrap444};
        use crate::proto::gfx::{CODEC_AVC420, CODEC_AVC444};

        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(64, 32), create(1, 32, 32), map(1, 0, 0), create(2, 32, 32), map(2, 32, 0)]);
        let masked = |rect: (u16, u16, u16, u16)| [metablock(&[(rect, 20, 100)]), vec![0, 0, 1, 0x65]].concat();
        let whole = (0, 0, 32, 32);
        let run = [
            start(1),
            // Unit 0, on the first surface.
            wire(1, CODEC_AVC420, whole, &masked((2, 2, 30, 20))),
            // No unit: a metablock that does not read.
            wire(1, CODEC_AVC420, whole, &[9, 0, 0, 0]),
            // Unit 1, for a surface that does not exist: counted all the same.
            wire(7, CODEC_AVC420, whole, &masked((0, 0, 8, 8))),
            // Units 2 and 3, the two views of one picture on the second surface.
            wire(2, CODEC_AVC444, whole, &wrap444(Some(&masked((0, 0, 16, 16))), Some(&masked((0, 0, 16, 16))))),
            // Unit 4, which is supplied no picture.
            wire(2, CODEC_AVC420, whole, &masked((16, 16, 32, 32))),
            end(1),
        ]
        .concat();
        let units = scan(&run).unwrap().into_iter().filter(|found| matches!(found, Scanned::Unit(_))).count();
        assert_eq!(units, 5, "the scan numbers what the compositor counts");

        let (first, second) = ([140u8, 90, 170], [100u8, 80, 160]);
        graphics.supply(0, flat(32, 32, first));
        graphics.supply(2, flat(32, 32, second));
        // One flat colour in both views: under the first layout each macroblock of
        // the chroma view's luma plane holds U's odd rows in its top eight rows and
        // V's in its bottom eight, and its chroma planes the odd columns.
        let mut aux = flat(32, 32, [0, second[1], second[2]]);
        for (row, luma) in aux.data[..32 * 32].as_chunks_mut::<32>().0.iter_mut().enumerate() {
            luma.fill(if row & 15 < 8 { second[1] } else { second[2] });
        }
        graphics.supply(3, aux);
        let updates = graphics.compose(&run, &framebuffer).unwrap();
        let painted = [Rect { x: 2, y: 2, width: 28, height: 18 }, Rect { x: 32, y: 0, width: 16, height: 16 }];
        assert_eq!(updates, vec![Update::Paint(painted[0]), Update::Paint(painted[1]), Update::Frame { id: 1, decoded: 1 }]);
        framebuffer.with(|frame| {
            for (rect, yuv) in painted.iter().zip([first, second]) {
                for row in frame.rows(*rect) {
                    for px in row.as_chunks::<4>().0 {
                        assert!(close(&px[..3], rgb_of(yuv)), "{px:?} vs {:?}", rgb_of(yuv));
                    }
                }
            }
            assert_eq!(&frame.pixels[..4], &[0, 0, 0, 0], "outside the mask is untouched");
            let unsupplied = &frame.pixels[(20 * 64 + 52) * 4..][..4];
            assert_eq!(unsupplied, &[0, 0, 0, 0], "a unit with no picture paints nothing");
        });
        assert!(graphics.supplied.is_empty(), "a run's pictures do not outlive it");

        // The next run counts from nothing again, and a picture left over from
        // this one is not there to be taken by it.
        graphics.supply(1, flat(32, 32, first));
        let next = [start(2), wire(1, CODEC_AVC420, whole, &masked((0, 0, 4, 4))), end(2)].concat();
        assert_eq!(graphics.compose(&next, &framebuffer).unwrap(), vec![Update::Frame { id: 2, decoded: 2 }]);
        graphics.supply(0, flat(32, 32, second));
        let updates = graphics.compose(&next, &framebuffer).unwrap();
        assert_eq!(updates[0], Update::Paint(Rect { x: 0, y: 0, width: 4, height: 4 }));
    }
    /// An H.264 unit's rectangles in a stream of its own, for the GPU tests below.
    fn masked(rect: (u16, u16, u16, u16)) -> Vec<u8> {
        use crate::proto::avc::tests::metablock;
        [metablock(&[(rect, 20, 100)]), vec![0, 0, 1, 0x65]].concat()
    }

    /// The pixels the framebuffer's paints cover, each counted.
    fn covered(updates: &[Update], width: u32, height: u32) -> Vec<u32> {
        let mut counts = vec![0; (width * height) as usize];
        for update in updates {
            if let Update::Paint(rect) = update {
                for y in rect.y..rect.y + rect.height {
                    for x in rect.x..rect.x + rect.width {
                        counts[(y * width + x) as usize] += 1;
                    }
                }
            }
        }
        counts
    }

    /// A picture shown from the GPU is not copied to the framebuffer: what the run
    /// painted around it is, once, and the page is told to draw it where the
    /// surface is mapped. A later copy out of it is planned as a read, and once its
    /// pixels are read back into the surface the copy carries them.
    #[test]
    fn a_picture_shown_from_the_gpu_is_left_to_the_page_and_read_back_before_a_copy() {
        use crate::proto::gfx::CODEC_AVC420;

        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(64, 32), create(1, 32, 32), map(1, 16, 0)]);
        let run = [
            start(1),
            solidfill(1, [9, 9, 9, 0], &[(0, 0, 32, 32)]),
            wire(1, CODEC_AVC420, (0, 0, 32, 32), &masked((4, 4, 20, 20))),
            end(1),
        ]
        .concat();
        let plan = graphics.plan(&run).unwrap();
        assert_eq!(plan, Plan { gpu: vec![true], restore: vec![] });
        graphics.show_on_gpu(0);
        let updates = graphics.compose(&run, &framebuffer).unwrap();
        let shown = Rect { x: 20, y: 4, width: 16, height: 16 };
        let counts = covered(&updates, 64, 32);
        for y in 0..32 {
            for x in 16..48 {
                let pixel = Rect { x, y, width: 1, height: 1 };
                let want = u32::from(!shown.overlaps(&pixel));
                assert_eq!(counts[(y * 64 + x) as usize], want, "pixel {x},{y}");
            }
        }
        assert_eq!(graphics.take_gpu_paints(), vec![GpuPaint { unit: 0, output: shown, source: (4, 4) }]);
        let area = GpuArea { surface: 1, rect: Rect { x: 4, y: 4, width: 16, height: 16 }, output: shown };
        assert_eq!(graphics.gpu_areas(), vec![area]);

        // A copy out of it: planned as a read of what the GPU shows, read back, and
        // then carried where it goes like any other pixels.
        let copy = [start(2), s2s(1, 1, (8, 8, 12, 12), &[(24, 24)]), end(2)].concat();
        let plan = graphics.plan(&copy).unwrap();
        assert_eq!(plan, Plan { gpu: vec![], restore: vec![area] });
        let read_back: Vec<u8> = [40u8, 50, 60, 255].repeat(16 * 16);
        graphics.restore(1, area.rect, &read_back).unwrap();
        assert_eq!(graphics.gpu_areas(), vec![], "read back, the pixels are the surface's again");
        let updates = graphics.compose(&copy, &framebuffer).unwrap();
        assert_eq!(updates[0], Update::Paint(Rect { x: 40, y: 24, width: 4, height: 4 }));
        framebuffer.with(|frame| {
            for row in frame.rows(Rect { x: 40, y: 24, width: 4, height: 4 }) {
                for px in row.as_chunks::<4>().0 {
                    assert_eq!(px, &[40, 50, 60, 0]);
                }
            }
        });
    }

    /// What the run writes over a picture shown from the GPU is the surface's
    /// again: copied to the framebuffer, and left out of what the page draws.
    #[test]
    fn a_write_over_a_picture_shown_from_the_gpu_takes_that_part_back() {
        use crate::proto::gfx::CODEC_AVC420;

        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(32, 32), create(1, 32, 32), map(1, 0, 0)]);
        let run = [
            start(1),
            wire(1, CODEC_AVC420, (0, 0, 32, 32), &masked((0, 0, 16, 16))),
            solidfill(1, [9, 9, 9, 0], &[(0, 0, 16, 8)]),
            end(1),
        ]
        .concat();
        assert_eq!(graphics.plan(&run).unwrap().gpu, vec![true]);
        graphics.show_on_gpu(0);
        let updates = graphics.compose(&run, &framebuffer).unwrap();
        assert_eq!(updates[0], Update::Paint(Rect { x: 0, y: 0, width: 16, height: 8 }));
        let rest = Rect { x: 0, y: 8, width: 16, height: 8 };
        assert_eq!(graphics.take_gpu_paints(), vec![GpuPaint { unit: 0, output: rest, source: (0, 8) }]);

        // And in a later run: the fill is copied, and nothing is left to draw.
        let later = [start(2), solidfill(1, [9, 9, 9, 0], &[(0, 8, 16, 16)]), end(2)].concat();
        assert_eq!(graphics.plan(&later).unwrap(), Plan::default());
        let updates = graphics.compose(&later, &framebuffer).unwrap();
        assert_eq!(updates[0], Update::Paint(rest));
        assert_eq!(graphics.take_gpu_paints(), vec![]);
        assert_eq!(graphics.gpu_areas(), vec![]);
    }

    /// A desktop drawn all in H.264 stays a few rectangles shown from the GPU: a
    /// mask of tiles is one, and a mask over part of it, which cuts it into pieces,
    /// is one again once its run is drawn.
    #[test]
    fn pictures_shown_from_the_gpu_over_one_another_stay_a_few_rectangles() {
        use crate::proto::avc::tests::metablock;
        use crate::proto::gfx::CODEC_AVC420;

        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(64, 64), create(1, 64, 64), map(1, 0, 0)]);
        let tiles: Vec<_> =
            (0..16u16).map(|i| (((i % 4) * 16, (i / 4) * 16, (i % 4) * 16 + 16, (i / 4) * 16 + 16), 20, 100)).collect();
        let tiled = [metablock(&tiles), vec![0, 0, 1, 0x65]].concat();
        let whole = (0, 0, 64, 64);
        for (frame, mask) in [(1, tiled), (2, masked((8, 8, 40, 40)))] {
            let run = [start(frame), wire(1, CODEC_AVC420, whole, &mask), end(frame)].concat();
            assert_eq!(graphics.plan(&run).unwrap().gpu, vec![true]);
            graphics.show_on_gpu(0);
            graphics.compose(&run, &framebuffer).unwrap();
            let all = Rect { x: 0, y: 0, width: 64, height: 64 };
            assert_eq!(graphics.gpu_areas(), vec![GpuArea { surface: 1, rect: all, output: all }], "frame {frame}");
        }
    }

    /// A picture is shown from the GPU only where the page can draw it and read it
    /// back: not past the run's last EndFrame, not where the run reads it again,
    /// not on a surface unmapped, partly off the output or under another, and not
    /// where its luma view combines with a chroma view.
    #[test]
    fn the_plan_shows_from_the_gpu_only_what_the_page_can_draw_and_read_back() {
        use crate::proto::avc::tests::wrap444;
        use crate::proto::gfx::{CODEC_AVC420, CODEC_AVC444_V2, CODEC_CLEARCODEC};

        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(
            &mut graphics,
            &framebuffer,
            &[reset(64, 32), create(1, 32, 32), map(1, 0, 0), create(2, 16, 16), create(3, 32, 32), map(3, 48, 0)],
        );
        let whole = (0, 0, 32, 32);
        let unit = |surface: u16| wire(surface, CODEC_AVC420, whole, &masked((0, 0, 8, 8)));
        let plan = |commands: &[Vec<u8>]| graphics.plan(&commands.concat()).unwrap().gpu;

        assert_eq!(plan(&[start(1), unit(1), end(1)]), vec![true]);
        assert_eq!(plan(&[start(1), unit(1)]), vec![false], "its frame ends in another run");
        assert_eq!(plan(&[start(1), unit(1), s2c(1, 0, (0, 0, 4, 4)), end(1)]), vec![false], "the run reads it");
        assert_eq!(
            plan(&[start(1), unit(1), wire(1, CODEC_CLEARCODEC, (20, 20, 24, 24), &[0]), end(1)]),
            vec![false],
            "a ClearCodec rectangle is drawn over what the surface holds"
        );
        assert_eq!(plan(&[start(1), unit(2), end(1)]), vec![false], "an unmapped surface");
        assert_eq!(plan(&[start(1), map(2, 0, 0), unit(2), end(1)]), vec![false], "under another surface");
        assert_eq!(plan(&[start(1), unit(2), map(2, 32, 0), end(1)]), vec![true], "mapped by its frame's end");
        assert_eq!(plan(&[start(1), unit(3), end(1)]), vec![false], "partly off the output");
        let luma = masked((0, 0, 8, 8));
        assert_eq!(
            plan(&[start(1), wire(1, CODEC_AVC444_V2, whole, &wrap444(Some(&luma), None)), end(1)]),
            vec![true],
            "a luma view alone"
        );
        assert_eq!(
            plan(&[
                start(1),
                wire(1, CODEC_AVC444_V2, whole, &wrap444(Some(&luma), None)),
                wire(1, CODEC_AVC444_V2, whole, &wrap444(Some(&luma), Some(&luma))),
                end(1),
            ]),
            vec![false, false, false],
            "luma views that a chroma view combines with"
        );
    }
}
