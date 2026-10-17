//! The picture this gateway codes itself, plain VNC's and the RDP bitmap path's: the
//! desktop in 64×64 tiles, each changed tile an image the browser decodes on its own.
//!
//! A changed tile goes as PNG where it holds few enough colours to be one exactly
//! ([`PALETTE`]), which is most of a desktop: text, rules and flat panels. Any other,
//! a photograph or a gradient, goes as JPEG at the plan's quality, and is owed its
//! exact picture: once it has gone a whole [settle tick](Tiles::tick) without
//! changing, it is sent again as PNG, as TigerVNC's server refreshes what it sent
//! lossy, region by region rather than when the whole screen is still. Every tile is
//! drawn on its own, so a tile sent replaces what the browser held there whatever it
//! held, and none depends on another.
//!
//! The coder holds the desktop's pixels in a [`Mirror`] and which tiles the browser
//! is owed. A round ([`Tiles::take_round`]) takes the tiles owed, a row's run of them
//! at a time, out of the mirror, so the engine goes on blitting into it while the
//! round is coded on a blocking worker; what lands meanwhile marks its tiles owed
//! again for the next. Runs are coded on every core ([`Round::code`]), and each run
//! is cut where its tiles change format, so a row is as few images as it can be.
//! What paces the rounds is [`crate::encode`]'s: the browser's painting, which a
//! slow link holds back, so damage merges in the mirror.
//!
//! A round the link would take too long to carry at the plan's quality goes coarse
//! instead ([`Round::coarse`]): every changed tile a JPEG at [`COARSE_QUALITY`], text
//! and panels included, which the settle tick makes exact once they are still. So a
//! slow link shows a whole new picture soon, blurred, and it sharpens where it stays,
//! as a Mac's Adaptive picture does, rather than a sharp one arriving row by row.

use anyhow::Context as _;
use rayon::prelude::*;

use crate::shadow::Rect;
use crate::video::{Mirror, check_picture};

/// The side of a tile, in pixels. A tile at the right or bottom edge of a desktop
/// whose side is not a multiple of it is cut there.
pub const TILE: u16 = 64;

/// The most colours a tile may hold to be sent exactly as it changes: a PNG of a
/// palette, which a tile of text, rules and flat panels fits, and which costs less
/// than its JPEG would. A tile with more is a picture of something, sent as JPEG and
/// made exact once it is still.
const PALETTE: usize = 256;

/// How a JPEG samples colour: once per 2×2 pixels. A JPEG tile is the lossy pass of
/// a tile that will be sent exactly once it is still, so it is the one that costs the
/// least, and a tile of text, where 4:2:0 would show, goes as PNG anyway.
const JPEG_SAMPLING: jpeg_encoder::SamplingFactor = jpeg_encoder::SamplingFactor::R_4_2_0;

/// What a tile's exact picture is taken to cost before one has been coded: what a
/// refresh is budgeted by until [`Tiles::exact_bytes`] has a measure, and a round of
/// changed tiles weighed by until [`Tiles::fine_bytes`] has one.
const EXACT_GUESS: u64 = 8 * 1024;

/// The JPEG quality of a coarse round ([`Round::coarse`]): a whole desktop in a
/// fifth of what it is at 90, about 100 KiB for a 1440×900 photograph, so a second
/// on a 1 Mbit/s link where 90 took four. Never finer than the plan's.
const COARSE_QUALITY: u8 = 20;

/// The image a coded rectangle is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A JPEG at the plan's quality, or at [`COARSE_QUALITY`] in a coarse round, its
    /// colour at [`JPEG_SAMPLING`]: lossy, and so owed its exact picture once its
    /// tiles are still.
    Jpeg,
    /// A PNG, the exact pixels.
    Png,
}

impl Format {
    /// The format's byte in a `TILE` record ([`crate::protocol::batch`]).
    pub fn code(self) -> u8 {
        match self {
            Self::Jpeg => crate::protocol::batch::TILE_JPEG,
            Self::Png => crate::protocol::batch::TILE_PNG,
        }
    }
}

