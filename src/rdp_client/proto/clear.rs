//! ClearCodec, the codec a Windows desktop draws its flat regions and text in.
//!
//! ClearCodec (`CLEARCODEC`, [MS-RDPEGFX] 3.1.8) is built for the desktop rather than
//! for photographs: runs of one colour, vertical bars it caches and repeats, whole
//! rectangles it stores under a glyph index and stamps down again, and — for the
//! parts that are neither flat nor repeated — a small run-length subcodec (RLEX) over
//! a per-rectangle palette. On the sandbox this client is written against it is the
//! desktop's primary codec, so a lit desktop needs it.
//!
//! A rectangle is decoded whole into a packed `BGRX32` buffer, top row first, the way
//! [`super::planar`] produces one, so the surface above swizzles it into the
//! framebuffer's order with the same path. The decoder keeps its caches — the glyph
//! cache, and the two vertical-bar caches — for the life of the channel, because a
//! later rectangle refers back to them by index.
//!
//! # What is and is not read
//!
//! Everything a modern Windows RDS host sends: the residual run-length layer, the
//! banded vertical bars with both their caches, the glyph cache, and all three
//! subcodecs — raw, RLEX, and NSCodec, which lives in [`super::nsc`] and which the
//! sandbox turned out to lean on for pictures and anti-aliased text. A rectangle that
//! cannot be read is refused, which the caller turns into one unpainted rectangle
//! rather than the end of the session.
//!
//! Ported from FreeRDP's `libfreerdp/codec/clear.c`.
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use super::nsc::Nsc;
use super::wire::{Malformed, Reader};

const WHAT: &str = "a ClearCodec rectangle";

/// Glyph flags, [MS-RDPEGFX] 2.2.4.1.
const FLAG_GLYPH_INDEX: u8 = 0x01;
const FLAG_GLYPH_HIT: u8 = 0x02;
const FLAG_CACHE_RESET: u8 = 0x04;

/// The glyph cache holds this many entries, the vertical-bar caches these many.
const GLYPH_COUNT: usize = 4000;
const VBAR_SIZE: usize = 32768;
const VBAR_SHORT_SIZE: usize = 16384;

/// A glyph no larger than this many pixels may be cached — FreeRDP's own bound.
const MAX_GLYPH_PIXELS: usize = 1024 * 1024;
/// A vertical bar is at most this tall.
const MAX_VBAR_HEIGHT: usize = 52;

/// Low-bit masks, `CLEAR_8BIT_MASKS`: index `n` is the low `n` bits set.
const MASKS: [u8; 9] = [0x00, 0x01, 0x03, 0x07, 0x0F, 0x1F, 0x3F, 0x7F, 0xFF];

fn refuse(field: &'static str, value: u64) -> Malformed {
    Malformed::Refused { what: WHAT, field, value }
}

/// A rectangle stored under a glyph index, in packed `BGRX32`.
#[derive(Clone, Default)]
struct Glyph {
    pixels: Vec<u8>,
    /// The pixel count the rectangle was stored at.
    count: usize,
}

/// One vertical bar's pixels, packed `BGRX32`; its height is `pixels.len() / 4`.
#[derive(Clone, Default)]
struct VBar {
    pixels: Vec<u8>,
}

/// The codec's state, kept for the life of the channel.
pub struct Clear {
    /// The sequence number the next rectangle must carry, as FreeRDP tracks it.
    seq: u32,
    glyphs: Vec<Glyph>,
    vbars: Vec<VBar>,
    vbar_cursor: usize,
    short_vbars: Vec<VBar>,
    short_cursor: usize,
    /// The NSCodec subcodec's plane buffers.
    nsc: Nsc,
}

impl Default for Clear {
    fn default() -> Self {
        Self::new()
    }
}

/// What the glyph header at the front of a rectangle asked for.
enum Glyphish {
    /// No glyph index: decode into the rectangle, store nothing.
    None,
    /// A glyph hit: the rectangle is the cached glyph, already written.
    Hit,
    /// A glyph index to store the decoded rectangle under.
    Store(usize),
}

