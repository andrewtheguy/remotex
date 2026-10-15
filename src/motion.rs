//! A picture sent for motion: exact PNG where the desktop is still, and a VP9 stream
//! of its own for each region that moves.
//!
//! The picture of a session started with Motion ([`crate::config::Choices::motion`]):
//! an RDP host's graphics pipeline, composed here. It is the gateway's motion mode
//! of old (`render_motion`, removed with the tile transport in 6c948ad3) brought
//! back with its rules — which tiles are moving, which become regions
//! ([`crate::regions`]), when a region ends and what it then owes — and one
//! difference: what is still goes as an exact PNG, where it went as the target's
//! still codec.
//!
//! - **Churn.** A tile changed in [`CHURN_MOVING`] of its last [`CHURN_WINDOW`]
//!   slots of [`CHURN_SLOT`] is moving, counted only where its pixels really
//!   changed ([`Regions::changed`]).
//! - **What is sent at once.** A tile that changed and that no stream covers is sent
//!   as a PNG in the next round, as a run of its row's changed tiles, cut to what
//!   changed in them. A tile a stream covers is never also sent as a PNG.
//! - **Cleanups.** A tile a stream carried is owed a PNG once the stream has ended
//!   and the tile has been still for [`CLEANUP_IDLE`], [`MAX_CLEANUPS_PER_TICK`] of
//!   them every [`CLEANUP_TICK`], held up to [`CLEANUP_HELD`] while an adaptive
//!   plan's page is behind.
//!
//! It is the shape of [`crate::stream::DesktopStream`], for [`crate::encode`]'s
//! sink: damage is blitted into a mirror, a round is taken to a blocking worker with
//! what it needs and encoded there, and comes back. Under `render_motion_debug`
//! what each decision sent is outlined in the pixels themselves: a region's stream
//! in magenta, a PNG cut from beside a stream in cyan, and a cleanup in green.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use anyhow::Context as _;

use crate::protocol::{Held, ServerMsg, TileUnit};
use crate::regions::{Regions, Round, tiles_of};
use crate::shadow::Rect;
use crate::video::{Mark, outline};

pub use crate::regions::TILE;

/// How long one slot of churn is.
///
/// A unit of time rather than of the remote's frames, because frames are not a
/// rate: a busy desktop reports damage far more often than anything presents it,
/// and the same movement reported in ten rectangles is not ten times the movement.
/// A slot means the same thing whoever reports, and "in motion" stays one statement
/// about the remote.
const CHURN_SLOT: Duration = Duration::from_millis(100);

/// Slots of change history each tile keeps — the width of the `u8` shift register
/// in [`ChurnCell`], so it may not exceed 8. With [`CHURN_SLOT`] that is a window
/// of 800 ms.
const CHURN_WINDOW: u64 = 8;

/// Churn — how many of a tile's last [`CHURN_WINDOW`] slots changed it — at which
/// the tile is taken to be in motion.
///
/// Half the window, so one isolated change is never motion — a popup, an image
/// that just finished loading, a cursor — and a tile has to keep changing for
/// 400 ms of the last 800 ms before it is. Anything less would only draw a
/// pointless cleanup a moment later.
const CHURN_MOVING: u32 = 4;

/// How long a tile a stream carried must sit unchanged before it is sent as a PNG.
/// Long enough that a brief pause in motion is not chased with a redundant
/// re-send, short enough that a settled region sharpens while the eye is still on
/// it.
const CLEANUP_IDLE: Duration = Duration::from_millis(500);

/// How long a cleanup may be held back for a page that is behind, on an adaptive
/// plan. Five seconds is the walk from a stream's usual dial down to its floor: a
/// link still behind after that is not one the streams can make room on, and a
/// tile left at a stream's quality for good is the worse of the two failures.
const CLEANUP_HELD: Duration = Duration::from_secs(5);

/// How often settled tiles are looked for. Its own clock rather than something the
/// next change does, because a screen that stops changing produces no next change
/// — which is exactly the case a cleanup is for.
const CLEANUP_TICK: Duration = Duration::from_millis(250);

/// Cleanups per tick, so a whole stopped video settles over a few ticks rather
/// than in one burst competing with live motion for the socket. Forty 64-pixel
/// tiles is a 640×256 patch per tick: a stopped 720p player sharpens in about two
/// seconds, and a full 1080p desktop in a little over three.
const MAX_CLEANUPS_PER_TICK: usize = 40;

