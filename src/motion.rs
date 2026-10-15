//! A picture sent for motion: exact PNG where the desktop is still, and a VP9 stream
//! of its own for each region that moves.
//!
//! The picture of a session started with Motion ([`crate::config::Choices::motion`]):
//! an RDP host's graphics pipeline, composed here. It is the gateway's motion mode
//! of old (`render_motion`, removed with the tile transport in 6c948ad3) brought
//! back as it was — which tiles are moving, which become regions
//! ([`crate::regions`]), what is sent at once, when a region ends and what it then
//! owes — with one difference: what is still goes as an exact PNG, where it went as
//! the target's still codec.
//!
//! A picture that starts over — the session's first, a resize's, a repaint's — is
//! seen first as video and settled after: one stream over the whole desktop carries
//! it ([`crate::regions::Regions::first_sight`]), and the cleanups send it exactly
//! once it has gone out, as they do for a region that stops. Every tile as PNG
//! before the page sees anything was the largest thing the page was ever sent.
//!
//! The work is split as it was between the engine's damage, its frames and a timer
//! of the sink's own ([`crate::encode`]):
//!
//! - **Damage** ([`MotionStream::damage`]) goes into the mirror, counts towards the
//!   churn of the tiles whose pixels it changes, and comes back as the pieces to
//!   send at once as PNG: a band of the report no stream touches whole, and from a
//!   band a stream splits, the runs of changed tiles beside it. A tile a stream
//!   covers is never also sent as a PNG.
//! - **A frame** ([`MotionStream::take_round`]) lays the regions out again from the
//!   churn and encodes the streams with pixels waiting.
//! - **The cleanup tick** ([`MotionStream::cleanups`], every [`CLEANUP_TICK`]) ends
//!   the streams whose motion has stopped and sends, as PNG, the tiles a stream left
//!   lossy once they have been still for [`CLEANUP_IDLE`].
//!
//! Under `render_motion_debug = "motion"` what each decision sent is outlined in the
//! pixels themselves: a region's stream in magenta, a PNG cut from a band a stream
//! splits in cyan, and a cleanup in green. `"tiles"` draws the tile grid under them
//! on everything sent, and tints each tile a report changed that went as PNG, with a
//! pip for every slot of its churn: yellow below [`CHURN_MOVING`], orange at it — a
//! tile in motion that no stream took, which is one or two tiles changing apart from
//! any other.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use anyhow::Context as _;

use crate::config::MotionDebug;
use crate::protocol::{Held, ServerMsg, TileUnit};
use crate::regions::{Regions, Round, tiles_of};
use crate::shadow::Rect;
use crate::video::{Grid, Mark, grid, outline};

pub use crate::regions::TILE;

/// How long one slot of churn is.
///
/// Time rather than frames, because neither engine has a frame worth counting: a
/// busy desktop reports damage far more often than anything presents it, and the
/// same movement reported in ten rectangles is not ten times the movement. A slot
/// means the same thing whoever reports, and "in motion" stays one statement about
/// the remote.
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
pub const CLEANUP_IDLE: Duration = Duration::from_millis(500);

/// How long a cleanup may be held back for a page that is behind, on an adaptive
/// plan — see [`crate::encode`]'s cleanup tick. Five seconds is the walk from a
/// stream's usual dial down to its floor: a link still behind after that is not one
/// the streams can make room on, and a tile left at a stream's quality for good is
/// the worse of the two failures.
pub const CLEANUP_HELD: Duration = Duration::from_secs(5);

/// How often the sink's cleanup tick wakes to look for settled tiles. It has to be
/// its own timer rather than something the next frame does, because a screen that
/// stops changing produces no next frame — which is exactly the case a cleanup is
/// for.
pub const CLEANUP_TICK: Duration = Duration::from_millis(250);

/// Cleanups per tick, so a whole stopped video settles over a few ticks rather
/// than in one burst competing with live motion for the socket. Forty 64-pixel
/// tiles is a 640×256 patch per tick: a stopped 720p player sharpens in about two
/// seconds, and a full 1080p desktop in a little over three.
pub const MAX_CLEANUPS_PER_TICK: usize = 40;

/// The rows a report of damage is cut into before it is sent, as every payload was:
/// a band is sent whole when no stream touches it.
const BAND_ROWS: u16 = 64;

