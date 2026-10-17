//! The page's decoder for a Mac's Adaptive picture: one of the Mac's rectangles
//! in, the framebuffer it paints out.
//!
//! A session started with Standard's Adaptive quality lists the encodings Apple's
//! viewer lists for it, and a Mac sends only the first it knows of them: its 8×8
//! tiles (`0x3f3`). The gateway passes each rectangle as it came, so the page
//! decodes them here, against a framebuffer of its own: a rectangle draws on
//! what the ones before it left, by copies of tiles already there, by tiles
//! cached by an index, and by refining the coefficients the last full update
//! left in each tile.
//!
//! What follows is Apple's viewer's own decoding, as docs/apple-vnc-889.md
//! describes it ("Apple's own framebuffer encodings"), down to what it does with
//! a stream that is not what it should be, since the Mac codes each update
//! against what its viewer will have made of the last. A body is:
//! - **a full update** (`0`): two precisions, a command stream and a data stream;
//! - **a partial update** (`1`): one stream refining the last full update's tiles;
//! - **the quantization tables** (`2`), which come before the first picture.
//!
//! The coefficients are JPEG's: zigzag order, the reference library's integer
//! inverse transform and its YCbCr, each block at full resolution.

use std::fmt;

/// The encoding a Mac's Adaptive picture comes in.
pub const ENCODING: i32 = 0x3f3;

const FULL: u8 = 0;
const PARTIAL: u8 = 1;
const TABLES: u8 = 2;
/// The quantization message: its kind, then 64 luminance and 64 chrominance
/// entries.
const TABLES_LEN: usize = 129;
/// How far past a body's end its streams may be read before the body is refused,
/// as Apple's viewer allows: a stream is read a few bytes ahead of what it
/// spends, and an update's last tile can end in its last byte.
const OVERREAD: usize = 500;
/// The end of both of a full update's streams, and the first of a partial
/// update's three.
const MARK: u32 = 0x6d;
/// A partial update ends with `mvs`.
const PARTIAL_MARKS: [u32; 3] = [0x6d, 0x76, 0x73];

/// The cache of refined tiles: indices run from 1, are written up to 64,999 and
/// then from 1 again, and are read up to 65,000.
const CACHE_READ_MAX: u16 = 65_000;
const CACHE_WRITE_MAX: u16 = 64_999;
/// A cached tile: 64 luminance coefficients, 15 Cr and 20 Cb, in zigzag order.
const CACHED_Y: usize = 64;
const CACHED_CR: usize = 15;
const CACHED_CB: usize = 20;

/// A pixel as the page's canvas takes it: R, G, B, A in memory.
const OPAQUE: u32 = 0xff00_0000;
const WHITE: u32 = 0xffff_ffff;
const BLACK: u32 = OPAQUE;
/// What a two-colour tile that reuses its colours draws before any were read: the
/// viewer's own starting values.
const FIRST_PAIR: (u32, u32) = (0xffff_ffff, 0xffb5_d5fe);

/// JPEG's zigzag order: the natural position of the `k`th coefficient, with
/// sixteen entries past the end for a run that reads beyond it.
const NATURAL: [usize; 80] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21,
    28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54,
    47, 55, 62, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63,
];

/// Why a rectangle could not be decoded. The framebuffer keeps whatever the
/// rectangle drew before it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A body too short to hold its own header.
    Short { kind: u8, len: usize },
    /// A first byte that is none of the three bodies.
    Kind(u8),
    /// A quantization message of another length than 129 bytes.
    Tables(usize),
    /// A picture before the quantization tables.
    NoTables,
    /// A rectangle that does not fit in the framebuffer.
    Bounds { x: u16, y: u16, w: u16, h: u16, width: usize, height: usize },
    /// A full update's data stream said to start past its end.
    Offset { offset: usize, len: usize },
    /// A stream read past its body without reaching the rectangle's last tile.
    Overrun,
    /// A stream that ended its last tile on something other than its mark.
    Mark { want: u32, got: u32 },
    /// A tile's coefficients that cannot be read.
    Coefficients,
    /// A partial update refining a tile the last full update did not code with
    /// coefficients.
    Unrefinable,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Short { kind, len } => write!(f, "a body of kind {kind} in {len} bytes, under its header"),
            Self::Kind(kind) => write!(f, "a body of kind {kind}, which is no update"),
            Self::Tables(len) => write!(f, "quantization tables in {len} bytes, where they are {TABLES_LEN}"),
            Self::NoTables => write!(f, "a picture before the quantization tables"),
            Self::Bounds { x, y, w, h, width, height } => {
                write!(f, "a rectangle {w}x{h} at {x},{y} on a {width}x{height} framebuffer")
            }
            Self::Offset { offset, len } => write!(f, "a data stream at {offset} of a {len}-byte body"),
            Self::Overrun => write!(f, "an update that ran past its body"),
            Self::Mark { want, got } => write!(f, "an update that ended on {got:#04x}, where {want:#04x} ends it"),
            Self::Coefficients => write!(f, "a tile's coefficients that do not read"),
            Self::Unrefinable => write!(f, "a refinement of a tile the last full update did not code"),
        }
    }
}

impl std::error::Error for Error {}

/// A stream read most significant bit first. Past its end it reads zeros, and
/// [`Self::past`] says how far.
struct Bits<'a> {
    data: &'a [u8],
    /// The next byte to take into `window`.
    next: usize,
    window: u64,
    held: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], at: usize) -> Self {
        Self { data, next: at, window: 0, held: 0 }
    }

    /// The next `n` bits, at most 24.
    fn get(&mut self, n: u32) -> u32 {
        while self.held < n {
            let byte = self.data.get(self.next).copied().unwrap_or(0);
            self.next += 1;
            self.window = (self.window << 8 | u64::from(byte)) & 0xffff_ffff_ffff;
            self.held += 8;
        }
        self.held -= n;
        ((self.window >> self.held) & ((1 << n) - 1)) as u32
    }

    fn bit(&mut self) -> bool {
        self.get(1) == 1
    }

    /// Whether the stream has been read further than [`OVERREAD`] past the body.
    fn past(&self) -> bool {
        self.next > self.data.len() + OVERREAD
    }
}

