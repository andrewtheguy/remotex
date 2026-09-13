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
//! bitmap update uses, the rows the right way up this time. Every other codec a
//! server names is counted and left unpainted, with the first sighting of each said
//! once in the log: that count is what decides which decoder is written next, and a
//! codec this client cannot read is a hole in the picture rather than the end of the
//! session. The commands that copy between surfaces and the cache are counted the
//! same way for now.
//!
//! [`Graphics::receive`] is the whole of it: one channel PDU in, and out come the
//! things the session has to act on that the framebuffer cannot show — the output
//! being redefined, the rectangles painted, and each frame's end.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use log::{debug, info, warn};

use super::framebuffer::{Framebuffer, Rect, affordable, stage};
use super::proto::bitmap::MAX_DESKTOP_BYTES;
use super::proto::gfx::{self, Message, Point16, Rect16};
use super::proto::wire::Malformed;
use super::proto::{clear, planar, progressive, zgfx};

/// Most rectangles a surface holds as changed before two of them are merged to
/// make room — coarser, never longer. See [`stage`] for which two.
const DAMAGE_CAP: usize = 64;

/// Most bytes the cache slots hold together. The caps this client advertises promise
/// the host a 16 MiB cache (`SMALL_CACHE`), so a host that keeps its word never comes
/// near this; one that does not is refused the entry rather than the process's memory.
const CACHE_BUDGET: usize = 64 << 20;

/// Something the pipeline did that the session has to act on, in the order it
/// happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Update {
    /// The host confirmed the pipeline: everything it draws from here on comes
    /// inside a frame. Reported before the first paint, which is what a consumer
    /// that paces frames itself needs to know before it receives one.
    Confirmed,
    /// The output is now this size. The framebuffer has already been resized and
    /// cleared; the caller is what tells the world.
    Reset { width: u32, height: u32 },
    /// This rectangle of the framebuffer was painted.
    Paint(Rect),
    /// The server finished frame `id`, and this client has finished `decoded` in all.
    Frame { id: u32, decoded: u32 },
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
        self.write_rows(rect, bgrx, usize::from(rect.width()) * 4);
    }

    /// Write one rectangle of `BGRX32` rows that start `stride` bytes apart — a
    /// window onto a larger buffer — swizzling into the framebuffer's order.
    fn write_rows(&mut self, rect: Rect16, bgrx: &[u8], src_stride: usize) {
        let width = usize::from(rect.width());
        let stride = self.stride();
        for row in 0..usize::from(rect.height()) {
            let src = &bgrx[row * src_stride..row * src_stride + width * 4];
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 4;
            let dst = &mut self.pixels[at..at + width * 4];
            for (out, px) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
                *out = [px[2], px[1], px[0], 0];
            }
        }
        self.invalidate(Rect {
            x: u32::from(rect.left),
            y: u32::from(rect.top),
            width: u32::from(rect.width()),
            height: u32::from(rect.height()),
        });
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
        let stride = self.stride();
        let bytes = width as usize * 4;
        for (row, src) in packed.chunks_exact(bytes).take(height as usize).enumerate() {
            let at = (y as usize + row) * stride + x as usize * 4;
            self.pixels[at..at + bytes].copy_from_slice(src);
        }
        self.invalidate(Rect { x, y, width, height });
    }

    /// Record a rectangle drawn into — see [`stage`] for how the list is kept to
    /// [`DAMAGE_CAP`].
    fn invalidate(&mut self, rect: Rect) {
        stage(&mut self.invalid, rect, DAMAGE_CAP);
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
pub(super) struct Graphics {
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
}

impl Graphics {
    pub(super) fn new() -> Self {
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
        }
    }

    /// One PDU off the channel: unwrapped, decoded, and acted on. The pixels go
    /// into `framebuffer` here; what comes back is what the caller has to do
    /// about them.
    ///
    /// A PDU that does not decode ends the session, as any other malformed PDU
    /// does. A *codec payload* that does not decode does not: it is one rectangle
    /// the server will draw again, and it is logged and counted instead.
    pub(super) fn receive(&mut self, data: &[u8], framebuffer: &Framebuffer) -> Result<Vec<Update>> {
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
        self.zgfx.decompress(data, buffer).context("unwrapping a graphics pipeline PDU")?;
        let mut updates = Vec::new();
        for message in gfx::messages(buffer) {
            self.act(message?, framebuffer, &mut updates)?;
        }
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
                debug!("rdp: graphics reset to {width}x{height} over {monitors} monitors");
                framebuffer.resize(width, height);
                self.output = Some((width, height));
                // The surfaces stay, blank, until the server draws into them again;
                // FreeRDP keeps them too.
                for surface in self.surfaces.values_mut() {
                    surface.pixels.fill(0);
                    surface.invalid.clear();
                }
                updates.push(Update::Reset { width, height });
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
                anyhow::ensure!(
                    held + bytes <= MAX_DESKTOP_BYTES,
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
                self.draw(surface, codec, format, rect, data);
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
                let clear = self.clear.get_or_insert_with(|| Box::new(clear::Clear::new()));
                clear.decompress(data, &mut self.pixels, width, height).map(|()| &self.pixels[..])
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
            found.write_rows(rect, rows, stride);
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
            for rect in invalid {
                let Some(rect) = rect.clipped(surface.bounds()) else { continue };
                let placed = Rect { x: ox.saturating_add(rect.x), y: oy.saturating_add(rect.y), ..rect };
                let Some(placed) = placed.clipped(output) else { continue };
                // The source pixel under the placed rectangle's corner.
                let origin = ((placed.x - ox) as usize, (placed.y - oy) as usize);
                if framebuffer.blit_from(&surface.pixels, surface.stride(), origin, placed) {
                    updates.push(Update::Paint(placed));
                }
            }
        }
    }
}