/// Colour of a `render_motion_debug` outline round a region's stream.
///
/// Magenta, cyan and green rather than anything subtler: these survive a VP9 at a
/// low dial, and none of them is a colour a desktop produces in a straight line by
/// accident.
const MARK_MOTION: [u8; 3] = [255, 0, 255];
/// Colour of a `render_motion_debug` outline on a piece sent as PNG from inside a
/// band a stream splits — the still side of the split.
const MARK_CRISP: [u8; 3] = [0, 255, 255];
/// Colour of a `render_motion_debug` outline on a cleanup.
const MARK_CLEANUP: [u8; 3] = [0, 255, 0];
/// Thickness of a `render_motion_debug` outline. Two pixels, because one does not
/// reliably survive chroma subsampling at the quality a moving region is sent at.
const MARK_PX: u16 = 2;
/// The `render_motion_debug = "tiles"` grid: grey, which shows on a light desktop
/// and a dark one alike.
const GRID: Grid = Grid { colour: [128, 128, 128], cell: TILE };
/// Colour of the `"tiles"` mark on a changed tile sent as PNG whose churn is below
/// [`CHURN_MOVING`]: it changed too seldom to be motion.
const MARK_CHURN: [u8; 3] = [255, 255, 0];
/// Colour of the `"tiles"` mark on a changed tile sent as PNG whose churn is at
/// [`CHURN_MOVING`] or past it: in motion, and no stream took it — alone, or one of
/// too few to be worth one.
const MARK_CHURN_MOVING: [u8; 3] = [255, 128, 0];
/// How far a `"tiles"` mark's pips sit in from their tile's corner, past the grid
/// and any outline; their side; and the distance from one to the next. Eight pips
/// fit a tile's top edge.
const PIP_INSET: u16 = 4;
const PIP_PX: u16 = 4;
const PIP_STEP: u16 = 6;

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

    /// Drop everything. A resize changes what every key means, and a repaint
    /// re-sends every pixel the churn was counted from.
    fn clear(&mut self) {
        self.origin = None;
        self.churn.clear();
    }
}

/// A rectangle to send as a PNG, and its pixels as packed RGB888.
pub struct Piece {
    pub rect: Rect,
    pub rgb: Vec<u8>,
}

/// What a report of damage owes the page at once.
pub struct Damage {
    /// A `TilesStart` first: the picture starts over here.
    pub start: bool,
    /// The streams that have ended since the page was last told, said ahead of the
    /// pieces: a first sight ends when its round comes back, and a piece is never
    /// sent inside a stream the page still has live.
    pub ended: Vec<u8>,
    /// The pieces to send as PNG, in order.
    pub pieces: Vec<Piece>,
}

/// What a cleanup tick owes the page: the streams it ended, said first, and the
/// tiles they left lossy, as PNG.
pub struct Cleanups {
    pub ended: Vec<u8>,
    pub pieces: Vec<Piece>,
}

/// The picture, its churn, its regions, and what it owes the page.
pub struct MotionStream {
    regions: Regions,
    churn: Churn,
    /// Tiles whose pixels the page cannot be counted on to hold — every tile, after
    /// a resize or a repaint — and so changed by whatever is next drawn over them,
    /// whatever the mirror holds. The mirror is the newest source, not what the page
    /// was sent, so without this a repaint of the pixels the mirror already has
    /// would send nothing.
    unknown: HashSet<(u16, u16)>,
    /// The page is owed a `TilesStart` ahead of anything else: the picture starts
    /// over.
    start_owed: bool,
    /// The picture that starts over is owed its first sight, a stream over the whole
    /// desktop, started by the first damage that follows.
    first_owed: bool,
    round_out: bool,
    /// What `render_motion_debug` draws of what each decision sent.
    debug: MotionDebug,
}

impl MotionStream {
    pub fn new(quality: u8, native: bool, debug: MotionDebug) -> Self {
        let mark = (debug != MotionDebug::None).then_some(Mark { colour: MARK_MOTION, px: MARK_PX });
        let grid = (debug == MotionDebug::Tiles).then_some(GRID);
        Self {
            regions: Regions::new(quality, native, mark, grid),
            churn: Churn::default(),
            unknown: HashSet::new(),
            start_owed: true,
            first_owed: true,
            round_out: false,
            debug,
        }
    }

    /// Adopt the desktop the page is about to be told about. A different size drops
    /// every stream and every debt ([`Regions::want`]), and starts the picture over.
    pub fn want(&mut self, w: u16, h: u16) {
        if self.regions.size() == Some((w, h)) {
            return;
        }
        self.regions.want(w, h);
        self.forget();
    }

    /// The page holds nothing: a repaint or a reattach. Every stream and debt is
    /// dropped, and the repaint that follows is sent again as the first sight of the
    /// picture, a stream over the whole desktop that its cleanups settle; the churn is
    /// dropped, because a repaint is every tile drawn once, not motion.
    pub fn forget(&mut self) {
        self.regions.forget();
        self.first_owed = true;
        self.churn.clear();
        self.unknown = match self.regions.size().and_then(|(w, h)| Rect::from_size(0, 0, w, h)) {
            Some(whole) => tiles_of(whole).map(|(cell, _)| cell).collect(),
            None => HashSet::new(),
        };
        self.start_owed = true;
    }

