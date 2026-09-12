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

use std::collections::BTreeMap;

use super::gfx::Rect16;
use super::wire::{Malformed, Reader};

const WHAT: &str = "a Progressive graphics PDU";

/// Tile edge, in pixels. The only size the codec has ever had.
const TILE: usize = 64;
/// Coefficients per component of a tile.
const COEFFS: usize = TILE * TILE;
/// Bytes of `BGRX32` per tile row.
const TILE_STRIDE: usize = TILE * 4;

const WBT_SYNC: u16 = 0xCCC0;
const WBT_FRAME_BEGIN: u16 = 0xCCC1;
const WBT_FRAME_END: u16 = 0xCCC2;
const WBT_CONTEXT: u16 = 0xCCC3;
const WBT_REGION: u16 = 0xCCC4;
const WBT_TILE_SIMPLE: u16 = 0xCCC5;
const WBT_TILE_FIRST: u16 = 0xCCC6;
const WBT_TILE_UPGRADE: u16 = 0xCCC7;

const SYNC_MAGIC: u32 = 0xCACC_ACCA;
const SYNC_VERSION: u16 = 0x0100;

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
    /// The five packed bytes of a `TS_RFX_CODEC_QUANT`, [MS-RDPRFX] 2.2.2.1.5.
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
    /// The tile's pixels, `BGRX32`, 64 rows of 64.
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
    /// Decoded since it was last written to the surface.
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
}