impl Drop for Graphics {
    /// The measurement, said once when the channel is done with.
    fn drop(&mut self) {
        if !self.tally.commands.is_empty() {
            info!("rdp: the graphics pipeline carried {} frames; {}", self.decoded, self.tally.summary());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdp_client::proto::gfx::{
        CMD_CACHE_TO_SURFACE, CMD_CAPS_CONFIRM, CMD_CREATE_SURFACE, CMD_END_FRAME, CMD_MAP_SURFACE_TO_OUTPUT,
        CMD_RESET_GRAPHICS, CMD_SOLID_FILL, CMD_START_FRAME, CMD_SURFACE_TO_CACHE,
        CMD_SURFACE_TO_SURFACE, CMD_WIRE_TO_SURFACE_1, CMD_WIRE_TO_SURFACE_2, CODEC_CAPROGRESSIVE, CODEC_PLANAR,
        CODEC_UNCOMPRESSED, PIXEL_XRGB_8888, pdu,
    };
    use crate::rdp_client::proto::wire::Writer;

    fn reset(width: u32, height: u32) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32_le(width);
        w.u32_le(height);
        w.u32_le(0);
        w.zeros(340 - 8 - 12);
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
        assert_eq!(updates, vec![Update::Reset { width: 4, height: 4 }]);
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
            Update::Reset { width: 4, height: 4 },
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
        use crate::rdp_client::proto::progressive::testing::{empty_upgrade, flat_tile, grey, pdu as progressive, region};
        let framebuffer = Framebuffer::new();
        let mut graphics = Graphics::new();
        receive(&mut graphics, &framebuffer, &[reset(100, 70), create(1, 100, 70), map(1, 0, 0)]);
        let first = progressive(&[region(&[(60, 66, 100, 100)], 1, &[flat_tile(0xCCC6, 1, 1, 9)])]);
        let updates = receive(&mut graphics, &framebuffer, &[start(1), wire2(1, CODEC_CAPROGRESSIVE, &first), end(1)]);
        let painted = Rect { x: 64, y: 66, width: 36, height: 4 };
        assert_eq!(updates, vec![Update::Paint(painted), Update::Frame { id: 1, decoded: 1 }]);
        let expected = { let g = grey(9); [g[2], g[1], g[0], 0] };
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
        assert_eq!(updates, vec![Update::Confirmed, Update::Reset { width: 4, height: 4 }]);
    }
}
