//! Which parts of the framebuffer get an H.264 stream, when one starts and stops,
//! and what a client is owed when one does.
//!
//! Two dials arrive here and they differ only in how many rectangles they ask for.
//! `render_type = "video"` asks for one covering the whole desktop and never changes
//! its mind ([`Policy::Whole`]). `render_motion_subtype = "h264"` asks for one per
//! coalesced moving region, with the still codecs carrying everything else
//! ([`Policy::Moving`]). [`crate::h264`] knows how to encode a rectangle; this module
//! is every decision about *which*.
//!
//! Three things hold the whole design up:
//!
//! - **One mirror, several encoders.** [`crate::h264::Mirror`] holds the exact
//!   current source for every pixel, moving or not, and each stream encodes a crop
//!   of it. That is what lets a stream start in the middle of a session — its
//!   region's pixels are already there — and it is why the debts below hold no
//!   pixels at all.
//! - **A debt is a cell key, not a picture.** A cell whose only delivery was through
//!   a lossy stream is owed a crisp re-send, because
//!   [`crate::tiles::Shadow::accept`] already recorded those pixels as delivered and
//!   nothing else will send them again. The still `motion` path has to *remember the
//!   pixels* it approximated, with all the staleness that implies; here the cleanup
//!   crops the mirror and is the newest truth by construction.
//! - **Live regions are pairwise disjoint, and a cell in one is never also a tile.**
//!   Two deliveries of one cell in one frame is how a debt gets discharged by pixels
//!   that did not discharge it.
//!
//! Geometry moves at most once per [`RETUNE`], so a stream is not restarted for
//! every twitch: a region that shrinks keeps its stream (the idle margin costs
//! almost nothing to code), and only a region that grows past its stream's rectangle
//! pays for a new encoder and a keyframe.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use crate::h264::{Mark, Mirror, Stream};
use crate::protocol::{CELL_H, CELL_W, VideoUnit, batch};
use crate::tiles::Rect;

/// The most streams one session runs at once.
///
/// Four rather than one per moving region without limit: each is an encoder with its
/// own reference frame and its own decoder at the far end, and a desktop with five
/// genuinely independent moving regions is a desktop where the whole screen is
/// moving — which is what merging produces anyway.
pub const MAX_STREAMS: usize = 4;

/// The wire's `stream` byte has to be able to name every stream this will run.
const _: () = assert!(MAX_STREAMS <= batch::MAX_STREAMS as usize);

/// The least time between two changes of geometry.
///
/// Starting a stream costs an encoder and a keyframe, so geometry that followed the
/// churn map frame by frame would spend more on restarts than the streams save. Half
/// a second is long enough that a window being dragged settles first, and short
/// enough that a video that starts playing is streaming before it is worth watching.
const RETUNE: Duration = Duration::from_millis(500);

/// How long a live region must have nothing moving in it before its stream ends.
///
/// The answer to "what happens to a region that stops moving while its stream is
/// still the truth on screen": the stream ends, and every cell it covered becomes a
/// debt due for a crisp re-send. Long enough that a paused frame of video, or a
/// pointer that stops for a moment, does not tear the stream down and pay for a new
/// keyframe a moment later.
const STREAM_IDLE: Duration = Duration::from_millis(500);

/// The most idle cells a merge may sweep up, as a multiple of the moving cells it
/// joins.
///
/// Merging two regions into one box costs every cell between them: they are streamed
/// lossily, owed a cleanup, and mostly not moving. Past this the merge is refused and
/// the smallest region goes to the still codecs instead — crisp and merely expensive,
/// which is the safe direction. It is the same fault `Changed::cells` exists to
/// prevent, one level up: a banner ad in one corner must not put the screen in a
/// stream because a video is playing in the other.
const MERGE_WASTE: u32 = 2;

/// How many rectangles this target streams, and how they are chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// `render_type = "video"`: one stream over the whole desktop, for the whole
    /// session. No cells, no debts, no cleanup — nothing else is being sent.
    Whole,
    /// `render_motion_subtype = "h264"`: a stream per coalesced moving region, with
    /// the base codec carrying every cell outside one.
    Moving,
}

/// One live stream: its rectangle, the cells it covers, and what it owes.
struct Live {
    /// The `stream` byte on the wire. Reused as regions come and go; a client tells
    /// a reuse from a continuation by the rectangle changing.
    id: u8,
    rect: Rect,
    /// The grid cells inside [`Self::rect`], fixed for the stream's life. What the
    /// debts are keyed by, and what "this cell is in a stream" is answered from.
    cells: Vec<(u16, u16)>,
    stream: Stream,
    /// Whether anything has been blitted into this region since its last access unit.
    dirty: bool,
    /// Whether the next access unit must be one a decoder can start from.
    keyframe_owed: bool,
    /// Whether the round just encoded produced a unit for this stream — which is what
    /// says its cells have just been carried and are not idle.
    carried: bool,
    /// When something inside this region was last seen moving. What [`STREAM_IDLE`]
    /// is measured from.
    moving_at: Instant,
}