impl Clear {
    pub fn new() -> Self {
        Self {
            seq: 0,
            glyphs: vec![Glyph::default(); GLYPH_COUNT],
            vbars: vec![VBar::default(); VBAR_SIZE],
            vbar_cursor: 0,
            short_vbars: vec![VBar::default(); VBAR_SHORT_SIZE],
            short_cursor: 0,
            nsc: Nsc::default(),
        }
    }

    /// Decode one rectangle into `out` as `width * height` packed `BGRX32`, top row
    /// first.
    pub fn decompress(
        &mut self,
        src: &[u8],
        out: &mut Vec<u8>,
        width: u16,
        height: u16,
    ) -> Result<(), Malformed> {
        let (w, h) = (usize::from(width), usize::from(height));
        out.clear();
        out.resize(w * h * 4, 0);

        let mut r = Reader::new(WHAT, src);
        let glyph_flags = r.u8()?;
        let seq = u32::from(r.u8()?);

        // The sequence number: it starts at whatever the first rectangle carries,
        // then each rectangle must carry the one after the last, mod 256.
        if self.seq == 0 && seq != 0 {
            self.seq = seq;
        }
        if seq != self.seq {
            return Err(refuse("a ClearCodec sequence number", u64::from(seq)));
        }
        self.seq = (seq + 1) % 256;

        if glyph_flags & FLAG_CACHE_RESET != 0 {
            self.vbar_cursor = 0;
            self.short_cursor = 0;
        }

        let glyph = self.glyph_prologue(&mut r, glyph_flags, w, h, out)?;

        // A glyph hit with no room for a composition header is the whole rectangle.
        if r.rest().len() < 12 {
            let both = FLAG_GLYPH_HIT | FLAG_GLYPH_INDEX;
            if glyph_flags & both == both {
                return Ok(());
            }
            return Err(r.missing("a ClearCodec composition header"));
        }

        let residual = r.u32_le()? as usize;
        let bands = r.u32_le()? as usize;
        let subcodec = r.u32_le()? as usize;

        if residual > 0 {
            let section = r.bytes(residual)?;
            residual_data(section, w, h, out)?;
        }
        if bands > 0 {
            let section = r.bytes(bands)?;
            self.bands_data(section, w, h, out)?;
        }
        if subcodec > 0 {
            let section = r.bytes(subcodec)?;
            subcodec_data(section, w, h, out, &mut self.nsc)?;
        }

        if let Glyphish::Store(slot) = glyph {
            self.glyphs[slot] = Glyph { pixels: out.clone(), count: w * h };
        }
        Ok(())
    }

    /// Read the glyph header, and either fill `out` from the cache (a hit) or return
    /// the slot the decoded rectangle is to be stored under.
    fn glyph_prologue(
        &mut self,
        r: &mut Reader<'_>,
        flags: u8,
        w: usize,
        h: usize,
        out: &mut [u8],
    ) -> Result<Glyphish, Malformed> {
        if flags & FLAG_GLYPH_HIT != 0 && flags & FLAG_GLYPH_INDEX == 0 {
            return Err(refuse("a glyph hit without a glyph index, flags", u64::from(flags)));
        }
        if flags & FLAG_GLYPH_INDEX == 0 {
            return Ok(Glyphish::None);
        }
        if w * h > MAX_GLYPH_PIXELS {
            return Err(refuse("a glyph larger than a megapixel", (w * h) as u64));
        }
        let index = usize::from(r.u16_le()?);
        if index >= GLYPH_COUNT {
            return Err(refuse("a glyph index", index as u64));
        }
        if flags & FLAG_GLYPH_HIT != 0 {
            let entry = &self.glyphs[index];
            if entry.pixels.is_empty() || w * h > entry.count {
                return Err(refuse("a glyph hit on an entry too small, index", index as u64));
            }
            out[..w * h * 4].copy_from_slice(&entry.pixels[..w * h * 4]);
            return Ok(Glyphish::Hit);
        }
        Ok(Glyphish::Store(index))
    }

