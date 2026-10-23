//! RemoteFX Progressive, the codec a Windows desktop draws its pictures in.
//!
//! Progressive (`RDPGFX_CODECID_CAPROGRESSIVE`, [MS-RDPEGFX] 2.2.4) is RemoteFX with a
//! second thought: a 64×64 tile is sent once as a coarse first pass and then, while
//! the pixels underneath hold still, refined by *upgrade* passes that each add a few
//! more bits to the coefficients already held. The decoder therefore keeps, for every
//! tile of every surface, the coefficients as last decoded and the sign of each, and
//! an upgrade edits those in place before running the inverse transform again.
//!
//! Only what a modern Windows host sends is here, measured against the sandbox this
//! client is written against: the reduce-extrapolate wavelet, RLGR1 entropy coding,
//! 64-pixel tiles, and the simple, first and upgrade tile kinds. The classic
//! RemoteFX wavelet and RLGR3 are what the original, non-progressive codec used, and
//! a region asking for them is refused by name rather than decoded wrong.
//!
//! The structure follows FreeRDP's `progressive.c`, `rfx_rlgr.c` and
//! `rfx_dwt.c`, with the arithmetic kept bit for bit where the reference is the only
//! specification there is: the extrapolated transform's edge cases, the SRL reader's
//! parameter walk, and the fixed-point colour conversion.

use std::cell::RefCell;
use std::collections::BTreeMap;

use rayon::prelude::*;

use super::gfx::Rect16;
use super::wire::{Malformed, Reader};

const WHAT: &str = "a Progressive graphics PDU";

/// Tile edge, in pixels. The only size the codec has ever had.
const TILE: usize = 64;
/// Coefficients per component of a tile.
const COEFFS: usize = TILE * TILE;
/// What one [`Tile`] holds: its pixels, then two coefficient arrays of `i16`.
const TILE_BYTES: usize = COEFFS * 4 + COEFFS * 3 * 2 * 2;
/// Bytes of pixels per tile row.
const TILE_STRIDE: usize = TILE * 4;

const WBT_SYNC: u16 = 0xCCC0;
const WBT_FRAME_BEGIN: u16 = 0xCCC1;
const WBT_FRAME_END: u16 = 0xCCC2;
const WBT_CONTEXT: u16 = 0xCCC3;
const WBT_REGION: u16 = 0xCCC4;
const WBT_TILE_SIMPLE: u16 = 0xCCC5;
const WBT_TILE_FIRST: u16 = 0xCCC6;
const WBT_TILE_UPGRADE: u16 = 0xCCC7;

/// Region flag: the wavelet is the reduce-extrapolate variant. The only one decoded.
const DWT_REDUCE_EXTRAPOLATE: u8 = 0x01;
/// Tile flag: the coefficients are a difference against the tile as last decoded.
const TILE_DIFFERENCE: u8 = 0x01;

/// Where each sub-band sits in a component's 4096 coefficients under the
/// reduce-extrapolate layout, as `(offset, count)`, in the order the quantization
/// values are kept: HL1, LH1, HH1, HL2, LH2, HH2, HL3, LH3, HH3, LL3.
const BANDS: [(usize, usize); 10] = [
    (0, 1023),    // HL1  31×33
    (1023, 1023), // LH1  33×31
    (2046, 961),  // HH1  31×31
    (3007, 272),  // HL2  16×17
    (3279, 272),  // LH2  17×16
    (3551, 256),  // HH2  16×16
    (3807, 72),   // HL3  8×9
    (3879, 72),   // LH3  9×8
    (3951, 64),   // HH3  8×8
    (4015, 81),   // LL3  9×9
];
const LL3: usize = 9;

fn refuse(field: &'static str, value: impl Into<u64>) -> Malformed {
    Malformed::Refused { what: WHAT, field, value: value.into() }
}

/// One quantization value per sub-band, in [`BANDS`] order.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Quant([u8; 10]);

impl Quant {
    /// The five packed bytes of an `RFX_COMPONENT_CODEC_QUANT`, [MS-RDPEGFX]
    /// 2.2.4.2.1.5.2, whose bands run in a different order from the
    /// `TS_RFX_CODEC_QUANT` of [MS-RDPRFX].
    fn read(r: &mut Reader<'_>) -> Result<Self, Malformed> {
        let b = [r.u8()?, r.u8()?, r.u8()?, r.u8()?, r.u8()?];
        Ok(Self([
            b[3] >> 4,   // HL1
            b[4] & 0xF,  // LH1
            b[4] >> 4,   // HH1
            b[2] & 0xF,  // HL2
            b[2] >> 4,   // LH2
            b[3] & 0xF,  // HH2
            b[0] >> 4,   // HL3
            b[1] & 0xF,  // LH3
            b[1] >> 4,   // HH3
            b[0] & 0xF,  // LL3
        ]))
    }

    fn add(self, other: Self) -> Self {
        let mut out = [0; 10];
        for (o, (a, b)) in out.iter_mut().zip(self.0.iter().zip(&other.0)) {
            *o = a + b;
        }
        Self(out)
    }

    /// `self - other` per band, or `None` if any band would go negative.
    fn sub(self, other: Self) -> Option<Self> {
        let mut out = [0; 10];
        for (o, (a, b)) in out.iter_mut().zip(self.0.iter().zip(&other.0)) {
            *o = a.checked_sub(*b)?;
        }
        Some(Self(out))
    }

    fn all(self, ok: impl Fn(u8) -> bool) -> bool {
        self.0.iter().all(|&v| ok(v))
    }
}

/// Progressive quantization: how much of each component's precision a pass holds
/// back. The `quality` byte on the wire is not needed to decode and is skipped.
#[derive(Clone, Copy, Default)]
struct ProgQuant {
    y: Quant,
    cb: Quant,
    cr: Quant,
}

impl ProgQuant {
    fn read(r: &mut Reader<'_>) -> Result<Self, Malformed> {
        r.u8()?; // quality
        Ok(Self { y: Quant::read(r)?, cb: Quant::read(r)?, cr: Quant::read(r)? })
    }
}

/// One tile of one surface, as it stands between passes.
struct Tile {
    /// The tile's pixels, 64 rows of 64, in the surface's order: `RGBX`, as the
    /// framebuffer holds them, so that painting a tile is a copy.
    pixels: Vec<u8>,
    /// Each component's coefficients as last decoded, before the inverse wavelet;
    /// what an upgrade pass refines. Y, then Cb, then Cr, 4096 each.
    current: Vec<i16>,
    /// What the SRL upgrade reader has learnt of each coefficient's sign, laid out
    /// like `current`.
    sign: Vec<i16>,
    /// Each component's bit position after the last pass: the precision the
    /// coefficients are held to, and so how many bits the next upgrade adds.
    bitpos: [Quant; 3],
    /// Whether a first or simple pass has landed; an upgrade needs one.
    started: bool,
    /// Decoded in the frame being drawn, and so in its grid's `updated`.
    dirty: bool,
}

impl Tile {
    fn new() -> Self {
        Self {
            pixels: vec![0; COEFFS * 4],
            current: vec![0; COEFFS * 3],
            sign: vec![0; COEFFS * 3],
            bitpos: [Quant::default(); 3],
            started: false,
            dirty: false,
        }
    }
}

/// A surface's tiles, made as the host first draws each.
struct Grid {
    width: u32,
    height: u32,
    cols: usize,
    tiles: Vec<Option<Box<Tile>>>,
    /// How many of `tiles` have been made.
    made: usize,
    /// How many this grid may have made, set before each PDU from the budget.
    limit: usize,
    /// The tiles decoded since the graphics frame began, by grid index. A region is
    /// painted from all of them, not only from its own: [MS-RDPEGFX] 2.2.4.2.1.5 lets
    /// a region's rectangles be covered by tiles an earlier region of the same frame
    /// carried — in an earlier PDU, even — and a region painted from its own tiles
    /// alone leaves those rectangles holding whatever was there before.
    updated: Vec<usize>,
}

impl Grid {
    fn new(width: u32, height: u32) -> Self {
        let cols = (width as usize).div_ceil(TILE);
        let rows = (height as usize).div_ceil(TILE);
        Self {
            width,
            height,
            cols,
            tiles: (0..cols * rows).map(|_| None).collect(),
            made: 0,
            limit: 0,
            updated: Vec::new(),
        }
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        let (x, y) = (usize::from(x), usize::from(y));
        (x < self.cols && y * self.cols + x < self.tiles.len()).then_some(y * self.cols + x)
    }
}

/// What a region says about the tiles that follow it.
struct Region {
    /// The rectangles of the surface this region repaints; tiles are clipped to them.
    rects: Vec<Rect16>,
    quants: Vec<Quant>,
    prog: Vec<ProgQuant>,
}

/// The fields common to every tile kind.
struct TileHeader {
    quant: [u8; 3],
    x: u16,
    y: u16,
}

thread_local! {
    /// The scratch space one tile needs, one to a thread that decodes: three
    /// components' coefficients being transformed, and the inverse wavelet's
    /// intermediate.
    static SCRATCH: RefCell<(Vec<i16>, Vec<i16>)> = RefCell::new((vec![0; COEFFS * 3], vec![0; COEFFS]));
}

/// A region of fewer tiles than this is decoded on the thread that read it.
const PARALLEL_FROM: usize = 4;

/// The decoder: every surface's tiles.
pub struct Progressive {
    surfaces: BTreeMap<u16, Grid>,
}