/// Colour of a `render_motion_debug` outline round a region's stream.
///
/// Magenta, cyan and green rather than anything subtler: these survive a VP9 at a
/// low dial, and none of them is a colour a desktop produces in a straight line by
/// accident.
const MARK_MOTION: [u8; 3] = [255, 0, 255];
/// Colour of a `render_motion_debug` outline on a PNG cut from a row a stream
/// shares — the still side of the split.
const MARK_CRISP: [u8; 3] = [0, 255, 255];
/// Colour of a `render_motion_debug` outline on a cleanup.
const MARK_CLEANUP: [u8; 3] = [0, 255, 0];
/// Thickness of a `render_motion_debug` outline. Two pixels, because one does not
/// reliably survive chroma subsampling at the quality a moving region is sent at.
const MARK_PX: u16 = 2;

/// One tile's recent history: bit 0 is the slot it was last seen changing,
/// shifted left one place for every slot since. Its churn is the number of set
/// bits — how many of the last [`CHURN_WINDOW`] slots it changed in.
#[derive(Clone, Copy)]
struct ChurnCell {
    history: u8,
    last_slot: u64,
}

/// Which tiles are changing fast enough for a stream to be worth starting over them.
///
/// Keyed by tile, which is the reason the grid exists: a remote describes the same
/// moving region with different rectangles from one frame to the next, and a key
/// that moved with them would count no churn at all.
#[derive(Default)]
struct Churn {
    /// Where slot numbering starts, taken from the first observation and dropped
    /// by [`Self::clear`].
    ///
    /// Slots have to be numbered against one origin every tile shares rather than
    /// measured as a delta per tile: a tile changing faster than [`CHURN_SLOT`]
    /// would otherwise carry its own last-seen instant forward with every change,
    /// never advance its register, and so never be in motion — which is the case
    /// this exists to catch.
    origin: Option<Instant>,
    churn: HashMap<(u16, u16), ChurnCell>,
}

impl Churn {
    fn slot(origin: Instant, now: Instant) -> u64 {
        u64::try_from(now.saturating_duration_since(origin).as_millis() / CHURN_SLOT.as_millis()).unwrap_or(u64::MAX)
    }

    /// Record that `key` changed at `now`, and return its churn.
    ///
    /// Aging is lazy: a tile that did not change is not touched, and its history is
    /// shifted by the whole gap when it is next seen. That is also what keeps the
    /// shift from overflowing — a gap of a full window empties the history instead.
    fn observe(&mut self, key: (u16, u16), now: Instant) -> u32 {
        let origin = *self.origin.get_or_insert(now);
        let slot = Self::slot(origin, now);
        let cell = self.churn.entry(key).or_insert(ChurnCell { history: 0, last_slot: slot });
        let elapsed = slot.saturating_sub(cell.last_slot);
        cell.history = if elapsed >= CHURN_WINDOW { 0 } else { cell.history << elapsed };
        cell.history |= 1;
        cell.last_slot = slot;
        cell.history.count_ones()
    }

    /// The tiles in motion as of `now`.
    ///
    /// [`Self::observe`] ages a tile's history only when it changes, which is what
    /// keeps the hot path cheap; this is the other side of that bargain, and the one
    /// caller who needs the answer for tiles that are *not* changing. A tile whose
    /// history has emptied is dropped outright, so the map stays the size of the
    /// screen's recent activity rather than of the session.
    fn moving(&mut self, now: Instant) -> Vec<(u16, u16)> {
        let Some(origin) = self.origin else {
            return Vec::new();
        };
        let slot = Self::slot(origin, now);
        let mut moving = Vec::new();
        self.churn.retain(|key, cell| {
            let elapsed = slot.saturating_sub(cell.last_slot);
            cell.history = if elapsed >= CHURN_WINDOW { 0 } else { cell.history << elapsed };
            cell.last_slot = slot;
            if cell.history.count_ones() >= CHURN_MOVING {
                moving.push(*key);
            }
            cell.history != 0
        });
        moving.sort_unstable();
        moving
    }