    /// The banded vertical bars: the layer that carries the desktop's structure and
    /// leans on the two vertical-bar caches. [MS-RDPEGFX] 2.2.4.2 and 3.1.8.2.3.
    fn bands_data(&mut self, section: &[u8], w: usize, h: usize, out: &mut [u8]) -> Result<(), Malformed> {
        let mut r = Reader::new(WHAT, section);
        let mut dst = Dst { out, w, h };
        while !r.is_empty() {
            let x_start = r.u16_le()?;
            let x_end = r.u16_le()?;
            let y_start = usize::from(r.u16_le()?);
            let y_end = usize::from(r.u16_le()?);
            let bkg = [r.u8()?, r.u8()?, r.u8()?]; // B, G, R
            if x_end < x_start {
                return Err(refuse("a band ending left of its start", u64::from(x_end)));
            }
            if y_end < y_start {
                return Err(refuse("a band ending above its start", y_end as u64));
            }
            let vbar_count = usize::from(x_end - x_start) + 1;
            let vbar_height = y_end - y_start + 1;
            if vbar_height > MAX_VBAR_HEIGHT {
                return Err(refuse("a band taller than 52 rows", vbar_height as u64));
            }

            for i in 0..vbar_count {
                let header = r.u16_le()?;
                let column = if header & 0xC000 == 0x4000 {
                    // Short vertical bar, cache hit: its pixels are the cached ones,
                    // its start row comes on the wire.
                    let index = usize::from(header & 0x3FFF);
                    let yon = usize::from(r.u8()?);
                    let short_pixels = self.short_vbars[index].pixels.clone();
                    self.store_vbar(build_column(vbar_height, yon, &short_pixels, bkg))
                } else if header & 0xC000 == 0x0000 {
                    // Short vertical bar, cache miss: read its pixels, cache them.
                    let yon = usize::from(header & 0xFF);
                    let yoff = usize::from((header >> 8) & 0x3F);
                    if yoff < yon {
                        return Err(refuse("a short vBar ending above its start", yoff as u64));
                    }
                    let short_count = yoff - yon;
                    if short_count > MAX_VBAR_HEIGHT {
                        return Err(refuse("a short vBar taller than 52 rows", short_count as u64));
                    }
                    let mut short_pixels = Vec::with_capacity(short_count * 4);
                    for _ in 0..short_count {
                        let (b, g, red) = (r.u8()?, r.u8()?, r.u8()?);
                        short_pixels.extend_from_slice(&[b, g, red, 0]);
                    }
                    self.short_vbars[self.short_cursor] = VBar { pixels: short_pixels.clone() };
                    self.short_cursor = (self.short_cursor + 1) % VBAR_SHORT_SIZE;
                    self.store_vbar(build_column(vbar_height, yon, &short_pixels, bkg))
                } else if header & 0x8000 == 0x8000 {
                    // Full vertical bar, cache hit: the stored column, fitted to the
                    // band's height.
                    let index = usize::from(header & 0x7FFF);
                    if self.vbars[index].pixels.is_empty() {
                        self.vbars[index].pixels = vec![0; vbar_height * 4];
                    }
                    fit_column(&self.vbars[index].pixels, vbar_height)
                } else {
                    return Err(refuse("an invalid vBar header", u64::from(header)));
                };

                draw_column(&mut dst, usize::from(x_start), y_start, i, &column, vbar_height);
            }
        }
        Ok(())
    }

    /// Store a freshly built column under the rolling cursor, and hand it back for
    /// drawing.
    fn store_vbar(&mut self, column: Vec<u8>) -> Vec<u8> {
        self.vbars[self.vbar_cursor] = VBar { pixels: column.clone() };
        self.vbar_cursor = (self.vbar_cursor + 1) % VBAR_SIZE;
        column
    }
}