impl Grid {
    fn new(width: u32, height: u32) -> Self {
        let cols = (width as usize).div_ceil(TILE);
        let rows = (height as usize).div_ceil(TILE);
        Self { width, height, cols, tiles: (0..cols * rows).map(|_| None).collect() }
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

/// The decoder: every surface's tiles, and the scratch space one tile needs.
pub struct Progressive {
    surfaces: BTreeMap<u16, Grid>,
    /// Three components' coefficients, being transformed.
    work: Vec<i16>,
    /// The inverse wavelet's intermediate.
    temp: Vec<i16>,
    /// Tiles decoded by the region being processed, by grid index.
    touched: Vec<usize>,
}

impl Default for Progressive {
    fn default() -> Self {
        Self::new()
    }
}

impl Progressive {
    pub fn new() -> Self {
        Self {
            surfaces: BTreeMap::new(),
            work: vec![0; COEFFS * 3],
            temp: vec![0; COEFFS],
            touched: Vec::new(),
        }
    }

    /// Forget a surface's tiles: it was deleted, or created anew under the same
    /// number.
    pub fn forget(&mut self, surface: u16) {
        self.surfaces.remove(&surface);
    }

    /// Decode one PDU's worth of blocks for a surface of `width`×`height`, and hand
    /// each repainted rectangle to `paint` as `(rect, rows, stride)`: `rows` starts
    /// at the rectangle's top-left `BGRX32` pixel and each row is `stride` bytes
    /// after the last.
    pub fn decompress(
        &mut self,
        surface: u16,
        width: u32,
        height: u32,
        src: &[u8],
        mut paint: impl FnMut(Rect16, &[u8], usize),
    ) -> Result<(), Malformed> {
        let grid = self.surfaces.entry(surface).or_insert_with(|| Grid::new(width, height));
        if grid.width != width || grid.height != height {
            *grid = Grid::new(width, height);
        }
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
                    let magic = b.u32_le()?;
                    if magic != SYNC_MAGIC {
                        return Err(refuse("a sync magic", magic));
                    }
                    let version = b.u16_le()?;
                    if version != SYNC_VERSION {
                        return Err(refuse("a sync version", version));
                    }
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
                    match read_region(&mut b, grid, &mut self.work, &mut self.temp, &mut self.touched) {
                        Ok(region) => present(grid, &region, &mut self.touched, &mut paint),
                        Err(e) => {
                            // The tiles decoded before the fault are not written out;
                            // left marked as if they had been, nothing would ever
                            // write them out again.
                            for &index in &self.touched {
                                if let Some(tile) = grid.tiles[index].as_deref_mut() {
                                    tile.dirty = false;
                                }
                            }
                            self.touched.clear();
                            return Err(e);
                        }
                    }
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
/// tile in it, recording which of the grid's tiles were touched.
fn read_region(
    r: &mut Reader<'_>,
    grid: &mut Grid,
    work: &mut [i16],
    temp: &mut [i16],
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
    let mut t = Reader::new(WHAT, tiles);
    let mut count = 0u16;
    touched.clear();
    while !t.is_empty() {
        let kind = t.u16_le()?;
        let len = t.u32_le()?;
        let Some(body) = usize::try_from(len).ok().and_then(|len| len.checked_sub(6)) else {
            return Err(refuse("a tile block length", len));
        };
        let mut b = Reader::new(WHAT, t.bytes(body)?);
        match kind {
            WBT_TILE_SIMPLE | WBT_TILE_FIRST => {
                decode_first(&mut b, kind == WBT_TILE_SIMPLE, grid, &region, work, temp, touched)?;
            }
            WBT_TILE_UPGRADE => decode_upgrade(&mut b, grid, &region, work, temp, touched)?,
            other => return Err(refuse("a tile block type", other)),
        }
        if !b.is_empty() {
            return Err(refuse("a tile block with bytes past its fields", kind));
        }
        count = count.wrapping_add(1);
    }
    if count != num_tiles {
        return Err(refuse("a region's tile count", num_tiles));
    }
    Ok(region)
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
fn decode_first(
    r: &mut Reader<'_>,
    simple: bool,
    grid: &mut Grid,
    region: &Region,
    work: &mut [i16],
    temp: &mut [i16],
    touched: &mut Vec<usize>,
) -> Result<(), Malformed> {
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
    let diff = flags & TILE_DIFFERENCE != 0;
    for c in 0..3 {
        let bitpos = quants[c].add(progs[c]);
        let shift = bitpos.sub(Quant([1; 10])).ok_or_else(|| refuse("a zero quantization", 0u8))?;
        tile.bitpos[c] = bitpos;
        let out = &mut work[c * COEFFS..(c + 1) * COEFFS];
        let current = &mut tile.current[c * COEFFS..(c + 1) * COEFFS];
        let sign = &mut tile.sign[c * COEFFS..(c + 1) * COEFFS];
        decode_component(data[c], shift, out, current, sign, diff, temp)?;
    }
    tile.started = true;
    to_bgrx(work, &mut tile.pixels);
    if !tile.dirty {
        tile.dirty = true;
        touched.push(index);
    }
    Ok(())
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
) -> Result<(), Malformed> {
    rlgr1(data, out)?;
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
    Ok(())
}

/// A `TILE_UPGRADE`: more bits for the coefficients a first pass left.
fn decode_upgrade(
    r: &mut Reader<'_>,
    grid: &mut Grid,
    region: &Region,
    work: &mut [i16],
    temp: &mut [i16],
    touched: &mut Vec<usize>,
) -> Result<(), Malformed> {
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
    for c in 0..3 {
        let bitpos = quants[c].add(progs[c]);
        let bits = tile.bitpos[c].sub(bitpos).ok_or_else(|| refuse("an upgrade below the pass before it", quality))?;
        let shift = bitpos.sub(Quant([1; 10])).ok_or_else(|| refuse("a zero quantization", 0u8))?;
        tile.bitpos[c] = bitpos;
        let out = &mut work[c * COEFFS..(c + 1) * COEFFS];
        let current = &mut tile.current[c * COEFFS..(c + 1) * COEFFS];
        let sign = &mut tile.sign[c * COEFFS..(c + 1) * COEFFS];
        upgrade_component(data[c * 2], data[c * 2 + 1], shift, bits, current, sign);
        out.copy_from_slice(current);
        idwt(out, temp);
    }
    to_bgrx(work, &mut tile.pixels);
    if !tile.dirty {
        tile.dirty = true;
        touched.push(index);
    }
    Ok(())
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
        for (c, s) in cur.iter_mut().zip(&mut sign[at..at + len]) {
            let input = match (*s).cmp(&0) {
                std::cmp::Ordering::Greater => raw.take(num_bits) as i32,
                std::cmp::Ordering::Less => -(raw.take(num_bits) as i32),
                std::cmp::Ordering::Equal => {
                    let v = srl.read(num_bits);
                    *s = v;
                    i32::from(v)
                }
            };
            *c = (i32::from(*c) + (input << shift)) as i16;
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
        let mut mag: u32 = 1;
        let max = (1u32 << num_bits) - 1;
        while mag < max {
            if self.bits.take(1) == 1 {
                break;
            }
            mag += 1;
        }
        let mag = mag.min(i16::MAX as u32) as i16;
        if negative { -mag } else { mag }
    }
}

/// A bit stream read most-significant bit first, zero past its end, the way
/// WinPR's `wBitStream` presents one.
struct Bits<'a> {
    data: &'a [u8],
    /// Bits consumed.
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() * 8 - self.at
    }

    /// The next 32 bits, most significant first, zero past the end.
    fn peek(&self) -> u32 {
        let (byte, bit) = (self.at / 8, self.at % 8);
        let mut acc: u64 = 0;
        for i in 0..5 {
            acc = (acc << 8) | u64::from(self.data.get(byte + i).copied().unwrap_or(0));
        }
        ((acc >> (8 - bit)) & 0xFFFF_FFFF) as u32
    }

    fn skip(&mut self, n: usize) {
        self.at = (self.at + n).min(self.data.len() * 8);
    }

    /// Consume `n` bits, `n` in `0..=32`, as a number.
    fn take(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek() >> (32 - n);
        self.skip(n as usize);
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
fn rlgr1(src: &[u8], out: &mut [i16]) -> Result<(), Malformed> {
    if src.is_empty() {
        return Err(refuse("an empty RLGR stream", 0u8));
    }
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
    Ok(())
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

/// The reduce-extrapolate inverse wavelet over one component, three levels, in
/// place. [MS-RDPEGFX] 3.3.8.1.4, as FreeRDP has it.
fn idwt(buf: &mut [i16], temp: &mut [i16]) {
    idwt_level(&mut buf[3807..], temp, 3);
    idwt_level(&mut buf[3007..], temp, 2);
    idwt_level(buf, temp, 1);
}

fn band_l(level: u32) -> usize {
    (64 >> level) + 1
}

fn band_h(level: u32) -> usize {
    if level == 1 { 31 } else { (64 + (1 << (level - 1))) >> level }
}

/// One level: HL, LH, HH and LL sub-bands at the front of `buf` become the LL of
/// the level above, written over them.
fn idwt_level(buf: &mut [i16], temp: &mut [i16], level: u32) {
    let (l, h) = (band_l(level), band_h(level));
    let step = l + h;
    let hl = 0;
    let lh = hl + h * l;
    let hh = lh + l * h;
    let ll = hh + h * h;
    let (low, high) = temp.split_at_mut(l * step);

    // Horizontal: rows of (LL, HL) become L; rows of (LH, HH) become H.
    for row in 0..l {
        idwt_line(&buf[ll + row * l..], 1, &buf[hl + row * h..], 1, &mut low[row * step..], 1, l, h);
    }
    for row in 0..h {
        idwt_line(&buf[lh + row * l..], 1, &buf[hh + row * h..], 1, &mut high[row * step..], 1, l, h);
    }
    // Vertical: columns of (L, H) become the level above.
    for col in 0..step {
        idwt_line(&low[col..], step, &high[col..], step, &mut buf[col..], step, l, h);
    }
}

/// One line of the inverse transform: `nl` low and `nh` high samples, each `sl`,
/// `sh` apart, into `nl + nh` outputs `sx` apart. The last few outputs depend on how
/// the encoder padded the line, which the counts encode.
#[allow(clippy::too_many_arguments)]
fn idwt_line(low: &[i16], sl: usize, high: &[i16], sh: usize, dst: &mut [i16], sx: usize, nl: usize, nh: usize) {
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

/// The three transformed components — 11.5 fixed-point YCbCr — to a tile of
/// `BGRX32`, in the reference's fixed point.
fn to_bgrx(work: &[i16], pixels: &mut [u8]) {
    let (y, rest) = work.split_at(COEFFS);
    let (cb, cr) = rest.split_at(COEFFS);
    for (px, ((&y, &cb), &cr)) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(y.iter().zip(cb).zip(cr)) {
        let y = (i64::from(y) + 4096) << 16;
        let (cb, cr) = (i64::from(cb), i64::from(cr));
        let r = ((cr * 91916 + y) >> 16) >> 5;
        let g = ((y - cb * 22527 - cr * 46819) >> 16) >> 5;
        let b = ((cb * 115992 + y) >> 16) >> 5;
        *px = [clip(b), clip(g), clip(r), 0];
    }
}

fn clip(v: i64) -> u8 {
    v.clamp(0, 255) as u8
}

/// Write every tile the region touched to the surface, clipped to the region's
/// rectangles and to the surface.
fn present(grid: &mut Grid, region: &Region, touched: &mut Vec<usize>, paint: &mut impl FnMut(Rect16, &[u8], usize)) {
    for &index in touched.iter() {
        let Some(tile) = grid.tiles[index].as_deref_mut() else { continue };
        tile.dirty = false;
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
    touched.clear();
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

    /// The `BGRX` a flat tile at `dc` decodes to.
    pub(crate) fn grey(dc: u8) -> [u8; 4] {
        [128 + dc, 128 + dc, 128 + dc, 0]
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    /// Every paint call, as (rect, the pixels inside it, packed).
    fn collect(p: &mut Progressive, surface: u16, w: u32, h: u32, src: &[u8]) -> Result<Vec<(Rect16, Vec<u8>)>, Malformed> {
        let mut paints = Vec::new();
        p.decompress(surface, w, h, src, |rect, rows, stride| {
            let mut packed = Vec::new();
            for row in 0..usize::from(rect.height()) {
                packed.extend_from_slice(&rows[row * stride..row * stride + usize::from(rect.width()) * 4]);
            }
            paints.push((rect, packed));
        })?;
        Ok(paints)
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
        rlgr1(&encoded, &mut decoded).unwrap();
        assert_eq!(decoded, coeffs);
    }

    /// A component that encodes to nothing but its zero fill decodes to zeros.
    #[test]
    fn rlgr1_zero_fills_past_the_stream() {
        let mut decoded = vec![1i16; 16];
        rlgr1(&[0], &mut decoded).unwrap();
        assert_eq!(decoded, vec![0; 16]);
        assert!(rlgr1(&[], &mut decoded).is_err());
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
}