    /// Drop everything. A resize changes what every key means, and a repaint has
    /// already re-sent every pixel the churn was counted from.
    fn clear(&mut self) {
        self.origin = None;
        self.churn.clear();
    }
}

/// The picture, its churn, its regions, and what it owes the page.
pub struct MotionStream {
    regions: Regions,
    churn: Churn,
    /// Tiles changed since the last round, by (row, column) so they come out a row
    /// at a time: each with the box of what changed in it.
    pending: BTreeMap<(u16, u16), Rect>,
    /// The page is owed the picture from nothing: a `TilesStart`, and every tile.
    restart: bool,
    round_out: bool,
    /// When settled tiles are next looked for — see [`CLEANUP_TICK`].
    cleanup_at: Option<Instant>,
    /// Whether `render_motion_debug` outlines what each decision sent.
    debug: bool,
}

impl MotionStream {
    pub fn new(quality: u8, native: bool, debug: bool) -> Self {
        Self {
            regions: Regions::new(quality, native, debug.then_some(Mark { colour: MARK_MOTION, px: MARK_PX })),
            churn: Churn::default(),
            pending: BTreeMap::new(),
            restart: true,
            round_out: false,
            cleanup_at: None,
            debug,
        }
    }

    /// Adopt the desktop the page is about to be told about. A different size
    /// starts the picture over.
    pub fn want(&mut self, w: u16, h: u16) {
        if self.regions.size() == Some((w, h)) {
            return;
        }
        self.regions.want(w, h);
        self.pending.clear();
        self.churn.clear();
        self.restart = true;
    }

    /// Copy a changed rectangle's pixels into the mirror, and count it against the
    /// tiles whose pixels it changes.
    ///
    /// Churn is recorded for the tiles that *changed*, whether or not a stream is
    /// already carrying them, because that is what keeps the stream alive.
    pub fn blit(&mut self, rect: Rect, rgb: &[u8], now: Instant) -> anyhow::Result<()> {
        let changed = self.regions.changed(rect, rgb)?;
        self.regions.blit(rect, rgb)?;
        for ((col, row), part) in changed {
            self.churn.observe((col, row), now);
            self.pending
                .entry((row, col))
                .and_modify(|seen| {
                    seen.left = seen.left.min(part.left);
                    seen.top = seen.top.min(part.top);
                    seen.right = seen.right.max(part.right);
                    seen.bottom = seen.bottom.max(part.bottom);
                })
                .or_insert(part);
        }
        Ok(())
    }

    /// Whether a round is away being encoded.
    pub fn round_out(&self) -> bool {
        self.round_out
    }

    /// Whether settled tiles are to be looked for at `now`.
    fn cleanup_due(&self, now: Instant) -> bool {
        self.cleanup_at.is_none_or(|at| now >= at) && self.regions.due_at(CLEANUP_IDLE).is_some_and(|at| now >= at)
    }

    /// Whether there is something to send at `now`.
    pub fn dirty(&self, now: Instant) -> bool {
        if self.round_out || !self.regions.pictured() {
            return false;
        }
        self.restart
            || !self.pending.is_empty()
            || self.regions.dirty()
            || self.regions.expires_at().is_some_and(|at| now >= at)
            || self.cleanup_due(now)
    }

    /// When there is next something to send if nothing changes: a region's end, or
    /// a tile one carried owed its PNG.
    pub fn next_due(&self, now: Instant) -> Option<Instant> {
        if self.round_out || !self.regions.pictured() {
            return None;
        }
        if self.dirty(now) {
            return Some(now);
        }
        let cleanup = self.regions.due_at(CLEANUP_IDLE).map(|at| self.cleanup_at.map_or(at, |tick| tick.max(at)));
        [self.regions.expires_at(), cleanup].into_iter().flatten().min()
    }

    /// Start the picture over for a page that has nothing: a reattach, or a
    /// repaint.
    pub fn force_keyframe(&mut self) {
        self.restart = true;
    }

    /// Move every stream's dial, for the congestion walk.
    pub fn set_quality(&mut self, quality: u8) {
        self.regions.set_quality(quality);
    }

    pub fn quality(&self) -> u8 {
        self.regions.quality()
    }