/// The residual layer: a run-length fill of the whole rectangle, one colour a run.
/// [MS-RDPEGFX] 2.2.4.1.1.
fn residual_data(section: &[u8], w: usize, h: usize, out: &mut [u8]) -> Result<(), Malformed> {
    let mut r = Reader::new(WHAT, section);
    let pixels = w * h;
    let mut at = 0;
    while !r.is_empty() {
        let color = [r.u8()?, r.u8()?, r.u8()?]; // B, G, R
        let first = r.u8()?;
        let run = run_length(&mut r, first)? as usize;
        if at >= pixels || run > pixels - at {
            return Err(refuse("a residual run past the rectangle", run as u64));
        }
        for _ in 0..run {
            put(out, at, color);
            at += 1;
        }
    }
    if at != pixels {
        return Err(refuse("a residual layer short of the rectangle, at", at as u64));
    }
    Ok(())
}

/// The subcodec layer: raw `BGR24`, NSCodec, or the RLEX run-length subcodec, each
/// over a sub-rectangle. [MS-RDPEGFX] 2.2.4.3.
fn subcodec_data(section: &[u8], w: usize, h: usize, out: &mut [u8], nsc: &mut Nsc) -> Result<(), Malformed> {
    let mut r = Reader::new(WHAT, section);
    let mut dst = Dst { out, w, h };
    while !r.is_empty() {
        let x0 = usize::from(r.u16_le()?);
        let y0 = usize::from(r.u16_le()?);
        let sw = usize::from(r.u16_le()?);
        let sh = usize::from(r.u16_le()?);
        let count = r.u32_le()? as usize;
        let id = r.u8()?;
        let data = r.bytes(count)?;
        if x0 + sw > w || y0 + sh > h {
            return Err(refuse("a subcodec rectangle past its tile", (x0 + sw).max(y0 + sh) as u64));
        }
        match id {
            0 => subcode_raw(data, sw, sh, x0, y0, &mut dst)?,
            1 => nsc.decode(data, sw, sh, |x, y, bgr| dst.put(x0 + x, y0 + y, bgr))?,
            2 => subcode_rlex(data, sw, sh, x0, y0, &mut dst)?,
            other => return Err(refuse("an unsupported ClearCodec subcodec", u64::from(other))),
        }
    }
    Ok(())
}

/// A raw `BGR24` sub-rectangle, one pixel three bytes.
fn subcode_raw(
    data: &[u8],
    sw: usize,
    sh: usize,
    x0: usize,
    y0: usize,
    dst: &mut Dst<'_>,
) -> Result<(), Malformed> {
    if data.len() != sw * sh * 3 {
        return Err(refuse("a raw subcodec of the wrong size", data.len() as u64));
    }
    for (n, bgr) in data.as_chunks::<3>().0.iter().enumerate() {
        dst.put(x0 + n % sw, y0 + n / sw, [bgr[0], bgr[1], bgr[2]]);
    }
    Ok(())
}

/// The RLEX subcodec: a palette of up to 127 colours, then runs and colour suites
/// indexing it. [MS-RDPEGFX] 2.2.4.3.1.
fn subcode_rlex(
    data: &[u8],
    sw: usize,
    sh: usize,
    x0: usize,
    y0: usize,
    dst: &mut Dst<'_>,
) -> Result<(), Malformed> {
    let mut r = Reader::new(WHAT, data);
    let palette_count = r.u8()?;
    if !(1..=127).contains(&palette_count) {
        return Err(refuse("an RLEX palette count", u64::from(palette_count)));
    }
    let mut palette = [[0u8; 3]; 128];
    for entry in palette.iter_mut().take(usize::from(palette_count)) {
        *entry = [r.u8()?, r.u8()?, r.u8()?]; // B, G, R
    }
    let num_bits = u32::from(palette_count - 1).checked_ilog2().unwrap_or(0) as u8 + 1;
    let pixels = sw * sh;
    let (mut x, mut y) = (0usize, 0usize);
    let mut at = 0usize;

    while !r.is_empty() {
        let tmp = r.u8()?;
        let first = r.u8()?;
        let run = run_length(&mut r, first)? as usize;
        let suite_depth = (tmp >> num_bits) & MASKS[usize::from(8 - num_bits)];
        let stop_index = tmp & MASKS[usize::from(num_bits)];
        let start_index = stop_index.wrapping_sub(suite_depth);
        if start_index >= palette_count || stop_index >= palette_count {
            return Err(refuse("an RLEX index past its palette", u64::from(start_index)));
        }

        let color = palette[usize::from(start_index)];
        if at + run > pixels {
            return Err(refuse("an RLEX run past the rectangle", run as u64));
        }
        for _ in 0..run {
            dst.put(x0 + x, y0 + y, color);
            step(&mut x, &mut y, sw);
        }
        at += run;

        let suite = usize::from(suite_depth) + 1;
        if at + suite > pixels {
            return Err(refuse("an RLEX suite past the rectangle", suite as u64));
        }
        for k in 0..suite {
            dst.put(x0 + x, y0 + y, palette[usize::from(start_index) + k]);
            step(&mut x, &mut y, sw);
        }
        at += suite;
    }
    if at != pixels {
        return Err(refuse("an RLEX layer short of the rectangle, at", at as u64));
    }
    Ok(())
}