impl Default for Progressive {
    fn default() -> Self {
        Self::new()
    }
}

impl Progressive {
    pub fn new() -> Self {
        Self { surfaces: BTreeMap::new() }
    }

    /// A graphics frame begins: no tile has been decoded in it yet.
    pub fn start_frame(&mut self) {
        for grid in self.surfaces.values_mut() {
            for index in grid.updated.drain(..) {
                if let Some(tile) = grid.tiles[index].as_deref_mut() {
                    tile.dirty = false;
                }
            }
        }
    }

    /// Forget a surface's tiles: it was deleted, or created anew under the same
    /// number.
    pub fn forget(&mut self, surface: u16) {
        self.surfaces.remove(&surface);
    }

    /// The bytes every surface's tiles hold but `surface`'s.
    pub fn held_except(&self, surface: u16) -> usize {
        self.surfaces.iter().filter(|(id, _)| **id != surface).map(|(_, grid)| grid.made * TILE_BYTES).sum()
    }

    /// Decode one PDU's worth of blocks for a surface of `width`×`height`, and hand
    /// each repainted rectangle to `paint` as `(rect, rows, stride)`: `rows` starts
    /// at the rectangle's top-left pixel, `RGBX` as the surface holds it, and each
    /// row is `stride` bytes after the last. Every surface's tiles together stay
    /// within `budget` bytes; a tile past it is refused.
    pub fn decompress(
        &mut self,
        surface: u16,
        width: u32,
        height: u32,
        src: &[u8],
        budget: usize,
        mut paint: impl FnMut(Rect16, &[u8], usize),
    ) -> Result<(), Malformed> {
        let others = self.held_except(surface);
        let grid = self.surfaces.entry(surface).or_insert_with(|| Grid::new(width, height));
        if grid.width != width || grid.height != height {
            *grid = Grid::new(width, height);
        }
        grid.limit = budget.saturating_sub(others) / TILE_BYTES;
        let mut r = Reader::new(WHAT, src);
        while !r.is_empty() {
            let kind = r.u16_le()?;
            let len = r.u32_le()?;
            let Some(body) = usize::try_from(len).ok().and_then(|len| len.checked_sub(6)) else {
                return Err(refuse("a block length", len));
            };
            let mut b = Reader::new(WHAT, r.bytes(body)?);
            match kind {
                WBT_SYNC => {
                    // The magic and the version, both of which [MS-RDPEGFX] 2.2.4.2.1.1
                    // tells a decoder to ignore.
                    b.u32_le()?;
                    b.u16_le()?;
                }
                WBT_FRAME_BEGIN => {
                    b.u32_le()?; // frameIndex
                    b.u16_le()?; // regionCount, which a decoder is told to ignore
                }
                WBT_FRAME_END => {}
                WBT_CONTEXT => {
                    b.u8()?; // ctxId
                    let tile = b.u16_le()?;
                    if usize::from(tile) != TILE {
                        return Err(refuse("a context tile size", tile));
                    }
                    b.u8()?; // flags: subband diffing, which changes nothing here
                }
                WBT_REGION => {
                    let grid = self.surfaces.get_mut(&surface).expect("inserted above");
                    let mut updated = std::mem::take(&mut grid.updated);
                    let outcome = read_region(&mut b, grid, &mut updated);
                    if let Ok(region) = &outcome {
                        present(grid, region, &updated, &mut paint);
                    }
                    // A refused region's decoded tiles stay among the frame's, for a
                    // later region covering them to paint.
                    grid.updated = updated;
                    outcome?;
                }
                other => return Err(refuse("a block type", other)),
            }
            if !b.is_empty() {
                return Err(refuse("a block with bytes past its fields", kind));
            }
        }
        Ok(())
    }
}

/// Read a region's header, its rectangles and quantization tables, then decode each
/// tile in it, adding each tile decoded to the frame's `touched`.
fn read_region(
    r: &mut Reader<'_>,
    grid: &mut Grid,
    touched: &mut Vec<usize>,
) -> Result<Region, Malformed> {
    let tile = r.u8()?;
    if usize::from(tile) != TILE {
        return Err(refuse("a region tile size", tile));
    }
    let num_rects = r.u16_le()?;
    let num_quant = r.u8()?;
    let num_prog = r.u8()?;
    let flags = r.u8()?;
    let num_tiles = r.u16_le()?;
    let tile_bytes = r.u32_le()?;
    if num_rects == 0 {
        return Err(refuse("a region without rectangles", num_rects));
    }
    if num_quant > 7 {
        return Err(refuse("a region's quantization count", num_quant));
    }
    let mut rects = Vec::with_capacity(usize::from(num_rects));
    for _ in 0..num_rects {
        let (x, y, w, h) = (r.u16_le()?, r.u16_le()?, r.u16_le()?, r.u16_le()?);
        let right = x.checked_add(w).ok_or_else(|| refuse("a region rectangle past 65535", w))?;
        let bottom = y.checked_add(h).ok_or_else(|| refuse("a region rectangle past 65535", h))?;
        rects.push(Rect16 { left: x, top: y, right, bottom });
    }
    let mut quants = Vec::with_capacity(usize::from(num_quant));
    for _ in 0..num_quant {
        let quant = Quant::read(r)?;
        if !quant.all(|v| (6..=15).contains(&v)) {
            return Err(refuse("a quantization value outside 6..=15", 0u8));
        }
        quants.push(quant);
    }
    let mut prog = Vec::with_capacity(usize::from(num_prog));
    for _ in 0..num_prog {
        prog.push(ProgQuant::read(r)?);
    }
    if flags & DWT_REDUCE_EXTRAPOLATE == 0 {
        // The classic RemoteFX wavelet; not what a modern host sends.
        return Err(refuse("a region without the reduce-extrapolate flag", flags));
    }
    let region = Region { rects, quants, prog };

    let tiles = r.bytes(usize::try_from(tile_bytes).unwrap_or(usize::MAX))?;
    let mut passes = Vec::with_capacity(usize::from(num_tiles));
    let read = read_tiles(&mut Reader::new(WHAT, tiles), grid, &region, num_tiles, touched, &mut passes);
    // Whatever was read before a fault is decoded, fault or none.
    decode(grid, passes);
    read?;
    Ok(region)
}

/// One tile block as read: checked against the tile it names, which holds the
/// precision the pass leaves it at already, and not yet decoded.
struct Pass<'a> {
    index: usize,
    /// Each component's dequantization.
    shift: [Quant; 3],
    kind: PassKind<'a>,
}