    /// Take what is owed at `now`, for an encode on a blocking worker: the regions
    /// laid out again, the ends that says, the PNGs, and the streams with pixels
    /// waiting. `None` when there is nothing, or a round is already away.
    ///
    /// `behind` is an adaptive plan's page lagging, which holds the cleanups back
    /// for up to [`CLEANUP_HELD`]: they are the heaviest thing sent for pixels that
    /// are already right, and the link they would go into is one the streams are
    /// being coarsened to fit.
    pub fn take_round(&mut self, now: Instant, behind: bool) -> anyhow::Result<Option<MotionRound>> {
        if !self.dirty(now) {
            return Ok(None);
        }
        let mut msgs = Vec::new();
        if self.restart {
            self.restart = false;
            // Every stream ends, and is said to, ahead of the picture starting over.
            self.regions.forget();
            msgs.extend(self.regions.drain_ended().into_iter().map(|stream| ServerMsg::RegionEnd { stream }));
            msgs.push(ServerMsg::TilesStart);
            self.churn.clear();
            self.cleanup_at = None;
            let (w, h) = self.regions.size().expect("a pictured stream has a size");
            let whole = Rect::from_size(0, 0, w, h).expect("a desktop is never empty");
            self.pending = tiles_of(whole).map(|((col, row), part)| ((row, col), part)).collect();
        }
        // Geometry moves here, before anything is sent: that is what makes the set
        // of streamed tiles the PNGs below are cut against a stable thing.
        let moving = self.churn.moving(now);
        self.regions.retune(&moving, now)?;
        // A screen that has stopped changing has no motion to retune for, so this is
        // what notices its streams have gone quiet.
        self.regions.expire(now);
        // Ahead of this round's records and behind every one before it: the units
        // that used these ids are already queued in front, and the region that takes
        // one next is in this round.
        msgs.extend(self.regions.drain_ended().into_iter().map(|stream| ServerMsg::RegionEnd { stream }));

        let mut bands = Vec::new();
        // The changed tiles no stream covers go out as *runs*: along each row, every
        // stretch of neighbouring tiles that changed is one PNG, cut to the box of
        // what changed in it. The tile is the unit of identity, not of transport.
        let mut runs: Vec<(u16, u16, Rect)> = Vec::new();
        for (&(row, col), part) in &self.pending {
            if self.regions.covers((col, row)) {
                continue;
            }
            match runs.last_mut() {
                Some((run_row, run_col, run)) if *run_row == row && run_col.checked_add(1) == Some(col) => {
                    *run_col = col;
                    run.left = run.left.min(part.left);
                    run.top = run.top.min(part.top);
                    run.right = run.right.max(part.right);
                    run.bottom = run.bottom.max(part.bottom);
                }
                _ => runs.push((row, col, *part)),
            }
        }
        self.pending.clear();
        for (row, _, run) in runs {
            // Only a PNG cut from beside a stream is marked: the still majority of a
            // desktop sent unmarked is the quiet path.
            let mark = self.regions.row_covered(row).then_some(MARK_CRISP);
            bands.push(self.band(run, mark)?);
            self.regions.discharge(run);
        }

        if self.cleanup_due(now) {
            self.cleanup_at = Some(now + CLEANUP_TICK);
            let idle = if behind { CLEANUP_HELD } else { CLEANUP_IDLE };
            for rect in self.regions.due(now, idle, MAX_CLEANUPS_PER_TICK) {
                bands.push(self.band(rect, Some(MARK_CLEANUP))?);
            }
        }

        let streams = self.regions.take_round();
        if msgs.is_empty() && bands.is_empty() && streams.is_none() {
            return Ok(None);
        }
        self.round_out = true;
        Ok(Some(MotionRound { msgs, bands, streams, quality: self.regions.quality() }))
    }

    /// `rect` cropped from the mirror, outlined in `mark` under `render_motion_debug`.
    fn band(&self, rect: Rect, mark: Option<[u8; 3]>) -> anyhow::Result<(Rect, Vec<u8>)> {
        let mut rgb = Vec::new();
        self.regions.crop(rect, &mut rgb)?;
        if let Some(colour) = mark.filter(|_| self.debug) {
            outline(&mut rgb, (usize::from(rect.w()), usize::from(rect.h())), Mark { colour, px: MARK_PX });
        }
        Ok((rect, rgb))
    }