/// A run length: one byte, then two, then four, each escape widening the last.
fn run_length(r: &mut Reader<'_>, first: u8) -> Result<u32, Malformed> {
    if first < 0xFF {
        return Ok(u32::from(first));
    }
    let second = r.u16_le()?;
    if second < 0xFFFF {
        return Ok(u32::from(second));
    }
    r.u32_le()
}

/// Build one full-height column: background above `yon`, the short bar's pixels from
/// `yon`, background below, each clamped so the column is exactly `height` tall.
fn build_column(height: usize, yon: usize, short_pixels: &[u8], bkg: [u8; 3]) -> Vec<u8> {
    let mut column = vec![0u8; height * 4];
    let short_count = short_pixels.len() / 4;
    for row in 0..yon.min(height) {
        put(&mut column, row, bkg);
    }
    let seg = if height > yon { short_count.min(height - yon) } else { 0 };
    for k in 0..seg {
        column[(yon + k) * 4..(yon + k) * 4 + 4].copy_from_slice(&short_pixels[k * 4..k * 4 + 4]);
    }
    for row in (yon + short_count)..height {
        put(&mut column, row, bkg);
    }
    column
}

/// Fit a cached column to a band's height: its own pixels, truncated or zero-padded.
fn fit_column(stored: &[u8], height: usize) -> Vec<u8> {
    let mut column = vec![0u8; height * 4];
    let n = stored.len().min(height * 4);
    column[..n].copy_from_slice(&stored[..n]);
    column
}

/// Draw column `i` of a band down onto the rectangle at `(x_start + i, y_start..)`,
/// clipped to the rectangle. The reference decodes straight onto the surface and lets
/// a band spill past the rectangle onto it; here the spill has nowhere to go and is
/// dropped, rather than costing the whole rectangle.
fn draw_column(dst: &mut Dst<'_>, x_start: usize, y_start: usize, i: usize, column: &[u8], height: usize) {
    let x = x_start + i;
    if x >= dst.w {
        return;
    }
    let count = height.min(dst.h.saturating_sub(y_start));
    for row in 0..count {
        let at = ((y_start + row) * dst.w + x) * 4;
        dst.out[at..at + 4].copy_from_slice(&column[row * 4..row * 4 + 4]);
    }
}

/// Write one `BGRX` pixel at a linear index of a packed buffer.
fn put(out: &mut [u8], index: usize, bgr: [u8; 3]) {
    let at = index * 4;
    out[at..at + 4].copy_from_slice(&[bgr[0], bgr[1], bgr[2], 0]);
}

/// The rectangle being decoded into: its packed `BGRX32` pixels and its size, so the
/// layers that draw sub-rectangles into it carry one argument, not three.
struct Dst<'a> {
    out: &'a mut [u8],
    w: usize,
    h: usize,
}