/// What the browser is owed of one tile.
#[derive(Debug, Clone, Copy, Default)]
struct Tile {
    /// Changed since it was last taken for a round: the next round takes it.
    dirty: bool,
    /// What the browser was last sent of it is a JPEG: it is owed its exact picture
    /// once it is still.
    lossy: bool,
    /// Changed since the last settle tick, so not yet still.
    recent: bool,
    /// Lossy, and still for a whole settle tick: a refresh round takes it.
    refresh: bool,
}

/// The desktop's pixels and what the browser is owed of them.
pub struct Tiles {
    /// The JPEG quality, 1–100: the plan's ([`crate::config::RenderPlan::quality`]).
    quality: u8,
    /// The desktop, learned from [`crate::protocol::ServerMsg::Resize`]. `None` until
    /// the engine has announced one, which it always does before any damage.
    size: Option<(u16, u16)>,
    /// Built by the first blit of a desktop: see [`Self::mirror_mut`].
    mirror: Option<Mirror>,
    /// One per tile, row by row.
    grid: Vec<Tile>,
    cols: usize,
    rows: usize,
    /// How many tiles are [`Tile::dirty`] and how many [`Tile::refresh`].
    dirty: usize,
    refresh: usize,
    /// Whether a round is away being coded. At most one is, so the tiles a round took
    /// come back marked by what was sent before the next round looks at them.
    round_out: bool,
    /// Bumped whenever the desktop changes size: a round taken under an older one
    /// comes back to tiles that are no longer there.
    epoch: u64,
    /// The browser is to be told the picture starts here, from nothing, ahead of the
    /// next round's tiles: the first of a session, and after [`Self::reset`].
    start_owed: bool,
    /// What a tile's exact picture has come to lately, in bytes: what a refresh
    /// round is budgeted by.
    exact_bytes: u64,
    /// What a tile has come to lately sent at the plan's quality or exactly, in
    /// bytes: what the changed tiles owed are weighed by ([`Self::changed_bytes`]).
    fine_bytes: u64,
}

impl Tiles {
    pub fn new(quality: u8) -> Self {
        Self {
            quality,
            size: None,
            mirror: None,
            grid: Vec::new(),
            cols: 0,
            rows: 0,
            dirty: 0,
            refresh: 0,
            round_out: false,
            epoch: 0,
            start_owed: true,
            exact_bytes: EXACT_GUESS,
            fine_bytes: EXACT_GUESS,
        }
    }

    /// Adopt the desktop the client is about to be told about.
    ///
    /// Cannot fail, and is all that happens on the message path. A different size
    /// drops the mirror and every tile: the browser's canvas starts over black at the
    /// new size, and the remote repaints it, which the next blit builds the mirror for.
    pub fn want(&mut self, w: u16, h: u16) {
        if self.size != Some((w, h)) {
            self.size = Some((w, h));
            self.mirror = None;
            self.grid.clear();
            (self.cols, self.rows, self.dirty, self.refresh) = (0, 0, 0, 0);
            self.start_owed = true;
            self.epoch += 1;
        }
    }

    /// The mirror, built if it is not there yet, with every tile clean: what the
    /// mirror starts as is the black the browser's canvas does.
    ///
    /// Built on the pixel path rather than where the size arrives, because it can
    /// fail, and the size arrives inside `VideoSink::msg`, whose error every caller
    /// reads as "the browser has gone". Every caller of this is on the engines' `?`
    /// path, which ends the session with the message attached.
    fn mirror_mut(&mut self) -> anyhow::Result<&mut Mirror> {
        if self.mirror.is_none() {
            let (w, h) = self.size.ok_or_else(|| anyhow::anyhow!("the tiles were asked for pixels before a desktop size"))?;
            let mirror = Mirror::new(w, h)?;
            check_picture(mirror.size())?;
            self.cols = usize::from(w.div_ceil(TILE));
            self.rows = usize::from(h.div_ceil(TILE));
            self.grid = vec![Tile::default(); self.cols * self.rows];
            self.mirror = Some(mirror);
        }
        Ok(self.mirror.as_mut().expect("just built"))
    }