    /// Copy a changed rectangle into the mirror, count it against the tiles whose
    /// pixels it changes, and say what to send of it at once.
    ///
    /// Bands, and tiles only where it matters, with the one rule that decides
    /// everything: a tile a live stream covers is **not sent at all**. Its pixels
    /// reach the page through that stream, and sending them as a PNG as well would
    /// discharge a debt the stream has not paid. A band no stream touches is sent
    /// whole — a desktop with nothing moving is sent as it is drawn — and only a band
    /// a stream splits is cut at the grid, into the runs of its changed tiles beside
    /// the stream: a video in a window costs its own tiles and costs the text beside
    /// it nothing.
    ///
    /// Churn is recorded for the tiles that *changed*, not for every tile the
    /// rectangle covers, and whether or not a stream is already carrying them,
    /// because that is what keeps the stream alive.
    ///
    /// What is sent is paid for by [`Self::discharge`], once it is queued.
    pub fn damage(&mut self, rect: Rect, rgb: &[u8], now: Instant) -> anyhow::Result<Damage> {
        let start = std::mem::take(&mut self.start_owed);
        if std::mem::take(&mut self.first_owed) {
            self.regions.first_sight(now)?;
        }
        let mut changed: HashSet<(u16, u16)> =
            self.regions.changed(rect, rgb)?.into_iter().map(|(cell, _)| cell).collect();
        let (w, h) = self.regions.size().expect("a mirror means a size");
        for (cell, part) in tiles_of(rect) {
            if self.unknown.contains(&cell) {
                changed.insert(cell);
                let whole = Rect {
                    left: cell.0 * TILE,
                    top: cell.1 * TILE,
                    right: (cell.0 * TILE + TILE - 1).min(w - 1),
                    bottom: (cell.1 * TILE + TILE - 1).min(h - 1),
                };
                if part == whole {
                    self.unknown.remove(&cell);
                }
            }
        }

        let stride = usize::from(rect.w()) * 3;
        let mut pieces = Vec::new();
        // Each changed tile's churn, for the marks of `"tiles"`, and kept under it
        // alone: this is the path every report of damage takes.
        let tiles = self.debug == MotionDebug::Tiles;
        let mut churned: HashMap<(u16, u16), u32> = HashMap::new();
        for top in (rect.top..=rect.bottom).step_by(usize::from(BAND_ROWS)) {
            let band = Rect { top, bottom: top.saturating_add(BAND_ROWS - 1).min(rect.bottom), ..rect };
            let from = usize::from(band.top - rect.top) * stride;
            let to = (usize::from(band.bottom - rect.top) + 1) * stride;
            let band_rgb = &rgb[from..to];
            // Everything goes into the mirror, moving or not: it is what a cleanup
            // crops and what a stream starting later reads.
            self.regions.blit(band, band_rgb)?;
            let cells: Vec<((u16, u16), Rect)> = tiles_of(band).collect();
            for (cell, _) in &cells {
                if changed.contains(cell) {
                    let churn = self.churn.observe(*cell, now);
                    if tiles {
                        churned.insert(*cell, churn);
                    }
                }
            }
            let split = self.regions.covering() && cells.iter().any(|(cell, _)| self.regions.covers(*cell));
            if !split {
                let mut rgb = band_rgb.to_vec();
                self.draw(band, &mut rgb, None, &churned);
                pieces.push(Piece { rect: band, rgb });
                continue;
            }
            // The still tiles go out as *runs*: along each row of tiles in the band,
            // every stretch of tiles that changed and are not under a stream is one
            // PNG. Changed, because a split band is the one place a report's box is
            // known tile by tile and the box is loose here by construction: a stream
            // in the middle of a window and a scrollbar at its edge put every still
            // tile between them in one box.
            let mut runs: Vec<Rect> = Vec::new();
            for (cell, part) in cells {
                if self.regions.covers(cell) || !changed.contains(&cell) {
                    continue;
                }
                match runs.last_mut() {
                    Some(run) if run.top == part.top && run.right.checked_add(1) == Some(part.left) => {
                        run.right = part.right;
                    }
                    _ => runs.push(part),
                }
            }
            for run in runs {
                pieces.push(self.piece(run, Some(MARK_CRISP), &churned)?);
            }
        }
        Ok(Damage { start, ended: self.regions.drain_ended(), pieces })
    }

    /// The pieces [`Self::damage`] said to send have gone out exactly, so nothing is
    /// owed for any tile they cover in full.
    pub fn discharge(&mut self, sent: impl IntoIterator<Item = Rect>) {
        for rect in sent {
            self.regions.discharge(rect);
        }
    }

    /// `rect` cropped from the mirror, drawn on as [`Self::draw`] says.
    fn piece(&self, rect: Rect, mark: Option<[u8; 3]>, churned: &HashMap<(u16, u16), u32>) -> anyhow::Result<Piece> {
        let mut rgb = Vec::new();
        self.regions.crop(rect, &mut rgb)?;
        self.draw(rect, &mut rgb, mark, churned);
        Ok(Piece { rect, rgb })
    }

    /// Draw what `render_motion_debug` shows on `rgb`, the copy of `rect` about to
    /// go as PNG: under `"tiles"`, on each tile in `churned` — the tiles a report
    /// changed, with their churn — a tint and a pip for every slot of it, and the
    /// grid over them; then, under either, `mark` round the whole.
    fn draw(&self, rect: Rect, rgb: &mut [u8], mark: Option<[u8; 3]>, churned: &HashMap<(u16, u16), u32>) {
        let size = (usize::from(rect.w()), usize::from(rect.h()));
        if self.debug == MotionDebug::Tiles {
            for (cell, part) in tiles_of(rect) {
                if let Some(&churn) = churned.get(&cell) {
                    churn_mark(rgb, rect, cell, part, churn);
                }
            }
            grid(rgb, size, (rect.left, rect.top), GRID);
        }
        if let Some(colour) = mark.filter(|_| self.debug != MotionDebug::None) {
            outline(rgb, size, Mark { colour, px: MARK_PX });
        }
    }