/// A repeat count: how many tiles a command covers. A `0` bit is one tile; a `1`
/// and four bits below 15 are that many plus two; `1`, `1111` and a number of
/// up to three bytes, least significant seven bits first, are that number plus 17.
fn tiles_covered(bits: &mut Bits) -> u32 {
    if !bits.bit() {
        return 1;
    }
    let short = bits.get(4);
    if short != 15 {
        return short + 2;
    }
    let first = bits.get(8);
    if first & 0x80 == 0 {
        return first + 17;
    }
    let second = bits.get(8);
    if second & 0x80 == 0 {
        return (second << 7 | (first & 0x7f)) + 17;
    }
    let third = bits.get(8);
    (third << 14 | (second & 0x7f) << 7 | (first & 0x7f)) + 17
}

/// A signed number: `k` one bits and a zero, then for `k` 0 a bit, which when set
/// is 1 and is followed by a sign; for 1 and 2, 2 or 4 plus one or two bits; and
/// above, 8(k − 2) plus three bits, each followed by a sign, set for negative.
fn signed_number(bits: &mut Bits) -> Result<i16, Error> {
    let mut ones = 0u32;
    while bits.bit() {
        ones += 1;
        if ones == 39 {
            return Err(Error::Coefficients);
        }
    }
    let (magnitude, negative) = match ones {
        0 => {
            if !bits.bit() {
                return Ok(0);
            }
            (1, bits.bit())
        }
        1 => (2 + bits.get(1), bits.bit()),
        2 => (4 + bits.get(2), bits.bit()),
        k => ((8 * (k - 2) + bits.get(3)), bits.bit()),
    };
    let magnitude = magnitude as i16;
    Ok(if negative { magnitude.wrapping_neg() } else { magnitude })
}

/// JPEG's standard Huffman table for luminance AC coefficients, which a
/// refinement's chrominance is coded with.
struct Huffman {
    /// The largest code of each length, or -1 where there is none.
    max: [i32; 17],
    /// The first value of each length, less its first code.
    offset: [i32; 17],
}

const AC_LUMINANCE_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMINANCE_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14,
    0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09,
    0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a,
    0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65,
    0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88,
    0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9,
    0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
    0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea,
    0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];

impl Huffman {
    fn ac_luminance() -> Self {
        let mut max = [-1; 17];
        let mut offset = [0; 17];
        let mut code = 0i32;
        let mut first = 0i32;
        for (len, &count) in AC_LUMINANCE_BITS.iter().enumerate() {
            let len = len + 1;
            let count = i32::from(count);
            if count > 0 {
                offset[len] = first - code;
                code += count;
                max[len] = code - 1;
                first += count;
            }
            code <<= 1;
        }
        Self { max, offset }
    }

    /// The next symbol, or 0, an end of block, for a code no length has, as the
    /// viewer reads one.
    fn decode(&self, bits: &mut Bits) -> u8 {
        let mut code = 0i32;
        for len in 1..=16 {
            code = code << 1 | bits.get(1) as i32;
            if code <= self.max[len] {
                return AC_LUMINANCE_VALUES[(code + self.offset[len]) as usize];
            }
        }
        bits.get(1);
        0
    }
}

/// The three blocks of a tile, each in natural order: Y, Cb, Cr.
#[derive(Clone, Copy)]
struct Block {
    y: [i16; 64],
    cb: [i16; 64],
    cr: [i16; 64],
}

impl Block {
    const ZERO: Self = Self { y: [0; 64], cb: [0; 64], cr: [0; 64] };
}

/// What the viewer keeps for each tile of the framebuffer.
#[derive(Clone, Copy)]
struct Tile {
    /// What the last full update that coded the tile with coefficients left in
    /// it, for a partial update to refine.
    coded: Coded,
    /// The full update that last drew the tile.
    update: u32,
    /// The tile it was copied from, where that update copied it, and where in the
    /// framebuffer the copy was taken from.
    copied: Option<(usize, usize)>,
}

#[derive(Clone, Copy)]
struct Coded {
    precision: u8,
    /// Y's coefficients in zigzag order, the DC first, each in a signed byte.
    y: [i8; 64],
    /// The chrominance DCs, where the tile has them.
    cr: Option<i8>,
    cb: Option<i8>,
}

impl Tile {
    const BLANK: Self = Self {
        coded: Coded { precision: 0, y: [0; 64], cr: None, cb: None },
        update: 0,
        copied: None,
    };
}

/// How the tables of message 2 dequantize the three blocks, in natural order.
#[derive(Clone)]
struct Tables {
    luminance: [i32; 64],
    chrominance: [i32; 64],
}