    /// Copy a changed rectangle's source pixels into the mirror, and mark every tile
    /// it touches owed: changed, so not still, and owed no refresh of what it was.
    pub fn blit(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        self.mirror_mut()?.blit(rect, rgb)?;
        let (first_col, last_col) = (usize::from(rect.left / TILE), usize::from(rect.right / TILE));
        let (first_row, last_row) = (usize::from(rect.top / TILE), usize::from(rect.bottom / TILE));
        for row in first_row..=last_row {
            for tile in &mut self.grid[row * self.cols + first_col..=row * self.cols + last_col] {
                if !tile.dirty {
                    tile.dirty = true;
                    self.dirty += 1;
                }
                if tile.refresh {
                    tile.refresh = false;
                    self.refresh -= 1;
                }
                tile.recent = true;
            }
        }
        Ok(())
    }

    /// Owe the browser the whole picture again, from its start: for a client that has
    /// to be able to draw from here, after a reattach, a repaint or the gap of a passed
    /// stream. Before the first blit there is nothing to owe but the start.
    pub fn reset(&mut self) {
        self.start_owed = true;
        if self.mirror.is_some() {
            self.grid.fill(Tile { dirty: true, ..Tile::default() });
            (self.dirty, self.refresh) = (self.grid.len(), 0);
        }
    }

    /// Whether a round is away being coded.
    pub fn round_out(&self) -> bool {
        self.round_out
    }

    /// Whether a round would take anything now: tiles changed, or tiles owed their
    /// exact picture. Never while a round is out, whose return says so instead, so an
    /// engine waiting on this is not woken to take nothing.
    pub fn owes(&self) -> bool {
        !self.round_out && (self.dirty > 0 || self.refresh > 0)
    }

    /// Whether the next round starts the picture over: what the order task tells the
    /// browser ahead of its tiles, and what the frame interval does not hold back.
    pub fn restarting(&self) -> bool {
        self.start_owed && self.dirty > 0
    }

    /// One settle tick: every tile sent lossy and not changed since the last tick is
    /// still, and owed its exact picture. True when a refresh is owed, for the order
    /// task to wake the engine. Nothing while a round is out: its tiles are marked by
    /// what it sent only once it is back, and the tick after that one takes them.
    pub fn tick(&mut self) -> bool {
        if self.round_out {
            return false;
        }
        for tile in &mut self.grid {
            if tile.lossy && !tile.recent && !tile.dirty && !tile.refresh {
                tile.refresh = true;
                self.refresh += 1;
            }
            tile.recent = false;
        }
        self.refresh > 0 && self.dirty == 0
    }

    /// What the changed tiles owed would come to at the plan's quality, at what a tile
    /// has lately cost there: what decides whether their round goes coarse.
    pub fn changed_bytes(&self) -> u64 {
        self.dirty as u64 * self.fine_bytes
    }

    /// Take the tiles owed, for a round coded on a blocking worker.
    ///
    /// Every changed tile, when there are any, coded coarse where `coarse` says to
    /// ([`Round::coarse`]): a refresh waits for the picture to be current. Otherwise
    /// the tiles owed their exact picture, top row first, as many as `budget` bytes
    /// come to at what a tile's exact picture has lately cost, and at least one.
    /// `None` when nothing is owed, and while a round is out.
    pub fn take_round(&mut self, budget: u64, coarse: bool) -> Option<Round> {
        if !self.owes() {
            return None;
        }
        let mirror = self.mirror.as_ref()?;
        let exact = self.dirty == 0;
        let mut allowed = if exact { (budget / self.exact_bytes.max(1)).max(1) } else { u64::MAX };
        let (w, h) = mirror.size();
        let mut runs = Vec::new();
        for row in 0..self.rows {
            let mut col = 0;
            while col < self.cols {
                let start = col;
                while col < self.cols && allowed > 0 {
                    let tile = &mut self.grid[row * self.cols + col];
                    let taken = if exact { std::mem::take(&mut tile.refresh) } else { std::mem::take(&mut tile.dirty) };
                    if !taken {
                        break;
                    }
                    allowed -= 1;
                    col += 1;
                }
                if col == start {
                    col += 1;
                    continue;
                }
                let x = start as u16 * TILE;
                let y = row as u16 * TILE;
                let width = (col as u16 * TILE).min(w) - x;
                let height = TILE.min(h - y);
                let rect = Rect::from_size(x, y, width, height).expect("a run of tiles has a size");
                let mut rgb = Vec::new();
                mirror.crop_into(rect, &mut rgb).expect("a run of tiles is inside the desktop");
                runs.push(Run { row, col: start, cols: col - start, rect, rgb });
            }
        }
        let tiles: usize = runs.iter().map(|run| run.cols).sum();
        if exact {
            self.refresh -= tiles;
        } else {
            self.dirty -= tiles;
        }
        self.round_out = true;
        let start = std::mem::take(&mut self.start_owed);
        let coarse = coarse && !exact;
        let quality = if coarse { COARSE_QUALITY.min(self.quality) } else { self.quality };
        Some(Round { runs, exact, coarse, quality, epoch: self.epoch, start })
    }