enum PassKind<'a> {
    First { diff: bool, data: [&'a [u8]; 3] },
    /// `bits` is how many bits the pass adds to each band; `data` is each
    /// component's SRL stream, then its raw one.
    Upgrade { bits: [Quant; 3], data: [&'a [u8]; 6] },
}

/// Read a region's tile blocks into `passes`, in their order, adding each tile
/// named to the frame's `touched`.
fn read_tiles<'a>(
    t: &mut Reader<'a>,
    grid: &mut Grid,
    region: &Region,
    num_tiles: u16,
    touched: &mut Vec<usize>,
    passes: &mut Vec<Pass<'a>>,
) -> Result<(), Malformed> {
    let mut count = 0u16;
    while !t.is_empty() {
        let kind = t.u16_le()?;
        let len = t.u32_le()?;
        let Some(body) = usize::try_from(len).ok().and_then(|len| len.checked_sub(6)) else {
            return Err(refuse("a tile block length", len));
        };
        let mut b = Reader::new(WHAT, t.bytes(body)?);
        let pass = match kind {
            WBT_TILE_SIMPLE | WBT_TILE_FIRST => read_first(&mut b, kind == WBT_TILE_SIMPLE, grid, region)?,
            WBT_TILE_UPGRADE => read_upgrade(&mut b, grid, region)?,
            other => return Err(refuse("a tile block type", other)),
        };
        if let Some(tile) = grid.tiles[pass.index].as_deref_mut()
            && !tile.dirty
        {
            tile.dirty = true;
            touched.push(pass.index);
        }
        passes.push(pass);
        if !b.is_empty() {
            return Err(refuse("a tile block with bytes past its fields", kind));
        }
        count = count.wrapping_add(1);
    }
    if count != num_tiles {
        return Err(refuse("a region's tile count", num_tiles));
    }
    Ok(())
}

/// Decode what a region's tile blocks carried. A tile's passes run in the order
/// they were read in; tiles hold nothing of one another's, and are decoded side by
/// side.
fn decode(grid: &mut Grid, mut passes: Vec<Pass<'_>>) {
    passes.sort_by_key(|pass| pass.index);
    let mut tiles = grid.tiles.iter_mut().enumerate();
    let mut work = Vec::new();
    for passes in passes.chunk_by(|a, b| a.index == b.index) {
        let index = passes[0].index;
        let tile = tiles.by_ref().find(|(at, _)| *at == index).and_then(|(_, tile)| tile.as_deref_mut());
        work.push((tile.expect("a tile is made where its block is read"), passes));
    }
    if work.len() < PARALLEL_FROM {
        for (tile, passes) in work {
            decode_tile(tile, passes);
        }
    } else {
        crate::pool::install(|| work.into_par_iter().for_each(|(tile, passes)| decode_tile(tile, passes)));
    }
}

fn decode_tile(tile: &mut Tile, passes: &[Pass<'_>]) {
    SCRATCH.with_borrow_mut(|(work, temp)| {
        for pass in passes {
            for c in 0..3 {
                let out = &mut work[c * COEFFS..(c + 1) * COEFFS];
                let current = &mut tile.current[c * COEFFS..(c + 1) * COEFFS];
                let sign = &mut tile.sign[c * COEFFS..(c + 1) * COEFFS];
                match pass.kind {
                    PassKind::First { diff, data } => {
                        decode_component(data[c], pass.shift[c], out, current, sign, diff, temp);
                    }
                    PassKind::Upgrade { bits, data } => {
                        upgrade_component(data[c * 2], data[c * 2 + 1], pass.shift[c], bits[c], current, sign);
                        out.copy_from_slice(current);
                        idwt(out, temp);
                    }
                }
            }
            to_rgbx(work, &mut tile.pixels);
        }
    });
}

/// The fields every tile kind opens with, then the tile they name — made if this is
/// its first sighting.
fn tile_header<'g>(r: &mut Reader<'_>, grid: &'g mut Grid) -> Result<(TileHeader, usize, &'g mut Tile), Malformed> {
    let quant = [r.u8()?, r.u8()?, r.u8()?];
    let x = r.u16_le()?;
    let y = r.u16_le()?;
    let Some(index) = grid.index(x, y) else {
        return Err(refuse("a tile outside its surface", (u64::from(x) << 16) | u64::from(y)));
    };
    if grid.tiles[index].is_none() {
        if grid.made >= grid.limit {
            return Err(refuse("a tile past the memory this client will hold", grid.made as u64));
        }
        grid.made += 1;
    }
    let tile = grid.tiles[index].get_or_insert_with(|| Box::new(Tile::new()));
    Ok((TileHeader { quant, x, y }, index, tile))
}

/// The three components' quantization, and the progressive quantization the tile's
/// quality byte selects.
fn tile_quants(region: &Region, header: &TileHeader, quality: u8) -> Result<([Quant; 3], ProgQuant), Malformed> {
    let mut quants = [Quant::default(); 3];
    for (out, idx) in quants.iter_mut().zip(header.quant) {
        *out = *region.quants.get(usize::from(idx)).ok_or_else(|| refuse("a tile's quantization index", idx))?;
    }
    let prog = if quality == 0xFF {
        ProgQuant::default()
    } else {
        *region.prog.get(usize::from(quality)).ok_or_else(|| refuse("a tile's quality index", quality))?
    };
    Ok((quants, prog))
}

/// A `TILE_SIMPLE` or `TILE_FIRST`: the tile decoded from scratch.
fn read_first<'a>(r: &mut Reader<'a>, simple: bool, grid: &mut Grid, region: &Region) -> Result<Pass<'a>, Malformed> {
    let (header, index, tile) = tile_header(r, grid)?;
    let flags = r.u8()?;
    let quality = if simple { 0xFF } else { r.u8()? };
    let lens = [r.u16_le()?, r.u16_le()?, r.u16_le()?];
    let tail = r.u16_le()?;
    let data = [
        r.bytes(usize::from(lens[0]))?,
        r.bytes(usize::from(lens[1]))?,
        r.bytes(usize::from(lens[2]))?,
    ];
    r.bytes(usize::from(tail))?;

    let (quants, prog) = tile_quants(region, &header, quality)?;
    let progs = [prog.y, prog.cb, prog.cr];
    let mut bitpos = [Quant::default(); 3];
    let mut shift = [Quant::default(); 3];
    for c in 0..3 {
        bitpos[c] = quants[c].add(progs[c]);
        shift[c] = bitpos[c].sub(Quant([1; 10])).ok_or_else(|| refuse("a zero quantization", 0u8))?;
        if data[c].is_empty() {
            return Err(refuse("an empty RLGR stream", 0u8));
        }
    }
    tile.bitpos = bitpos;
    tile.started = true;
    Ok(Pass { index, shift, kind: PassKind::First { diff: flags & TILE_DIFFERENCE != 0, data } })
}

/// One component of a first pass: entropy decode, dequantize, difference against
/// the last pass if asked, and inverse transform.
fn decode_component(
    data: &[u8],
    shift: Quant,
    out: &mut [i16],
    current: &mut [i16],
    sign: &mut [i16],
    diff: bool,
    temp: &mut [i16],
) {
    rlgr1(data, out);
    sign.copy_from_slice(out);
    for (band, &(at, len)) in BANDS.iter().enumerate() {
        if band == LL3 {
            differential(&mut out[at..at + len]);
        }
        lshift(&mut out[at..at + len], shift.0[band]);
    }
    if diff {
        for (o, c) in out.iter_mut().zip(current.iter_mut()) {
            let v = o.saturating_add(*c);
            *o = v;
            *c = v;
        }
    } else {
        current.copy_from_slice(out);
    }
    idwt(out, temp);
}

/// A `TILE_UPGRADE`: more bits for the coefficients a first pass left.
fn read_upgrade<'a>(r: &mut Reader<'a>, grid: &mut Grid, region: &Region) -> Result<Pass<'a>, Malformed> {
    let (header, index, tile) = tile_header(r, grid)?;
    let quality = r.u8()?;
    let mut lens = [0u16; 6];
    for len in &mut lens {
        *len = r.u16_le()?;
    }
    let mut data = [&[][..]; 6];
    for (slot, len) in data.iter_mut().zip(lens) {
        *slot = r.bytes(usize::from(len))?;
    }
    if !tile.started {
        return Err(refuse("an upgrade of a tile with no first pass", (u64::from(header.x) << 16) | u64::from(header.y)));
    }
    let (quants, prog) = tile_quants(region, &header, quality)?;
    let progs = [prog.y, prog.cb, prog.cr];
    let mut bitpos = [Quant::default(); 3];
    let mut bits = [Quant::default(); 3];
    let mut shift = [Quant::default(); 3];
    for c in 0..3 {
        bitpos[c] = quants[c].add(progs[c]);
        bits[c] = tile.bitpos[c].sub(bitpos[c]).ok_or_else(|| refuse("an upgrade below the pass before it", quality))?;
        shift[c] = bitpos[c].sub(Quant([1; 10])).ok_or_else(|| refuse("a zero quantization", 0u8))?;
    }
    tile.bitpos = bitpos;
    Ok(Pass { index, shift, kind: PassKind::Upgrade { bits, data } })
}

/// One component of an upgrade: each sub-band's coefficients gain `bits` more bits,
/// read from the raw stream where the sign is already known and from the SRL stream
/// where it is not. [MS-RDPEGFX] 3.3.8.3.
fn upgrade_component(srl: &[u8], raw: &[u8], shift: Quant, bits: Quant, current: &mut [i16], sign: &mut [i16]) {
    let mut srl = Srl { bits: Bits::new(srl), kp: 8, mode: false, nz: 0 };
    let mut raw = Bits::new(raw);
    for (band, &(at, len)) in BANDS.iter().enumerate() {
        let (num_bits, shift) = (u32::from(bits.0[band]), u32::from(shift.0[band]));
        if num_bits == 0 {
            continue;
        }
        let cur = &mut current[at..at + len];
        if band == LL3 {
            for c in cur {
                let input = raw.take(num_bits) as i32;
                *c = (i32::from(*c) + (input << shift)) as i16;
            }
            continue;
        }
        let sign = &mut sign[at..at + len];
        let mut i = 0;
        while i < len {
            let input = if sign[i] == 0 {
                if srl.nz > 0 {
                    // Zeros still owed cover this coefficient and every one after
                    // it whose sign is unknown, as far as they go: nothing to add.
                    let run = sign[i..].iter().take(srl.nz as usize).take_while(|&&s| s == 0).count();
                    srl.nz -= run as u32;
                    i += run;
                    continue;
                }
                let v = srl.read(num_bits);
                sign[i] = v;
                i32::from(v)
            } else {
                let v = raw.take(num_bits) as i32;
                if sign[i] < 0 { -v } else { v }
            };
            cur[i] = (i32::from(cur[i]) + (input << shift)) as i16;
            i += 1;
        }
    }
}

/// The SRL reader: a run-length code for the coefficients whose sign is not yet
/// known, [MS-RDPEGFX] 3.3.8.3.2. Its parameter walk is the reference's, exactly.
struct Srl<'a> {
    bits: Bits<'a>,
    kp: u32,
    /// `true` between a run's `1` and its unary magnitude.
    mode: bool,
    /// Zeros still owed from the current run.
    nz: u32,
}

impl Srl<'_> {
    fn read(&mut self, num_bits: u32) -> i16 {
        if self.nz > 0 {
            self.nz -= 1;
            return 0;
        }
        let k = self.kp / 8;
        if !self.mode {
            if self.bits.take(1) == 0 {
                // A run of at least 1 << k zeros.
                self.nz = 1 << k;
                self.kp = (self.kp + 4).min(80);
                self.nz -= 1;
                return 0;
            }
            // A shorter run, its length in the next k bits, then a magnitude.
            self.mode = true;
            self.nz = if k > 0 { self.bits.take(k) } else { 0 };
            if self.nz > 0 {
                self.nz -= 1;
                return 0;
            }
        }
        self.mode = false;
        let negative = self.bits.take(1) == 1;
        self.kp = self.kp.saturating_sub(6);
        if num_bits == 1 {
            return if negative { -1 } else { 1 };
        }
        // The magnitude in unary: one more for each zero until a one, which is
        // consumed, or until `max`, which leaves the bit after alone.
        let mut mag: u32 = 1;
        let max = (1u32 << num_bits) - 1;
        while mag < max {
            let room = max - mag;
            let avail = (self.bits.remaining().min(32)) as u32;
            let zeros = self.bits.peek().leading_zeros().min(avail);
            if zeros < avail {
                // A one follows `zeros` zeros.
                let counted = zeros.min(room);
                self.bits.skip(counted as usize + usize::from(zeros < room));
                mag += counted;
                break;
            }
            let counted = zeros.min(room);
            self.bits.skip(counted as usize);
            mag += counted;
            if avail < 32 {
                // The stream has ended: every bit past it reads as zero.
                mag = max;
            }
        }
        let mag = mag.min(i16::MAX as u32) as i16;
        if negative { -mag } else { mag }
    }
}