/// One Mac's Adaptive picture: its framebuffer, what each of its tiles was last
/// coded as, and the state that outlives a rectangle.
pub struct Decoder {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
    tiles_across: usize,
    tiles: Vec<Tile>,
    tables: Option<Tables>,
    huffman: Huffman,
    /// Full updates read so far.
    updates: u32,
    /// Cached tiles by index, allocated at the first.
    cache: Vec<[i8; CACHED_Y + CACHED_CR + CACHED_CB]>,
    cached: u32,
    written: u16,
    /// The index a tile was last drawn from the cache by, which "the next" counts
    /// from.
    drawn: u16,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            pixels: Vec::new(),
            tiles_across: 0,
            tiles: Vec::new(),
            tables: None,
            huffman: Huffman::ac_luminance(),
            updates: 0,
            cache: Vec::new(),
            cached: 0,
            written: 0,
            drawn: 0,
        }
    }

    /// A framebuffer of a new size, black, with no tile coded. The tables and the
    /// cache outlive it, as the Mac's do.
    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.pixels = vec![BLACK; width * height];
        self.tiles_across = width.div_ceil(8);
        self.tiles = vec![Tile::BLANK; self.tiles_across * height.div_ceil(8)];
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The framebuffer, row after row, each pixel R, G, B, A in memory.
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    /// Decode one rectangle's body, the bytes after its `u32` length, onto the
    /// framebuffer. Returns whether it drew: the quantization tables do not.
    pub fn decode(&mut self, x: u16, y: u16, w: u16, h: u16, body: &[u8]) -> Result<bool, Error> {
        let Some(&kind) = body.first() else {
            return Err(Error::Short { kind: 0, len: 0 });
        };
        match kind {
            TABLES => {
                self.tables(body)?;
                Ok(false)
            }
            FULL | PARTIAL => {
                let rect = self.rect(x, y, w, h)?;
                if kind == FULL {
                    self.full(rect, body)?;
                } else {
                    self.partial(rect, body)?;
                }
                Ok(true)
            }
            other => Err(Error::Kind(other)),
        }
    }

    fn tables(&mut self, body: &[u8]) -> Result<(), Error> {
        if body.len() != TABLES_LEN {
            return Err(Error::Tables(body.len()));
        }
        let mut luminance = [0; 64];
        let mut chrominance = [0; 64];
        for i in 0..64 {
            luminance[i] = i32::from(body[1 + i]);
            chrominance[i] = i32::from(body[65 + i]);
        }
        self.tables = Some(Tables { luminance, chrominance });
        Ok(())
    }

    fn rect(&self, x: u16, y: u16, w: u16, h: u16) -> Result<Rect, Error> {
        let (x0, y0, w0, h0) = (usize::from(x), usize::from(y), usize::from(w), usize::from(h));
        let fits = x0 < self.width && x0 + w0 <= self.width && y0 < self.height && y0 + h0 <= self.height;
        if !fits {
            return Err(Error::Bounds { x, y, w, h, width: self.width, height: self.height });
        }
        Ok(Rect { x: x0, y: y0, w: w0, h: h0 })
    }

    fn full(&mut self, rect: Rect, body: &[u8]) -> Result<(), Error> {
        if body.len() < 6 {
            return Err(Error::Short { kind: FULL, len: body.len() });
        }
        let tables = self.tables.clone().ok_or(Error::NoTables)?;
        let precisions = [body[1], body[2]];
        let offset = usize::from(body[3]) << 16 | usize::from(body[4]) << 8 | usize::from(body[5]);
        if offset > body.len() {
            return Err(Error::Offset { offset, len: body.len() });
        }
        let mut commands = Bits::new(body, 6);
        let mut data = Bits::new(body, offset);
        commands.get(1);
        self.updates = self.updates.wrapping_add(1);

        let mut scan = Scan::new(rect, self.tiles_across);
        let mut single = BLACK;
        let mut pair = FIRST_PAIR;
        let mut previous = Block::ZERO;
        let mut last_coded: Option<usize> = None;
        // Where the tile before this one is, which "a copy of the previous tile"
        // copies, and at the update's first tile the tile itself.
        let mut before = scan.at(self.width);
        loop {
            if commands.past() {
                return Err(Error::Overrun);
            }
            let command = commands.get(3);
            let covered = tiles_covered(&mut commands);
            for _ in 0..covered {
                let index = scan.index;
                let here = scan.at(self.width);
                let tile = self.tiles.get_mut(index).ok_or(Error::Bounds {
                    x: rect.x as u16,
                    y: rect.y as u16,
                    w: rect.w as u16,
                    h: rect.h as u16,
                    width: self.width,
                    height: self.height,
                })?;
                tile.update = self.updates;
                tile.copied = None;
                let mut drawn = self.read_tile(&scan);
                match command {
                    0 => drawn = [WHITE; 64],
                    1 => {
                        self.tiles[index].copied = index.checked_sub(1).map(|from| (from, before));
                        self.copy_into(&mut drawn, before as isize);
                    }
                    2 => {
                        let above = here as isize - 8 * self.width as isize;
                        if let Some(from) = index.checked_sub(self.tiles_across) {
                            self.tiles[index].copied = Some((from, above.max(0) as usize));
                        }
                        self.copy_into(&mut drawn, above);
                    }
                    3 => masked(&mut data, &mut drawn, WHITE, BLACK),
                    4 => {
                        let two = data.bit();
                        let reuse = data.bit();
                        if two {
                            if !reuse {
                                pair = (colour(&mut data), colour(&mut data));
                            }
                            masked(&mut data, &mut drawn, pair.0, pair.1);
                        } else {
                            if !reuse {
                                single = colour(&mut data);
                            }
                            drawn = [single; 64];
                        }
                    }
                    5 => {
                        let (block, coded) = expand_block(&mut data, &previous, precisions, last_coded.map(|i| self.tiles[i]))?;
                        match coded {
                            Saved::Coded(coded) => self.tiles[index].coded = coded,
                            Saved::Last(last) => self.tiles[index] = last,
                            Saved::Nothing => {}
                        }
                        previous = block;
                        last_coded = Some(index);
                        inverse(&block, &tables, &mut drawn);
                    }
                    6 => {
                        let entry = data.get(16) as u16;
                        self.draw_cached(entry, &tables, &mut drawn);
                    }
                    _ => {
                        let entry = self.drawn.wrapping_add(1);
                        self.draw_cached(entry, &tables, &mut drawn);
                    }
                }
                self.write_tile(&scan, &drawn);
                before = here;
                if scan.advance() {
                    for stream in [&mut commands, &mut data] {
                        let got = stream.get(8);
                        if got != MARK {
                            return Err(Error::Mark { want: MARK, got });
                        }
                    }
                    return Ok(());
                }
            }
            if data.past() {
                return Err(Error::Overrun);
            }
        }
    }

    fn partial(&mut self, rect: Rect, body: &[u8]) -> Result<(), Error> {
        if body.len() < 3 {
            return Err(Error::Short { kind: PARTIAL, len: body.len() });
        }
        let tables = self.tables.clone().ok_or(Error::NoTables)?;
        let cr_count = body[1].min(64);
        let cb_count = body[2].min(64);
        let mut bits = Bits::new(body, 3);
        let mut scan = Scan::new(rect, self.tiles_across);
        loop {
            let index = scan.index;
            let tile = *self.tiles.get(index).ok_or(Error::Bounds {
                x: rect.x as u16,
                y: rect.y as u16,
                w: rect.w as u16,
                h: rect.h as u16,
                width: self.width,
                height: self.height,
            })?;
            match bits.get(2) {
                0 => {}
                1 => {
                    let mut drawn = self.read_tile(&scan);
                    let entry = refine(&mut bits, &tile.coded, cr_count, cb_count, &self.huffman)?;
                    let block = entry.block();
                    inverse(&block, &tables, &mut drawn);
                    self.cache_entry(entry);
                    self.write_tile(&scan, &drawn);
                }
                2 => {
                    // The copy the full update made, while the tile it copied is
                    // still that update's.
                    if let Some((from, at)) = tile.copied
                        && self.tiles.get(from).is_some_and(|source| source.update == tile.update)
                    {
                        let mut drawn = self.read_tile(&scan);
                        self.copy_into(&mut drawn, at as isize);
                        self.write_tile(&scan, &drawn);
                    }
                }
                _ => {
                    let entry = if bits.bit() { self.drawn.wrapping_add(1) } else { bits.get(16) as u16 };
                    let mut drawn = self.read_tile(&scan);
                    self.draw_cached(entry, &tables, &mut drawn);
                    self.write_tile(&scan, &drawn);
                }
            }
            if scan.advance() {
                for want in PARTIAL_MARKS {
                    let got = bits.get(8);
                    if got != want {
                        return Err(Error::Mark { want, got });
                    }
                }
                return Ok(());
            }
            if bits.past() {
                return Err(Error::Overrun);
            }
        }
    }

    /// The tile under `scan` as the framebuffer has it, its pixels past the
    /// rectangle's edge black.
    fn read_tile(&self, scan: &Scan) -> [u32; 64] {
        let mut tile = [BLACK; 64];
        let (w, h) = scan.clipped();
        for row in 0..h {
            let start = (scan.y() + row) * self.width + scan.x();
            tile[row * 8..row * 8 + w].copy_from_slice(&self.pixels[start..start + w]);
        }
        tile
    }

    /// `tile` onto the framebuffer, as much of it as the rectangle covers.
    fn write_tile(&mut self, scan: &Scan, tile: &[u32; 64]) {
        let (w, h) = scan.clipped();
        for row in 0..h {
            let start = (scan.y() + row) * self.width + scan.x();
            self.pixels[start..start + w].copy_from_slice(&tile[row * 8..row * 8 + w]);
        }
    }

    /// Eight rows of eight pixels from `at`, a pixel's place in the framebuffer
    /// read as one run of rows, into `tile`. A row that starts before the
    /// framebuffer is left as it was, and so is a pixel after its end.
    fn copy_into(&self, tile: &mut [u32; 64], at: isize) {
        for row in 0..8 {
            let start = at + row as isize * self.width as isize;
            if start < 0 {
                continue;
            }
            for column in 0..8 {
                if let Some(&pixel) = self.pixels.get(start as usize + column) {
                    tile[row * 8 + column] = pixel;
                }
            }
        }
    }

    /// The cached tile at `entry`, where there is one, which is then the one
    /// "the next" counts from.
    fn draw_cached(&mut self, entry: u16, tables: &Tables, tile: &mut [u32; 64]) {
        if entry == 0 || entry > CACHE_READ_MAX || u32::from(entry) > self.cached {
            return;
        }
        let Some(cached) = self.cache.get(usize::from(entry)) else {
            return;
        };
        let mut block = Block::ZERO;
        for k in 0..CACHED_Y {
            block.y[NATURAL[k]] = i16::from(cached[k]);
        }
        for k in 0..CACHED_CR {
            block.cr[NATURAL[k]] = i16::from(cached[CACHED_Y + k]);
        }
        for k in 0..CACHED_CB {
            block.cb[NATURAL[k]] = i16::from(cached[CACHED_Y + CACHED_CR + k]);
        }
        inverse(&block, tables, tile);
        self.drawn = entry;
    }

    fn cache_entry(&mut self, entry: Refined) {
        if self.cache.is_empty() {
            self.cache = vec![[0; CACHED_Y + CACHED_CR + CACHED_CB]; usize::from(CACHE_READ_MAX) + 1];
        }
        self.cached = self.cached.wrapping_add(1);
        self.written = if self.written == CACHE_WRITE_MAX { 1 } else { self.written + 1 };
        let slot = &mut self.cache[usize::from(self.written)];
        slot[..CACHED_Y].copy_from_slice(&entry.y);
        slot[CACHED_Y..CACHED_Y + CACHED_CR].copy_from_slice(&entry.cr);
        slot[CACHED_Y + CACHED_CR..].copy_from_slice(&entry.cb);
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// The tiles of a rectangle, in rows, left to right and top to bottom.
struct Scan {
    rect: Rect,
    /// Where the tile is in the rectangle.
    dx: usize,
    dy: usize,
    /// Its index in the framebuffer's tiles, and the index its row starts at.
    index: usize,
    row: usize,
    across: usize,
}

impl Scan {
    fn new(rect: Rect, across: usize) -> Self {
        let index = (rect.y >> 3) * across + (rect.x >> 3);
        Self { rect, dx: 0, dy: 0, index, row: index, across }
    }

    fn x(&self) -> usize {
        self.rect.x + self.dx
    }

    fn y(&self) -> usize {
        self.rect.y + self.dy
    }

    /// The tile's top left pixel's place in a framebuffer `width` wide.
    fn at(&self, width: usize) -> usize {
        self.y() * width + self.x()
    }

    /// How much of the tile the rectangle covers.
    fn clipped(&self) -> (usize, usize) {
        ((self.rect.w - self.dx).min(8), (self.rect.h - self.dy).min(8))
    }

    /// On to the next tile. Returns whether the one just done was the last.
    fn advance(&mut self) -> bool {
        self.dx += 8;
        if self.dx < self.rect.w {
            self.index += 1;
            return false;
        }
        self.dy += 8;
        if self.dy >= self.rect.h {
            return true;
        }
        self.dx = 0;
        self.row += self.across;
        self.index = self.row;
        false
    }
}

/// A row mask, then for each row whose bit is clear a pixel mask: a set bit takes
/// `set`, a clear one `clear`, and a row whose bit is set is `set` throughout.
fn masked(data: &mut Bits, tile: &mut [u32; 64], set: u32, clear: u32) {
    let rows = data.get(8);
    for row in 0..8 {
        if rows & (0x80 >> row) != 0 {
            tile[row * 8..row * 8 + 8].fill(set);
            continue;
        }
        let pixels = data.get(8);
        for column in 0..8 {
            tile[row * 8 + column] = if pixels & (0x80 >> column) != 0 { set } else { clear };
        }
    }
}

/// A colour: 8-bit Y, then the top six bits of Cb and of Cr.
fn colour(data: &mut Bits) -> u32 {
    let y = data.get(8) as u8;
    let cb = (data.get(6) << 2) as u8;
    let cr = (data.get(6) << 2) as u8;
    ycc(y, cb, cr)
}

/// JPEG's YCbCr to RGB, with the reference library's fixed-point tables.
fn ycc(y: u8, cb: u8, cr: u8) -> u32 {
    const ONE_HALF: i32 = 1 << 15;
    let cb = i32::from(cb) - 128;
    let cr = i32::from(cr) - 128;
    let y = i32::from(y);
    let r = y + ((91_881 * cr + ONE_HALF) >> 16);
    let g = y + ((-46_802 * cr + -22_554 * cb + ONE_HALF) >> 16);
    let b = y + ((116_130 * cb + ONE_HALF) >> 16);
    let clamp = |v: i32| v.clamp(0, 255) as u32;
    OPAQUE | clamp(b) << 16 | clamp(g) << 8 | clamp(r)
}

/// What a DCT tile leaves of itself for its tile's record.
enum Saved {
    Coded(Coded),
    /// The previous DCT tile's record over again, for a tile that copies it.
    Last(Tile),
    Nothing,
}

/// A DCT tile of a full update: the copy bit, the chroma and precision bits, the
/// chroma's DCs where they are sent, Y's DC, then Y's AC coefficients.
fn expand_block(data: &mut Bits, previous: &Block, precisions: [u8; 2], last: Option<Tile>) -> Result<(Block, Saved), Error> {
    let mut block = Block::ZERO;
    if data.bit() {
        let Some(last) = last else {
            return Ok((block, Saved::Nothing));
        };
        block.y = previous.y;
        block.cb[0] = previous.cb[0];
        block.cr[0] = previous.cr[0];
        return Ok((block, Saved::Last(last)));
    }
    let keep = data.bit();
    let precision = if data.bit() { precisions[1] } else { precisions[0] };
    if keep {
        block.cb[0] = previous.cb[0];
        block.cr[0] = previous.cr[0];
    } else {
        // Half the previous DC, rounded towards zero, less the number, doubled.
        let halved = |p: i16| p / 2;
        block.cb[0] = halved(previous.cb[0]).wrapping_sub(signed_number(data)?).wrapping_mul(2);
        block.cr[0] = halved(previous.cr[0]).wrapping_sub(signed_number(data)?).wrapping_mul(2);
    }
    block.y[0] = previous.y[0].wrapping_sub(signed_number(data)?);

    let fine = precision >= 15;
    let mut k = 1usize;
    let mut saved = true;
    'block: while k < 64 {
        let early = k < 6;
        let below = (k as u8) < precision;
        // A coefficient is sent short of its low bits: a unit is 1 shifted left by
        // that many.
        let shift = match (fine, below) {
            (false, true) => 3,
            (false, false) => 4,
            (true, true) => 1,
            (true, false) => 3,
        };
        if data.bit() {
            let mut ones = 0u32;
            while data.bit() {
                ones += 1;
                if early && ones == 33 {
                    return Err(Error::Coefficients);
                }
                if !early && ones == 19 {
                    // The viewer keeps what it has and leaves the record as it was.
                    saved = false;
                    break 'block;
                }
            }
            let magnitude: i32 = if early {
                if ones < 4 { 2 + 4 * ones as i32 + data.get(2) as i32 } else { 18 + 8 * (ones as i32 - 4) + data.get(3) as i32 }
            } else {
                match ones {
                    0 => 2 + data.get(1) as i32,
                    1 => 4 + data.get(2) as i32,
                    _ => 8 * (ones as i32 - 1) + data.get(3) as i32,
                }
            };
            let value = if data.bit() { -magnitude } else { magnitude };
            block.y[NATURAL[k]] = (value << shift) as i16;
            k += 1;
            continue;
        }
        let unit = 1i16 << shift;
        match data.get(2) {
            0b00 => k += 1,
            0b10 => {
                block.y[NATURAL[k]] = unit;
                k += 1;
            }
            0b11 => {
                block.y[NATURAL[k]] = -unit;
                k += 1;
            }
            _ => {
                if !data.bit() {
                    break 'block;
                }
                let r = data.get(2);
                let mut run = r + 3;
                if r == 3 {
                    loop {
                        let more = data.get(3);
                        run += more;
                        if more != 7 || k + (run & 0xff) as usize >= 64 {
                            break;
                        }
                    }
                    if k + (run & 0xff) as usize > 63 {
                        return Err(Error::Coefficients);
                    }
                    k += (run & 0xff) as usize;
                } else {
                    k += run as usize;
                }
            }
        }
    }
    if !saved {
        return Ok((block, Saved::Nothing));
    }
    let mut y = [0i8; 64];
    for (k, coefficient) in y.iter_mut().enumerate() {
        *coefficient = block.y[NATURAL[k]] as i8;
    }
    let coded = Coded { precision, y, cr: Some(block.cr[0] as i8), cb: Some(block.cb[0] as i8) };
    Ok((block, Saved::Coded(coded)))
}