    /// Bring back what a round took, with what it was coded as: a tile sent as JPEG is
    /// owed its exact picture once still, and one sent as PNG is owed nothing. A tile
    /// that changed while the round was out is owed again whatever the round sent of
    /// it, and is marked by the round that sends it next; a round taken before a resize
    /// marks nothing.
    pub fn put_back(&mut self, round: &Round, coded: &[Coded]) {
        self.round_out = false;
        if round.epoch != self.epoch {
            return;
        }
        for part in coded {
            for col in part.col..part.col + part.cols {
                let tile = &mut self.grid[part.row * self.cols + col];
                if !tile.dirty {
                    tile.lossy = part.format == Format::Jpeg;
                }
            }
            if part.format == Format::Png && round.exact {
                // Weighted towards the latest, so a budget follows what the desktop
                // shows now.
                let per_tile = part.data.len() as u64 / part.cols as u64;
                self.exact_bytes = (self.exact_bytes * 3 + per_tile) / 4;
            }
        }
        // A coarse round says nothing of what its tiles cost at the plan's quality;
        // a refresh, their exact picture, says no less.
        let tiles: usize = coded.iter().map(|part| part.cols).sum();
        if !round.coarse && tiles > 0 {
            let per_tile = coded.iter().map(|part| part.data.len() as u64).sum::<u64>() / tiles as u64;
            self.fine_bytes = (self.fine_bytes * 3 + per_tile) / 4;
        }
    }
}

/// A row's run of tiles a round took, and their pixels.
struct Run {
    row: usize,
    /// Its first tile's column, and how many tiles it is.
    col: usize,
    cols: usize,
    rect: Rect,
    /// The run's pixels, packed RGB888, `rect`'s width a row.
    rgb: Vec<u8>,
}

/// The tiles a round took, away from [`Tiles`] while a blocking worker codes them.
pub struct Round {
    runs: Vec<Run>,
    /// A refresh round: tiles owed their exact picture, every one coded as PNG.
    exact: bool,
    /// See [`Self::coarse`].
    coarse: bool,
    /// The JPEG quality its tiles are coded at.
    quality: u8,
    /// The [`Tiles::epoch`] it was taken under.
    epoch: u64,
    /// The browser is to be told the picture starts here, ahead of this round's tiles.
    pub start: bool,
}

impl Round {
    /// Whether this round sends tiles owed their exact picture rather than tiles that
    /// changed.
    pub fn exact(&self) -> bool {
        self.exact
    }

    /// Whether this round sends its changed tiles coarse: every one a JPEG at
    /// [`COARSE_QUALITY`] but a tile of one colour, whose PNG is exact and smaller,
    /// because the link would take too long to carry them at the plan's quality. Each
    /// is owed its exact picture once it is still, as any JPEG tile is.
    pub fn coarse(&self) -> bool {
        self.coarse
    }

    /// How many tiles the round took.
    pub fn tiles(&self) -> usize {
        self.runs.iter().map(|run| run.cols).sum()
    }

    /// Code every run, on every core, in the order the browser draws them: rows top
    /// to bottom, each left to right. Blocking: call it on a worker.
    pub fn code(&self) -> anyhow::Result<Vec<Coded>> {
        let coded: Vec<Vec<Coded>> = self.runs.par_iter().map(|run| code_run(run, self.exact, self.coarse, self.quality)).collect::<anyhow::Result<_>>()?;
        Ok(coded.into_iter().flatten().collect())
    }
}

/// One rectangle of a run, coded: where it is, in which image, and the tiles it is.
#[derive(Debug)]
pub struct Coded {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub format: Format,
    pub data: Vec<u8>,
    row: usize,
    col: usize,
    cols: usize,
}