    /// Put back what [`Self::take_round`] took: its streams, which have just carried
    /// their tiles at `now`.
    pub fn put_back(&mut self, round: MotionRound, now: Instant) {
        self.round_out = false;
        if let Some(streams) = round.streams {
            self.regions.put_back(streams, now);
        }
    }
}

/// One round: what it says first, the PNGs it sends, and the streams it encodes.
pub struct MotionRound {
    msgs: Vec<ServerMsg>,
    bands: Vec<(Rect, Vec<u8>)>,
    streams: Option<Round>,
    quality: u8,
}

impl MotionRound {
    /// Whether a stream's unit in it will be one a decoder can start from.
    pub fn keyframe(&self) -> bool {
        self.streams.as_ref().is_some_and(Round::keyframe)
    }

    /// The coarsest any stream in it is encoding at, or the dial with none.
    pub fn quality(&self) -> u8 {
        self.streams.as_ref().and_then(Round::quality).unwrap_or(self.quality)
    }

    /// Streams whose encode yielded no bitstream. Must stay zero.
    pub fn skipped(&self) -> u64 {
        self.streams.as_ref().map_or(0, Round::skipped)
    }

    /// Encode the round, in the order its records go out: what it says first, the
    /// PNGs, and each stream's configuration ahead of its unit. Blocking: call it on
    /// a worker. The PNGs and the streams are encoded side by side, the PNGs over as
    /// many threads as the gateway gives an encoder.
    pub fn encode(&mut self) -> anyhow::Result<Vec<ServerMsg>> {
        let bands = std::mem::take(&mut self.bands);
        let workers = crate::video::threads().max(1);
        let streams = self.streams.as_mut();
        let (tiles, units) = std::thread::scope(|scope| {
            let units = streams.map(|streams| scope.spawn(move || streams.encode()));
            let chunk = bands.len().div_ceil(workers).max(1);
            let pngs: Vec<_> = bands
                .chunks(chunk)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .map(|(rect, rgb)| {
                                Ok(TileUnit {
                                    x: rect.left,
                                    y: rect.top,
                                    w: rect.w(),
                                    h: rect.h(),
                                    png: encode_png(rect.w(), rect.h(), rgb)?,
                                    held: Held::default(),
                                })
                            })
                            .collect::<anyhow::Result<Vec<TileUnit>>>()
                    })
                })
                .collect();
            let tiles: anyhow::Result<Vec<TileUnit>> = pngs
                .into_iter()
                .map(|handle| handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
                .collect::<anyhow::Result<Vec<Vec<TileUnit>>>>()
                .map(|chunks| chunks.into_iter().flatten().collect());
            let units = units.map(|handle| handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)));
            (tiles, units)
        });
        let mut records = std::mem::take(&mut self.msgs);
        records.extend(tiles?.into_iter().map(ServerMsg::Tile));
        if let Some(units) = units {
            records.extend(units?);
        }
        Ok(records)
    }
}