/// A refined tile, in zigzag order, as the cache keeps it.
struct Refined {
    y: [i8; CACHED_Y],
    cr: [i8; CACHED_CR],
    cb: [i8; CACHED_CB],
    /// The whole of each chroma block, of which the cache keeps the start.
    cr_block: [i16; 64],
    cb_block: [i16; 64],
}

impl Refined {
    fn block(&self) -> Block {
        let mut block = Block::ZERO;
        for (k, &coefficient) in self.y.iter().enumerate() {
            block.y[NATURAL[k]] = i16::from(coefficient);
        }
        block.cr = self.cr_block;
        block.cb = self.cb_block;
        block
    }
}

/// A partial update's code 1: the bits the full update left out of a tile's Y
/// coefficients, and its chrominance as JPEG codes it.
fn refine(bits: &mut Bits, coded: &Coded, cr_count: u8, cb_count: u8, huffman: &Huffman) -> Result<Refined, Error> {
    let precision = usize::from(coded.precision);
    let count = bits.get(6) as usize + 1;
    let mut y = [0i8; 64];
    y[0] = coded.y[0];
    // Moved further from zero by `n` more bits.
    let further = |bits: &mut Bits, coefficient: i8, n: u32| {
        let more = bits.get(n) as i8;
        coefficient.wrapping_add(if coefficient < 0 { more.wrapping_neg() } else { more })
    };
    let below = count.min(precision);
    for (k, refined) in y.iter_mut().enumerate().take(below).skip(1) {
        let coefficient = coded.y[k];
        *refined = if precision < 15 {
            if coefficient == 0 { signed_number(bits)? as i8 } else { further(bits, coefficient, 3) }
        } else if coefficient == 0 {
            // Below the precision of a fine tile a zero is near enough: a bit, and
            // when it is set a sign for 1 or −1.
            if !bits.bit() {
                0
            } else if bits.bit() {
                -1
            } else {
                1
            }
        } else {
            further(bits, coefficient, 1)
        };
    }
    for (k, refined) in y.iter_mut().enumerate().take(count).skip(precision.max(1)) {
        let coefficient = coded.y[k];
        *refined = if coefficient == 0 {
            signed_number(bits)? as i8
        } else {
            further(bits, coefficient, if precision < 15 { 4 } else { 3 })
        };
    }

    let mut chroma = |dc: Option<i8>, count: u8| -> Result<[i16; 64], Error> {
        let dc = dc.ok_or(Error::Unrefinable)?;
        let mut block = [0i16; 64];
        let bit = bits.bit();
        block[0] = i16::from(if dc == 0 {
            match (bit, bit && bits.bit()) {
                (false, _) => 0,
                (true, false) => 1,
                (true, true) => -1,
            }
        } else {
            let step = i8::from(bit);
            dc.wrapping_add(if dc < 0 { -step } else { step })
        });
        let mut k = 0usize;
        while k < usize::from(count) {
            let symbol = huffman.decode(bits);
            let (run, size) = (usize::from(symbol >> 4), u32::from(symbol & 15));
            if size == 0 {
                if run != 15 {
                    break;
                }
                k += 15;
            } else {
                k += run;
                let value = bits.get(size) as i32;
                let value = if value < 1 << (size - 1) { value + 1 - (1 << size) } else { value };
                block[NATURAL[k + 1]] = value as i16;
            }
            k += 1;
        }
        Ok(block)
    };
    let cr_block = chroma(coded.cr, cr_count)?;
    let cb_block = chroma(coded.cb, cb_count)?;
    let mut cr = [0i8; CACHED_CR];
    let mut cb = [0i8; CACHED_CB];
    for k in 0..CACHED_CR {
        cr[k] = cr_block[NATURAL[k]] as i8;
    }
    for k in 0..CACHED_CB {
        cb[k] = cb_block[NATURAL[k]] as i8;
    }
    Ok(Refined { y, cr, cb, cr_block, cb_block })
}