/// Code `run`, cut where its tiles change format: a tile of few colours, and every
/// tile of an `exact` round, as PNG, and any other as JPEG at `quality`. In a
/// `coarse` round only a tile of one colour is a PNG.
fn code_run(run: &Run, exact: bool, coarse: bool, quality: u8) -> anyhow::Result<Vec<Coded>> {
    let (width, height) = (usize::from(run.rect.w()), usize::from(run.rect.h()));
    let tile = usize::from(TILE);
    let formats: Vec<Format> = (0..run.cols)
        .map(|i| {
            let left = i * tile;
            let right = (left + tile).min(width);
            let colours = || palette(rows(&run.rgb, width, left, right, height)).map(|(colours, _)| colours.len() / 3);
            let exactly = exact || if coarse { colours() == Some(1) } else { colours().is_some() };
            if exactly { Format::Png } else { Format::Jpeg }
        })
        .collect();
    let mut coded = Vec::new();
    let mut first = 0;
    while first < run.cols {
        let format = formats[first];
        let last = (first..run.cols).take_while(|&i| formats[i] == format).last().expect("a run of at least one");
        let left = first * tile;
        let right = ((last + 1) * tile).min(width);
        let rgb: Vec<u8> = rows(&run.rgb, width, left, right, height).flatten().copied().collect();
        let (w, h) = ((right - left) as u16, height as u16);
        let data = match format {
            Format::Png => png(w, h, &rgb),
            Format::Jpeg => jpeg(w, h, &rgb, quality),
        }
        .with_context(|| format!("coding a {w}x{h} tile as {format:?}"))?;
        coded.push(Coded {
            x: run.rect.left + left as u16,
            y: run.rect.top,
            w,
            h,
            format,
            data,
            row: run.row,
            col: run.col + first,
            cols: last + 1 - first,
        });
        first = last + 1;
    }
    Ok(coded)
}

/// The columns `left..right` of each of `height` rows of a `width`-wide RGB888
/// picture.
fn rows(rgb: &[u8], width: usize, left: usize, right: usize, height: usize) -> impl Iterator<Item = &[u8]> + Clone {
    (0..height).map(move |row| &rgb[(row * width + left) * 3..(row * width + right) * 3])
}

/// The picture's colours, at most [`PALETTE`] of them, and each pixel's index into
/// them; `None` for a picture of more.
///
/// An open-addressed table of twice the palette's slots, a pixel the same as the one
/// before it taking no lookup: a desktop's runs of one colour are most of its pixels.
fn palette<'a>(rows: impl Iterator<Item = &'a [u8]>) -> Option<(Vec<u8>, Vec<u8>)> {
    const SLOTS: usize = PALETTE * 4;
    // A slot holds its colour plus one, so zero is empty, and its index.
    let mut table = [(0u32, 0u8); SLOTS];
    let mut colours: Vec<u8> = Vec::new();
    let mut indices = Vec::new();
    let mut last: Option<(u32, u8)> = None;
    for row in rows {
        for pixel in row.as_chunks::<3>().0 {
            let key = u32::from(pixel[0]) << 16 | u32::from(pixel[1]) << 8 | u32::from(pixel[2]);
            if let Some((colour, index)) = last
                && colour == key
            {
                indices.push(index);
                continue;
            }
            let mut slot = (key.wrapping_mul(0x9E37_79B1) >> 22) as usize % SLOTS;
            let index = loop {
                match table[slot] {
                    (0, _) => {
                        let count = colours.len() / 3;
                        if count == PALETTE {
                            return None;
                        }
                        colours.extend_from_slice(pixel);
                        table[slot] = (key + 1, count as u8);
                        break count as u8;
                    }
                    (held, index) if held == key + 1 => break index,
                    _ => slot = (slot + 1) % SLOTS,
                }
            };
            last = Some((key, index));
            indices.push(index);
        }
    }
    Some((colours, indices))
}