    /// Whether a round is away being encoded.
    pub fn round_out(&self) -> bool {
        self.round_out
    }

    /// Whether any stream holds pixels no access unit has carried yet.
    pub fn dirty(&self) -> bool {
        !self.round_out && self.regions.dirty()
    }

    /// Whether nothing is streaming and nothing is owed: the page holds the picture
    /// exactly.
    pub fn settled(&self) -> bool {
        !self.round_out && !self.regions.covering() && !self.regions.owes()
    }

    /// Whether a stream is carrying any tile, for the sink's tests.
    #[cfg(test)]
    pub(crate) fn covering(&self) -> bool {
        self.regions.covering()
    }

    /// Arm a keyframe on every live stream, and announce each again: the page is
    /// one that has to be able to start over.
    pub fn force_keyframe(&mut self) {
        self.regions.force_keyframes();
    }

    /// Move every stream's dial, for the congestion walk.
    pub fn set_quality(&mut self, quality: u8) {
        self.regions.set_quality(quality);
    }

    pub fn quality(&self) -> u8 {
        self.regions.quality()
    }

    /// One remote frame has ended: lay the regions out again from the churn, and
    /// take the streams with pixels waiting, for an encode on a blocking worker —
    /// with the ends that says, ahead of them. `None` when there is nothing, or a
    /// round is already away.
    ///
    /// Geometry moves here, on the engine's own task, before anything is encoded:
    /// that is what makes the set of streamed tiles [`Self::damage`] reads a stable
    /// thing rather than a race.
    pub fn take_round(&mut self, now: Instant) -> anyhow::Result<Option<MotionRound>> {
        if self.round_out {
            return Ok(None);
        }
        let mut msgs = Vec::new();
        if std::mem::take(&mut self.start_owed) {
            msgs.push(ServerMsg::TilesStart);
        }
        let moving = self.churn.moving(now);
        self.regions.retune(&moving, now)?;
        // Ahead of this round and behind every one before it: the units that used
        // these ids are already queued in front of these, and the region that takes
        // one next is in the round about to be pushed behind them.
        msgs.extend(self.regions.drain_ended().into_iter().map(|stream| ServerMsg::RegionEnd { stream }));
        let streams = self.regions.take_round();
        if msgs.is_empty() && streams.is_none() {
            return Ok(None);
        }
        self.round_out = true;
        Ok(Some(MotionRound { msgs, streams, quality: self.regions.quality() }))
    }

    /// Put back what [`Self::take_round`] took: its streams, which have just carried
    /// their tiles at `now`.
    ///
    /// The first sight ends here once it has carried the whole picture: every tile
    /// has been drawn since the page started over, and nothing arrived for the stream
    /// while it encoded.
    pub fn put_back(&mut self, round: MotionRound, now: Instant) {
        self.round_out = false;
        if let Some(streams) = round.streams {
            self.regions.put_back(streams, now);
        }
        if self.unknown.is_empty() {
            self.regions.end_first_sight();
        }
    }

    /// The cleanup tick: end every stream whose region has stopped moving, and take
    /// up to `max` tiles a stream left lossy that have been still for
    /// [`CLEANUP_IDLE`] — or [`CLEANUP_HELD`] while `behind` — cropped from the
    /// mirror, which is the newest truth by construction.
    ///
    /// A screen that has stopped changing produces no frame, so this is the only
    /// thing that will ever notice its streams have gone quiet — and a stream that
    /// never ends is a region that never comes due.
    ///
    /// A tile that will not crop is dropped with its debt, which is the safe
    /// direction: it keeps the stream's rendition until it changes again.
    pub fn cleanups(&mut self, now: Instant, behind: bool, max: usize) -> Cleanups {
        self.regions.expire(now);
        let ended = self.regions.drain_ended();
        let idle = if behind { CLEANUP_HELD } else { CLEANUP_IDLE };
        let mut pieces = Vec::new();
        for rect in self.regions.due(now, idle, max) {
            match self.piece(rect, Some(MARK_CLEANUP), &HashMap::new()) {
                Ok(piece) => pieces.push(piece),
                Err(e) => log::warn!("motion: dropping a cleanup that would not crop: {e:#}"),
            }
        }
        Cleanups { ended, pieces }
    }
}

/// One round: the ends and the start it says first, and the streams it encodes.
pub struct MotionRound {
    msgs: Vec<ServerMsg>,
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

    /// Encode the round, in the order its records go out: what it says first, then
    /// each stream's configuration ahead of its unit. Blocking: call it on a worker.
    pub fn encode(&mut self) -> anyhow::Result<Vec<ServerMsg>> {
        let mut records = std::mem::take(&mut self.msgs);
        if let Some(streams) = &mut self.streams {
            records.extend(streams.encode()?);
        }
        Ok(records)
    }
}