/// Packed RGB888 as an 8-bit RGB PNG. `Fast`: a still tile is sent once, so its
/// bytes count for more than its encode, and the five filters scored per row are
/// what keep a desktop's text and flats small.
fn encode_png(w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(rgb.len() / 4 + 64);
    let mut encoder = png::Encoder::new(&mut out, u32::from(w), u32::from(h));
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().context("a tile's PNG header")?;
    writer.write_image_data(rgb).context("a tile's PNG")?;
    writer.finish().context("a tile's PNG")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u16 = 640;
    const H: u16 = 390;

    /// A picture of `w`×`h` where every pixel is a function of its place and `seed`.
    fn pattern(w: u16, h: u16, seed: u8) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for y in 0..h {
            for x in 0..w {
                rgb.extend_from_slice(&[(x as u8) ^ seed, (y as u8).wrapping_add(seed), seed.wrapping_mul(3)]);
            }
        }
        rgb
    }

    fn whole() -> Rect {
        Rect::from_size(0, 0, W, H).unwrap()
    }

    /// The page's picture: tiles drawn where they say, read back with an independent
    /// PNG decoder, and the rectangles regions drew, which are not exact.
    struct Page {
        rgb: Vec<u8>,
        regions: Vec<(u8, Rect)>,
        started: bool,
    }

    impl Page {
        fn new() -> Self {
            Self { rgb: vec![0; usize::from(W) * usize::from(H) * 3], regions: Vec::new(), started: false }
        }

        fn take(&mut self, records: Vec<ServerMsg>) {
            for record in records {
                match record {
                    ServerMsg::TilesStart => {
                        assert!(self.regions.is_empty(), "a region outlived the picture it was in");
                        self.started = true;
                    }
                    ServerMsg::Tile(tile) => {
                        assert!(self.started, "a tile before tilesStart");
                        let rect = Rect::from_size(tile.x, tile.y, tile.w, tile.h).unwrap();
                        for (id, region) in &self.regions {
                            assert!(rect.intersect(region).is_none(), "a tile inside live region {id}");
                        }
                        let mut reader = png::Decoder::new(std::io::Cursor::new(tile.png)).read_info().unwrap();
                        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
                        let info = reader.next_frame(&mut buf).unwrap();
                        assert_eq!((info.width, info.height), (u32::from(tile.w), u32::from(tile.h)));
                        assert_eq!(info.color_type, png::ColorType::Rgb);
                        paint(&mut self.rgb, rect, &buf);
                    }
                    ServerMsg::RegionFormat { .. } => {}
                    ServerMsg::Region(unit) => {
                        let rect = Rect::from_size(unit.x, unit.y, unit.w, unit.h).unwrap();
                        if !self.regions.contains(&(unit.stream, rect)) {
                            assert!(unit.keyframe, "a region starts at a keyframe");
                            self.regions.retain(|(id, _)| *id != unit.stream);
                            for (id, other) in &self.regions {
                                assert!(rect.intersect(other).is_none(), "regions {id} and {} overlap", unit.stream);
                            }
                            self.regions.push((unit.stream, rect));
                        }
                    }
                    ServerMsg::RegionEnd { stream } => {
                        self.regions.retain(|(id, _)| *id != stream);
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
    }

    /// Copy `rgb`, packed for `rect`, into a picture as wide as the page.
    fn paint(picture: &mut [u8], rect: Rect, rgb: &[u8]) {
        let stride = usize::from(W) * 3;
        for row in 0..usize::from(rect.h()) {
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 3;
            let width = usize::from(rect.w()) * 3;
            picture[at..at + width].copy_from_slice(&rgb[row * width..(row + 1) * width]);
        }
    }

    /// Take and encode a round at `now`, if one is owed, onto `page`.
    fn round(stream: &mut MotionStream, page: &mut Page, now: Instant) -> bool {
        let Some(mut round) = stream.take_round(now, false).unwrap() else { return false };
        let records = round.encode().unwrap();
        stream.put_back(round, now);
        page.take(records);
        true
    }

    /// Rounds until nothing is owed, the clock run forward to each due time.
    fn settle(stream: &mut MotionStream, page: &mut Page, mut now: Instant) -> Instant {
        let limit = now + Duration::from_secs(20);
        while let Some(due) = stream.next_due(now) {
            now = due.max(now);
            assert!(round(stream, page, now), "a due time with nothing to send");
            assert!(now < limit, "it settles");
        }
        now
    }

    fn opened(at: Instant, debug: bool) -> (MotionStream, Page, Vec<u8>) {
        let mut stream = MotionStream::new(60, false, debug);
        stream.want(W, H);
        let picture = pattern(W, H, 7);
        stream.blit(whole(), &picture, at).unwrap();
        let mut page = Page::new();
        while round(&mut stream, &mut page, at) {}
        (stream, page, picture)
    }

    /// Play `moving` changing every 40 ms for two seconds, and say whether a region
    /// carried it.
    fn play(
        stream: &mut MotionStream,
        page: &mut Page,
        picture: &mut [u8],
        moving: Rect,
        mut now: Instant,
    ) -> (bool, Instant) {
        let mut seen = false;
        for frame in 0..50u8 {
            now += Duration::from_millis(40);
            let rgb = pattern(moving.w(), moving.h(), frame);
            stream.blit(moving, &rgb, now).unwrap();
            paint(picture, moving, &rgb);
            round(stream, page, now);
            seen |= page.regions.iter().any(|(_, region)| region.contains(&moving));
        }
        (seen, now)
    }

    #[test]
    fn a_still_desktop_reaches_the_page_exactly() {
        let at = Instant::now();
        let (stream, page, picture) = opened(at, false);
        assert!(page.started);
        assert_eq!(page.rgb, picture, "every tile, exactly");
        assert!(!stream.dirty(at + Duration::from_secs(5)), "and nothing more is owed");
        assert_eq!(stream.next_due(at + Duration::from_secs(5)), None);
    }

    #[test]
    fn what_moves_goes_as_a_region_and_is_sent_exact_once_still() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture) = opened(at, false);
        // Three by two tiles' worth changes every 40 ms for two seconds, from
        // (100, 70): four by three tiles.
        let moving = Rect::from_size(100, 70, 3 * TILE, 2 * TILE).unwrap();
        let (seen, now) = play(&mut stream, &mut page, &mut picture, moving, at);
        assert!(seen, "the moving tiles became a region");
        // Still from here: the region ends, and its tiles are sent once still.
        settle(&mut stream, &mut page, now);
        assert!(page.regions.is_empty(), "the region ended");
        assert_eq!(page.rgb, picture, "and the page holds the picture exactly");
    }

    /// A spinner — a tile, however it straddles the lines — is not a video, however
    /// long it spins.
    #[test]
    fn a_spinner_is_never_a_region() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture) = opened(at, false);
        let spinner = Rect::from_size(200, 100, TILE, TILE).unwrap();
        let (seen, now) = play(&mut stream, &mut page, &mut picture, spinner, at);
        assert!(!seen && page.regions.is_empty(), "a spinner took a stream");
        settle(&mut stream, &mut page, now);
        assert_eq!(page.rgb, picture, "every turn of it went as PNG");
    }

    #[test]
    fn a_change_that_does_not_keep_changing_is_never_a_region() {
        let at = Instant::now();
        let (mut stream, mut page, _) = opened(at, false);
        let caret = Rect::from_size(300, 200, 2, 16).unwrap();
        let mut now = at;
        for blink in 0..20u8 {
            now += Duration::from_millis(500);
            stream.blit(caret, &pattern(2, 16, blink + 1), now).unwrap();
            round(&mut stream, &mut page, now);
            assert!(page.regions.is_empty(), "a blinking caret is a PNG each time");
        }
    }

    /// A caret's PNG is the caret, not the tile it is in: what is sent at once is cut
    /// to what changed.
    #[test]
    fn a_change_is_sent_cut_to_what_changed() {
        let at = Instant::now();
        let (mut stream, _, _) = opened(at, false);
        let caret = Rect::from_size(300, 200, 2, 16).unwrap();
        stream.blit(caret, &pattern(2, 16, 1), at).unwrap();
        let mut round = stream.take_round(at, false).unwrap().expect("the caret is owed");
        let records = round.encode().unwrap();
        let tiles: Vec<(u16, u16, u16, u16)> = records
            .iter()
            .filter_map(|r| match r {
                ServerMsg::Tile(t) => Some((t.x, t.y, t.w, t.h)),
                _ => None,
            })
            .collect();
        assert_eq!(tiles, vec![(300, 200, 2, 16)]);
    }

    /// A rectangle drawn again with the pixels it already had changes nothing: it
    /// sends nothing, and it does not count towards motion.
    #[test]
    fn a_redraw_of_the_same_pixels_is_not_a_change() {
        let at = Instant::now();
        let (mut stream, mut page, picture) = opened(at, false);
        let mut now = at;
        for _ in 0..30 {
            now += Duration::from_millis(40);
            stream.blit(whole(), &picture, now).unwrap();
            assert!(!round(&mut stream, &mut page, now), "an unchanged redraw was sent");
        }
        assert!(stream.churn.moving(now).is_empty(), "an unchanged redraw counted as motion");
    }

    #[test]
    fn a_restart_sends_the_whole_picture_again() {
        let at = Instant::now();
        let (mut stream, _, picture) = opened(at, false);
        stream.force_keyframe();
        let mut page = Page::new();
        while round(&mut stream, &mut page, at) {}
        assert_eq!(page.rgb, picture);
    }

    /// A restart while a region is live ends it ahead of the `tilesStart`, so the
    /// page hands its decoder back, and drops every history the churn was counted
    /// from.
    #[test]
    fn a_restart_ends_the_regions_and_forgets_the_churn() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture) = opened(at, false);
        let moving = Rect::from_size(0, 0, 5 * TILE, TILE).unwrap();
        let (seen, now) = play(&mut stream, &mut page, &mut picture, moving, at);
        assert!(seen);
        stream.force_keyframe();
        // The test page asserts no region outlives the `tilesStart`.
        assert!(round(&mut stream, &mut page, now));
        assert!(stream.churn.churn.is_empty() && stream.churn.origin.is_none());
        settle(&mut stream, &mut page, now);
        assert_eq!(page.rgb, picture);
    }

    /// A page that is behind on an adaptive plan holds the cleanups back, but not
    /// for good.
    #[test]
    fn a_cleanup_is_held_for_a_page_behind_but_not_for_good() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture) = opened(at, false);
        let moving = Rect::from_size(0, 0, 5 * TILE, TILE).unwrap();
        let (seen, mut now) = play(&mut stream, &mut page, &mut picture, moving, at);
        assert!(seen);
        let stopped = now;
        let mut cleaned = None;
        while let Some(due) = stream.next_due(now) {
            now = due.max(now);
            let Some(mut round) = stream.take_round(now, true).unwrap() else { continue };
            let records = round.encode().unwrap();
            stream.put_back(round, now);
            if records.iter().any(|r| matches!(r, ServerMsg::Tile(_))) {
                cleaned.get_or_insert(now);
            }
            page.take(records);
            assert!(now < stopped + Duration::from_secs(20), "it settles");
        }
        let cleaned = cleaned.expect("the cleanups went out in the end");
        assert!(cleaned >= stopped + CLEANUP_HELD, "a cleanup went out to a page {:?} behind", cleaned - stopped);
        assert_eq!(page.rgb, picture);
    }

    /// The debug outlines go on the copy sent, never on the mirror: a cleanup sends
    /// the picture as it is, and only its own outline is on it.
    #[test]
    fn a_debug_outline_is_on_what_is_sent_and_never_on_the_mirror() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture) = opened(at, true);
        let moving = Rect::from_size(0, 0, 5 * TILE, TILE).unwrap();
        let (seen, now) = play(&mut stream, &mut page, &mut picture, moving, at);
        assert!(seen);
        settle(&mut stream, &mut page, now);
        let stride = usize::from(W) * 3;
        // The cleanup's corner is green, and the pixel inside its border is the
        // picture's.
        assert_eq!(page.rgb[..3], MARK_CLEANUP, "the cleanup is outlined");
        let inside = 3 * stride + 3 * 3;
        assert_eq!(page.rgb[inside..inside + 3], picture[inside..inside + 3], "the outline went past its border");
        let mut rgb = Vec::new();
        stream.regions.crop(moving, &mut rgb).unwrap();
        assert_eq!(rgb[..3], picture[..3], "the outline was painted on the mirror");
    }

    /// A slot's worth of instants, for driving the churn by hand.
    fn slots(base: Instant, n: u64) -> Instant {
        base + CHURN_SLOT * u32::try_from(n).unwrap()
    }

    #[test]
    fn churn_counts_recent_slots_and_ages_out() {
        let base = Instant::now();
        let mut churn = Churn::default();
        let key = (0, 0);

        // One change per slot for a full window: every bit set.
        for slot in 0..CHURN_WINDOW {
            churn.observe(key, slots(base, slot));
        }
        assert_eq!(
            u64::from(churn.observe(key, slots(base, CHURN_WINDOW))),
            CHURN_WINDOW,
            "a tile changing every slot should have a full history"
        );

        // A gap of more than the window empties it: the next change is a first change.
        assert_eq!(churn.observe(key, slots(base, CHURN_WINDOW * 3)), 1, "history survived a gap longer than the window");
    }

    /// Several changes inside one slot are one slot's worth of churn, not several.
    /// A remote that reports damage in ten small rectangles is not ten times as
    /// busy as one that reports it in one.
    #[test]
    fn changes_inside_one_slot_count_once() {
        let base = Instant::now();
        let mut churn = Churn::default();
        let key = (0, 0);
        for _ in 0..10 {
            churn.observe(key, base);
        }
        assert_eq!(churn.observe(key, base), 1, "one slot counted more than once");
    }
}