/// A rectangle of the cell grid, in cell coordinates. What [`coalesce`] works in,
/// because a region is a union of whole cells by construction — which is what makes
/// [`crate::h264::Stream`]'s evenness a theorem rather than a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellBox {
    c0: u16,
    r0: u16,
    c1: u16,
    r1: u16,
}

impl CellBox {
    fn of(cell: (u16, u16)) -> Self {
        Self { c0: cell.0, r0: cell.1, c1: cell.0, r1: cell.1 }
    }

    /// Cells this box covers, moving or not.
    fn area(&self) -> u32 {
        u32::from(self.c1 - self.c0 + 1) * u32::from(self.r1 - self.r0 + 1)
    }

    fn union(&self, other: &Self) -> Self {
        Self {
            c0: self.c0.min(other.c0),
            r0: self.r0.min(other.r0),
            c1: self.c1.max(other.c1),
            r1: self.r1.max(other.r1),
        }
    }

    /// This box in framebuffer pixels, clipped to a `w`×`h` desktop.
    ///
    /// The clip is the only place a region's size can come out odd, and the only
    /// place it may: a cell column starts at a multiple of [`CELL_W`] and a row at a
    /// multiple of [`CELL_H`], both even, so an interior box is even on both axes and
    /// an edge one is odd exactly where the desktop is.
    fn to_rect(self, w: u16, h: u16) -> Option<Rect> {
        let left = self.c0.checked_mul(CELL_W)?;
        let top = self.r0.checked_mul(CELL_H)?;
        if left >= w || top >= h {
            return None;
        }
        Some(Rect {
            left,
            top,
            right: (self.c1.saturating_mul(CELL_W).saturating_add(CELL_W - 1)).min(w - 1),
            bottom: (self.r1.saturating_mul(CELL_H).saturating_add(CELL_H - 1)).min(h - 1),
        })
    }
}

/// One candidate region: the box it would stream, and how many of the cells inside
/// it are actually moving. The second is what [`MERGE_WASTE`] is judged against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Component {
    bbox: CellBox,
    moving: u32,
}

/// Group moving cells into at most `max` rectangles of the cell grid.
///
/// Connected components first (4-connected, so cells that merely touch at a corner
/// are two regions), then each component's bounding box. If that leaves more boxes
/// than `max`, the cheapest pair — the one whose merged box adds the fewest cells —
/// is merged, and so on. A merge whose box would cover more than [`MERGE_WASTE`]
/// times the cells actually moving inside it is refused; when nothing may be merged
/// and there are still too many, the region with the fewest moving cells is dropped
/// and its cells take the still codecs.
///
/// Pure, and deliberately: which rectangles a churn map deserves is the one decision
/// here that can be argued about entirely on paper.
fn coalesce(cells: &[(u16, u16)], max: usize) -> Vec<CellBox> {
    if cells.is_empty() || max == 0 {
        return Vec::new();
    }
    let remaining: HashSet<(u16, u16)> = cells.iter().copied().collect();
    let mut sorted: Vec<(u16, u16)> = remaining.iter().copied().collect();
    // Sorted so the component order — and so the merge order, and so which region is
    // dropped when nothing may be merged — is the same for the same input.
    sorted.sort_unstable();

    let mut seen: HashSet<(u16, u16)> = HashSet::new();
    let mut components: Vec<Component> = Vec::new();
    for start in sorted {
        if !seen.insert(start) {
            continue;
        }
        let mut bbox = CellBox::of(start);
        let mut moving = 0;
        let mut stack = vec![start];
        while let Some((c, r)) = stack.pop() {
            moving += 1;
            bbox = bbox.union(&CellBox::of((c, r)));
            let neighbours = [
                (c.wrapping_sub(1), r),
                (c + 1, r),
                (c, r.wrapping_sub(1)),
                (c, r + 1),
            ];
            for next in neighbours {
                if remaining.contains(&next) && seen.insert(next) {
                    stack.push(next);
                }
            }
        }
        components.push(Component { bbox, moving });
    }

    while components.len() > max {
        let mut best: Option<(usize, usize, u32)> = None;
        for i in 0..components.len() {
            for j in i + 1..components.len() {
                let merged = components[i].bbox.union(&components[j].bbox);
                let moving = components[i].moving + components[j].moving;
                if merged.area() > MERGE_WASTE * moving {
                    continue;
                }
                let cost = merged.area() - components[i].bbox.area() - components[j].bbox.area();
                if best.is_none_or(|(_, _, at)| cost < at) {
                    best = Some((i, j, cost));
                }
            }
        }
        match best {
            Some((i, j, _)) => {
                let taken = components.remove(j);
                components[i] = Component {
                    bbox: components[i].bbox.union(&taken.bbox),
                    moving: components[i].moving + taken.moving,
                };
            }
            // Nothing may be merged without swallowing a screenful of still pixels,
            // so the smallest region loses its stream rather than the biggest one
            // losing its shape.
            None => {
                let smallest = components
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, c)| (c.moving, c.bbox.area()))
                    .map(|(i, _)| i)
                    .expect("more components than max, so at least one");
                components.remove(smallest);
            }
        }
    }
    components.into_iter().map(|c| c.bbox).collect()
}