/// A bit stream read most-significant bit first, zero past its end, the way
/// WinPR's `wBitStream` presents one. The bits ahead are held in a 64-bit window
/// that is topped up a byte at a time as they are consumed, so that a read is a
/// shift and a mask rather than a walk over the bytes it spans.
struct Bits<'a> {
    data: &'a [u8],
    /// The next byte to load into the window.
    next: usize,
    /// The bits not yet consumed, the next one at the top, zero below `have`.
    window: u64,
    /// How many of the window's bits are the stream's.
    have: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, next: 0, window: 0, have: 0 }
    }

    fn remaining(&self) -> usize {
        self.have as usize + (self.data.len() - self.next) * 8
    }

    /// Load bytes until the window holds more than 56 bits, or the whole rest of
    /// the stream.
    fn fill(&mut self) {
        while self.have <= 56 {
            let Some(&byte) = self.data.get(self.next) else { return };
            self.window |= u64::from(byte) << (56 - self.have);
            self.have += 8;
            self.next += 1;
        }
    }

    /// The next 32 bits, most significant first, zero past the end.
    fn peek(&mut self) -> u32 {
        if self.have < 32 {
            self.fill();
        }
        (self.window >> 32) as u32
    }

    /// Consume `n` bits, `n` in `0..=32`, or what remains if fewer.
    fn skip(&mut self, n: usize) {
        let n = n as u32;
        if n > self.have {
            // Which leaves the window short only when the stream is in it whole.
            self.fill();
        }
        let n = n.min(self.have);
        self.window <<= n;
        self.have -= n;
    }

    /// Consume `n` bits, `n` in `0..=32`, as a number, zero past the end.
    fn take(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.have < n {
            self.fill();
        }
        let v = (self.window >> (64 - n)) as u32;
        let n = n.min(self.have);
        self.window <<= n;
        self.have -= n;
        v
    }

    /// Count and consume the leading bits equal to `one`, as many as remain.
    fn run_of(&mut self, one: bool) -> u32 {
        let mut total = 0;
        loop {
            let acc = if one { !self.peek() } else { self.peek() };
            let count = (acc.leading_zeros() as usize).min(self.remaining());
            total += count as u32;
            self.skip(count);
            if count < 32 {
                return total;
            }
        }
    }
}

const KPMAX: u32 = 80;

/// RLGR1 entropy decoding of one component's 4096 coefficients, [MS-RDPRFX]
/// 3.1.8.1.7.3. Coefficients past the end of the stream are zero.
fn rlgr1(src: &[u8], out: &mut [i16]) {
    let mut bits = Bits::new(src);
    let (mut k, mut kp, mut kr, mut krp) = (1u32, 8u32, 1u32, 8u32);
    let mut n = 0;
    while bits.remaining() > 0 && n < out.len() {
        if k > 0 {
            // Run-length mode: a run of zeros, then one nonzero.
            let mut run = 0usize;
            let vk = bits.run_of(false);
            if bits.remaining() < 1 {
                break;
            }
            bits.skip(1);
            for _ in 0..vk {
                run += 1usize << k;
                kp = (kp + 4).min(KPMAX);
                k = kp >> 3;
            }
            if bits.remaining() < k as usize {
                break;
            }
            run += bits.take(k) as usize;
            if bits.remaining() < 1 {
                break;
            }
            let negative = bits.take(1) == 1;
            let vk = bits.run_of(true);
            if bits.remaining() < 1 {
                break;
            }
            bits.skip(1);
            if bits.remaining() < kr as usize {
                break;
            }
            let code = (bits.take(kr) | (vk << kr)) as u16;
            if vk == 0 {
                krp = krp.saturating_sub(2);
                kr = krp >> 3;
            } else if vk != 1 {
                krp = (krp + vk).min(KPMAX);
                kr = krp >> 3;
            }
            kp = kp.saturating_sub(6);
            k = kp >> 3;
            let mag = (i32::from(code) + 1) as i16;
            let mag = if negative { mag.wrapping_neg() } else { mag };
            let zeros = run.min(out.len() - n);
            out[n..n + zeros].fill(0);
            n += zeros;
            if n < out.len() {
                out[n] = mag;
                n += 1;
            }
        } else {
            // Golomb-Rice mode: one value.
            let vk = bits.run_of(true);
            if bits.remaining() < 1 {
                break;
            }
            bits.skip(1);
            if bits.remaining() < kr as usize {
                break;
            }
            let code = (bits.take(kr) | (vk << kr)) as u16;
            if vk == 0 {
                krp = krp.saturating_sub(2);
                kr = krp >> 3;
            } else if vk != 1 {
                krp = (krp + vk).min(KPMAX);
                kr = krp >> 3;
            }
            let mag = if code == 0 {
                kp = (kp + 3).min(KPMAX);
                k = kp >> 3;
                0
            } else {
                kp = kp.saturating_sub(3);
                k = kp >> 3;
                if code & 1 == 1 { -(((i32::from(code) + 1) >> 1) as i16) } else { (code >> 1) as i16 }
            };
            out[n] = mag;
            n += 1;
        }
    }
    out[n..].fill(0);
}

/// Undo differential coding: each value is a delta on the one before.
fn differential(v: &mut [i16]) {
    let Some((first, rest)) = v.split_first_mut() else { return };
    let mut acc = *first;
    for x in rest {
        acc = acc.wrapping_add(*x);
        *x = acc;
    }
}

/// Dequantize: shift each coefficient up by `shift` bits, as a 16-bit wrap.
fn lshift(v: &mut [i16], shift: u8) {
    if shift == 0 {
        return;
    }
    for x in v {
        *x = (i32::from(*x) << shift) as i16;
    }
}

fn clamp16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// `v / 2`, spelled as shifts: the quotient towards zero, as a division rounds,
/// which is the floor of the value one up for a negative one. A division has no
/// vector instruction in WebAssembly, so a compiler pulls each lane out to divide
/// it; shifts it keeps in the lanes.
fn half(v: i32) -> i32 {
    (v + ((v >> 31) & 1)) >> 1
}

/// The reduce-extrapolate inverse wavelet over one component, three levels, in
/// place. [MS-RDPEGFX] 3.3.8.1.4, as FreeRDP has it.
fn idwt(buf: &mut [i16], temp: &mut [i16]) {
    idwt_level::<8>(&mut buf[3807..], temp, 3);
    idwt_level::<16>(&mut buf[3007..], temp, 2);
    idwt_level::<32>(buf, temp, 1);
}

fn band_l(level: u32) -> usize {
    (64 >> level) + 1
}

fn band_h(level: u32) -> usize {
    if level == 1 { 31 } else { (64 + (1 << (level - 1))) >> level }
}

/// One level: HL, LH, HH and LL sub-bands at the front of `buf` become the LL of
/// the level above, written over them. `N` is the lanes of the horizontal pass
/// ([`idwt_row`]): the level's high band's length, rounded up to eight.
///
/// Every sample of a line depends on the bands alone, or on the even samples
/// beside it — never on the sample computed before it — so a line is computed as
/// whole runs rather than one sample after another: along a row for the horizontal
/// pass, and a row of every column at once for the vertical. This is where a
/// Progressive tile's time goes, and such runs are what a compiler turns into
/// vector instructions, here and in the page's WebAssembly alike.
fn idwt_level<const N: usize>(buf: &mut [i16], temp: &mut [i16], level: u32) {
    let (l, h) = (band_l(level), band_h(level));
    let step = l + h;
    let hl = 0;
    let lh = hl + h * l;
    let hh = lh + l * h;
    let ll = hh + h * h;
    let (low, high) = temp.split_at_mut(l * step);

    // Horizontal: rows of (LL, HL) become L; rows of (LH, HH) become H.
    let mut line = Line::new();
    for row in 0..l {
        idwt_row::<N>(&buf[ll + row * l..], l, &buf[hl + row * h..], h, &mut low[row * step..][..step], &mut line);
    }
    for row in 0..h {
        idwt_row::<N>(&buf[lh + row * l..], l, &buf[hh + row * h..], h, &mut high[row * step..][..step], &mut line);
    }
    // Vertical: columns of (L, H) become the level above.
    idwt_columns(&low[..l * step], &high[..h * step], &mut buf[..step * step], step);
}

/// `low - high`, the first even sample of a line.
fn even_first(low: &[i16], high: &[i16], out: &mut [i16]) {
    for ((out, &l), &h) in out.iter_mut().zip(low).zip(high) {
        *out = clamp16(i32::from(l) - i32::from(h));
    }
}

/// `low - (before + after) / 2`, an even sample between two high ones.
fn even_between(low: &[i16], before: &[i16], after: &[i16], out: &mut [i16]) {
    for (((out, &l), &h0), &h1) in out.iter_mut().zip(low).zip(before).zip(after) {
        *out = clamp16(i32::from(l) - half(i32::from(h0) + i32::from(h1)));
    }
}