impl Dst<'_> {
    /// Write one `BGRX` pixel at `(x, y)`, dropping it if outside the rectangle.
    fn put(&mut self, x: usize, y: usize, bgr: [u8; 3]) {
        if x < self.w && y < self.h {
            put(self.out, y * self.w + x, bgr);
        }
    }
}

/// Advance a raster cursor one pixel, wrapping to the next row at width `w`.
fn step(x: &mut usize, y: &mut usize, w: usize) {
    *x += 1;
    if *x >= w {
        *y += 1;
        *x = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a ClearCodec rectangle: glyph header, then the composition sections.
    fn rect(flags: u8, seq: u8, glyph: Option<u16>, residual: &[u8], bands: &[u8], sub: &[u8]) -> Vec<u8> {
        let mut v = vec![flags, seq];
        if let Some(g) = glyph {
            v.extend_from_slice(&g.to_le_bytes());
        }
        v.extend_from_slice(&(residual.len() as u32).to_le_bytes());
        v.extend_from_slice(&(bands.len() as u32).to_le_bytes());
        v.extend_from_slice(&(sub.len() as u32).to_le_bytes());
        v.extend_from_slice(residual);
        v.extend_from_slice(bands);
        v.extend_from_slice(sub);
        v
    }

    /// The residual layer fills the whole rectangle by runs, top row first.
    #[test]
    fn a_residual_run_fills_the_rectangle() {
        let mut clear = Clear::new();
        let mut out = Vec::new();
        // One run of four pixels: B=30, G=20, R=10.
        let src = rect(0, 0, None, &[30, 20, 10, 4], &[], &[]);
        clear.decompress(&src, &mut out, 2, 2).unwrap();
        assert_eq!(out, [30, 20, 10, 0].repeat(4));
    }

    /// The RLEX subcodec paints a run and then a colour suite off its palette.
    #[test]
    fn an_rlex_suite_walks_its_palette() {
        let mut clear = Clear::new();
        let mut out = Vec::new();
        // Palette of two: colour 0 = B3 G2 R1, colour 1 = B6 G5 R4. numBits = 1.
        // tmp = 0x03: stopIndex 1, suiteDepth 1, startIndex 0; run 0, then a suite of
        // two — palette[0] then palette[1].
        let rlex = [2u8, 3, 2, 1, 6, 5, 4, 0x03, 0];
        let mut sub = Vec::new();
        sub.extend_from_slice(&0u16.to_le_bytes()); // xStart
        sub.extend_from_slice(&0u16.to_le_bytes()); // yStart
        sub.extend_from_slice(&2u16.to_le_bytes()); // width
        sub.extend_from_slice(&1u16.to_le_bytes()); // height
        sub.extend_from_slice(&(rlex.len() as u32).to_le_bytes());
        sub.push(2); // RLEX
        sub.extend_from_slice(&rlex);
        let src = rect(0, 0, None, &[], &[], &sub);
        clear.decompress(&src, &mut out, 2, 1).unwrap();
        assert_eq!(out, vec![3, 2, 1, 0, 6, 5, 4, 0]);
    }

    /// A rectangle stored under a glyph index and then hit paints from the cache.
    #[test]
    fn a_glyph_is_stored_and_hit_from_the_cache() {
        let mut clear = Clear::new();
        let mut out = Vec::new();
        let store = rect(FLAG_GLYPH_INDEX, 0, Some(5), &[30, 20, 10, 4], &[], &[]);
        clear.decompress(&store, &mut out, 2, 2).unwrap();
        assert_eq!(out, [30, 20, 10, 0].repeat(4));

        // A hit carries only the glyph header; the composition sections are absent.
        let mut hit = vec![FLAG_GLYPH_HIT | FLAG_GLYPH_INDEX, 1];
        hit.extend_from_slice(&5u16.to_le_bytes());
        let mut lit = Vec::new();
        clear.decompress(&hit, &mut lit, 2, 2).unwrap();
        assert_eq!(lit, [30, 20, 10, 0].repeat(4));
    }

    /// A banded short vertical bar, cache miss, paints its own pixels down a column.
    #[test]
    fn a_short_vbar_band_paints_a_column() {
        let mut clear = Clear::new();
        let mut out = Vec::new();
        let mut band = Vec::new();
        band.extend_from_slice(&0u16.to_le_bytes()); // xStart
        band.extend_from_slice(&0u16.to_le_bytes()); // xEnd
        band.extend_from_slice(&0u16.to_le_bytes()); // yStart
        band.extend_from_slice(&1u16.to_le_bytes()); // yEnd (height 2)
        band.extend_from_slice(&[0, 0, 0]); // background B, G, R
        band.extend_from_slice(&0x0200u16.to_le_bytes()); // yOn 0, yOff 2 -> short miss
        band.extend_from_slice(&[3, 2, 1]); // pixel 0: B3 G2 R1
        band.extend_from_slice(&[6, 5, 4]); // pixel 1: B6 G5 R4
        let src = rect(0, 0, None, &[], &band, &[]);
        clear.decompress(&src, &mut out, 1, 2).unwrap();
        assert_eq!(out, vec![3, 2, 1, 0, 6, 5, 4, 0]);
    }

    /// A band that spills past the rectangle paints the rows and columns that fit,
    /// as the reference lets it spill onto the surface, rather than refusing the
    /// whole rectangle.
    #[test]
    fn a_band_past_the_rectangle_is_clipped_to_it() {
        let mut clear = Clear::new();
        let mut out = Vec::new();
        let mut band = Vec::new();
        band.extend_from_slice(&0u16.to_le_bytes()); // xStart
        band.extend_from_slice(&1u16.to_le_bytes()); // xEnd: two columns, one past a 1-wide rectangle
        band.extend_from_slice(&0u16.to_le_bytes()); // yStart
        band.extend_from_slice(&3u16.to_le_bytes()); // yEnd: four rows, two past a 2-tall rectangle
        band.extend_from_slice(&[0, 0, 0]); // background
        for _ in 0..2 {
            band.extend_from_slice(&0x0400u16.to_le_bytes()); // yOn 0, yOff 4 -> short miss
            band.extend_from_slice(&[3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
        }
        let src = rect(0, 0, None, &[], &band, &[]);
        clear.decompress(&src, &mut out, 1, 2).unwrap();
        assert_eq!(out, vec![3, 2, 1, 0, 6, 5, 4, 0]);
    }

    /// The NSCodec subcodec paints its sub-rectangle where the layer puts it; one
    /// that cannot be read is refused, so the caller can leave the rectangle a hole
    /// rather than end the session.
    #[test]
    fn the_nscodec_subcodec_paints_and_a_broken_one_is_refused() {
        // A 1×1 NSCodec bitmap: raw planes Y = 100, Co = 20, Cg = -10, alpha.
        let mut nsc = Vec::new();
        for plane in [1u32, 1, 1, 1] {
            nsc.extend_from_slice(&plane.to_le_bytes());
        }
        nsc.extend_from_slice(&[1, 0, 0, 0, 100, 20, 0xF6, 0xFF]);
        let subrect = |data: &[u8]| {
            let mut sub = Vec::new();
            sub.extend_from_slice(&1u16.to_le_bytes()); // x
            sub.extend_from_slice(&0u16.to_le_bytes()); // y
            sub.extend_from_slice(&1u16.to_le_bytes());
            sub.extend_from_slice(&1u16.to_le_bytes());
            sub.extend_from_slice(&(data.len() as u32).to_le_bytes());
            sub.push(1); // NSCodec
            sub.extend_from_slice(data);
            sub
        };
        let mut clear = Clear::new();
        let mut out = Vec::new();
        let src = rect(0, 0, None, &[], &[], &subrect(&nsc));
        clear.decompress(&src, &mut out, 2, 1).unwrap();
        assert_eq!(out, [0, 0, 0, 0, 90, 90, 130, 0]);

        let src = rect(0, 1, None, &[], &[], &subrect(&[0]));
        let err = clear.decompress(&src, &mut out, 2, 1).unwrap_err();
        assert!(matches!(err, Malformed::Short { .. }), "{err}");
    }
}