/// The mirror, the live streams, and the cells they owe.
pub struct Regions {
    policy: Policy,
    /// The 1–100 dial, straight from the config and never changed.
    quality: u8,
    /// The quantizer new streams start at — the dial's, unless the congestion loop
    /// has moved it. Without this a region that appears on a struggling link would
    /// start at full quality and make the struggle worse.
    qp: u8,
    /// The desktop, learned from [`crate::protocol::ServerMsg::Resize`]. `None` until
    /// the engine has announced one, which it always does before any damage.
    size: Option<(u16, u16)>,
    mirror: Option<Mirror>,
    live: Vec<Live>,
    /// Cells inside a live region, so [`Self::covers`] is a lookup rather than a scan.
    covered: HashSet<(u16, u16)>,
    /// Cell → when an access unit last carried it. A cell in here is one the client
    /// holds only a lossy copy of, and the gateway owes a crisp re-send.
    debts: HashMap<(u16, u16), Instant>,
    retuned_at: Option<Instant>,
    /// The debug outline, painted on the crop handed to the encoder and never on the
    /// mirror.
    mark: Option<Mark>,
}

impl Regions {
    pub fn new(policy: Policy, quality: u8, mark: Option<Mark>) -> Self {
        Self {
            policy,
            quality,
            qp: crate::h264::qp_for(quality),
            size: None,
            mirror: None,
            live: Vec::new(),
            covered: HashSet::new(),
            debts: HashMap::new(),
            retuned_at: None,
            mark,
        }
    }

    /// Adopt the desktop the client is about to be told about.
    ///
    /// Cannot fail, and is all that happens on the message path. A different size
    /// drops everything: every cell key means somewhere else on a new framebuffer,
    /// and an encoder cannot change picture size without starting over anyway. The
    /// debts go with them because the shadow is resized in the same breath — nothing
    /// is owed on a framebuffer that no longer exists.
    pub fn want(&mut self, w: u16, h: u16) {
        if self.size != Some((w, h)) {
            self.size = Some((w, h));
            self.mirror = None;
            self.forget();
        }
    }

    /// Drop every stream and every debt, keeping the mirror.
    ///
    /// For a repaint, a reattach or a takeover: every pixel is about to be re-sent at
    /// the base encode, which discharges anything that was owed, and a client with
    /// nothing on screen cannot decode from the middle of a stream.
    pub fn forget(&mut self) {
        self.live.clear();
        self.covered.clear();
        self.debts.clear();
        self.retuned_at = None;
    }

    /// Whether any stream holds pixels no access unit has carried yet.
    ///
    /// The question `TileSink::due_at` answers for the engines, and the reason a
    /// deferred frame is safe: these pixels are already counted as delivered by the
    /// shadow, so something has to come back for them.
    pub fn dirty(&self) -> bool {
        self.live.iter().any(|live| live.dirty)
    }

    /// Whether this cell is inside a live region — and so is *not* to be sent as a
    /// tile, however it was cut.
    pub fn covers(&self, cell: (u16, u16)) -> bool {
        self.covered.contains(&cell)
    }

    /// The mirror, built if it is not there yet.
    ///
    /// Construction is deferred to the pixel path — rather than done where the size
    /// arrives — because it can fail, and the size arrives inside `TileSink::msg`,
    /// whose error every caller reads as "the browser has gone" and answers by
    /// returning without a word. Every caller of this is on the engines' `?` path,
    /// which ends the session with the message attached.
    fn mirror_mut(&mut self) -> anyhow::Result<&mut Mirror> {
        if self.mirror.is_none() {
            let (w, h) = self
                .size
                .ok_or_else(|| anyhow::anyhow!("the h264 mirror was asked for pixels before a desktop size"))?;
            self.mirror = Some(Mirror::new(w, h)?);
        }
        Ok(self.mirror.as_mut().expect("just built"))
    }