/// `low - high / 2`, the even sample past the last high one of a band two longer.
fn even_past(low: &[i16], high: &[i16], out: &mut [i16]) {
    for ((out, &l), &h) in out.iter_mut().zip(low).zip(high) {
        *out = clamp16(i32::from(l) - half(i32::from(h)));
    }
}

/// `(before + after) / 2 + 2 * high`, the odd sample between two even ones.
fn odd_between(before: &[i16], after: &[i16], high: &[i16], out: &mut [i16]) {
    for (((out, &x0), &x2), &h) in out.iter_mut().zip(before).zip(after).zip(high) {
        *out = clamp16(half(i32::from(x0) + i32::from(x2)) + 2 * i32::from(h));
    }
}

/// `even + 2 * high`, the odd sample that ends a line with no even one after it.
fn odd_last(even: &[i16], high: &[i16], out: &mut [i16]) {
    for ((out, &x), &h) in out.iter_mut().zip(even).zip(high) {
        *out = clamp16(i32::from(x) + 2 * i32::from(h));
    }
}

/// `(even + low) / 2`, the sample that ends a line whose low band is two longer.
fn mean(even: &[i16], low: &[i16], out: &mut [i16]) {
    for ((out, &x), &l) in out.iter_mut().zip(even).zip(low) {
        *out = clamp16(half(i32::from(x) + i32::from(l)));
    }
}

/// The most samples a line holds, and then some: the width of the scratch a row
/// is computed in.
const LINE: usize = 48;

/// A row's scratch: the even samples, which are the line's every other one, and
/// the odd ones between them. One per level, not per row, so that no row zeroes
/// it.
struct Line {
    even: [i16; LINE],
    odd: [i16; LINE],
}

impl Line {
    fn new() -> Self {
        Self { even: [0; LINE], odd: [0; LINE] }
    }
}

/// One line along a row: the first `nl` samples of `low` and the first `nh` of
/// `high` interleave into `dst`, whose length is theirs together. A row's line is
/// short — 33 samples at the widest — and a loop as long as it is not one a
/// compiler vectorizes well at every level, so every sample between the line's
/// ends is computed over `N` lanes at once, `N` at least `nh` and a multiple of
/// eight, and the lanes past the line are thrown away. The ends themselves are one
/// sample each. `high` is read one sample past `N`, which is within the band's
/// coefficients, or the ones after them.
fn idwt_row<const N: usize>(low: &[i16], nl: usize, high: &[i16], nh: usize, dst: &mut [i16], line: &mut Line) {
    let Line { even, odd } = line;
    let evens = if nl > nh { nh + 1 } else { nh };
    even_lanes::<N>(lanes(&low[1..]), lanes(high), lanes(&high[1..]), lanes_mut(&mut even[1..]));
    even[0] = clamp16(i32::from(low[0]) - i32::from(high[0]));
    if nl == nh + 1 {
        even[nh] = clamp16(i32::from(low[nh]) - i32::from(high[nh - 1]));
    } else if nl > nh + 1 {
        even[nh] = clamp16(i32::from(low[nh]) - half(i32::from(high[nh - 1])));
    }
    odd_lanes::<N>(lanes(even), lanes(&even[1..]), lanes(high), lanes_mut(odd));
    let mut odds = evens - 1;
    if nl <= nh {
        odd[nh - 1] = clamp16(i32::from(even[nh - 1]) + 2 * i32::from(high[nh - 1]));
        odds = nh;
    } else if nl > nh + 1 {
        odd[nh] = clamp16(half(i32::from(even[nh]) + i32::from(low[nh + 1])));
        odds = nh + 1;
    }
    // Interleaved: eight pairs at a time, then the pairs left, then an even sample
    // with no odd one after it.
    let blocks = odds / 8;
    let pairs = even.as_chunks::<8>().0.iter().zip(odd.as_chunks::<8>().0);
    for (out, (even, odd)) in dst.as_chunks_mut::<16>().0.iter_mut().zip(pairs).take(blocks) {
        for k in 0..8 {
            out[2 * k] = even[k];
            out[2 * k + 1] = odd[k];
        }
    }
    for i in blocks * 8..odds {
        dst[2 * i] = even[i];
        dst[2 * i + 1] = odd[i];
    }
    if evens > odds {
        dst[2 * odds] = even[odds];
    }
}

/// The first `N` samples of a line, as the lanes.
fn lanes<const N: usize>(samples: &[i16]) -> &[i16; N] {
    samples.first_chunk().expect("a line holds its lanes")
}

fn lanes_mut<const N: usize>(samples: &mut [i16]) -> &mut [i16; N] {
    samples.first_chunk_mut().expect("a line holds its lanes")
}

/// [`even_between`] over the lanes: a constant width, whatever the line's.
fn even_lanes<const N: usize>(low: &[i16; N], before: &[i16; N], after: &[i16; N], out: &mut [i16; N]) {
    for i in 0..N {
        out[i] = clamp16(i32::from(low[i]) - half(i32::from(before[i]) + i32::from(after[i])));
    }
}

/// [`odd_between`] over the lanes.
fn odd_lanes<const N: usize>(before: &[i16; N], after: &[i16; N], high: &[i16; N], out: &mut [i16; N]) {
    for i in 0..N {
        out[i] = clamp16(half(i32::from(before[i]) + i32::from(after[i])) + 2 * i32::from(high[i]));
    }
}

/// The same line down every column at once: `low` and `high` are rows of `step`
/// samples, as many rows as each band is long, and `dst` takes a row for each of
/// them.
fn idwt_columns(low: &[i16], high: &[i16], dst: &mut [i16], step: usize) {
    let (nl, nh) = (low.len() / step, high.len() / step);
    let l = |row: usize| &low[row * step..][..step];
    let h = |row: usize| &high[row * step..][..step];
    // The even rows first, each where it belongs.
    let evens = if nl > nh { nh + 1 } else { nh };
    even_first(l(0), h(0), &mut dst[..step]);
    for row in 1..nh {
        even_between(l(row), h(row - 1), h(row), &mut dst[2 * row * step..][..step]);
    }
    if nl == nh + 1 {
        even_first(l(nh), h(nh - 1), &mut dst[2 * nh * step..][..step]);
    } else if nl > nh + 1 {
        even_past(l(nh), h(nh - 1), &mut dst[2 * nh * step..][..step]);
    }
    // Then the odd rows, each from the even rows either side of it.
    for row in 0..evens - 1 {
        let (before, rest) = dst[2 * row * step..].split_at_mut(step);
        let (odd, after) = rest.split_at_mut(step);
        odd_between(before, &after[..step], h(row), odd);
    }
    if nl <= nh {
        let (even, odd) = dst[2 * (nh - 1) * step..].split_at_mut(step);
        odd_last(even, h(nh - 1), &mut odd[..step]);
    } else if nl > nh + 1 {
        let (even, odd) = dst[2 * nh * step..].split_at_mut(step);
        mean(even, l(nh + 1), &mut odd[..step]);
    }
}

/// The three transformed components — 11.5 fixed-point YCbCr — to a tile of
/// pixels in the surface's order, `RGBX`, in the reference's fixed point.
///
/// The reference works in 64 bits, `(y + 4096) << 16` and each product added to it,
/// then down 21. The same numbers are reached here in 32, which is what lets the
/// loop be vector instructions: red's and blue's coefficients are multiples of
/// four and eight, which come out of the sum and the shift together, and green's
/// two products are taken apart at the sixteenth bit and carried by hand.
fn to_rgbx(work: &[i16], pixels: &mut [u8]) {
    let (y, rest) = work.split_at(COEFFS);
    let (cb, cr) = rest.split_at(COEFFS);
    for (px, ((&y, &cb), &cr)) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(y.iter().zip(cb).zip(cr)) {
        let y = i32::from(y) + 4096;
        let (cb, cr) = (i32::from(cb), i32::from(cr));
        // (cr * 91916 + (y << 16)) >> 21, with the four taken out.
        let r = (cr * 22979 + y * 16384) >> 19;
        // (cb * 115992 + (y << 16)) >> 21, with the eight taken out.
        let b = (cb * 14499 + y * 8192) >> 18;
        // ((y << 16) - cb * 22527 - cr * 46819) >> 21: the products' sum over
        // 65536 and what is left of it, so that y never meets the whole of it.
        let (p, q) = (cb * 22527, cr * 46819);
        let left = (p & 0xFFFF) + (q & 0xFFFF);
        let whole = (p >> 16) + (q >> 16) + (left >> 16);
        let g = (y - whole - i32::from(left & 0xFFFF != 0)) >> 5;
        // One word a pixel, rather than four bytes: a store the lanes make whole.
        *px = (clip(r) | (clip(g) << 8) | (clip(b) << 16)).to_le_bytes();
    }
}

fn clip(v: i32) -> u32 {
    v.clamp(0, 255) as u32
}