/// `rgb`, a `w`×`h` picture, as a PNG: of a palette where it fits one, unfiltered,
/// since a palette's indices are not a signal a filter predicts, and otherwise of RGB
/// with each row's filter chosen.
fn png(w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Vec<u8>> {
    let width = usize::from(w);
    let mut out = Vec::with_capacity(rgb.len() / 4 + 256);
    let mut encoder = png::Encoder::new(&mut out, u32::from(w), u32::from(h));
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let indexed = palette(rgb.chunks_exact(width * 3));
    if let Some((colours, _)) = &indexed {
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_palette(colours.clone());
        encoder.set_filter(png::Filter::NoFilter);
    } else {
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_filter(png::Filter::Adaptive);
    }
    let mut writer = encoder.write_header()?;
    match &indexed {
        Some((_, indices)) => writer.write_image_data(indices)?,
        None => writer.write_image_data(rgb)?,
    }
    writer.finish()?;
    Ok(out)
}

/// `rgb`, a `w`×`h` picture, as a baseline JPEG at `quality`, its colour at
/// [`JPEG_SAMPLING`].
fn jpeg(w: u16, h: u16, rgb: &[u8], quality: u8) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(rgb.len() / 8 + 1024);
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder.set_sampling_factor(JPEG_SAMPLING);
    encoder.encode(rgb, w, h, jpeg_encoder::ColorType::Rgb)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    fn flat(w: u16, h: u16, colour: [u8; 3]) -> Vec<u8> {
        colour.iter().copied().cycle().take(usize::from(w) * usize::from(h) * 3).collect()
    }

    /// `w`×`h` of noise, more colours than any palette.
    fn noise(w: u16, h: u16, salt: u32) -> Vec<u8> {
        let mut seed = salt;
        (0..usize::from(w) * usize::from(h) * 3)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 16) as u8
            })
            .collect()
    }

    fn tiles(w: u16, h: u16) -> Tiles {
        let mut tiles = Tiles::new(80);
        tiles.want(w, h);
        tiles
    }

    /// A round, coded and brought back.
    fn sent(tiles: &mut Tiles, budget: u64) -> (Round, Vec<Coded>) {
        let round = tiles.take_round(budget, false).expect("something owed");
        let coded = round.code().expect("a round codes");
        tiles.put_back(&round, &coded);
        (round, coded)
    }

    /// A PNG's pixels, as RGB.
    fn unpng(data: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().expect("a PNG");
        let mut buf = vec![0; reader.output_buffer_size().expect("a size")];
        let info = reader.next_frame(&mut buf).expect("a frame");
        assert_eq!(info.color_type, png::ColorType::Rgb);
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    /// A JPEG's size, from its baseline frame header.
    fn unjpeg(data: &[u8]) -> (u16, u16) {
        assert_eq!(&data[..2], [0xff, 0xd8], "no JPEG start of image");
        let at = data.windows(2).position(|pair| pair == [0xff, 0xc0]).expect("a baseline frame header");
        let field = |i: usize| u16::from_be_bytes([data[at + i], data[at + i + 1]]);
        (field(7), field(5))
    }

    /// A blit owes the tiles it touches and no others, a round takes a row's owed
    /// tiles as one run, and a round taken leaves nothing owed.
    #[test]
    fn a_round_takes_the_tiles_a_blit_touched_a_rows_run_at_a_time() {
        let mut tiles = tiles(320, 200);
        tiles.blit(placed(60, 10, 80, 60), &flat(80, 60, [10, 20, 30])).expect("a blit");
        assert!(tiles.owes());
        let (round, coded) = sent(&mut tiles, 0);
        assert!(round.start, "the session's first round starts the picture");
        assert_eq!(round.tiles(), 6, "columns 0–2 of rows 0 and 1");
        let rects: Vec<_> = coded.iter().map(|part| (part.x, part.y, part.w, part.h, part.format)).collect();
        assert_eq!(rects, [(0, 0, 192, 64, Format::Png), (0, 64, 192, 64, Format::Png)]);
        assert!(!tiles.owes(), "a round taken leaves nothing owed");
        assert!(tiles.take_round(0, false).is_none());
    }

    /// A tile of few colours is sent exactly, and one of many as JPEG, a run cut where
    /// its tiles' formats change.
    #[test]
    fn a_run_is_cut_where_its_tiles_change_format() {
        let mut tiles = tiles(256, 64);
        let mut picture = flat(256, 64, [200, 200, 200]);
        let photo = noise(64, 64, 7);
        for row in 0..64 {
            picture[(row * 256 + 64) * 3..(row * 256 + 128) * 3].copy_from_slice(&photo[row * 192..(row + 1) * 192]);
        }
        tiles.blit(placed(0, 0, 256, 64), &picture).expect("a blit");
        let (_, coded) = sent(&mut tiles, 0);
        let parts: Vec<_> = coded.iter().map(|part| (part.x, part.w, part.format)).collect();
        assert_eq!(parts, [(0, 64, Format::Png), (64, 64, Format::Jpeg), (128, 128, Format::Png)]);
        let (w, h, rgb) = unpng(&coded[2].data);
        assert_eq!((w, h), (128, 64));
        assert!(rgb.as_chunks::<3>().0.iter().all(|pixel| *pixel == [200, 200, 200]), "the PNG is not the pixels");
        assert_eq!(unjpeg(&coded[1].data), (64, 64));
    }

    /// The exact picture of a tile sent as JPEG: owed once a whole tick has gone by
    /// without it changing, not while it keeps changing, and sent as PNG of the
    /// mirror's pixels, after which it is owed nothing.
    #[test]
    fn a_lossy_tile_is_sent_exactly_once_it_is_still() {
        let mut tiles = tiles(128, 64);
        let photo = noise(64, 64, 3);
        tiles.blit(placed(0, 0, 64, 64), &photo).expect("a blit");
        let (_, coded) = sent(&mut tiles, 0);
        assert_eq!(coded[0].format, Format::Jpeg);
        assert!(!tiles.tick(), "changed in this tick, so not still");
        assert!(tiles.tick(), "a whole tick without a change");
        let (round, coded) = sent(&mut tiles, 0);
        assert!(round.exact());
        assert_eq!(coded.iter().map(|part| part.format).collect::<Vec<_>>(), [Format::Png]);
        assert_eq!(unpng(&coded[0].data).2, photo, "the refresh is not the mirror's pixels");
        assert!(!tiles.tick() && !tiles.tick(), "a tile sent exactly is owed nothing");

        // One that changes again before it is still is owed its changed pixels, not a
        // refresh of the old.
        tiles.blit(placed(0, 0, 64, 64), &noise(64, 64, 4)).expect("a blit");
        sent(&mut tiles, 0);
        tiles.tick();
        tiles.blit(placed(0, 0, 64, 64), &noise(64, 64, 5)).expect("a blit");
        assert!(!tiles.tick(), "changed in the tick");
        let (round, _) = sent(&mut tiles, 0);
        assert!(!round.exact(), "a changed tile goes ahead of any refresh");
    }

    /// Changed tiles go ahead of a refresh, and a refresh takes as many tiles as its
    /// budget comes to, and at least one.
    #[test]
    fn a_refresh_waits_for_changes_and_keeps_to_its_budget() {
        let mut tiles = tiles(256, 64);
        tiles.blit(placed(0, 0, 256, 64), &noise(256, 64, 9)).expect("a blit");
        sent(&mut tiles, 0);
        tiles.tick();
        assert!(tiles.tick());
        tiles.blit(placed(0, 0, 1, 1), &[1, 2, 3]).expect("a blit");
        assert!(!tiles.take_round(u64::MAX, false).expect("owed").exact(), "a refresh went ahead of a change");
        tiles.round_out = false;
        let one = tiles.take_round(0, false).expect("owed");
        assert_eq!((one.exact(), one.tiles()), (true, 1), "a refresh of no budget is one tile");
        tiles.round_out = false;
        let two = tiles.take_round(2 * EXACT_GUESS, false).expect("owed");
        assert_eq!(two.tiles(), 2);
    }

    /// A coarse round sends a tile of text as a JPEG at the coarse quality, smaller
    /// than its PNG, and keeps a tile of one colour exact; the JPEG is made exact once
    /// it is still, and a round of no change is never coarse.
    #[test]
    fn a_coarse_round_sends_text_as_jpeg_and_makes_it_exact_once_still() {
        let mut tiles = tiles(128, 64);
        // Two colours in stripes: a palette's, which a fine round sends as PNG.
        let text: Vec<u8> = (0..64 * 64).flat_map(|i| if (i * 7 / 3) % 5 < 2 { [0, 0, 0] } else { [250, 250, 250] }).collect();
        let mut picture = flat(128, 64, [40, 40, 40]);
        for row in 0..64 {
            picture[row * 384..row * 384 + 192].copy_from_slice(&text[row * 192..(row + 1) * 192]);
        }
        tiles.blit(placed(0, 0, 128, 64), &picture).expect("a blit");
        assert_eq!(tiles.changed_bytes(), 2 * EXACT_GUESS);
        let round = tiles.take_round(0, true).expect("owed");
        assert!(round.coarse() && round.quality == COARSE_QUALITY);
        let coded = round.code().expect("a round codes");
        tiles.put_back(&round, &coded);
        let parts: Vec<_> = coded.iter().map(|part| (part.x, part.format)).collect();
        assert_eq!(parts, [(0, Format::Jpeg), (64, Format::Png)], "text coarse, the flat tile exact");
        assert_eq!(tiles.fine_bytes, EXACT_GUESS, "a coarse round weighed what a fine tile costs");

        tiles.tick();
        assert!(tiles.tick(), "the coarse tile is owed its exact picture");
        let refresh = tiles.take_round(u64::MAX, true).expect("owed");
        assert!(refresh.exact() && !refresh.coarse(), "a refresh went coarse");
        let coded = refresh.code().expect("a round codes");
        assert_eq!(coded.iter().map(|part| (part.x, part.format)).collect::<Vec<_>>(), [(0, Format::Png)]);
        assert_eq!(unpng(&coded[0].data).2, text);
    }

    /// The whole picture again, for a client starting over: every tile, behind a start.
    #[test]
    fn a_reset_owes_the_whole_picture_from_its_start() {
        let mut tiles = tiles(200, 100);
        tiles.blit(placed(0, 0, 10, 10), &flat(10, 10, [1, 1, 1])).expect("a blit");
        sent(&mut tiles, 0);
        tiles.reset();
        assert!(tiles.restarting());
        let (round, coded) = sent(&mut tiles, 0);
        assert!(round.start && round.tiles() == 8);
        // An odd desktop's last column and row are cut at its edge.
        let rects: Vec<_> = coded.iter().map(|part| (part.x, part.y, part.w, part.h)).collect();
        assert_eq!(rects, [(0, 0, 200, 64), (0, 64, 200, 36)]);
        assert!(!tiles.restarting());
    }

    /// A round that comes back after a resize marks nothing of the new desktop, whose
    /// first pixels are owed their own round.
    #[test]
    fn a_round_that_outlives_its_desktop_marks_nothing() {
        let mut tiles = tiles(128, 128);
        tiles.blit(placed(0, 0, 128, 128), &noise(128, 128, 1)).expect("a blit");
        let stale = tiles.take_round(0, false).expect("owed");
        let coded = stale.code().expect("a round codes");
        tiles.want(64, 64);
        tiles.blit(placed(0, 0, 64, 64), &flat(64, 64, [5, 5, 5])).expect("a blit at the new size");
        assert!(!tiles.owes(), "nothing while the round is out");
        tiles.put_back(&stale, &coded);
        let (round, coded) = sent(&mut tiles, 0);
        assert!(round.start, "the new desktop's picture starts over");
        assert_eq!(coded.len(), 1);
        assert!(!tiles.tick() && !tiles.tick(), "the stale round's JPEG marked a tile of the new desktop");
    }

    /// Pixels before a size are refused by name, and a desktop past the ceiling is
    /// refused by its first blit.
    #[test]
    fn a_desktop_without_a_size_or_past_the_ceiling_is_refused() {
        let mut tiles = Tiles::new(80);
        assert!(tiles.blit(placed(0, 0, 1, 1), &[0, 0, 0]).is_err());
        tiles.want(5120, 2880);
        let refused = tiles.blit(placed(0, 0, 1, 1), &[0, 0, 0]).expect_err("a 5K desktop was taken");
        assert!(format!("{refused:#}").contains("5120x2880"), "{refused:#}");
    }

    /// The palette is at most 256 colours, each pixel its index.
    #[test]
    fn a_palette_holds_up_to_256_colours() {
        let colours: Vec<u8> = (0..=255u8).flat_map(|i| [i, 255 - i, i / 2]).collect();
        let (held, indices) = palette(std::iter::once(colours.as_slice())).expect("256 colours");
        assert_eq!(held, colours);
        assert_eq!(indices, (0..=255u8).collect::<Vec<_>>());
        let more: Vec<u8> = [colours.as_slice(), &[1, 1, 1]].concat();
        assert!(palette(std::iter::once(more.as_slice())).is_none());
    }
}