    /// Copy a changed rectangle's source pixels into the mirror.
    ///
    /// Every rectangle, whether or not anything is streaming it: the mirror is what a
    /// cleanup crops and what a stream that starts later reads, and both want the
    /// truth rather than the parts that happened to be moving.
    pub fn blit(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        self.mirror_mut()?.blit(rect, rgb)?;
        // A whole-desktop stream is created here rather than in `retune`, because
        // there is no decision to make: the region is the desktop, and the first
        // pixels to arrive are the ones it exists to carry.
        if self.policy == Policy::Whole && self.live.is_empty() {
            let whole = self.mirror.as_ref().expect("just blitted into it").rect();
            self.start(whole, Instant::now())?;
        }
        for live in &mut self.live {
            if live.rect.intersect(&rect).is_some() {
                live.dirty = true;
            }
        }
        Ok(())
    }

    /// `cell`'s pixels as the mirror holds them — the newest source, which is what
    /// makes a cleanup safe to send without remembering anything.
    pub fn crop(&self, cell: Rect, out: &mut Vec<u8>) -> anyhow::Result<()> {
        let mirror = self
            .mirror
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("a cleanup was asked for before any pixels arrived"))?;
        mirror.crop_into(cell, out)
    }

    /// Choose the regions to stream, given the cells currently in motion.
    ///
    /// A no-op under [`Policy::Whole`], and at most once per [`RETUNE`] otherwise.
    pub fn retune(&mut self, moving: &[(u16, u16)], now: Instant) -> anyhow::Result<()> {
        if self.policy == Policy::Whole {
            return Ok(());
        }
        if self.retuned_at.is_some_and(|at| now.saturating_duration_since(at) < RETUNE) {
            return Ok(());
        }
        self.retuned_at = Some(now);
        let Some((w, h)) = self.size else {
            return Ok(());
        };

        // A region with anything moving in it is a region still in use, whatever the
        // coalescing decides to do about it.
        let busy: HashSet<(u16, u16)> = moving.iter().copied().collect();
        for live in &mut self.live {
            if live.cells.iter().any(|cell| busy.contains(cell)) {
                live.moving_at = now;
            }
        }

        let wanted: Vec<Rect> = coalesce(moving, MAX_STREAMS)
            .into_iter()
            .filter_map(|bbox| bbox.to_rect(w, h))
            .collect();

        let mut old = std::mem::take(&mut self.live);
        let mut next: Vec<Live> = Vec::new();
        for rect in wanted {
            // Shrinking is free: a stream whose rectangle already covers the region
            // keeps going, and the idle margin codes as skipped macroblocks. Growing
            // is not, and pays for a new encoder and a keyframe.
            if let Some(at) = old.iter().position(|live| live.rect.contains(&rect)) {
                let mut live = old.remove(at);
                live.moving_at = now;
                next.push(live);
            } else if next.len() < MAX_STREAMS {
                // Whatever this replaces is simply not carried over, which is what
                // keeps live rectangles disjoint: the new one is built from the
                // mirror, so nothing it swallows is lost.
                old.retain(|live| live.rect.intersect(&rect).is_none());
                next.push(self.build(rect, now)?);
            }
        }
        // A region that has stopped moving keeps its stream for a moment — a video
        // pauses, a pointer comes to rest — and then ends, which is what makes its
        // cells due for a crisp re-send.
        for live in old {
            let idle = now.saturating_duration_since(live.moving_at) >= STREAM_IDLE;
            let overlaps = next.iter().any(|kept| kept.rect.intersect(&live.rect).is_some());
            if !idle && !overlaps && next.len() < MAX_STREAMS {
                next.push(live);
            }
        }

        if self.live.len() != next.len()
            || self.live.iter().zip(&next).any(|(a, b)| a.rect != b.rect)
        {
            log::debug!(
                "h264 regions: {} cell(s) moving -> {}",
                moving.len(),
                if next.is_empty() {
                    "nothing".to_owned()
                } else {
                    next.iter()
                        .map(|l| format!("{}x{} at ({},{})", l.rect.w(), l.rect.h(), l.rect.left, l.rect.top))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
        }
        self.live = next;
        self.covered = self.live.iter().flat_map(|live| live.cells.iter().copied()).collect();
        Ok(())
    }

    /// End every stream whose region has stopped moving, without choosing new ones.
    ///
    /// The cleanup timer's half of the lifetime, and it has to exist separately for
    /// the case the cleanup timer exists for: a screen that has stopped producing
    /// frames produces no [`Self::retune`] either, and a region whose stream never
    /// ended would never come due — the client would keep a lossy rendition of a
    /// paused screen forever.
    ///
    /// Deliberately cannot start a stream, and so cannot fail: starting one builds an
    /// encoder, which belongs on the engine's own task, and a stream appearing between
    /// a `damage` reading [`Self::covers`] and acting on it would be a cell delivered
    /// twice. Ending one the other way round is safe — the cell is simply not sent
    /// this frame, its debt still stands, and the cleanup carries it.
    pub fn expire(&mut self, now: Instant) {
        let before = self.live.len();
        self.live
            .retain(|live| now.saturating_duration_since(live.moving_at) < STREAM_IDLE);
        if self.live.len() != before {
            self.covered = self.live.iter().flat_map(|live| live.cells.iter().copied()).collect();
        }
    }

    /// Start a stream over `rect` and record what it will owe.
    fn start(&mut self, rect: Rect, now: Instant) -> anyhow::Result<()> {
        let live = self.build(rect, now)?;
        self.covered.extend(live.cells.iter().copied());
        self.live.push(live);
        Ok(())
    }

    fn build(&mut self, rect: Rect, now: Instant) -> anyhow::Result<Live> {
        let mirror = self.mirror_mut()?.coded();
        let mut stream = Stream::new(rect, mirror, self.quality)?;
        // A region that appears while the link is behind starts where the link left
        // off, not at the dial.
        stream.set_qp(self.qp)?;
        let cells: Vec<(u16, u16)> = rect.cells().map(|cell| cell.cell_key()).collect();
        // Every cell of the region is owed from the moment it is streamed, including
        // the ones that are not moving: the stream codes them lossily whether they
        // change or not, and nothing else is going to send them.
        if self.policy == Policy::Moving {
            for cell in &cells {
                self.debts.entry(*cell).or_insert(now);
            }
        }
        Ok(Live {
            id: self.free_id(),
            rect,
            cells,
            stream,
            // Its whole region is owed: nothing has carried these pixels yet.
            dirty: true,
            keyframe_owed: true,
            carried: false,
            moving_at: now,
        })
    }

    /// The lowest stream id nothing is using.
    fn free_id(&self) -> u8 {
        (0..u8::try_from(MAX_STREAMS).expect("a small cap"))
            .find(|id| !self.live.iter().any(|live| live.id == *id))
            .unwrap_or(0)
    }

    /// A crisp copy of `sent` has gone out, so nothing is owed for any cell it covers
    /// in full.
    ///
    /// **In full** is the whole rule. Damage is clipped to what changed rather than
    /// snapped out to the grid, so a piece covering part of a cell is the common case;
    /// leaving that cell's debt standing costs one redundant re-send later and is
    /// always correct, where cancelling on a sliver would leave the rest of the cell
    /// lossy with nothing left that knows it is owed.
    pub fn discharge(&mut self, sent: Rect) {
        let Some((w, h)) = self.size else {
            return;
        };
        for piece in sent.cells() {
            let key = piece.cell_key();
            if CellBox::of(key).to_rect(w, h).is_some_and(|cell| sent.contains(&cell)) {
                self.debts.remove(&key);
            }
        }
    }

    /// Take up to `max` cells that have been owed for `idle`, oldest first, for the
    /// caller to re-encode at the base quality.
    ///
    /// A cell a live stream still covers is never due: its stream is carrying it, and
    /// a crisp copy would be overwritten by the next access unit anyway.
    pub fn due(&mut self, now: Instant, idle: Duration, max: usize) -> Vec<Rect> {
        let (w, h) = match self.size {
            Some(size) => size,
            None => return Vec::new(),
        };
        let mut ready: Vec<((u16, u16), Instant)> = self
            .debts
            .iter()
            .filter(|(cell, at)| {
                !self.covered.contains(*cell) && now.saturating_duration_since(**at) >= idle
            })
            .map(|(cell, at)| (*cell, *at))
            .collect();
        ready.sort_unstable_by_key(|(cell, at)| (*at, *cell));
        ready.truncate(max);
        ready
            .into_iter()
            .filter_map(|(cell, _)| {
                self.debts.remove(&cell);
                CellBox::of(cell).to_rect(w, h)
            })
            .collect()
    }

    /// Take the mirror and every stream, for an encode on a blocking worker.
    ///
    /// `None` when no stream has anything waiting, so a still screen costs neither
    /// the hand-off nor the encode. Everything is taken, not just the dirty ones: an
    /// encoder cannot be borrowed across a `spawn_blocking`, and leaving half of them
    /// behind would make [`Self::covers`] answer differently depending on whether a
    /// round is out.
    pub fn take_round(&mut self) -> Option<Round> {
        if !self.dirty() {
            return None;
        }
        Some(Round {
            mirror: self.mirror.take().expect("a dirty stream means a mirror"),
            live: std::mem::take(&mut self.live),
            mark: self.mark,
            skipped: 0,
        })
    }

    /// Put back what [`Self::take_round`] took, and note that the cells those streams
    /// cover have just been carried.
    ///
    /// The debts are refreshed here rather than where the units are pushed, which is
    /// the conservative side of the same argument the still path's stash makes: a
    /// cell whose stream produced a frame this round cannot also be idle, so nothing
    /// can clean it up from underneath a unit still on its way to the socket.
    pub fn put_back(&mut self, round: Round, now: Instant) {
        self.mirror = Some(round.mirror);
        self.live = round.live;
        if self.policy == Policy::Moving {
            for live in &self.live {
                if live.carried {
                    for cell in &live.cells {
                        self.debts.insert(*cell, now);
                    }
                }
            }
        }
    }

    /// Move every live stream's quantizer, for the congestion loop.
    ///
    /// One link, one verdict: the streams share a socket, and a policy that treated
    /// them separately would be reasoning about a bottleneck none of them can see on
    /// its own.
    pub fn set_qp(&mut self, qp: u8) -> anyhow::Result<()> {
        self.qp = qp;
        for live in &mut self.live {
            live.stream.set_qp(qp)?;
        }
        Ok(())
    }

    /// The coarsest quantizer any live stream is encoding at, for the totals.
    pub fn qp(&self) -> u8 {
        self.qp
    }

    /// Arm a keyframe on every live stream. Its callers are exactly the moments a
    /// client's decoder has to be able to start over.
    pub fn force_keyframes(&mut self) {
        for live in &mut self.live {
            live.keyframe_owed = true;
            // Without this a keyframe would wait for the region to change again,
            // which on a paused desktop is never — and a client that just attached
            // would sit in front of nothing.
            live.dirty = true;
        }
    }
}

/// One round of encoding: the mirror and every stream, away from [`Regions`] for the
/// duration of a blocking worker.
///
/// The streams are encoded one after another rather than fanned out across workers.
/// Their rectangles are disjoint and bounded by the desktop, so the round's total
/// work is what the whole-desktop transport already spends on one frame — and doing
/// it in one task keeps the mirror a plain `&`, with no sharing to reason about.
pub struct Round {
    mirror: Mirror,
    live: Vec<Live>,
    mark: Option<Mark>,
    skipped: u64,
}

impl Round {
    /// Encode every stream with pixels waiting. Blocking: call it on a worker.
    ///
    /// A stream the encoder produced no bitstream for keeps its dirty flag and its
    /// keyframe, so those pixels ride the next round — which is what stops a frame
    /// that produced nothing from becoming pixels the client never gets.
    pub fn encode(&mut self) -> anyhow::Result<Vec<VideoUnit>> {
        self.mirror.pad_edges();
        let mut units = Vec::with_capacity(self.live.len());
        for live in &mut self.live {
            live.carried = false;
            if !live.dirty {
                continue;
            }
            if live.keyframe_owed {
                live.stream.force_keyframe();
            }
            let Some(unit) = live.stream.encode(&self.mirror, self.mark)? else {
                self.skipped += 1;
                continue;
            };
            live.dirty = false;
            live.keyframe_owed = false;
            live.carried = true;
            units.push(VideoUnit {
                stream: live.id,
                x: live.rect.left,
                y: live.rect.top,
                w: live.rect.w(),
                h: live.rect.h(),
                keyframe: unit.keyframe,
                data: unit.data,
            });
        }
        Ok(units)
    }

    /// Streams whose encode yielded no bitstream. Must stay zero: `skip_frames(false)`
    /// should make it unreachable, and a non-zero count is how we would find out that
    /// it is not.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cell-grid box, which reads better in tests than the struct literal.
    fn boxed(c0: u16, r0: u16, c1: u16, r1: u16) -> CellBox {
        CellBox { c0, r0, c1, r1 }
    }

    /// Every cell of a rectangular block, which is what a video in a window looks
    /// like to the churn map.
    fn block(c0: u16, r0: u16, c1: u16, r1: u16) -> Vec<(u16, u16)> {
        (c0..=c1).flat_map(|c| (r0..=r1).map(move |r| (c, r))).collect()
    }

    #[test]
    fn a_contiguous_block_is_one_region() {
        let cells = block(1, 2, 3, 5);
        assert_eq!(coalesce(&cells, MAX_STREAMS), vec![boxed(1, 2, 3, 5)]);
    }

    /// The fault this is here to prevent, one level up from `Changed::cells`: two
    /// things moving at opposite ends of the screen are two regions, not one box with
    /// the desktop inside it.
    #[test]
    fn two_separated_blocks_stay_two_regions() {
        let mut cells = block(0, 0, 1, 1);
        cells.extend(block(10, 12, 11, 13));
        let regions = coalesce(&cells, MAX_STREAMS);
        assert_eq!(regions.len(), 2, "{regions:?}");
        assert!(regions.contains(&boxed(0, 0, 1, 1)));
        assert!(regions.contains(&boxed(10, 12, 11, 13)));
    }

    /// Touching at a corner is not touching: 4-connectivity, so a diagonal pair is
    /// two regions rather than one box twice their size.
    #[test]
    fn cells_that_only_touch_at_a_corner_are_two_regions() {
        assert_eq!(coalesce(&[(0, 0), (1, 1)], MAX_STREAMS).len(), 2);
        // Side by side is one, which is the same statement from the other direction.
        assert_eq!(coalesce(&[(0, 0), (1, 0)], MAX_STREAMS), vec![boxed(0, 0, 1, 0)]);
    }

    /// Over the cap, the pair that wastes least is merged — here the two neighbours,
    /// not either of them with the far one.
    #[test]
    fn too_many_regions_merge_the_cheapest_pair() {
        let mut cells = block(0, 0, 1, 3);
        cells.extend(block(3, 0, 4, 3));
        cells.extend(block(20, 0, 21, 3));
        let regions = coalesce(&cells, 2);
        assert_eq!(regions.len(), 2, "{regions:?}");
        assert!(regions.contains(&boxed(0, 0, 4, 3)), "the neighbours did not merge: {regions:?}");
        assert!(regions.contains(&boxed(20, 0, 21, 3)), "the far one was dragged in: {regions:?}");
    }

    /// And when no merge is cheap enough, the smallest region loses its stream rather
    /// than the screen being swallowed. Its cells take the base encode, which is
    /// crisp and merely expensive — the safe direction.
    #[test]
    fn a_merge_that_would_swallow_the_screen_drops_the_smallest_region_instead() {
        let mut cells = block(0, 0, 2, 2);
        cells.push((30, 20));
        let regions = coalesce(&cells, 1);
        assert_eq!(
            regions,
            vec![boxed(0, 0, 2, 2)],
            "one far cell took the whole screen into a stream"
        );
    }

    /// Whatever the cap does, the result is a set of disjoint boxes — which is what
    /// makes "a cell is in at most one stream" true by construction.
    #[test]
    fn regions_never_overlap_each_other() {
        let mut cells = Vec::new();
        for (c, r) in [(0, 0), (5, 1), (6, 9), (12, 3), (13, 3), (2, 7)] {
            cells.extend(block(c, r, c + 1, r + 1));
        }
        for max in 1..=MAX_STREAMS {
            let regions = coalesce(&cells, max);
            assert!(regions.len() <= max);
            for (i, a) in regions.iter().enumerate() {
                for b in &regions[i + 1..] {
                    let overlap = a.c0 <= b.c1 && b.c0 <= a.c1 && a.r0 <= b.r1 && b.r0 <= a.r1;
                    assert!(!overlap, "at max {max}, {a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn nothing_moving_is_no_regions() {
        assert!(coalesce(&[], MAX_STREAMS).is_empty());
    }

    // ---- the live table ------------------------------------------------------
    //
    // Every instant below is made up rather than waited for, so nothing here changes
    // if the machine is twice as slow.

    /// A 640×128 desktop — two cells across, two down — with its mirror already
    /// built, which is what a first blit does.
    async fn regions() -> Regions {
        let mut regions = Regions::new(Policy::Moving, 60, None);
        regions.want(640, 128);
        regions
            .blit(Rect { left: 0, top: 0, right: 639, bottom: 127 }, &vec![0; 640 * 128 * 3])
            .expect("a full-desktop blit");
        regions
    }

    /// The rectangle of one live stream, so a test can say whether geometry moved.
    fn only_rect(regions: &Regions) -> Rect {
        assert_eq!(regions.live.len(), 1, "expected exactly one stream");
        regions.live[0].rect
    }

    #[tokio::test]
    async fn geometry_does_not_move_inside_the_retune_interval() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&[(0, 0)], t0).expect("a stream for the moving cell");
        let first = only_rect(&regions);

        // Twice as much is moving, but not yet.
        regions.retune(&[(0, 0), (1, 0)], t0 + RETUNE / 2).expect("no work");
        assert_eq!(only_rect(&regions), first, "geometry moved inside the interval");
    }

    /// Shrinking is free: the idle margin codes as skipped macroblocks, where a
    /// restart costs an encoder and a keyframe.
    #[tokio::test]
    async fn a_shrinking_region_keeps_its_stream() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&[(0, 0), (1, 0)], t0).expect("a stream");
        let wide = only_rect(&regions);
        assert_eq!(wide.w(), 640);

        regions.retune(&[(0, 0)], t0 + RETUNE).expect("no restart");
        assert_eq!(only_rect(&regions), wide, "a shrinking region paid for a new encoder");
    }

    /// Growing is not free, and must not be: the stream's rectangle is fixed for its
    /// life, so a region that outgrows it needs a new one — and a keyframe.
    #[tokio::test]
    async fn a_growing_region_starts_a_new_stream() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&[(0, 0)], t0).expect("a stream");
        assert_eq!(only_rect(&regions).w(), 320);

        regions.retune(&[(0, 0), (1, 0)], t0 + RETUNE).expect("a wider stream");
        let grown = only_rect(&regions);
        assert_eq!(grown.w(), 640, "the stream kept a rectangle its region outgrew");
        assert!(regions.live[0].keyframe_owed, "a client cannot start on the new picture");
    }

    /// The roadmap's third question, answered: the stream ends, and every cell it
    /// covered is owed a crisp re-send.
    #[tokio::test]
    async fn a_region_that_stops_moving_ends_and_its_cells_come_due() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&[(0, 0)], t0).expect("a stream");
        assert!(regions.covers((0, 0)));
        assert!(
            regions.due(t0 + CLEANUP_IDLE_FOR_TESTS, CLEANUP_IDLE_FOR_TESTS, 8).is_empty(),
            "a cell a live stream is carrying was cleaned up underneath it"
        );

        // Nothing moving, and long enough for the stream to have gone quiet.
        let later = t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS;
        regions.expire(later);
        assert!(regions.live.is_empty(), "a region with nothing moving kept its stream");
        assert!(!regions.covers((0, 0)));
        let due = regions.due(later, CLEANUP_IDLE_FOR_TESTS, 8);
        assert_eq!(due.len(), 1, "the cells it streamed are owed nothing");
        assert_eq!(due[0], Rect { left: 0, top: 0, right: 319, bottom: 63 });
    }

    /// Only in full. Damage is clipped to what changed rather than snapped out to the
    /// grid, so a sliver of a cell says nothing about the rest of it.
    #[tokio::test]
    async fn only_a_whole_cell_discharges_what_that_cell_owes() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&[(0, 0)], t0).expect("a stream");
        regions.expire(t0 + STREAM_IDLE);
        let later = t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS;

        regions.discharge(Rect { left: 0, top: 0, right: 15, bottom: 15 });
        assert_eq!(
            regions.due(later, CLEANUP_IDLE_FOR_TESTS, 8).len(),
            1,
            "a sliver discharged the whole cell's debt"
        );

        // And a send that covers the cell outright does discharge it.
        regions.retune(&[(0, 0)], later).expect("a stream again");
        regions.expire(later + STREAM_IDLE);
        regions.discharge(Rect { left: 0, top: 0, right: 319, bottom: 63 });
        assert!(
            regions
                .due(later + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS, CLEANUP_IDLE_FOR_TESTS, 8)
                .is_empty(),
            "a whole cell went out crisp and is still owed"
        );
    }

    /// Every cell a stream covers is owed, not only the ones that were moving: the
    /// stream codes them lossily whether they change or not, and nothing else is
    /// going to send them.
    #[tokio::test]
    async fn a_still_cell_swept_into_a_region_is_owed_too() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        // Two cells moving with one between them, so the box covers a cell that never
        // moved at all.
        regions.retune(&[(0, 0), (0, 1)], t0).expect("a stream");
        assert_eq!(only_rect(&regions).h(), 128);
        regions.expire(t0 + STREAM_IDLE);
        let due = regions.due(t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS, CLEANUP_IDLE_FOR_TESTS, 8);
        assert_eq!(due.len(), 2, "a cell inside the stream's rectangle was owed nothing");
    }

    /// What `crate::encode` passes as the cleanup's idle threshold. Its own constant
    /// lives there, with the rest of the cleanup policy.
    const CLEANUP_IDLE_FOR_TESTS: Duration = Duration::from_millis(500);

    /// The geometry theorem, from the coalescing side: a box becomes a rectangle
    /// whose origin is on the grid and whose size is odd only where the desktop is.
    #[test]
    fn a_region_is_even_unless_the_desktop_is_odd_at_that_edge() {
        let interior = boxed(1, 1, 2, 2).to_rect(1919, 1079).expect("a rectangle");
        assert_eq!((interior.left, interior.top), (320, 64));
        assert_eq!((interior.w() % 2, interior.h() % 2), (0, 0));

        let edge = boxed(5, 16, 5, 16).to_rect(1919, 1079).expect("a rectangle");
        assert_eq!((edge.right, edge.bottom), (1918, 1078), "clipped to the desktop");
        assert_eq!((edge.w() % 2, edge.h() % 2), (1, 1), "odd exactly where the desktop is");

        // A box entirely off the desktop is not a rectangle at all, which is what a
        // stale churn key after a resize would produce.
        assert!(boxed(40, 0, 40, 0).to_rect(1919, 1079).is_none());
    }
}