/// The `"tiles"` mark on `part`, the share of tile `cell` inside `rect`, painted on
/// `rgb`, which holds `rect`: the part tinted a quarter of the way to the mark's
/// colour, and `churn` pips along the tile's top edge, clipped to the part. Yellow
/// below [`CHURN_MOVING`], orange at it.
fn churn_mark(rgb: &mut [u8], rect: Rect, cell: (u16, u16), part: Rect, churn: u32) {
    let colour = if churn >= CHURN_MOVING { MARK_CHURN_MOVING } else { MARK_CHURN };
    let w = usize::from(rect.w());
    let at = |x: u16, y: u16| (usize::from(y - rect.top) * w + usize::from(x - rect.left)) * 3;
    for y in part.top..=part.bottom {
        for x in part.left..=part.right {
            let pixel = &mut rgb[at(x, y)..][..3];
            for (channel, mark) in pixel.iter_mut().zip(colour) {
                *channel = ((u16::from(*channel) * 3 + u16::from(mark)) / 4) as u8;
            }
        }
    }
    let (left, top) = (cell.0 * TILE + PIP_INSET, cell.1 * TILE + PIP_INSET);
    for pip in 0..u16::try_from(churn).unwrap_or(u16::MAX) {
        let pip_left = left + pip * PIP_STEP;
        let square = Rect { left: pip_left, top, right: pip_left + PIP_PX - 1, bottom: top + PIP_PX - 1 };
        let Some(square) = square.intersect(&part) else { continue };
        for y in square.top..=square.bottom {
            for x in square.left..=square.right {
                rgb[at(x, y)..][..3].copy_from_slice(&colour);
            }
        }
    }
}