/// Write every tile decoded so far in the frame to the surface, clipped to the
/// region's rectangles and to the surface — see [`Grid::updated`].
fn present(grid: &Grid, region: &Region, updated: &[usize], paint: &mut impl FnMut(Rect16, &[u8], usize)) {
    for &index in updated {
        let Some(tile) = grid.tiles[index].as_deref() else { continue };
        let (tx, ty) = ((index % grid.cols) * TILE, (index / grid.cols) * TILE);
        let tile_rect = (tx, ty, (tx + TILE).min(grid.width as usize), (ty + TILE).min(grid.height as usize));
        for rect in &region.rects {
            let left = tile_rect.0.max(usize::from(rect.left));
            let top = tile_rect.1.max(usize::from(rect.top));
            let right = tile_rect.2.min(usize::from(rect.right));
            let bottom = tile_rect.3.min(usize::from(rect.bottom));
            if left >= right || top >= bottom {
                continue;
            }
            let at = (top - ty) * TILE_STRIDE + (left - tx) * 4;
            let clipped = Rect16 {
                left: left as u16,
                top: top as u16,
                right: right as u16,
                bottom: bottom as u16,
            };
            paint(clipped, &tile.pixels[at..], TILE_STRIDE);
        }
    }
}

/// Builders the tests here and the compositor's share: a Progressive PDU that draws
/// one flat tile.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Bits written most significant first, padded with zeros to a byte.
    struct BitWriter {
        bytes: Vec<u8>,
        used: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self { bytes: Vec::new(), used: 0 }
        }

        fn put(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                if self.used.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = ((value >> i) & 1) as u8;
                let last = self.bytes.len() - 1;
                self.bytes[last] |= bit << (7 - (self.used % 8));
                self.used += 1;
            }
        }

        fn finish(self) -> Vec<u8> {
            self.bytes
        }
    }

    /// Golomb-Rice: `code` as `code >> kr` ones, a zero, then `kr` low bits; then the
    /// decoder's `kr` update.
    fn golomb(w: &mut BitWriter, code: u32, kr: &mut u32, krp: &mut u32) {
        let vk = code >> *kr;
        for _ in 0..vk {
            w.put(1, 1);
        }
        w.put(0, 1);
        w.put(code & ((1 << *kr) - 1), *kr);
        if vk == 0 {
            *krp = krp.saturating_sub(2);
        } else if vk != 1 {
            *krp = (*krp + vk).min(KPMAX);
        }
        *kr = *krp >> 3;
    }

    /// RLGR1 encoding, the inverse of [`rlgr1`] step for step; trailing zeros are
    /// left to the decoder's zero fill.
    pub(crate) fn rlgr1_encode(coeffs: &[i16]) -> Vec<u8> {
        let end = coeffs.iter().rposition(|&c| c != 0).map_or(0, |p| p + 1);
        let coeffs = &coeffs[..end];
        let mut w = BitWriter::new();
        let (mut k, mut kp, mut kr, mut krp) = (1u32, 8u32, 1u32, 8u32);
        let mut i = 0;
        while i < coeffs.len() {
            if k > 0 {
                let mut run = 0u32;
                while coeffs[i] == 0 {
                    run += 1;
                    i += 1;
                }
                while run >= (1 << k) {
                    w.put(0, 1);
                    run -= 1 << k;
                    kp = (kp + 4).min(KPMAX);
                    k = kp >> 3;
                }
                w.put(1, 1);
                w.put(run, k);
                let v = i32::from(coeffs[i]);
                i += 1;
                w.put(u32::from(v < 0), 1);
                golomb(&mut w, v.unsigned_abs() - 1, &mut kr, &mut krp);
                kp = kp.saturating_sub(6);
                k = kp >> 3;
            } else {
                let v = i32::from(coeffs[i]);
                i += 1;
                let code = if v < 0 { 2 * v.unsigned_abs() - 1 } else { 2 * v as u32 };
                golomb(&mut w, code, &mut kr, &mut krp);
                kp = if code == 0 { (kp + 3).min(KPMAX) } else { kp.saturating_sub(3) };
                k = kp >> 3;
            }
        }
        let mut bytes = w.finish();
        if bytes.is_empty() {
            bytes.push(0);
        }
        bytes
    }

    fn block(kind: u16, body: &[u8]) -> Vec<u8> {
        let mut v = kind.to_le_bytes().to_vec();
        v.extend_from_slice(&(body.len() as u32 + 6).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    /// The coefficients of a tile component whose every pixel is `dc` above the
    /// middle: LL3's first value, differentially coded, with quantization 6.
    fn flat_component(dc: i16) -> Vec<u8> {
        let mut coeffs = vec![0i16; COEFFS];
        coeffs[BANDS[LL3].0] = dc;
        rlgr1_encode(&coeffs)
    }

    /// A tile block of `kind` (simple or first) at `(x, y)`, flat at `dc` over grey.
    pub(crate) fn flat_tile(kind: u16, x: u16, y: u16, dc: i16) -> Vec<u8> {
        let (y_data, c_data) = (flat_component(dc), flat_component(0));
        let mut b = vec![0u8, 0, 0];
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
        b.push(0); // flags
        if kind == WBT_TILE_FIRST {
            b.push(0); // quality: the region's first progressive quant
        }
        for len in [y_data.len(), c_data.len(), c_data.len(), 0] {
            b.extend_from_slice(&(len as u16).to_le_bytes());
        }
        b.extend_from_slice(&y_data);
        b.extend_from_slice(&c_data);
        b.extend_from_slice(&c_data);
        block(kind, &b)
    }

    /// An upgrade block at `(x, y)` with quality 0 and no data: nothing to add.
    pub(crate) fn empty_upgrade(x: u16, y: u16) -> Vec<u8> {
        let mut b = vec![0u8, 0, 0];
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
        b.push(0); // quality
        b.extend_from_slice(&[0; 12]);
        block(WBT_TILE_UPGRADE, &b)
    }

    /// A region over `rects` (x, y, w, h) with one quantization table of all 6, one
    /// progressive table of all 0, `flags`, and the given tile blocks.
    pub(crate) fn region(rects: &[(u16, u16, u16, u16)], flags: u8, tiles: &[Vec<u8>]) -> Vec<u8> {
        let mut b = vec![64u8];
        b.extend_from_slice(&(rects.len() as u16).to_le_bytes());
        b.push(1); // numQuant
        b.push(1); // numProgQuant
        b.push(flags);
        b.extend_from_slice(&(tiles.len() as u16).to_le_bytes());
        let data: Vec<u8> = tiles.concat();
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        for r in rects {
            for v in [r.0, r.1, r.2, r.3] {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        b.extend_from_slice(&[0x66; 5]);
        b.extend_from_slice(&[0; 16]);
        b.extend_from_slice(&data);
        block(WBT_REGION, &b)
    }

    /// A whole PDU: sync, frame begin, context, the regions, frame end.
    pub(crate) fn pdu(regions: &[Vec<u8>]) -> Vec<u8> {
        let mut v = block(WBT_SYNC, &[0xCA, 0xAC, 0xCC, 0xCA, 0x00, 0x01]);
        v.extend(block(WBT_FRAME_BEGIN, &[1, 0, 0, 0, regions.len() as u8, 0]));
        v.extend(block(WBT_CONTEXT, &[0, 64, 0, 1]));
        for r in regions {
            v.extend_from_slice(r);
        }
        v.extend(block(WBT_FRAME_END, &[]));
        v
    }

    /// The pixel a flat tile at `dc` decodes to.
    pub(crate) fn grey(dc: u8) -> [u8; 4] {
        [128 + dc, 128 + dc, 128 + dc, 0]
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    /// The wavelet's line as it was first written, one sample after another from
    /// FreeRDP's own: what the runs above must agree with, sample for sample.
    #[allow(clippy::too_many_arguments)]
    fn reference_line(low: &[i16], sl: usize, high: &[i16], sh: usize, dst: &mut [i16], sx: usize, nl: usize, nh: usize) {
        let mut li = 0;
        let mut hi = 0;
        let mut xi = 0;
        let mut h0 = i32::from(high[hi]);
        hi += sh;
        let mut l0 = i32::from(low[li]);
        li += sl;
        let mut x0 = clamp16(l0 - h0);
        let mut x2 = x0;
        for _ in 0..nh - 1 {
            let h1 = i32::from(high[hi]);
            hi += sh;
            l0 = i32::from(low[li]);
            li += sl;
            x2 = clamp16(l0 - ((h0 + h1) / 2));
            let x1 = clamp16((i32::from(x0) + i32::from(x2)) / 2 + 2 * h0);
            dst[xi] = x0;
            dst[xi + sx] = x1;
            xi += 2 * sx;
            x0 = x2;
            h0 = h1;
        }
        if nl <= nh + 1 {
            if nl <= nh {
                dst[xi] = x2;
                dst[xi + sx] = clamp16(i32::from(x2) + 2 * h0);
            } else {
                l0 = i32::from(low[li]);
                x0 = clamp16(l0 - h0);
                dst[xi] = x2;
                dst[xi + sx] = clamp16((i32::from(x0) + i32::from(x2)) / 2 + 2 * h0);
                dst[xi + 2 * sx] = x0;
            }
        } else {
            l0 = i32::from(low[li]);
            li += sl;
            x0 = clamp16(l0 - (h0 / 2));
            dst[xi] = x2;
            dst[xi + sx] = clamp16((i32::from(x0) + i32::from(x2)) / 2 + 2 * h0);
            dst[xi + 2 * sx] = x0;
            l0 = i32::from(low[li]);
            dst[xi + 3 * sx] = clamp16((i32::from(x0) + l0) / 2);
        }
    }

    /// The colour conversion in the reference's own 64 bits.
    fn reference_rgbx(y: i16, cb: i16, cr: i16) -> [u8; 4] {
        let clip = |v: i64| v.clamp(0, 255) as u8;
        let y = (i64::from(y) + 4096) << 16;
        let (cb, cr) = (i64::from(cb), i64::from(cr));
        let r = ((cr * 91916 + y) >> 16) >> 5;
        let g = ((y - cb * 22527 - cr * 46819) >> 16) >> 5;
        let b = ((cb * 115992 + y) >> 16) >> 5;
        [clip(r), clip(g), clip(b), 0]
    }

    /// The conversion in 32 bits is the reference's in 64, for every sample sixteen
    /// bits hold: the ordinary ones, the ones that clip, and the ones whose products
    /// would not fit had they been summed whole.
    #[test]
    fn the_colour_conversion_is_the_reference_for_every_sample() {
        let mut edges: Vec<i16> = vec![i16::MIN, i16::MIN + 1, -4097, -4096, -4095, -1, 0, 1, 4095, 4096, i16::MAX - 1, i16::MAX];
        edges.extend(samples(40, 5));
        let mut work = vec![0i16; COEFFS * 3];
        let mut pixels = vec![0u8; COEFFS * 4];
        let mut checked = 0;
        for &y in &edges {
            // A tile's worth of chroma pairs for each luma: the edges against each
            // other, then samples.
            let mut pairs: Vec<(i16, i16)> = edges.iter().flat_map(|&cb| edges.iter().map(move |&cr| (cb, cr))).collect();
            pairs.truncate(COEFFS);
            let fill = samples(2 * (COEFFS - pairs.len()), u32::from(y as u16));
            pairs.extend(fill.as_chunks::<2>().0.iter().map(|pair| (pair[0], pair[1])));
            for (i, (cb, cr)) in pairs.iter().enumerate() {
                work[i] = y;
                work[COEFFS + i] = *cb;
                work[2 * COEFFS + i] = *cr;
            }
            to_rgbx(&work, &mut pixels);
            for (px, (cb, cr)) in pixels.as_chunks::<4>().0.iter().zip(&pairs) {
                assert_eq!(*px, reference_rgbx(y, *cb, *cr), "y {y}, cb {cb}, cr {cr}");
                checked += 1;
            }
        }
        assert_eq!(checked, edges.len() * COEFFS);
    }

    /// Samples that reach every branch of the clamp: small ones, and ones at both
    /// ends of what sixteen bits hold.
    fn samples(count: usize, seed: u32) -> Vec<i16> {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        (0..count)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let v = (state >> 16) as i16;
                match state & 3 {
                    0 => v,
                    1 => v / 64,
                    2 => i16::MAX - (v & 0x3F),
                    _ => i16::MIN + (v & 0x3F),
                }
            })
            .collect()
    }

    /// A line along a row is the reference's line, for each shape a band has: a low
    /// band two longer than the high one, one longer, and no longer.
    #[test]
    fn a_row_of_the_wavelet_is_the_reference_sample_for_sample() {
        let mut line = Line::new();
        let mut row = |lanes: usize, low: &[i16], nl: usize, high: &[i16], nh: usize, dst: &mut [i16]| match lanes {
            32 => idwt_row::<32>(low, nl, high, nh, dst, &mut line),
            16 => idwt_row::<16>(low, nl, high, nh, dst, &mut line),
            _ => idwt_row::<8>(low, nl, high, nh, dst, &mut line),
        };
        for (seed, (nl, nh, lanes)) in [(33, 31, 32), (17, 16, 16), (9, 8, 8), (8, 8, 8), (2, 1, 8), (3, 1, 8)].into_iter().enumerate() {
            for round in 0..50 {
                // Each band with what lies past it, which the lanes read and must not show.
                let low = samples(nl + lanes, (seed * 100 + round) as u32);
                let high = samples(nh + lanes, (seed * 100 + round + 7_000) as u32);
                let len = if nl <= nh { 2 * nh } else { nl + nh };
                let mut expected = vec![0i16; len];
                reference_line(&low[..nl], 1, &high[..nh], 1, &mut expected, 1, nl, nh);
                let mut got = vec![0i16; len];
                row(lanes, &low, nl, &high, nh, &mut got);
                assert_eq!(got, expected, "a {nl}+{nh} line, round {round}");
            }
        }
    }

    /// And down the columns: every column of the rows at once is each column on
    /// its own.
    #[test]
    fn the_columns_of_the_wavelet_are_the_reference_sample_for_sample() {
        for (seed, (nl, nh)) in [(33usize, 31usize), (17, 16), (9, 8)].into_iter().enumerate() {
            let step = nl + nh;
            for round in 0..20 {
                let low = samples(nl * step, (seed * 1_000 + round) as u32);
                let high = samples(nh * step, (seed * 1_000 + round + 90_000) as u32);
                let mut expected = vec![0i16; step * step];
                for col in 0..step {
                    reference_line(&low[col..], step, &high[col..], step, &mut expected[col..], step, nl, nh);
                }
                let mut got = vec![0i16; step * step];
                idwt_columns(&low, &high, &mut got, step);
                assert_eq!(got, expected, "a {nl}+{nh} level, round {round}");
            }
        }
    }

    /// Every paint call, as (rect, the pixels inside it, packed).
    /// Decode `src` as a graphics frame of its own, and gather what it paints.
    fn collect(p: &mut Progressive, surface: u16, w: u32, h: u32, src: &[u8]) -> Result<Vec<(Rect16, Vec<u8>)>, Malformed> {
        p.start_frame();
        in_frame(p, surface, w, h, src)
    }

    /// Decode `src` into the graphics frame already under way, and gather what it paints.
    fn in_frame(p: &mut Progressive, surface: u16, w: u32, h: u32, src: &[u8]) -> Result<Vec<(Rect16, Vec<u8>)>, Malformed> {
        let mut paints = Vec::new();
        p.decompress(surface, w, h, src, usize::MAX, |rect, rows, stride| {
            let mut packed = Vec::new();
            for row in 0..usize::from(rect.height()) {
                packed.extend_from_slice(&rows[row * stride..row * stride + usize::from(rect.width()) * 4]);
            }
            paints.push((rect, packed));
        })?;
        Ok(paints)
    }

    /// Tiles are made only within the budget, a tile already made is drawn again
    /// without spending more of it, and a forgotten surface gives its share back.
    /// The sync block's magic and version are read past, as a decoder is told to: a
    /// host that bumped either is decoded rather than dropped.
    #[test]
    fn a_sync_block_of_another_version_is_read_past() {
        let mut src = pdu(&[region(&[(0, 0, 64, 64)], 1, &[flat_tile(WBT_TILE_SIMPLE, 0, 0, 5)])]);
        // The sync block's six bytes of body follow its six-byte header.
        src[6..12].copy_from_slice(&[0, 0, 0, 0, 0x00, 0x02]);
        let mut painted = 0;
        Progressive::new().decompress(1, 64, 64, &src, TILE_BYTES, |_, _, _| painted += 1).unwrap();
        assert!(painted > 0);
    }

    #[test]
    fn tiles_past_the_budget_are_refused() {
        let mut p = Progressive::new();
        let one = pdu(&[region(&[(0, 0, 128, 64)], 1, &[flat_tile(WBT_TILE_SIMPLE, 0, 0, 9)])]);
        let two = pdu(&[region(&[(0, 0, 128, 64)], 1, &[flat_tile(WBT_TILE_SIMPLE, 1, 0, 9)])]);
        p.decompress(1, 128, 64, &one, TILE_BYTES, |_, _, _| {}).unwrap();
        p.decompress(1, 128, 64, &one, TILE_BYTES, |_, _, _| {}).unwrap();
        assert!(p.decompress(1, 128, 64, &two, TILE_BYTES, |_, _, _| {}).is_err());
        assert!(p.decompress(2, 128, 64, &one, TILE_BYTES, |_, _, _| {}).is_err());
        assert_eq!(p.held_except(2), TILE_BYTES);
        p.forget(1);
        p.decompress(2, 128, 64, &one, TILE_BYTES, |_, _, _| {}).unwrap();
    }

    /// RLGR1 comes back as it went in: long zero runs, small and large magnitudes of
    /// both signs, and a dense stretch that drives the coder into Golomb-Rice mode.
    #[test]
    fn rlgr1_round_trips_runs_and_dense_stretches() {
        let mut coeffs = vec![0i16; COEFFS];
        coeffs[0] = 7;
        coeffs[300] = -1;
        coeffs[301] = 3;
        coeffs[302] = -2;
        coeffs[303] = 5;
        coeffs[304] = -700;
        coeffs[305] = 1;
        coeffs[306] = 0;
        coeffs[307] = 0;
        coeffs[308] = 2;
        coeffs[2000] = 32767;
        coeffs[2001] = -32768;
        coeffs[4015] = 12;
        coeffs[4095] = -3;
        let encoded = rlgr1_encode(&coeffs);
        let mut decoded = vec![1i16; COEFFS];
        rlgr1(&encoded, &mut decoded);
        assert_eq!(decoded, coeffs);
    }

    /// A component that encodes to nothing but its zero fill decodes to zeros.
    #[test]
    fn rlgr1_zero_fills_past_the_stream() {
        let mut decoded = vec![1i16; 16];
        rlgr1(&[0], &mut decoded);
        assert_eq!(decoded, vec![0; 16]);
    }

    /// A tile whose only coefficient is LL3's first comes out one flat grey: the
    /// differential decode, dequantization, every level of the inverse wavelet and the
    /// colour conversion agree on a constant.
    #[test]
    fn a_flat_tile_decodes_to_one_grey() {
        let mut p = Progressive::new();
        let src = pdu(&[region(&[(0, 0, 64, 64)], 1, &[flat_tile(WBT_TILE_SIMPLE, 0, 0, 5)])]);
        let paints = collect(&mut p, 1, 64, 64, &src).unwrap();
        assert_eq!(paints.len(), 1);
        assert_eq!(paints[0].0, Rect16 { left: 0, top: 0, right: 64, bottom: 64 });
        assert_eq!(paints[0].1, grey(5).repeat(COEFFS));
    }

    /// A tile past the surface's edge is clipped to the surface, and to the region's
    /// rectangle; the rows handed over start at the clipped corner.
    #[test]
    fn a_tile_is_clipped_to_the_surface_and_the_region() {
        let mut p = Progressive::new();
        let tiles = [flat_tile(WBT_TILE_FIRST, 1, 1, 9), flat_tile(WBT_TILE_FIRST, 0, 0, 1)];
        let src = pdu(&[region(&[(60, 66, 100, 100)], 1, &tiles)]);
        let paints = collect(&mut p, 1, 100, 70, &src).unwrap();
        assert_eq!(paints.len(), 1, "the (0,0) tile lies outside the region's rectangle");
        assert_eq!(paints[0].0, Rect16 { left: 64, top: 66, right: 100, bottom: 70 });
        assert_eq!(paints[0].1, grey(9).repeat(36 * 4));
    }

    /// An upgrade that adds no bits repaints the tile unchanged; one for a tile that
    /// was never sent is refused.
    #[test]
    fn an_upgrade_needs_a_first_pass_and_may_add_nothing() {
        let mut p = Progressive::new();
        let src = pdu(&[region(&[(0, 0, 64, 64)], 1, &[empty_upgrade(0, 0)])]);
        let err = collect(&mut p, 1, 64, 64, &src).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "an upgrade of a tile with no first pass", .. }), "{err}");

        let first = pdu(&[region(&[(0, 0, 64, 64)], 1, &[flat_tile(WBT_TILE_FIRST, 0, 0, 5)])]);
        collect(&mut p, 1, 64, 64, &first).unwrap();
        let paints = collect(&mut p, 1, 64, 64, &src).unwrap();
        assert_eq!(paints.len(), 1);
        assert_eq!(paints[0].1, grey(5).repeat(COEFFS));
    }

    /// The classic wavelet is refused by name; the frame is not decoded wrong.
    #[test]
    fn a_region_without_reduce_extrapolate_is_refused() {
        let mut p = Progressive::new();
        let src = pdu(&[region(&[(0, 0, 64, 64)], 0, &[flat_tile(WBT_TILE_SIMPLE, 0, 0, 5)])]);
        let err = collect(&mut p, 1, 64, 64, &src).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "a region without the reduce-extrapolate flag", .. }), "{err}");
    }

    /// A region refused after some of its tiles decoded leaves those tiles free to be
    /// drawn again: the next region that touches them paints them.
    #[test]
    fn tiles_of_a_refused_region_are_painted_by_the_next() {
        let mut p = Progressive::new();
        let tiles = [flat_tile(WBT_TILE_FIRST, 0, 0, 5), vec![0xC9, 0xCC, 6, 0, 0, 0]]; // then a block of no known kind
        let src = pdu(&[region(&[(0, 0, 64, 64)], 1, &tiles)]);
        let err = collect(&mut p, 1, 64, 64, &src).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "a tile block type", .. }), "{err}");

        let again = pdu(&[region(&[(0, 0, 64, 64)], 1, &[flat_tile(WBT_TILE_SIMPLE, 0, 0, 7)])]);
        let paints = collect(&mut p, 1, 64, 64, &again).unwrap();
        assert_eq!(paints.len(), 1);
        assert_eq!(paints[0].1, grey(7).repeat(COEFFS));
    }

    /// A region whose rectangles are covered by tiles an earlier region of the frame
    /// carried paints those tiles — whether the earlier region came in the same PDU
    /// or an earlier one — and a region in the next frame no longer does.
    #[test]
    fn a_region_paints_the_tiles_its_frame_already_carried() {
        let mut p = Progressive::new();
        let tiles = [flat_tile(WBT_TILE_FIRST, 0, 0, 5), flat_tile(WBT_TILE_FIRST, 1, 0, 9)];
        let both = pdu(&[region(&[(0, 0, 64, 64)], 1, &tiles), region(&[(64, 0, 64, 64)], 1, &[])]);
        let paints = collect(&mut p, 1, 128, 64, &both).unwrap();
        assert_eq!(paints.len(), 2, "each region paints the tile under its rectangle");
        assert_eq!(paints[1].0, Rect16 { left: 64, top: 0, right: 128, bottom: 64 });
        assert_eq!(paints[1].1, grey(9).repeat(COEFFS));

        let first = pdu(&[region(&[(0, 0, 64, 64)], 1, &tiles)]);
        let second = pdu(&[region(&[(64, 0, 64, 64)], 1, &[])]);
        collect(&mut p, 1, 128, 64, &first).unwrap();
        let paints = in_frame(&mut p, 1, 128, 64, &second).unwrap();
        assert_eq!(paints.len(), 1, "a later PDU of the same frame still has the tile");
        assert_eq!(paints[0].1, grey(9).repeat(COEFFS));

        let paints = collect(&mut p, 1, 128, 64, &second).unwrap();
        assert!(paints.is_empty(), "a new frame carried no tile yet");
    }

    /// A tile difference adds to the coefficients held from the pass before.
    #[test]
    fn a_difference_tile_adds_to_the_last_pass() {
        let mut p = Progressive::new();
        let first = pdu(&[region(&[(0, 0, 64, 64)], 1, &[flat_tile(WBT_TILE_FIRST, 0, 0, 5)])]);
        collect(&mut p, 1, 64, 64, &first).unwrap();
        let mut diff = flat_tile(WBT_TILE_FIRST, 0, 0, 3);
        diff[6 + 7] |= TILE_DIFFERENCE; // flags, after the 6-byte block header and 7 header bytes
        let second = pdu(&[region(&[(0, 0, 64, 64)], 1, &[diff])]);
        let paints = collect(&mut p, 1, 64, 64, &second).unwrap();
        assert_eq!(paints[0].1, grey(8).repeat(COEFFS));
    }

    /// A region of enough tiles to be decoded side by side: each is what it would
    /// be alone, whichever thread decoded it.
    #[test]
    fn tiles_decoded_side_by_side_are_each_their_own() {
        let mut p = Progressive::new();
        let tiles: Vec<Vec<u8>> = (0..8u8).map(|i| flat_tile(WBT_TILE_FIRST, u16::from(i % 4), u16::from(i / 4), i16::from(i) + 1)).collect();
        assert!(tiles.len() >= PARALLEL_FROM);
        let src = pdu(&[region(&[(0, 0, 256, 128)], 1, &tiles)]);
        let paints = collect(&mut p, 1, 256, 128, &src).unwrap();
        assert_eq!(paints.len(), 8);
        for (rect, pixels) in paints {
            let tile = (rect.top / 64) * 4 + rect.left / 64;
            assert_eq!(pixels, grey(tile as u8 + 1).repeat(COEFFS), "the tile at {rect:?}");
        }
    }

    /// A tile a region carries twice is decoded in the order it was sent in, among
    /// the tiles decoded beside it.
    #[test]
    fn a_tile_sent_twice_in_a_region_is_the_later_of_the_two() {
        let mut p = Progressive::new();
        let mut tiles: Vec<Vec<u8>> = (0..4).map(|x| flat_tile(WBT_TILE_FIRST, x, 0, 3)).collect();
        tiles.push(flat_tile(WBT_TILE_FIRST, 2, 0, 40));
        let src = pdu(&[region(&[(0, 0, 256, 64)], 1, &tiles)]);
        let paints = collect(&mut p, 1, 256, 64, &src).unwrap();
        assert_eq!(paints.len(), 4, "a tile is painted once however often it was sent");
        for (rect, pixels) in paints {
            let dc = if rect.left == 128 { 40 } else { 3 };
            assert_eq!(pixels, grey(dc).repeat(COEFFS), "the tile at {rect:?}");
        }
    }

    /// A component with no stream is refused where its tile is read, and the tiles
    /// read before it are decoded for the region that paints them.
    #[test]
    fn a_component_with_no_stream_is_refused_and_the_tiles_before_it_are_kept() {
        let mut p = Progressive::new();
        // Flat at the middle, the three components' streams are the same bytes: the
        // first is given no length, and the tile's tail is given its bytes.
        let mut empty = flat_tile(WBT_TILE_FIRST, 1, 0, 0);
        let lens = 6 + 9; // after the block header, the tile's header, flags and quality
        let len = [empty[lens], empty[lens + 1]];
        empty[lens..lens + 2].copy_from_slice(&[0, 0]);
        empty[lens + 6..lens + 8].copy_from_slice(&len);
        let src = pdu(&[region(&[(0, 0, 128, 64)], 1, &[flat_tile(WBT_TILE_FIRST, 0, 0, 9), empty])]);
        let err = collect(&mut p, 1, 128, 64, &src).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "an empty RLGR stream", .. }), "{err}");

        let next = pdu(&[region(&[(0, 0, 128, 64)], 1, &[])]);
        let paints = in_frame(&mut p, 1, 128, 64, &next).unwrap();
        assert_eq!(paints.len(), 1);
        assert_eq!(paints[0].1, grey(9).repeat(COEFFS));
    }
}