/// The three blocks dequantized, inverted and converted to pixels.
fn inverse(block: &Block, tables: &Tables, tile: &mut [u32; 64]) {
    let y = idct(&block.y, &tables.luminance);
    let cb = idct(&block.cb, &tables.chrominance);
    let cr = idct(&block.cr, &tables.chrominance);
    for i in 0..64 {
        tile[i] = ycc(y[i], cb[i], cr[i]);
    }
}

/// The JPEG reference library's integer inverse DCT (`jidctint.c`), with its
/// output range limit.
fn idct(coefficients: &[i16; 64], quantization: &[i32; 64]) -> [u8; 64] {
    const CONST_BITS: u32 = 13;
    const PASS1_BITS: u32 = 2;
    const FIX_0_298631336: i64 = 2446;
    const FIX_0_390180644: i64 = 3196;
    const FIX_0_541196100: i64 = 4433;
    const FIX_0_765366865: i64 = 6270;
    const FIX_0_899976223: i64 = 7373;
    const FIX_1_175875602: i64 = 9633;
    const FIX_1_501321110: i64 = 12299;
    const FIX_1_847759065: i64 = 15137;
    const FIX_1_961570560: i64 = 16069;
    const FIX_2_053119869: i64 = 16819;
    const FIX_2_562915447: i64 = 20995;
    const FIX_3_072711026: i64 = 25172;
    fn descale(x: i64, n: u32) -> i64 {
        (x + (1 << (n - 1))) >> n
    }
    /// The even and odd halves of one row or column, its eight outputs before
    /// they are descaled, in order.
    fn butterfly(v: [i64; 8]) -> [i64; 8] {
        let (z2, z3) = (v[2], v[6]);
        let z1 = (z2 + z3) * FIX_0_541196100;
        let tmp2 = z1 + z3 * -FIX_1_847759065;
        let tmp3 = z1 + z2 * FIX_0_765366865;
        let tmp0 = (v[0] + v[4]) << CONST_BITS;
        let tmp1 = (v[0] - v[4]) << CONST_BITS;
        let (tmp10, tmp13, tmp11, tmp12) = (tmp0 + tmp3, tmp0 - tmp3, tmp1 + tmp2, tmp1 - tmp2);
        let (mut t0, mut t1, mut t2, mut t3) = (v[7], v[5], v[3], v[1]);
        let z1 = t0 + t3;
        let z2 = t1 + t2;
        let z3 = t0 + t2;
        let z4 = t1 + t3;
        let z5 = (z3 + z4) * FIX_1_175875602;
        t0 *= FIX_0_298631336;
        t1 *= FIX_2_053119869;
        t2 *= FIX_3_072711026;
        t3 *= FIX_1_501321110;
        let z1 = z1 * -FIX_0_899976223;
        let z2 = z2 * -FIX_2_562915447;
        let z3 = z3 * -FIX_1_961570560 + z5;
        let z4 = z4 * -FIX_0_390180644 + z5;
        t0 += z1 + z3;
        t1 += z2 + z4;
        t2 += z2 + z3;
        t3 += z1 + z4;
        [tmp10 + t3, tmp11 + t2, tmp12 + t1, tmp13 + t0, tmp13 - t0, tmp12 - t1, tmp11 - t2, tmp10 - t3]
    }
    // What a sample past 0–255 becomes: the library's table, read through its
    // ten-bit mask.
    fn limit(x: i64) -> u8 {
        match (x as i32 & 1023) as u32 {
            i @ 0..128 => (i + 128) as u8,
            128..512 => 255,
            512..896 => 0,
            i => (i - 896) as u8,
        }
    }
    let mut workspace = [0i32; 64];
    for column in 0..8 {
        let at = |row: usize| i64::from(coefficients[row * 8 + column]) * i64::from(quantization[row * 8 + column]);
        if (1..8).all(|row| coefficients[row * 8 + column] == 0) {
            let dc = (at(0) << PASS1_BITS) as i32;
            for row in 0..8 {
                workspace[row * 8 + column] = dc;
            }
            continue;
        }
        let out = butterfly(std::array::from_fn(at));
        for (row, value) in out.into_iter().enumerate() {
            workspace[row * 8 + column] = descale(value, CONST_BITS - PASS1_BITS) as i32;
        }
    }
    let mut samples = [0u8; 64];
    for row in 0..8 {
        let line = &workspace[row * 8..row * 8 + 8];
        let out = &mut samples[row * 8..row * 8 + 8];
        if line[1..].iter().all(|&v| v == 0) {
            out.fill(limit(descale(i64::from(line[0]), PASS1_BITS + 3)));
            continue;
        }
        let values = butterfly(std::array::from_fn(|i| i64::from(line[i])));
        for (sample, value) in out.iter_mut().zip(values) {
            *sample = limit(descale(value, CONST_BITS + PASS1_BITS + 3));
        }
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream written most significant bit first, padded with zeros.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        window: u64,
        held: u32,
    }

    impl Writer {
        fn put(&mut self, value: u32, bits: u32) -> &mut Self {
            self.window = self.window << bits | u64::from(value) & ((1 << bits) - 1);
            self.held += bits;
            while self.held >= 8 {
                self.held -= 8;
                self.bytes.push((self.window >> self.held) as u8);
            }
            self
        }

        fn bits(&mut self, text: &str) -> &mut Self {
            for bit in text.chars().filter(|c| !c.is_whitespace()) {
                self.put(u32::from(bit == '1'), 1);
            }
            self
        }

        fn done(&mut self) -> Vec<u8> {
            if self.held > 0 {
                let pad = 8 - self.held;
                self.put(0, pad);
            }
            std::mem::take(&mut self.bytes)
        }
    }

    /// Tables of ones, so a coefficient is its own dequantized value.
    fn tables() -> Vec<u8> {
        let mut body = vec![TABLES];
        body.extend([1; 128]);
        body
    }

    /// A full update of `commands` and `data`, each ended with its mark, at the
    /// coarse precisions a Mac starts with.
    fn full(commands: &mut Writer, data: &mut Writer) -> Vec<u8> {
        commands.put(MARK, 8);
        data.put(MARK, 8);
        let (commands, data) = (commands.done(), data.done());
        let offset = 6 + commands.len();
        let mut body = vec![FULL, 3, 5, (offset >> 16) as u8, (offset >> 8) as u8, offset as u8];
        body.extend(commands);
        body.extend(data);
        body
    }

    /// A decoder of `w`×`h` with the tables of ones.
    fn decoder(w: usize, h: usize) -> Decoder {
        let mut decoder = Decoder::new();
        decoder.resize(w, h);
        assert_eq!(decoder.decode(0, 0, 0, 0, &tables()), Ok(false));
        decoder
    }

    fn tile(decoder: &Decoder, x: usize, y: usize) -> Vec<u32> {
        (0..64).map(|i| decoder.pixels()[(y + i / 8) * decoder.width() + x + i % 8]).collect()
    }

    fn rgb(r: u8, g: u8, b: u8) -> u32 {
        OPAQUE | u32::from(b) << 16 | u32::from(g) << 8 | u32::from(r)
    }

    /// A command, after the one bit the stream starts with, covering one tile.
    fn one(command: u32) -> Writer {
        let mut commands = Writer::default();
        commands.bits("0").put(command, 3).bits("0");
        commands
    }

    #[test]
    fn a_white_tile_fills_the_tile_and_nothing_else() {
        let mut decoder = decoder(16, 8);
        let body = full(&mut one(0), &mut Writer::default());
        assert_eq!(decoder.decode(0, 0, 8, 8, &body), Ok(true));
        assert!(tile(&decoder, 0, 0).iter().all(|&p| p == WHITE));
        assert!(tile(&decoder, 8, 0).iter().all(|&p| p == BLACK));
    }

    #[test]
    fn a_repeat_covers_the_tiles_in_rows_and_clips_the_edges() {
        let mut decoder = decoder(24, 16);
        // `1`, four bits of 3: five tiles, the rectangle's six less one, which a
        // second command of one tile finishes.
        let mut commands = Writer::default();
        commands.bits("0").put(0, 3).bits("1 0011").put(0, 3).bits("0");
        let body = full(&mut commands, &mut Writer::default());
        assert_eq!(decoder.decode(0, 0, 20, 12, &body), Ok(true));
        for y in 0..16 {
            for x in 0..24 {
                let want = if x < 20 && y < 12 { WHITE } else { BLACK };
                assert_eq!(decoder.pixels()[y * 24 + x], want, "{x},{y}");
            }
        }
    }

    #[test]
    fn a_repeat_of_more_than_sixteen_is_counted_in_seven_bit_groups() {
        let mut bits = Writer::default();
        bits.bits("0 1 0011 1 1111").put(3, 8).bits("1 1111").put(0x80 | 5, 8).put(2, 8);
        let bytes = bits.done();
        let mut reader = Bits::new(&bytes, 0);
        assert_eq!(tiles_covered(&mut reader), 1);
        assert_eq!(tiles_covered(&mut reader), 5);
        assert_eq!(tiles_covered(&mut reader), 3 + 17);
        assert_eq!(tiles_covered(&mut reader), (2 << 7 | 5) + 17);
    }

    #[test]
    fn a_black_and_white_tile_is_a_row_mask_then_its_pixels() {
        let mut decoder = decoder(8, 8);
        let mut data = Writer::default();
        // Every row but the first is white; the first is its pixel mask.
        data.put(0b0111_1111, 8).put(0b1000_0001, 8);
        let body = full(&mut one(3), &mut data);
        decoder.decode(0, 0, 8, 8, &body).unwrap();
        let pixels = tile(&decoder, 0, 0);
        assert_eq!(&pixels[..8], &[WHITE, BLACK, BLACK, BLACK, BLACK, BLACK, BLACK, WHITE]);
        assert!(pixels[8..].iter().all(|&p| p == WHITE));
    }

    #[test]
    fn a_coloured_tile_is_one_colour_or_two_and_may_take_the_last_ones_again() {
        let mut decoder = decoder(32, 8);
        let mut commands = Writer::default();
        // Four tiles of the one command: `1` and two, which is two more than that.
        commands.bits("0").put(4, 3).bits("1 0010");
        let mut data = Writer::default();
        // One colour: Y 76, Cb and Cr at the centre, a grey.
        data.bits("0 0").put(76, 8).put(32, 6).put(32, 6);
        // Two, red and blue: every row their mask, the first colour where it is set.
        data.bits("1 0").put(76, 8).put(21, 6).put(63, 6).put(29, 8).put(63, 6).put(26, 6);
        data.put(0, 8);
        for _ in 0..8 {
            data.put(0b1111_0000, 8);
        }
        // The one colour again, without reading it, and the two again.
        data.bits("0 1").bits("1 1").put(0xff, 8);
        let body = full(&mut commands, &mut data);
        decoder.decode(0, 0, 32, 8, &body).unwrap();
        let grey = ycc(76, 128, 128);
        assert_eq!(grey, rgb(76, 76, 76));
        assert!(tile(&decoder, 0, 0).iter().all(|&p| p == grey));
        let (red, blue) = (ycc(76, 84, 252), ycc(29, 252, 104));
        let two = tile(&decoder, 8, 0);
        assert_eq!(&two[..8], &[red, red, red, red, blue, blue, blue, blue]);
        assert!(tile(&decoder, 16, 0).iter().all(|&p| p == grey));
        assert!(tile(&decoder, 24, 0).iter().all(|&p| p == red), "a row whose bit is set is the first colour");
    }

    #[test]
    fn copies_take_the_tile_before_and_the_tile_above() {
        let mut decoder = decoder(16, 16);
        let mut commands = Writer::default();
        // White, a copy of it to the right; then a copy from above, and black.
        commands.bits("0").put(0, 3).bits("0").put(1, 3).bits("0").put(2, 3).bits("0").put(3, 3).bits("0");
        let mut data = Writer::default();
        data.put(0, 8);
        for _ in 0..8 {
            data.put(0, 8);
        }
        let body = full(&mut commands, &mut data);
        decoder.decode(0, 0, 16, 16, &body).unwrap();
        assert!(tile(&decoder, 8, 0).iter().all(|&p| p == WHITE));
        assert!(tile(&decoder, 0, 8).iter().all(|&p| p == WHITE));
        assert!(tile(&decoder, 8, 8).iter().all(|&p| p == BLACK));
    }

    /// A DCT tile whose Y DC is −8 and the rest nothing: a flat 127 grey through
    /// the reference transform.
    fn flat_tile(data: &mut Writer) {
        // Not a copy, the chroma sent, the first precision; both chroma DCs 0.
        data.bits("0 0 0").bits("00 00");
        // The Y DC, 0 less 8: three ones and a zero, three bits of 0, positive.
        data.bits("1110 000 0");
        // The end of the block.
        data.bits("0 01 0");
    }

    #[test]
    fn a_dct_tile_is_inverted_by_the_reference_transform() {
        let mut decoder = decoder(8, 8);
        let mut data = Writer::default();
        flat_tile(&mut data);
        let body = full(&mut one(5), &mut data);
        decoder.decode(0, 0, 8, 8, &body).unwrap();
        assert!(tile(&decoder, 0, 0).iter().all(|&p| p == rgb(127, 127, 127)), "{:x?}", tile(&decoder, 0, 0));
    }

    #[test]
    fn a_refined_tile_is_cached_and_drawn_again_by_its_index() {
        let mut decoder = decoder(16, 8);
        let mut commands = Writer::default();
        commands.bits("0").put(5, 3).bits("0").put(0, 3).bits("0");
        let mut data = Writer::default();
        flat_tile(&mut data);
        decoder.decode(0, 0, 16, 8, &full(&mut commands, &mut data)).unwrap();

        // Refined: one Y coefficient, the DC; each chroma DC left at 0 and its AC
        // coefficients an end of block.
        let mut refinement = Writer::default();
        refinement.bits("01").put(0, 6).bits("0 1010").bits("0 1010");
        refinement.put(0x6d, 8).put(0x76, 8).put(0x73, 8);
        let mut body = vec![PARTIAL, 14, 19];
        body.extend(refinement.done());
        assert_eq!(decoder.decode(0, 0, 8, 8, &body), Ok(true));
        assert!(tile(&decoder, 0, 0).iter().all(|&p| p == rgb(127, 127, 127)));
        assert!(tile(&decoder, 8, 0).iter().all(|&p| p == WHITE));

        // The next cached tile after none drawn yet is the first: the refined one.
        let mut later = one(7);
        assert_eq!(decoder.decode(8, 0, 8, 8, &full(&mut later, &mut Writer::default())), Ok(true));
        assert!(tile(&decoder, 8, 0).iter().all(|&p| p == rgb(127, 127, 127)));
    }

    #[test]
    fn a_tile_the_full_update_did_not_code_cannot_be_refined() {
        let mut decoder = decoder(8, 8);
        decoder.decode(0, 0, 8, 8, &full(&mut one(0), &mut Writer::default())).unwrap();
        let mut refinement = Writer::default();
        refinement.bits("01").put(0, 6).bits("0 1010").bits("0 1010");
        let mut body = vec![PARTIAL, 14, 19];
        body.extend(refinement.done());
        assert_eq!(decoder.decode(0, 0, 8, 8, &body), Err(Error::Unrefinable));
    }

    #[test]
    fn a_partial_copy_repeats_the_full_updates_copy() {
        let mut decoder = decoder(16, 8);
        let mut commands = Writer::default();
        commands.bits("0").put(0, 3).bits("0").put(1, 3).bits("0");
        decoder.decode(0, 0, 16, 8, &full(&mut commands, &mut Writer::default())).unwrap();
        // Something else over the copy, then the partial update's copy again.
        decoder.pixels[8] = BLACK;
        decoder.pixels[0] = rgb(1, 2, 3);
        let mut refinement = Writer::default();
        refinement.bits("00 10");
        refinement.put(0x6d, 8).put(0x76, 8).put(0x73, 8);
        let mut body = vec![PARTIAL, 14, 19];
        body.extend(refinement.done());
        decoder.decode(0, 0, 16, 8, &body).unwrap();
        assert_eq!(decoder.pixels()[8], rgb(1, 2, 3), "copied again from where the copy came from");
    }

    #[test]
    fn what_does_not_fit_or_does_not_end_on_its_mark_is_refused() {
        let mut decoder = Decoder::new();
        decoder.resize(8, 8);
        let body = full(&mut one(0), &mut Writer::default());
        assert_eq!(decoder.decode(0, 0, 8, 8, &body), Err(Error::NoTables));
        decoder.decode(0, 0, 0, 0, &tables()).unwrap();
        assert!(matches!(decoder.decode(0, 0, 16, 8, &body), Err(Error::Bounds { .. })));
        let mut cut = body.clone();
        let end = cut.len() - 1;
        cut[end] = 0;
        assert_eq!(decoder.decode(0, 0, 8, 8, &cut), Err(Error::Mark { want: MARK, got: 0 }));
        assert_eq!(decoder.decode(0, 0, 0, 0, &[TABLES, 1]), Err(Error::Tables(2)));
        assert_eq!(decoder.decode(0, 0, 8, 8, &[7]), Err(Error::Kind(7)));
    }

    #[test]
    fn the_reference_transform_matches_its_known_outputs() {
        // A DC alone is the mean, the transform's rounding on top.
        let mut coefficients = [0i16; 64];
        coefficients[0] = 80;
        assert!(idct(&coefficients, &[1; 64]).iter().all(|&s| s == 138));
        // One horizontal frequency: symmetric about the centre, and inverted across it.
        let mut coefficients = [0i16; 64];
        coefficients[1] = 100;
        let samples = idct(&coefficients, &[1; 64]);
        for row in samples.chunks(8) {
            assert_eq!(row, samples.chunks(8).next().unwrap(), "every row the same");
            for x in 0..4 {
                assert_eq!(i32::from(row[x]) - 128, 128 - i32::from(row[7 - x]), "odd about the centre");
            }
            assert!(row[0] > row[3] && row[3] > 128);
        }
    }
}