/// `piece` as a `TILE` record: packed RGB888 as an 8-bit RGB PNG, at `Fastest`, as
/// every PNG tile was.
pub fn encode_tile(piece: &Piece) -> anyhow::Result<TileUnit> {
    let (w, h) = (piece.rect.w(), piece.rect.h());
    let mut png = Vec::with_capacity(piece.rgb.len() / 4 + 256);
    let mut encoder = png::Encoder::new(&mut png, u32::from(w), u32::from(h));
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fastest);
    let mut writer = encoder.write_header().context("a tile's PNG header")?;
    writer.write_image_data(&piece.rgb).context("a tile's PNG")?;
    writer.finish().context("a tile's PNG")?;
    Ok(TileUnit { x: piece.rect.left, y: piece.rect.top, w, h, png, held: Held::default() })
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

    /// Copy `rgb`, packed for `rect`, into a picture as wide as the page.
    fn paint(picture: &mut [u8], rect: Rect, rgb: &[u8]) {
        let stride = usize::from(W) * 3;
        for row in 0..usize::from(rect.h()) {
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 3;
            let width = usize::from(rect.w()) * 3;
            picture[at..at + width].copy_from_slice(&rgb[row * width..(row + 1) * width]);
        }
    }

    /// The page's picture: tiles drawn where they say, read back with an independent
    /// PNG decoder, and the rectangles regions drew, which are not exact.
    struct Page {
        rgb: Vec<u8>,
        regions: Vec<(u8, Rect)>,
        started: bool,
        tiles: Vec<Rect>,
    }

    impl Page {
        fn new() -> Self {
            Self { rgb: vec![0; usize::from(W) * usize::from(H) * 3], regions: Vec::new(), started: false, tiles: Vec::new() }
        }

        fn take(&mut self, record: ServerMsg) {
            match record {
                ServerMsg::TilesStart => self.started = true,
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
                    self.tiles.push(rect);
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
                ServerMsg::RegionEnd { stream } => self.regions.retain(|(id, _)| *id != stream),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// What the sink does with a report of damage: send its pieces in order, then
    /// discharge them.
    fn damage(stream: &mut MotionStream, page: &mut Page, rect: Rect, rgb: &[u8], now: Instant) {
        let damage = stream.damage(rect, rgb, now).unwrap();
        if damage.start {
            page.take(ServerMsg::TilesStart);
        }
        for stream in damage.ended {
            page.take(ServerMsg::RegionEnd { stream });
        }
        for piece in &damage.pieces {
            page.take(ServerMsg::Tile(encode_tile(piece).unwrap()));
        }
        stream.discharge(damage.pieces.iter().map(|piece| piece.rect));
    }

    /// What the sink does at a frame: encode the round, if one is owed.
    fn frame(stream: &mut MotionStream, page: &mut Page, now: Instant) {
        let Some(mut round) = stream.take_round(now).unwrap() else { return };
        for record in round.encode().unwrap() {
            page.take(record);
        }
        stream.put_back(round, now);
    }

    /// What the sink's cleanup tick does.
    fn tick(stream: &mut MotionStream, page: &mut Page, now: Instant, behind: bool) -> usize {
        let cleanups = stream.cleanups(now, behind, MAX_CLEANUPS_PER_TICK);
        for stream in cleanups.ended {
            page.take(ServerMsg::RegionEnd { stream });
        }
        let sent = cleanups.pieces.len();
        for piece in &cleanups.pieces {
            page.take(ServerMsg::Tile(encode_tile(piece).unwrap()));
        }
        sent
    }

    /// Cleanup ticks, and frames while a stream holds pixels, until nothing is owed.
    fn settle(stream: &mut MotionStream, page: &mut Page, mut now: Instant) -> Instant {
        let limit = now + Duration::from_secs(20);
        while !stream.settled() {
            now += CLEANUP_TICK;
            if stream.dirty() {
                frame(stream, page, now);
            }
            tick(stream, page, now, false);
            assert!(now < limit, "it settles");
        }
        now
    }

    /// A stream with its first picture sent, as the first sight and nothing else yet.
    fn opened(at: Instant, debug: MotionDebug) -> (MotionStream, Page, Vec<u8>) {
        let mut stream = MotionStream::new(60, false, debug);
        stream.want(W, H);
        let picture = pattern(W, H, 7);
        let mut page = Page::new();
        damage(&mut stream, &mut page, whole(), &picture, at);
        frame(&mut stream, &mut page, at);
        (stream, page, picture)
    }

    /// A stream whose first picture has settled: the page holds it exactly.
    fn settled(at: Instant, debug: MotionDebug) -> (MotionStream, Page, Vec<u8>, Instant) {
        let (mut stream, mut page, picture) = opened(at, debug);
        let now = settle(&mut stream, &mut page, at);
        page.tiles.clear();
        (stream, page, picture, now)
    }

    /// Play `moving` changing every 40 ms for two seconds, a frame each time and a
    /// cleanup tick every 250 ms, and say whether a region carried it.
    fn play(
        stream: &mut MotionStream,
        page: &mut Page,
        picture: &mut [u8],
        moving: Rect,
        at: Instant,
    ) -> (bool, Instant) {
        let mut seen = false;
        let mut now = at;
        let mut ticked = at;
        for frame_at in 0..50u8 {
            now += Duration::from_millis(40);
            let rgb = pattern(moving.w(), moving.h(), frame_at);
            damage(stream, page, moving, &rgb, now);
            paint(picture, moving, &rgb);
            frame(stream, page, now);
            if now >= ticked + CLEANUP_TICK {
                ticked = now;
                tick(stream, page, now, false);
            }
            seen |= page.regions.iter().any(|(_, region)| region.contains(&moving));
        }
        (seen, now)
    }

    /// The first picture is seen as video: one stream over the whole desktop, and no
    /// PNG at all until it has gone out.
    #[test]
    fn a_first_picture_is_seen_first_as_one_stream_over_the_desktop() {
        let at = Instant::now();
        let (stream, page, _) = opened(at, MotionDebug::None);
        assert!(page.started);
        assert!(page.tiles.is_empty(), "a tile went out ahead of the first sight");
        assert_eq!(page.regions.len(), 1);
        assert_eq!(page.regions[0].1, whole(), "the first sight is the whole desktop");
        assert!(!stream.covering(), "the first sight ends once it has carried the picture");
    }

    #[test]
    fn a_still_desktop_reaches_the_page_exactly() {
        let at = Instant::now();
        let (stream, page, picture, _) = settled(at, MotionDebug::None);
        assert!(page.regions.is_empty(), "the first sight ended");
        assert_eq!(page.rgb, picture, "every tile, exactly");
        assert!(stream.settled() && !stream.dirty(), "and nothing more is owed");
    }

    /// A first picture that arrives in parts is carried by the first sight until the
    /// last part has gone out, and settles exactly after.
    #[test]
    fn a_first_picture_in_parts_is_carried_until_every_tile_has_arrived() {
        let at = Instant::now();
        let mut stream = MotionStream::new(60, false, MotionDebug::None);
        stream.want(W, H);
        let picture = pattern(W, H, 7);
        let mut page = Page::new();
        let top = Rect { bottom: 127, ..whole() };
        let rest = Rect { top: 128, ..whole() };
        let stride = usize::from(W) * 3;
        damage(&mut stream, &mut page, top, &picture[..128 * stride], at);
        frame(&mut stream, &mut page, at);
        assert!(stream.covering(), "the first sight ended with tiles not yet drawn");
        let now = at + Duration::from_millis(40);
        damage(&mut stream, &mut page, rest, &picture[128 * stride..], now);
        frame(&mut stream, &mut page, now);
        assert!(page.tiles.is_empty(), "a part of the first picture went out as PNG");
        assert!(!stream.covering(), "the first sight outlived the picture it carries");
        settle(&mut stream, &mut page, now);
        assert_eq!(page.rgb, picture);
    }

    /// A first sight the remote never finishes painting ends anyway, and what it
    /// carried settles exactly.
    #[test]
    fn a_first_sight_does_not_outlast_the_stream_idle() {
        let at = Instant::now();
        let mut stream = MotionStream::new(60, false, MotionDebug::None);
        stream.want(W, H);
        let picture = pattern(W, H, 7);
        let mut page = Page::new();
        let top = Rect { bottom: 127, ..whole() };
        damage(&mut stream, &mut page, top, &picture[..128 * usize::from(W) * 3], at);
        frame(&mut stream, &mut page, at);
        assert!(stream.covering());
        settle(&mut stream, &mut page, at);
        let stride = usize::from(W) * 3;
        assert_eq!(page.rgb[..128 * stride], picture[..128 * stride]);
    }

    /// A band no stream touches is sent whole, as it is drawn: the still majority of
    /// a desktop costs what it would with nothing moving at all.
    #[test]
    fn a_band_no_stream_touches_is_sent_whole() {
        let at = Instant::now();
        let (mut stream, mut page, _, now) = settled(at, MotionDebug::None);
        damage(&mut stream, &mut page, whole(), &pattern(W, H, 8), now);
        let bands: Vec<Rect> = (0..H).step_by(64).map(|top| Rect { top, bottom: (top + 63).min(H - 1), ..whole() }).collect();
        assert_eq!(page.tiles, bands);
    }

    #[test]
    fn what_moves_goes_as_a_region_and_is_sent_exact_once_still() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture, at) = settled(at, MotionDebug::None);
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

    /// A band a stream splits is cut at the grid, and only the tiles beside the
    /// stream that changed go as PNG.
    #[test]
    fn a_band_a_stream_splits_sends_only_its_changed_tiles_beside_it() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture, at) = settled(at, MotionDebug::None);
        let moving = Rect::from_size(0, 0, 5 * TILE, TILE).unwrap();
        let (seen, now) = play(&mut stream, &mut page, &mut picture, moving, at);
        assert!(seen);
        // One report across the top row: the stream's tiles and a caret two tiles
        // past it.
        page.tiles.clear();
        let report = Rect::from_size(0, 0, 8 * TILE, TILE).unwrap();
        let mut rgb = Vec::new();
        stream.regions.crop(report, &mut rgb).unwrap();
        let stride = usize::from(report.w()) * 3;
        rgb[10 * stride + (7 * usize::from(TILE) + 3) * 3] ^= 0xff;
        paint(&mut picture, report, &rgb);
        damage(&mut stream, &mut page, report, &rgb, now);
        assert_eq!(page.tiles, vec![Rect::from_size(7 * TILE, 0, TILE, TILE).unwrap()], "only the changed tile beside it");
    }

    /// A spinner — a tile, however it straddles the lines — is not a video, however
    /// long it spins.
    #[test]
    fn a_spinner_is_never_a_region() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture, at) = settled(at, MotionDebug::None);
        let spinner = Rect::from_size(200, 100, TILE, TILE).unwrap();
        let (seen, now) = play(&mut stream, &mut page, &mut picture, spinner, at);
        assert!(!seen && page.regions.is_empty(), "a spinner took a stream");
        settle(&mut stream, &mut page, now);
        assert_eq!(page.rgb, picture, "every turn of it went as PNG");
    }

    #[test]
    fn a_change_that_does_not_keep_changing_is_never_a_region() {
        let at = Instant::now();
        let (mut stream, mut page, _, at) = settled(at, MotionDebug::None);
        let caret = Rect::from_size(300, 200, 2, 16).unwrap();
        let mut now = at;
        for blink in 0..20u8 {
            now += Duration::from_millis(500);
            damage(&mut stream, &mut page, caret, &pattern(2, 16, blink + 1), now);
            frame(&mut stream, &mut page, now);
            assert!(page.regions.is_empty(), "a blinking caret is a PNG each time");
        }
    }

    /// A rectangle drawn again with the pixels it already had does not count
    /// towards motion.
    #[test]
    fn a_redraw_of_the_same_pixels_is_not_motion() {
        let at = Instant::now();
        let (mut stream, mut page, picture, at) = settled(at, MotionDebug::None);
        let mut now = at;
        for _ in 0..30 {
            now += Duration::from_millis(40);
            damage(&mut stream, &mut page, whole(), &picture, now);
        }
        assert!(stream.churn.moving(now).is_empty(), "an unchanged redraw counted as motion");
    }

    /// A repaint for a page that holds nothing sends the whole picture again, though
    /// the mirror already holds every pixel of it, after a `tilesStart`: first seen
    /// as video, and then exactly.
    #[test]
    fn a_repaint_after_a_forget_sends_the_whole_picture_again() {
        let at = Instant::now();
        let (mut stream, _, picture, now) = settled(at, MotionDebug::None);
        stream.forget();
        let mut page = Page::new();
        damage(&mut stream, &mut page, whole(), &picture, now);
        frame(&mut stream, &mut page, now);
        assert!(page.started);
        assert_eq!(page.regions.iter().map(|(_, rect)| *rect).collect::<Vec<_>>(), vec![whole()]);
        settle(&mut stream, &mut page, now);
        assert_eq!(page.rgb, picture);
    }

    /// A reset drops every history the churn was counted from and every stream: the
    /// page that starts over is sent one first sight of the whole desktop, on an id
    /// of its own, after the ends of the streams it replaces.
    #[test]
    fn a_reset_drops_every_history_and_every_stream() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture, now) = settled(at, MotionDebug::None);
        let moving = Rect::from_size(0, 0, 5 * TILE, TILE).unwrap();
        let (seen, now) = play(&mut stream, &mut page, &mut picture, moving, now);
        assert!(seen);
        let streamed: Vec<u8> = page.regions.iter().map(|(id, _)| *id).collect();
        stream.forget();
        stream.force_keyframe();
        assert!(stream.churn.churn.is_empty() && stream.churn.origin.is_none());
        let mut fresh = Page::new();
        let repaint = stream.damage(whole(), &picture, now).unwrap();
        assert!(repaint.start && repaint.pieces.is_empty(), "a repaint went out as PNG");
        assert_eq!(repaint.ended, streamed, "the streams the reset dropped were ended");
        fresh.take(ServerMsg::TilesStart);
        frame(&mut stream, &mut fresh, now);
        assert_eq!(fresh.regions.len(), 1);
        assert_eq!(fresh.regions[0].1, whole(), "the picture started over as one first sight");
        assert!(!streamed.contains(&fresh.regions[0].0), "the first sight took an id still being ended");
        settle(&mut stream, &mut fresh, now);
        assert_eq!(fresh.rgb, picture);
    }

    /// A page that is behind on an adaptive plan holds the cleanups back, but not
    /// for good.
    #[test]
    fn a_cleanup_is_held_for_a_page_behind_but_not_for_good() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture, at) = settled(at, MotionDebug::None);
        let moving = Rect::from_size(0, 0, 5 * TILE, TILE).unwrap();
        let (seen, mut now) = play(&mut stream, &mut page, &mut picture, moving, at);
        assert!(seen);
        let stopped = now;
        let cleaned = loop {
            now += CLEANUP_TICK;
            if tick(&mut stream, &mut page, now, true) > 0 {
                break now;
            }
            assert!(now < stopped + Duration::from_secs(20), "it settles");
        };
        assert!(cleaned >= stopped + CLEANUP_HELD, "a cleanup went out to a page {:?} behind", cleaned - stopped);
        settle(&mut stream, &mut page, now);
        assert_eq!(page.rgb, picture);
    }

    /// The debug outlines go on the copy sent, never on the mirror: a cleanup sends
    /// the picture as it is, and only its own outline is on it.
    #[test]
    fn a_debug_outline_is_on_what_is_sent_and_never_on_the_mirror() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture, at) = settled(at, MotionDebug::Motion);
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

    /// Under `"tiles"` a tile changed alone goes out on the grid, tinted, with a pip
    /// for its churn — yellow while it is below motion, orange once a spinner keeps it
    /// changing with no stream taking it — and none of it reaches the mirror.
    #[test]
    fn the_tiles_marks_show_the_grid_and_the_churn_below_motion() {
        let at = Instant::now();
        let (mut stream, mut page, mut picture, settled_at) = settled(at, MotionDebug::Tiles);
        // Past the churn the first picture left.
        let at = settled_at + CHURN_SLOT * u32::try_from(CHURN_WINDOW).unwrap();
        let stride = usize::from(W) * 3;
        let pixel = |rgb: &[u8], x: u16, y: u16| -> [u8; 3] {
            rgb[usize::from(y) * stride + usize::from(x) * 3..][..3].try_into().unwrap()
        };

        let tile = Rect::from_size(2 * TILE, TILE, TILE, TILE).unwrap();
        let rgb = pattern(TILE, TILE, 9);
        damage(&mut stream, &mut page, tile, &rgb, at);
        paint(&mut picture, tile, &rgb);
        assert_eq!(page.tiles, vec![tile], "a tile changed alone goes as PNG");
        assert_eq!(pixel(&page.rgb, tile.left, tile.top + 20), GRID.colour, "the grid is on its left edge");
        assert_eq!(pixel(&page.rgb, tile.left + 20, tile.top), GRID.colour, "and on its top edge");
        let (pip, past) = (tile.left + PIP_INSET, tile.left + PIP_INSET + PIP_STEP);
        assert_eq!(pixel(&page.rgb, pip, tile.top + PIP_INSET), MARK_CHURN, "one pip for one slot of churn");
        assert_ne!(pixel(&page.rgb, past, tile.top + PIP_INSET), MARK_CHURN, "and only one");
        let (x, y) = (tile.left + 40, tile.top + 40);
        let tinted: Vec<u8> =
            pixel(&picture, x, y).iter().zip(MARK_CHURN).map(|(&c, m)| ((u16::from(c) * 3 + u16::from(m)) / 4) as u8).collect();
        assert_eq!(pixel(&page.rgb, x, y)[..], tinted[..], "the tile is tinted");

        let spinner = Rect::from_size(4 * TILE, 3 * TILE, TILE, TILE).unwrap();
        let (seen, _) = play(&mut stream, &mut page, &mut picture, spinner, at);
        assert!(!seen, "a spinner took a stream");
        let moving = pixel(&page.rgb, spinner.left + PIP_INSET + (CHURN_MOVING as u16 - 1) * PIP_STEP, spinner.top + PIP_INSET);
        assert_eq!(moving, MARK_CHURN_MOVING, "a spinner is marked in motion with no stream");

        let mut mirror = Vec::new();
        stream.regions.crop(whole(), &mut mirror).unwrap();
        assert_eq!(mirror, picture, "a mark was painted on the mirror");
    }

    /// The grid falls where the desktop's tiles meet, whatever the corner of the
    /// picture it is painted on.
    #[test]
    fn the_grid_follows_the_desktop_not_the_picture() {
        let (w, h) = (100, 80);
        let mut rgb = vec![0; w * h * 3];
        grid(&mut rgb, (w, h), (TILE - 10, TILE - 30), GRID);
        let lit = |x: usize, y: usize| rgb[(y * w + x) * 3..][..3] == GRID.colour;
        assert!(lit(10, 5) && lit(10, 79), "the column at the desktop's x = TILE");
        assert!(lit(0, 30) && lit(99, 30), "the row at the desktop's y = TILE");
        assert!(!lit(0, 0) && !lit(11, 31) && !lit(9, 29));
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
