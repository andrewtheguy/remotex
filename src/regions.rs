//! Which parts of a picture sent for motion get a video stream, when one starts and
//! stops, and what the page is owed when one does.
//!
//! The gateway's motion mode once, for every decoded picture (`render_motion`,
//! removed with the tile transport in 6c948ad3), brought back for a session started
//! with Motion ([`crate::config::Choices::motion`]): its rules and its tests as they
//! were, with what the page is owed for a tile now an exact PNG
//! ([`crate::motion`]), where it was the target's still codec.
//!
//! A stream per coalesced moving region, with PNG carrying every tile outside one. A
//! codec module knows how to encode a rectangle; this module is every decision about
//! *which*.
//!
//! Three things hold the whole design up:
//!
//! - **One mirror, several encoders.** [`Mirror`] holds the exact current source for
//!   every pixel, moving or not, and each stream encodes a crop of it. That is what
//!   lets a stream start in the middle of a session — its region's pixels are already
//!   there — and it is why the debts below hold no pixels at all.
//! - **A debt is a tile key, not a picture.** A tile whose only delivery was through
//!   a lossy stream is owed an exact re-send, because nothing else will send it
//!   again. The cleanup crops the mirror and is the newest truth by construction.
//! - **Live regions are pairwise disjoint, and a tile in one is never also sent as
//!   a PNG.** Two deliveries of one tile in one frame is how a debt gets discharged
//!   by pixels that did not discharge it.
//!
//! A stream is built over exactly its region's tile bounding box. Nothing outside
//! what [`coalesce`] chose is streamed when a stream starts: the still pixels beside
//! a moving region keep whatever exact tile last painted them, and are owed nothing.
//! The one stream that is not a region is the first sight of a picture that starts
//! over ([`Regions::first_sight`]): the whole desktop, carried once and owed in full.
//!
//! While a stream is live, geometry moves at most once per `RETUNE`, so a stream is
//! not restarted for every twitch: a region that shrinks keeps its stream (the idle
//! margin costs almost nothing to code), and only a region that grows past its
//! stream's rectangle pays for a new encoder and a keyframe. While none is, the
//! first region streams at the frame it qualifies on. A kept stream keeps its whole
//! rectangle, and every tile of it — the margin the region has left behind included
//! — stays covered and owed a cleanup until the stream ends.
//!
//! The grid is [`TILE`] pixels from the desktop's top-left corner, ZRLE's tile, so
//! that a picture an RFB server sends in ZRLE can be cut the same way.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use anyhow::Context as _;

use crate::protocol::{Held, RegionUnit, ServerMsg};
use crate::shadow::Rect;
use crate::video::{AccessUnit, Mark, Mirror};
use crate::vp9::Stream;

/// A tile's side, in the desktop's pixels: ZRLE's.
pub const TILE: u16 = 64;

/// The most streams one session runs at once.
///
/// Four rather than one per moving region without limit: each is an encoder with its
/// own reference frame and its own decoder at the far end, and a desktop with five
/// genuinely independent moving regions is a desktop where the whole screen is
/// moving — which is what merging produces anyway.
pub const MAX_STREAMS: usize = 4;

/// Stream ids the wire's `stream` byte is given. More than [`MAX_STREAMS`], and the
/// difference is what makes a retune safe: a stream being replaced keeps its id
/// until the moment it is dropped, so a retune can have both the outgoing and the
/// incoming set in hand at once — more ids in play than there are streams alive at
/// the end of it.
const STREAM_IDS: u8 = 16;

/// Most rectangles the staged-damage list holds before collapsing to a bounding
/// box — see [`Regions::stage`].
const STAGED_CAP: usize = 32;

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
/// still the truth on screen": the stream ends, and every tile it covered becomes a
/// debt due for an exact re-send. Long enough that a paused frame of video, or a
/// pointer that stops for a moment, does not tear the stream down and pay for a new
/// keyframe a moment later.
///
/// Twice [`RETUNE`], and it has to be well clear of it. A stream's [`Live::moving_at`]
/// is refreshed only by a retune, which runs at the first round past `RETUNE`, and
/// [`Regions::expire`] ends idle streams on a clock of its own. With the two equal,
/// an expiry landing after a retune plus `STREAM_IDLE` and before the next retune
/// ended a stream whose region was still moving, and that retune rebuilt it with a
/// keyframe — every 5–6 s on both of the old mode's tapes, which put a second
/// against half a second at 3% more lossily carried tiles on a paged document and
/// 10% on a video, for half the keyframes.
const STREAM_IDLE: Duration = Duration::from_millis(1000);

/// The most idle tiles a merge may sweep up, as a multiple of the moving tiles it
/// joins.
///
/// Merging two regions into one box costs every tile between them: they are streamed
/// lossily, owed a cleanup, and mostly not moving. Past this the merge is refused and
/// the smallest region goes to PNG instead — exact and merely expensive, which is
/// the safe direction. It is the same fault comparing each tile exactly exists to
/// prevent ([`Regions::changed`]), one level up: a banner ad in one corner must not
/// put the screen in a stream because a video is playing in the other.
const MERGE_WASTE: u32 = 2;

/// The fewest moving tiles a component needs before it is worth a stream.
///
/// Five, so a 2×2 block of the grid — 128×128 pixels, the most anything up to a
/// tile wide can touch however it straddles the lines — is never streamed on its
/// own. What churns in that few tiles is a spinner, a progress bar, a typing
/// indicator, a blinking badge: not a video, and small enough that PNG carries it
/// for a few kilobytes a change, where a stream costs an encoder, a keyframe, a
/// decoder at the far end and a cleanup when it stops — and a spinner stops and
/// starts with every page load, so all of that is spent and torn down again each
/// time. A component this small goes to PNG the way one the cap dropped does, and
/// for the same reason: exact and merely expensive is the safe direction. It is not
/// the same as being out of a stream altogether — a larger component whose box
/// covers its tiles carries them, as any box carries every tile inside it.
const MIN_STREAM_CELLS: u32 = 5;

/// Note that the page is owed an end for `id`, once: an end is said once however
/// many paths notice it before [`Regions::drain_ended`] takes it.
fn record_end(ended: &mut Vec<u8>, id: u8) {
    if !ended.contains(&id) {
        ended.push(id);
    }
}

/// The tiles `rect` touches, each with the part of it inside that tile, row by row.
pub fn tiles_of(rect: Rect) -> impl Iterator<Item = ((u16, u16), Rect)> {
    (rect.top / TILE..=rect.bottom / TILE).flat_map(move |row| {
        (rect.left / TILE..=rect.right / TILE).map(move |col| {
            let tile = Rect {
                left: col * TILE,
                top: row * TILE,
                right: col.saturating_mul(TILE).saturating_add(TILE - 1),
                bottom: row.saturating_mul(TILE).saturating_add(TILE - 1),
            };
            ((col, row), tile.intersect(&rect).expect("a tile the rectangle touches"))
        })
    })
}

/// One live stream: its rectangle, the tiles it covers, and what it owes.
struct Live {
    /// The `stream` byte on the wire. Reused as regions come and go; a page tells a
    /// reuse from a continuation by the rectangle changing.
    id: u8,
    rect: Rect,
    /// The grid tiles inside [`Self::rect`], fixed for the stream's life. What the
    /// debts are keyed by, and what "this tile is in a stream" is answered from.
    cells: Vec<(u16, u16)>,
    stream: Stream,
    /// The crop of the mirror the stream encodes, kept between rounds.
    picture: Mirror,
    /// Whether anything has been blitted into this region since its last access unit.
    dirty: bool,
    /// Whether the next access unit must be one a decoder can start from.
    keyframe_owed: bool,
    /// Whether the round just encoded produced a unit for this stream — which is what
    /// says its tiles have just been carried and are not idle.
    carried: bool,
    /// The configuration string this stream has already announced to the page, if any.
    ///
    /// Cleared by [`Regions::force_keyframes`], which is what makes a fresh page
    /// hear it again: it never saw the original, and its decoder cannot be configured
    /// from the units alone.
    announced: Option<String>,
    /// When something inside this region was last seen moving. What [`STREAM_IDLE`]
    /// is measured from.
    moving_at: Instant,
    /// Whether this is the first sight of a picture that starts over
    /// ([`Regions::first_sight`]) rather than a region that moves: it lasts until it
    /// has carried that picture, never past [`STREAM_IDLE`], and is never kept for
    /// motion inside it.
    first: bool,
}

/// A rectangle of the tile grid, in tile coordinates. What [`coalesce`] works in,
/// because a region is a union of whole tiles by construction.
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

    /// Tiles this box covers, moving or not.
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

    fn overlaps(&self, other: &Self) -> bool {
        self.c0 <= other.c1 && other.c0 <= self.c1 && self.r0 <= other.r1 && other.r0 <= self.r1
    }

    fn contains(&self, (c, r): (u16, u16)) -> bool {
        self.c0 <= c && c <= self.c1 && self.r0 <= r && r <= self.r1
    }

    /// This box in framebuffer pixels, clipped to a `w`×`h` desktop.
    ///
    /// The clip is the only place a region's size can come out odd, and the only
    /// place it may: a tile column starts at a multiple of [`TILE`], so an interior
    /// box is even on both axes and an edge one is odd exactly where the desktop is.
    fn to_rect(self, w: u16, h: u16) -> Option<Rect> {
        let left = self.c0.checked_mul(TILE)?;
        let top = self.r0.checked_mul(TILE)?;
        if left >= w || top >= h {
            return None;
        }
        Some(Rect {
            left,
            top,
            right: (self.c1.saturating_mul(TILE).saturating_add(TILE - 1)).min(w - 1),
            bottom: (self.r1.saturating_mul(TILE).saturating_add(TILE - 1)).min(h - 1),
        })
    }
}

/// One candidate region: the box it would stream, and how many of the tiles inside
/// it are actually moving. The second is what [`MERGE_WASTE`] is judged against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Component {
    bbox: CellBox,
    moving: u32,
}

/// Group moving tiles into at most `max` rectangles of the tile grid.
///
/// Connected components first (4-connected, so tiles that merely touch at a corner
/// are two regions), then each component's bounding box. A component with fewer
/// than [`MIN_STREAM_CELLS`] moving tiles is not a video and is left to PNG before
/// anything else is decided: it neither takes a stream of its own nor counts towards
/// the cap, so a spinner in one corner does not cost the video in the other a merge
/// or a slot. Two components' *tiles* cannot overlap, but their boxes can — an L and
/// a tile tucked into its corner, or two L's interlocked — and a tile inside two live
/// regions is a tile two streams both carry, which is the one delivery rule this
/// module exists to keep. So an overlapping pair is merged into its union, and that
/// merge is judged like any other: a union that adds no tile (one box inside the
/// other) is always taken, and one that would cover more than [`MERGE_WASTE`] times
/// the tiles actually moving inside it is refused, with the component that has fewer
/// moving tiles dropped to PNG instead.
///
/// If that leaves more boxes than `max`, the cheapest pair — the one whose merged
/// box adds the fewest tiles — is merged, and so on, under the same veto; when
/// nothing may be merged and there are still too many, the region with the fewest
/// moving tiles is dropped. Every pass removes a component, which is what makes
/// the loop terminate, and a merge that creates a new overlap is settled on the
/// next pass. The result is pairwise disjoint.
///
/// Pure, and deliberately: which rectangles a churn map deserves is the one decision
/// here that can be argued about entirely on paper — which is what the tests do
/// through this composition. [`Regions::retune`] runs the two halves itself,
/// because it needs the components for a second question.
#[cfg(test)]
fn coalesce(cells: &[(u16, u16)], max: usize) -> Vec<CellBox> {
    merge(components(cells), max)
}

/// The 4-connected components of `cells` worth a stream: each one's bounding box
/// and how many tiles are moving inside it, with every component under
/// [`MIN_STREAM_CELLS`] already left out.
///
/// The first half of [`coalesce`], on its own because [`Regions::retune`] needs the
/// same answer for a second question — whether a live stream still has motion worth
/// a stream inside it — and that question must not be asked of the boxes the cap
/// and the veto have already been at, or a region that lost its stream to the cap
/// this retune would be read as having stopped.
fn components(cells: &[(u16, u16)]) -> Vec<Component> {
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
            let neighbours = [(c.wrapping_sub(1), r), (c + 1, r), (c, r.wrapping_sub(1)), (c, r + 1)];
            for next in neighbours {
                if remaining.contains(&next) && seen.insert(next) {
                    stack.push(next);
                }
            }
        }
        components.push(Component { bbox, moving });
    }
    // Before any merging, so a component too small to stream is also too small to
    // fill a slot or to pull a larger region's box out to meet it. Its tiles inside
    // some larger component's box are carried by that stream whatever is decided
    // later; the rest go as PNG.
    components.retain(|c| c.moving >= MIN_STREAM_CELLS);
    components
}

/// The second half of [`coalesce`]: at most `max` pairwise disjoint boxes from the
/// components, merging and dropping under [`MERGE_WASTE`] as described there.
///
/// Pair decisions live in heaps rather than being rediscovered after every
/// removal. A generation makes an entry stale when one of its components is
/// merged; only its new pairs are then evaluated. The original index is kept as a
/// stable id, so equal-cost pairs and equal-size drops resolve in the same order as
/// the component list. That bounds the work to O(n² log n) and the stored
/// candidates to O(n²).
fn merge(components: Vec<Component>, max: usize) -> Vec<CellBox> {
    if max == 0 {
        return Vec::new();
    }

    // (left id, right id, left generation, right generation). `Reverse` makes the
    // lexicographically first overlap the min of this max-heap.
    let mut overlaps = OverlapQueue::new();
    // Cost comes first, then stable ids, matching a row-major pair scan's
    // first-wins tie break.
    let mut merges = MergeQueue::new();
    let mut components: Vec<Option<Component>> = components.into_iter().map(Some).collect();
    let mut generations = vec![0; components.len()];
    let mut active = components.len();

    for i in 0..components.len() {
        for j in i + 1..components.len() {
            queue_pair(i, j, &components, &generations, &mut overlaps, &mut merges);
        }
    }

    while active != 0 {
        // Overlaps first, because they are forced rather than chosen: the delivery
        // rule leaves no option of keeping both boxes as they are.
        let overlapping = loop {
            let Some(Reverse((i, j, i_generation, j_generation))) = overlaps.pop() else {
                break None;
            };
            if pair_is_current(&components, &generations, i, j, i_generation, j_generation) {
                break Some((i, j));
            }
        };
        if let Some((i, j)) = overlapping {
            let left = components[i].expect("a current pair has two components");
            let right = components[j].expect("a current pair has two components");
            debug_assert!(left.bbox.overlaps(&right.bbox));
            let merged = left.bbox.union(&right.bbox);
            let moving = left.moving + right.moving;
            // A nested pair's union is the larger box itself. Refusing that would
            // drop the inner component to PNG while the larger stream carried its
            // tiles anyway — the delivery rule broken from the other side — so it
            // is always taken.
            let nested = merged == left.bbox || merged == right.bbox;
            if nested || merged.area() <= MERGE_WASTE * moving {
                components[j] = None;
                components[i] = Some(Component { bbox: merged, moving });
                generations[i] += 1;
                active -= 1;
                queue_component(i, &components, &generations, &mut overlaps, &mut merges);
            } else {
                // The union would swallow still tiles outside both boxes, so the
                // smaller component loses its stream instead: its tiles inside the
                // survivor's box are carried by the survivor, and the rest are PNG.
                let loser = if (right.moving, right.bbox.area()) < (left.moving, left.bbox.area()) { j } else { i };
                components[loser] = None;
                active -= 1;
            }
            continue;
        }
        if active <= max {
            break;
        }

        let best = loop {
            let Some(Reverse((_, i, j, i_generation, j_generation))) = merges.pop() else {
                break None;
            };
            if pair_is_current(&components, &generations, i, j, i_generation, j_generation) {
                break Some((i, j));
            }
        };
        match best {
            Some((i, j)) => {
                let left = components[i].expect("a current pair has two components");
                let right = components[j].take().expect("a current pair has two components");
                components[i] =
                    Some(Component { bbox: left.bbox.union(&right.bbox), moving: left.moving + right.moving });
                generations[i] += 1;
                active -= 1;
                queue_component(i, &components, &generations, &mut overlaps, &mut merges);
            }
            // Nothing may be merged without swallowing a screenful of still pixels,
            // so the smallest region loses its stream rather than the biggest one
            // losing its shape.
            None => {
                let smallest = components
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| c.map(|c| (i, c)))
                    .min_by_key(|(_, c)| (c.moving, c.bbox.area()))
                    .map(|(i, _)| i)
                    .expect("more components than max, so at least one");
                components[smallest] = None;
                active -= 1;
            }
        }
    }
    components.into_iter().flatten().map(|c| c.bbox).collect()
}

type OverlapQueue = BinaryHeap<Reverse<(usize, usize, u32, u32)>>;
type MergeQueue = BinaryHeap<Reverse<(u32, usize, usize, u32, u32)>>;

/// Add every pair changed by replacing component `at`.
fn queue_component(
    at: usize,
    components: &[Option<Component>],
    generations: &[u32],
    overlaps: &mut OverlapQueue,
    merges: &mut MergeQueue,
) {
    for other in 0..components.len() {
        if other == at || components[other].is_none() {
            continue;
        }
        queue_pair(at.min(other), at.max(other), components, generations, overlaps, merges);
    }
}

/// Evaluate one pair once, until a merge changes one of its endpoints.
fn queue_pair(
    i: usize,
    j: usize,
    components: &[Option<Component>],
    generations: &[u32],
    overlaps: &mut OverlapQueue,
    merges: &mut MergeQueue,
) {
    let left = components[i].expect("only live components are queued");
    let right = components[j].expect("only live components are queued");
    let i_generation = generations[i];
    let j_generation = generations[j];
    if left.bbox.overlaps(&right.bbox) {
        overlaps.push(Reverse((i, j, i_generation, j_generation)));
        return;
    }

    let merged = left.bbox.union(&right.bbox);
    let moving = left.moving + right.moving;
    if merged.area() <= MERGE_WASTE * moving {
        // The boxes do not overlap, so the union is at least the sum of the parts
        // and the subtraction cannot wrap.
        let cost = merged.area() - left.bbox.area() - right.bbox.area();
        merges.push(Reverse((cost, i, j, i_generation, j_generation)));
    }
}

fn pair_is_current(
    components: &[Option<Component>],
    generations: &[u32],
    i: usize,
    j: usize,
    i_generation: u32,
    j_generation: u32,
) -> bool {
    components[i].is_some() && components[j].is_some() && generations[i] == i_generation && generations[j] == j_generation
}

/// The lowest stream id none of `taken` is using, or `None` when the wire has none
/// left.
fn free_id(taken: &[u8]) -> Option<u8> {
    (0..STREAM_IDS).find(|id| !taken.contains(id))
}

/// The mirror, the live streams, and the tiles they owe.
pub struct Regions {
    /// The 1–100 dial a new stream starts at — the plan's, unless the congestion
    /// walk has moved it. Without that a region appearing on a struggling link would
    /// start at full quality and make the struggle worse.
    quality: u8,
    /// Whether streams are coded by screen-vp9's encoder in Rust
    /// ([`crate::config::RenderPlan::native_vp9`]).
    native: bool,
    /// The desktop, from [`Self::want`]. `None` until the engine has announced one,
    /// which it always does before any damage.
    size: Option<(u16, u16)>,
    mirror: Option<Mirror>,
    /// The mirror's double buffer. While a round is away being encoded it holds that
    /// round's mirror's *twin*: [`Self::take_round`] hands the up-to-date mirror to
    /// the encode and installs this one as current, so blits keep landing while the
    /// encode runs instead of waiting out the whole of it under the lock.
    spare: Option<Mirror>,
    /// Rectangles blitted into `mirror` since `spare` last matched it — what
    /// [`Self::take_round`] copies across before the swap, and what
    /// [`Self::put_back`] replays as dirty marks for damage that arrived while the
    /// live table was away.
    staged: Vec<Rect>,
    /// Whether a round is away on a blocking worker. At most one ever is: taking a
    /// second would hand two encoders one chain of frames.
    round_out: bool,
    /// Bumped whenever everything is dropped ([`Self::forget`]). A returning round
    /// stamped with an older epoch is state for a desktop that no longer exists,
    /// and is discarded rather than restored.
    epoch: u64,
    live: Vec<Live>,
    /// Stream ids whose region has ended and that the page has not been told about
    /// yet — see [`Self::drain_ended`].
    ended: Vec<u8>,
    /// Tiles inside a live region, so [`Self::covers`] is a lookup rather than a scan.
    covered: HashSet<(u16, u16)>,
    /// Tile → when the lossy copy the page holds was sent. A tile in here is one the
    /// gateway owes an exact re-send.
    debts: HashMap<(u16, u16), Instant>,
    retuned_at: Option<Instant>,
    /// The `render_motion_debug` outline, painted on the crop handed to the encoder
    /// and never on the mirror.
    mark: Option<Mark>,
}

impl Regions {
    pub fn new(quality: u8, native: bool, mark: Option<Mark>) -> Self {
        Self {
            quality,
            native,
            size: None,
            mirror: None,
            spare: None,
            staged: Vec::new(),
            round_out: false,
            epoch: 0,
            live: Vec::new(),
            ended: Vec::new(),
            covered: HashSet::new(),
            debts: HashMap::new(),
            retuned_at: None,
            mark,
        }
    }

    /// The desktop, once one is known.
    pub fn size(&self) -> Option<(u16, u16)> {
        self.size
    }

    /// Adopt the desktop the page is about to be told about.
    ///
    /// Cannot fail, and is all that happens on the message path. A different size
    /// drops everything: every tile key means somewhere else on a new framebuffer,
    /// and an encoder cannot change picture size without starting over anyway.
    pub fn want(&mut self, w: u16, h: u16) {
        if self.size != Some((w, h)) {
            self.size = Some((w, h));
            self.mirror = None;
            self.spare = None;
            self.staged.clear();
            self.forget();
        }
    }

    /// Drop every stream and every debt, keeping the mirror.
    ///
    /// For a repaint or a reattach: every pixel is about to be sent again, which
    /// settles anything that was owed, and a page with nothing on screen cannot
    /// decode from the middle of a stream.
    pub fn forget(&mut self) {
        // A round away on a worker carries streams and a mirror this is dropping;
        // the epoch is what tells `put_back` not to bring them back from the dead.
        self.epoch += 1;
        // The streams this drops are ends the page is owed like any other, and the
        // only ones nothing else is ever going to say: an id that was live here and is
        // not handed back afterwards would sit on a decode session for the rest of
        // the session. Recorded here, delivered by the next drain, on the ordered
        // path.
        for live in &self.live {
            record_end(&mut self.ended, live.id);
        }
        self.live.clear();
        self.covered.clear();
        self.debts.clear();
        self.retuned_at = None;
    }

    /// Stream ids whose region is over, taken once.
    ///
    /// A page keys its decoders by id and is otherwise never told that one is
    /// finished: an id simply stops arriving, and the decoder sits there holding
    /// whatever the platform gave it — which for a hardware decoder is a decode
    /// session, a resource there are few of and which the next stream to start will
    /// be asking for. So the end is said, from the side that knows it, rather than
    /// guessed at by a timer on the side that does not.
    ///
    /// **Ordered against the units, or it is worse than nothing.** Ids are reused as
    /// regions come and go, so an end that overtakes the units of the next stream on
    /// that id would close a decoder that had just been built for a picture still
    /// arriving. [`crate::motion`] drains this ahead of the round it takes.
    pub fn drain_ended(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.ended)
    }

    /// Whether any stream holds pixels no access unit has carried yet.
    pub fn dirty(&self) -> bool {
        self.live.iter().any(|live| live.dirty)
    }

    /// Whether this tile is inside a live region — and so is *not* to be sent as a
    /// PNG, however it was cut.
    pub fn covers(&self, cell: (u16, u16)) -> bool {
        self.covered.contains(&cell)
    }

    /// Whether any tile at all is under a stream.
    pub fn covering(&self) -> bool {
        !self.covered.is_empty()
    }

    /// Whether any tile is owed an exact re-send.
    pub fn owes(&self) -> bool {
        !self.debts.is_empty()
    }

    /// The mirror, built if it is not there yet.
    fn mirror_mut(&mut self) -> anyhow::Result<&mut Mirror> {
        if self.mirror.is_none() {
            let (w, h) = self
                .size
                .ok_or_else(|| anyhow::anyhow!("the motion mirror was asked for pixels before a desktop size"))?;
            let mirror = Mirror::new(w, h)?;
            crate::video::check_picture(mirror.size())?;
            self.mirror = Some(mirror);
        }
        Ok(self.mirror.as_mut().expect("just built"))
    }

    /// The tiles `rgb` — packed RGB888 for `rect` — changes, each with the part of
    /// it inside that tile. Asked before the rectangle is blitted.
    ///
    /// A tile the remote redrew with the pixels it already had is not a tile that
    /// changed, and neither is one a rectangle's box merely reached over: a video
    /// at one end and an animated banner at the other used to sweep up every tile
    /// between them, and four reports like that in 800 ms was the whole screen in
    /// motion.
    pub fn changed(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<Vec<((u16, u16), Rect)>> {
        let (w, h) = (usize::from(rect.w()), usize::from(rect.h()));
        anyhow::ensure!(
            rgb.len() == w * h * 3,
            "a motion blit is {} bytes, expected {} for {w}x{h} RGB",
            rgb.len(),
            w * h * 3
        );
        let mirror = self.mirror_mut()?;
        let (dw, dh) = mirror.size();
        anyhow::ensure!(
            rect.right < dw && rect.bottom < dh,
            "a motion blit of {w}x{h} at ({},{}) falls outside a {dw}x{dh} desktop",
            rect.left,
            rect.top
        );
        Ok(tiles_of(rect).filter(|(_, part)| mirror.differs(rect, rgb, *part)).collect())
    }

    /// Copy a changed rectangle's source pixels into the mirror.
    ///
    /// Every rectangle, whether or not anything is streaming it: the mirror is what a
    /// cleanup crops and what a stream that starts later reads, and both want the
    /// truth rather than the parts that happened to be moving.
    pub fn blit(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        self.mirror_mut()?.blit(rect, rgb)?;
        self.stage(rect);
        for live in &mut self.live {
            if live.rect.intersect(&rect).is_some() {
                live.dirty = true;
            }
        }
        Ok(())
    }

    /// Note that `rect` now differs between the current mirror and the spare.
    ///
    /// A rectangle that continues the last one is merged into it rather than pushed.
    /// Exactly, not approximately: same left and right, and starting on the row after
    /// it ends, so the pair covers what the two covered and nothing else. This is what
    /// a caller reporting one rectangle in bands looks like from here, and it means
    /// how a report was cut up on the way in does not decide how much slop the sync
    /// below copies.
    ///
    /// Capped: past [`STAGED_CAP`] the list collapses to one bounding box, so the
    /// sync copies some slop that did not change, where an unbounded list on a
    /// picture that never takes a round would grow forever.
    fn stage(&mut self, rect: Rect) {
        if let Some(last) = self.staged.last_mut()
            && last.left == rect.left
            && last.right == rect.right
            && last.bottom.checked_add(1) == Some(rect.top)
        {
            last.bottom = rect.bottom;
            return;
        }
        if self.staged.len() >= STAGED_CAP {
            let mut whole = rect;
            for r in &self.staged {
                whole.left = whole.left.min(r.left);
                whole.top = whole.top.min(r.top);
                whole.right = whole.right.max(r.right);
                whole.bottom = whole.bottom.max(r.bottom);
            }
            self.staged.clear();
            self.staged.push(whole);
        } else {
            self.staged.push(rect);
        }
    }

    /// Whether a round is away being encoded. While one is, the live table and the
    /// hot mirror are on the worker, and a second round cannot be taken.
    pub fn round_out(&self) -> bool {
        self.round_out
    }

    /// `rect`'s pixels as the mirror holds them — the newest source, which is what
    /// makes a cleanup safe to send without remembering anything.
    pub fn crop(&self, rect: Rect, out: &mut Vec<u8>) -> anyhow::Result<()> {
        let mirror =
            self.mirror.as_ref().ok_or_else(|| anyhow::anyhow!("a tile was asked for before any pixels arrived"))?;
        mirror.crop_into(rect, out)
    }

    /// Choose the regions to stream, given the tiles currently in motion.
    ///
    /// A no-op while a round is out: the live table is away on the worker, and a
    /// stream built into the empty one left behind would be dropped when
    /// [`Self::put_back`] restores it, with the id it took and the debts it recorded.
    /// Otherwise at most once per `RETUNE` while a stream is live — the interval
    /// exists to keep a live stream's geometry from following the churn map frame by
    /// frame — and at every round while none is: with nothing to restart there is
    /// nothing for waiting to save, and every frame of a video that has qualified but
    /// has no stream yet goes out as PNG.
    pub fn retune(&mut self, moving: &[(u16, u16)], now: Instant) -> anyhow::Result<()> {
        if self.round_out {
            return Ok(());
        }
        if !self.live.is_empty() && self.retuned_at.is_some_and(|at| now.saturating_duration_since(at) < RETUNE) {
            return Ok(());
        }
        self.retuned_at = Some(now);
        let Some((w, h)) = self.size else {
            return Ok(());
        };

        // A region with motion worth a stream in it is a region still in use, whatever
        // the cap and the veto decide to do about it. Motion *not* worth a stream is
        // not: what would not start a stream does not keep one alive either, so a
        // video that pauses under a buffering spinner ends its stream and sharpens,
        // exactly as it would with no spinner — and the spinner goes to PNG, where
        // the gate would have put it anyway.
        let worthy = components(moving);
        let busy: HashSet<(u16, u16)> =
            moving.iter().copied().filter(|cell| worthy.iter().any(|c| c.bbox.contains(*cell))).collect();
        for live in &mut self.live {
            if !live.first && live.cells.iter().any(|cell| busy.contains(cell)) {
                live.moving_at = now;
            }
        }

        // The exact boxes, and nothing around them: a stream built here covers only
        // what is moving, and the still tiles beside it are never re-sent lossily. A
        // stream kept from an earlier retune is the exception — it keeps its whole
        // rectangle, margin included, and every tile of it stays covered and owed.
        let wanted: Vec<Rect> =
            merge(worthy, MAX_STREAMS).into_iter().filter_map(|bbox| bbox.to_rect(w, h)).collect();

        // Shrinking is free: a stream whose rectangle already covers the region keeps
        // going, and the idle margin codes as skipped macroblocks. Growing is not, and
        // pays for a new encoder and a keyframe. A kept stream carries *every* region
        // inside it — one rectangle that has split into two keeps one stream, not one
        // stream and a second built inside it — and a stream a region straddles
        // cannot be kept at all, because the stream that region needs would overlap
        // it. So a stream is kept exactly when it touches at least one region and
        // contains each region it touches, which is what keeps the live rectangles
        // pairwise disjoint: a built rectangle is a region no kept stream contains,
        // and so — by that rule — one no kept stream touches.
        let (mut next, mut old): (Vec<Live>, Vec<Live>) =
            std::mem::take(&mut self.live).into_iter().partition(|live| {
                if live.first {
                    return false;
                }
                let mut touched = wanted.iter().filter(|rect| live.rect.intersect(rect).is_some());
                touched.clone().next().is_some() && touched.all(|rect| live.rect.contains(rect))
            });
        for live in &mut next {
            live.moving_at = now;
        }
        for rect in wanted {
            if next.iter().any(|live| live.rect.contains(&rect)) {
                continue;
            }
            if next.len() < MAX_STREAMS {
                // Whatever this replaces is simply not carried over, which is what
                // keeps live rectangles disjoint: the new one is built from the
                // mirror, so nothing it swallows is lost.
                let ended = &mut self.ended;
                old.retain(|live| {
                    let keep = live.rect.intersect(&rect).is_none();
                    if !keep {
                        record_end(ended, live.id);
                    }
                    keep
                });
                // Every id that could still be live when this retune ends: the ones
                // already kept or built, and every survivor of `old`. Avoiding the
                // survivors too is what lets a carried stream keep its own id — and
                // it must keep it, because a page would read a new id as a new
                // region and start a decoder with no history to decode from.
                let taken: Vec<u8> = next.iter().chain(old.iter()).map(|live| live.id).collect();
                next.push(self.build(rect, now, &taken)?);
            }
        }
        debug_assert!(
            next.iter().enumerate().all(|(i, a)| { next[i + 1..].iter().all(|b| a.rect.intersect(&b.rect).is_none()) }),
            "two live streams overlap"
        );
        // A region that has stopped moving keeps its stream for a moment — a video
        // pauses, a pointer comes to rest — and then ends, which is what makes its
        // tiles due for an exact re-send.
        for live in old {
            let idle = now.saturating_duration_since(live.moving_at) >= STREAM_IDLE;
            let overlaps = next.iter().any(|kept| kept.rect.intersect(&live.rect).is_some());
            if !idle && !overlaps && next.len() < MAX_STREAMS {
                debug_assert!(
                    !next.iter().any(|kept| kept.id == live.id),
                    "a carried stream kept an id a new one had taken"
                );
                next.push(live);
            } else {
                self.ended.push(live.id);
            }
        }

        if self.live.len() != next.len() || self.live.iter().zip(&next).any(|(a, b)| a.rect != b.rect) {
            log::debug!(
                "motion regions: {} tile(s) moving -> {}",
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
        // An id this retune both freed and handed straight back is not an end: the
        // region on it is a different one, and it announces a format and a keyframe of
        // its own, but the decoder at the far end is still the decoder for that id —
        // and, where the picture size happens to match, still configured for the
        // picture about to arrive. Telling the page to close it would throw away a
        // hardware decode session it could have kept.
        self.ended.retain(|id| !self.live.iter().any(|live| live.id == *id));
        self.covered = self.live.iter().flat_map(|live| live.cells.iter().copied()).collect();
        Ok(())
    }

    /// End every stream whose region has stopped moving, without choosing new ones.
    ///
    /// It has to exist apart from [`Self::retune`] for the case a cleanup exists
    /// for: a screen that has stopped changing has no motion to retune for, and a
    /// region whose stream never ended would never come due — the page would keep a
    /// lossy rendition of a paused screen forever.
    pub fn expire(&mut self, now: Instant) {
        let before = self.live.len();
        let ended = &mut self.ended;
        self.live.retain(|live| {
            let keep = now.saturating_duration_since(live.moving_at) < STREAM_IDLE;
            if !keep {
                record_end(ended, live.id);
            }
            keep
        });
        if self.live.len() != before {
            self.covered = self.live.iter().flat_map(|live| live.cells.iter().copied()).collect();
        }
    }

    /// Stream the whole desktop: the first sight of a picture the page holds nothing
    /// of, after a [`Self::forget`].
    ///
    /// A picture that starts over is every tile at once, and as PNG that is the
    /// largest thing the page is ever sent, all of it before it sees anything. As one
    /// VP9 keyframe it is a fraction of that, and it is what the page sees first; the
    /// exact picture follows as cleanups, from a tick of their own, as it does for a
    /// region that stops. So the picture is eventually lossless from the first frame,
    /// not lossless first.
    ///
    /// The stream lasts until a round has carried it with nothing more arriving for it
    /// ([`Self::end_first_sight`]), and never past [`STREAM_IDLE`]: a caret that
    /// blinks on, or a remote that never paints part of its desktop, must not keep the
    /// whole of it lossy. Motion found while it lasts ends it, as it does any stream a
    /// region straddles, and builds its own.
    ///
    /// Its id avoids the ends not yet said as well as the live ids, so the end of a
    /// stream [`Self::forget`] dropped still reaches the page.
    pub fn first_sight(&mut self, now: Instant) -> anyhow::Result<()> {
        let Some(rect) = self.size.and_then(|(w, h)| Rect::from_size(0, 0, w, h)) else {
            return Ok(());
        };
        debug_assert!(self.live.is_empty(), "a first sight over live streams");
        let taken: Vec<u8> = self.live.iter().map(|live| live.id).chain(self.ended.iter().copied()).collect();
        let mut live = self.build(rect, now, &taken).context("the first sight of a picture that starts over")?;
        live.first = true;
        self.covered = live.cells.iter().copied().collect();
        self.live.push(live);
        Ok(())
    }

    /// End the first sight once a round has carried it and nothing more has arrived
    /// for it. Its tiles are owed from that round, and come due like any stopped
    /// region's.
    pub fn end_first_sight(&mut self) {
        let before = self.live.len();
        let ended = &mut self.ended;
        self.live.retain(|live| {
            let keep = !live.first || live.dirty;
            if !keep {
                record_end(ended, live.id);
            }
            keep
        });
        if self.live.len() != before {
            self.covered = self.live.iter().flat_map(|live| live.cells.iter().copied()).collect();
        }
    }

    /// Build a stream over `rect`, with an id none of `taken` is using.
    ///
    /// The ids in use are passed in rather than read off `self.live`, because the
    /// caller that matters — [`Self::retune`] — has taken the live table *out* of
    /// `self` for the duration, so a stream built from what is left there would
    /// always be handed id 0. Two live regions sharing an id is not a cosmetic
    /// fault: a page keys its decoders by it, so both chains would be fed to one
    /// decoder and neither would decode.
    fn build(&mut self, rect: Rect, now: Instant, taken: &[u8]) -> anyhow::Result<Live> {
        let id = free_id(taken).ok_or_else(|| {
            anyhow::anyhow!(
                "motion regions: no stream id left for a {}x{} region; {} are in use and the wire allows {}",
                rect.w(),
                rect.h(),
                taken.len(),
                STREAM_IDS
            )
        })?;
        // At `self.quality` rather than the plan's: a region that appears while the
        // link is behind starts where the link left off.
        let stream = Stream::new((rect.w(), rect.h()), self.quality, self.native)?;
        let picture = Mirror::new(rect.w(), rect.h())?;
        let cells: Vec<(u16, u16)> = tiles_of(rect).map(|(cell, _)| cell).collect();
        // Every tile of the region is owed from the moment it is streamed, including
        // the ones that are not moving: the stream codes them lossily whether they
        // change or not, and nothing else is going to send them.
        for cell in &cells {
            self.debts.insert(*cell, now);
        }
        Ok(Live {
            id,
            rect,
            cells,
            stream,
            picture,
            // Its whole region is owed: nothing has carried these pixels yet.
            dirty: true,
            keyframe_owed: true,
            carried: false,
            announced: None,
            moving_at: now,
            first: false,
        })
    }

    /// An exact copy of `sent` has gone out, so nothing is owed for any tile it
    /// covers in full.
    ///
    /// **In full** is the whole rule. Damage is clipped to what changed rather than
    /// snapped out to the grid, so a piece covering part of a tile is the common
    /// case; leaving that tile's debt standing costs one redundant re-send later and
    /// is always correct, where cancelling on a sliver would leave the rest of the
    /// tile lossy with nothing left that knows it is owed.
    pub fn discharge(&mut self, sent: Rect) {
        let Some((w, h)) = self.size else {
            return;
        };
        for (key, _) in tiles_of(sent) {
            if CellBox::of(key).to_rect(w, h).is_some_and(|cell| sent.contains(&cell)) {
                self.debts.remove(&key);
            }
        }
    }

    /// Take up to `max` tiles that have been owed for `idle`, oldest first, as
    /// rectangles for the caller to send exactly — one per run of neighbouring tiles
    /// in a row, not one per tile.
    ///
    /// A tile a live stream still covers is never due: its stream is carrying it, and
    /// an exact copy would be overwritten by the next access unit anyway.
    pub fn due(&mut self, now: Instant, idle: Duration, max: usize) -> Vec<Rect> {
        let (w, h) = match self.size {
            Some(size) => size,
            None => return Vec::new(),
        };
        let mut ready: Vec<((u16, u16), Instant)> = self
            .debts
            .iter()
            .filter(|(cell, at)| !self.covered.contains(*cell) && now.saturating_duration_since(**at) >= idle)
            .map(|(cell, at)| (*cell, *at))
            .collect();
        // Oldest first, and among tiles owed since the same instant — every tile a
        // stream carried, once it ends — row by row rather than column by column, so
        // that what the budget cuts off is whole stripes: a stopped video sharpens
        // top-down a stripe at a time, not left-to-right in slivers a tile wide.
        ready.sort_unstable_by_key(|((col, row), at)| (*at, *row, *col));
        ready.truncate(max);
        let mut cells: Vec<(u16, u16)> = ready
            .into_iter()
            .map(|(cell, _)| {
                self.debts.remove(&cell);
                cell
            })
            .collect();
        // The batch goes out as runs: consecutive columns of one row are one
        // rectangle, so a stopped video is restored a stripe at a time rather than a
        // tile at a time, and pays PNG's fixed cost once per stripe. Age chose the
        // tiles; it does not also have to order them, which all leave together.
        cells.sort_unstable_by_key(|(col, row)| (*row, *col));
        let mut runs: Vec<CellBox> = Vec::new();
        for (col, row) in cells {
            match runs.last_mut() {
                Some(run) if run.r0 == row && run.c1.checked_add(1) == Some(col) => run.c1 = col,
                _ => runs.push(CellBox::of((col, row))),
            }
        }
        runs.into_iter().filter_map(|run| run.to_rect(w, h)).collect()
    }

    /// Take the mirror and every stream, for an encode on a blocking worker.
    ///
    /// `None` when no stream has anything waiting — so a still screen costs neither
    /// the hand-off nor the encode — and while a previous round is still away, which
    /// is what keeps the inter-frame chain serial. Everything is taken, not just the
    /// dirty ones: an encoder cannot be borrowed across a `spawn_blocking`, and
    /// leaving half of them behind would make [`Self::covers`] answer differently
    /// depending on whether a round is out.
    ///
    /// The spare mirror is brought up to date — rect by rect, bounded by the damage
    /// since the last round rather than by the desktop — and installed as current,
    /// so blits land somewhere real while the encode runs.
    pub fn take_round(&mut self) -> Option<Round> {
        if self.round_out || !self.dirty() {
            return None;
        }
        let current = self.mirror.take().expect("a dirty stream means a mirror");
        let spare = match self.spare.take() {
            Some(mut spare) => {
                for rect in &self.staged {
                    spare.adopt(&current, *rect);
                }
                spare
            }
            // The first round of a session (or the first after a resize) clones
            // whole: there is no spare yet to sync.
            None => current.clone(),
        };
        self.staged.clear();
        self.mirror = Some(spare);
        self.round_out = true;
        Some(Round {
            mirror: current,
            live: std::mem::take(&mut self.live),
            mark: self.mark,
            skipped: 0,
            epoch: self.epoch,
            #[cfg(test)]
            rendezvous: None,
        })
    }

    /// Put back what [`Self::take_round`] took, and note that the tiles those streams
    /// cover have just been carried.
    ///
    /// The debts are refreshed here rather than where the units are pushed, which is
    /// the conservative side: a tile whose stream produced a frame this round cannot
    /// also be idle, so nothing can clean it up from underneath a unit still on its
    /// way to the socket.
    pub fn put_back(&mut self, round: Round, now: Instant) {
        self.round_out = false;
        if round.epoch != self.epoch {
            // The desktop was resized, or everything forgotten, while this round was
            // encoding: its mirror and streams describe a framebuffer that no longer
            // exists. Its access units were still delivered, but nothing here is
            // worth keeping — except the ids. `forget` records the ends it can see,
            // and a round that is out has taken the live table with it, so these are
            // the streams it could not.
            for live in &round.live {
                record_end(&mut self.ended, live.id);
            }
            return;
        }
        // The returned mirror is stale by exactly the rects blitted since the swap,
        // which is what `staged` has been accumulating; it becomes the spare and the
        // next take's sync settles the difference.
        self.spare = Some(round.mirror);
        self.live = round.live;
        for live in &mut self.live {
            // Damage that arrived while the round was out marked no stream dirty —
            // the live table was empty — so it is replayed from the staged rects.
            if self.staged.iter().any(|rect| live.rect.intersect(rect).is_some()) {
                live.dirty = true;
            }
            // And the congestion walk may have moved the dial while the streams were
            // out of reach.
            if live.stream.quality() != self.quality {
                live.stream.set_quality(self.quality);
            }
        }
        for live in &self.live {
            if live.carried {
                for cell in &live.cells {
                    self.debts.insert(*cell, now);
                }
            }
        }
    }

    /// Move every live stream's quality, for the congestion walk.
    ///
    /// One link, one verdict: the streams share a socket, and a policy that treated
    /// them separately would be reasoning about a bottleneck none of them can see on
    /// its own. Streams away on a round take it when they come back.
    pub fn set_quality(&mut self, quality: u8) {
        for live in &mut self.live {
            live.stream.set_quality(quality);
        }
        self.quality = quality;
    }

    /// The dial every live stream is encoding at.
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// Arm a keyframe on every live stream. Its callers are exactly the moments a
    /// page's decoder has to be able to start over.
    pub fn force_keyframes(&mut self) {
        for live in &mut self.live {
            live.keyframe_owed = true;
            // And the format is owed again, for the same reason the keyframe is: the
            // page this is for may be one that has never seen either.
            live.announced = None;
            // Without this a keyframe would wait for the region to change again,
            // which on a paused desktop is never.
            live.dirty = true;
        }
    }
}

/// One round of encoding: the mirror and every stream, away from [`Regions`] for the
/// duration of a blocking worker.
///
/// One round is one frame of the desktop however many regions it took, and what the
/// pipelined queue pays for it is its **wall-clock**, not its total CPU — no new
/// round can be taken while one is out, so a slow round is what caps the frame
/// rate. The dirty streams therefore encode concurrently ([`Self::encode`]).
pub struct Round {
    mirror: Mirror,
    live: Vec<Live>,
    mark: Option<Mark>,
    skipped: u64,
    /// The [`Regions::epoch`] this round was taken under, so [`Regions::put_back`]
    /// can tell a round that outlived its desktop from one worth restoring.
    epoch: u64,
    /// A rendezvous every encode passes through when a test arms one, so a test can
    /// prove the streams really overlap: under a serial loop the first encode waits
    /// for a sibling that never starts, and the bounded wait fails the test.
    #[cfg(test)]
    rendezvous: Option<std::sync::Arc<Rendezvous>>,
}

/// See [`Round::rendezvous`]. Its wait is bounded so a regression to serial
/// encoding fails with a message instead of hanging the suite; the bound is a
/// hang guard, not a timing assertion — both threads exist before either waits,
/// so only an encode that cannot start until its sibling *finishes* can miss it.
#[cfg(test)]
struct Rendezvous {
    expected: usize,
    arrived: std::sync::Mutex<usize>,
    all_here: std::sync::Condvar,
}

#[cfg(test)]
impl Rendezvous {
    fn new(expected: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            expected,
            arrived: std::sync::Mutex::new(0),
            all_here: std::sync::Condvar::new(),
        })
    }

    fn arrive(&self) {
        let mut arrived = self.arrived.lock().unwrap();
        *arrived += 1;
        if *arrived >= self.expected {
            self.all_here.notify_all();
            return;
        }
        let (_arrived, waited) = self
            .all_here
            .wait_timeout_while(arrived, Duration::from_secs(10), |n| *n < self.expected)
            .unwrap();
        assert!(!waited.timed_out(), "the round's streams did not encode concurrently: a sibling never started");
    }
}

/// [`Rendezvous::arrive`] when one is armed; nothing otherwise.
#[cfg(test)]
fn rendezvous_arrive(rendezvous: &Option<std::sync::Arc<Rendezvous>>) {
    if let Some(rendezvous) = rendezvous {
        rendezvous.arrive();
    }
}

/// One stream's encode of its crop of the mirror: the crop is copied in, outlined
/// under `render_motion_debug`, and coded.
fn encode_live(live: &mut Live, mirror: &Mirror, mark: Option<Mark>) -> anyhow::Result<Option<AccessUnit>> {
    let mut rgb = Vec::new();
    mirror.crop_into(live.rect, &mut rgb)?;
    if let Some(mark) = mark {
        crate::video::outline(&mut rgb, (usize::from(live.rect.w()), usize::from(live.rect.h())), mark);
    }
    let whole = Rect::from_size(0, 0, live.rect.w(), live.rect.h()).expect("a region is never empty");
    live.picture.blit(whole, &rgb)?;
    live.stream.encode(&live.picture, None)
}

impl Round {
    /// The coarsest quality any of this round's encoders is running at. `None` for a
    /// round with no streams.
    pub fn quality(&self) -> Option<u8> {
        self.live.iter().map(|live| live.stream.quality()).min()
    }

    /// Whether a unit this round encodes will be one a decoder can start from.
    pub fn keyframe(&self) -> bool {
        self.live.iter().any(|live| live.dirty && live.keyframe_owed)
    }

    /// Encode every stream with pixels waiting, and say what goes out for them:
    /// every announcement first, and then the access units. Blocking: call it on a
    /// worker.
    ///
    /// The dirty streams encode **concurrently** — one scoped thread per stream past
    /// the first, which runs on the worker itself, since most rounds have exactly
    /// one. Each thread holds one stream `&mut`, and the mirror is read everywhere
    /// and written nowhere, so `std::thread::scope` proves the whole arrangement at
    /// compile time. Units keep stream order whichever encode finishes first, and an
    /// error is returned only after every stream has had its attempt.
    ///
    /// A stream the encoder produced no bitstream for keeps its dirty flag and its
    /// keyframe, so those pixels ride the next round — which is what stops a frame
    /// that produced nothing from becoming pixels the page never gets.
    pub fn encode(&mut self) -> anyhow::Result<Vec<ServerMsg>> {
        let (mirror, mark) = (&self.mirror, self.mark);
        #[cfg(test)]
        let rendezvous = self.rendezvous.clone();

        // Keyframes are armed before the fan-out: arming needs the same `&mut` the
        // encode does, and it is not the part worth overlapping.
        let mut waiting: Vec<(usize, &mut Live)> = Vec::new();
        for (at, live) in self.live.iter_mut().enumerate() {
            live.carried = false;
            if !live.dirty {
                continue;
            }
            if live.keyframe_owed {
                live.stream.force_keyframe();
            }
            waiting.push((at, live));
        }

        let outcomes: Vec<(usize, anyhow::Result<Option<AccessUnit>>)> = std::thread::scope(|scope| {
            let mut waiting = waiting.into_iter();
            let first = waiting.next();
            let spawned: Vec<_> = waiting
                .map(|(at, live)| {
                    #[cfg(test)]
                    let rendezvous = rendezvous.clone();
                    let handle = scope.spawn(move || {
                        #[cfg(test)]
                        rendezvous_arrive(&rendezvous);
                        encode_live(live, mirror, mark)
                    });
                    (at, handle)
                })
                .collect();
            let mut outcomes = Vec::with_capacity(spawned.len() + 1);
            if let Some((at, live)) = first {
                #[cfg(test)]
                rendezvous_arrive(&rendezvous);
                outcomes.push((at, encode_live(live, mirror, mark)));
            }
            for (at, handle) in spawned {
                // A panicking encode is re-raised rather than absorbed: release
                // builds abort on panic anyway, and a debug run should die where
                // the fault is.
                let outcome = handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                outcomes.push((at, outcome));
            }
            outcomes
        });

        let mut formats = Vec::new();
        let mut units = Vec::with_capacity(outcomes.len());
        let mut failed: Option<anyhow::Error> = None;
        for (at, outcome) in outcomes {
            let live = &mut self.live[at];
            let unit = match outcome {
                Ok(Some(unit)) => unit,
                Ok(None) => {
                    self.skipped += 1;
                    continue;
                }
                Err(e) => {
                    failed.get_or_insert(e);
                    continue;
                }
            };
            live.dirty = false;
            live.keyframe_owed = false;
            live.carried = true;
            if let Some(decode) = live.stream.decode_string()
                && live.announced.as_deref() != Some(decode)
            {
                live.announced = Some(decode.to_owned());
                formats.push(ServerMsg::RegionFormat { stream: live.id, decode: decode.to_owned() });
            }
            units.push(ServerMsg::Region(RegionUnit {
                stream: live.id,
                x: live.rect.left,
                y: live.rect.top,
                w: live.rect.w(),
                h: live.rect.h(),
                keyframe: unit.keyframe,
                data: unit.data,
                held: Held::default(),
            }));
        }
        match failed {
            Some(e) => Err(e),
            None => {
                formats.extend(units);
                Ok(formats)
            }
        }
    }

    /// Streams whose encode yielded no bitstream. Must stay zero.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }
}

#[cfg(test)]
impl Round {
    /// The mirror this round would encode, for tests asserting what the double
    /// buffer hands the worker.
    fn mirror(&self) -> &Mirror {
        &self.mirror
    }

    /// Make the next [`Self::encode`] prove that `streams` encodes overlap in time —
    /// each waits at a rendezvous until all have started, which a serial loop can
    /// never satisfy.
    fn expect_overlap(&mut self, streams: usize) {
        self.rendezvous = Some(Rendezvous::new(streams));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tile-grid box, which reads better in tests than the struct literal.
    fn boxed(c0: u16, r0: u16, c1: u16, r1: u16) -> CellBox {
        CellBox { c0, r0, c1, r1 }
    }

    /// Every tile of a rectangular block, which is what a video in a window looks
    /// like to the churn map.
    fn block(c0: u16, r0: u16, c1: u16, r1: u16) -> Vec<(u16, u16)> {
        (c0..=c1).flat_map(|c| (r0..=r1).map(move |r| (c, r))).collect()
    }

    /// Two components' tiles cannot overlap, but their boxes can: a tile tucked into
    /// an L's corner is its own region inside the L's box. A tile inside two live
    /// regions is a tile two streams both carry, so the overlap is merged away —
    /// whatever the cap — and separated blocks are left exactly as they are.
    #[test]
    fn regions_are_disjoint_even_where_their_boxes_nest() {
        let mut cells = vec![(0, 0), (0, 1), (0, 2), (1, 2), (2, 2), (2, 0)];
        cells.extend(block(4, 0, 5, 2));
        cells.extend(block(0, 8, 2, 9));
        let inside = |boxes: &[CellBox], cell: (u16, u16)| boxes.iter().any(|b| b.contains(cell));
        for max in 1..=MAX_STREAMS {
            let boxes = coalesce(&cells, max);
            assert!(boxes.len() <= max, "the merge invented a stream at max {max}");
            for (i, a) in boxes.iter().enumerate() {
                for b in &boxes[i + 1..] {
                    assert!(!a.overlaps(b), "at max {max}, {a:?} overlaps {b:?}");
                }
            }
            // The corner tile is inside the L's box whichever way the cap goes, so
            // it is streamed at every cap.
            assert!(inside(&boxes, (2, 0)), "at max {max}, the corner tile lost its region");
        }
        // And the boxes are exactly the regions' own: the L with its corner tile, and
        // the two blocks, none of them grown by a tile.
        let boxes = coalesce(&cells, MAX_STREAMS);
        assert_eq!(boxes.len(), 3, "{boxes:?}");
        for want in [boxed(0, 0, 2, 2), boxed(4, 0, 5, 2), boxed(0, 8, 2, 9)] {
            assert!(boxes.contains(&want), "{want:?} is not among {boxes:?}");
        }
    }

    /// Two boxes that overlap without nesting have a union bigger than either, and
    /// that union is judged like any other merge. Within the veto it is taken.
    #[test]
    fn a_partial_overlap_within_the_waste_veto_merges() {
        // An L, and a hook whose box reaches into the L's box and past it.
        let mut cells = vec![(0, 0), (0, 1), (0, 2), (1, 2), (2, 2)];
        cells.extend([(2, 0), (3, 0), (4, 0), (4, 1), (4, 2)]);
        assert_eq!(coalesce(&cells, MAX_STREAMS), vec![boxed(0, 0, 4, 2)]);
    }

    /// And past the veto the smaller component goes to PNG, so no tile outside both
    /// original boxes is streamed — the union would have swallowed a screenful of
    /// still ones for the sake of a line.
    #[test]
    fn a_partial_overlap_past_the_waste_veto_drops_the_smaller_region() {
        // A T: eleven tiles along row 0 and ten down column 5, in a 11×11 box.
        let mut cells: Vec<(u16, u16)> = (0..=10).map(|c| (c, 0)).collect();
        cells.extend((1..=10).map(|r| (5, r)));
        // A line down column 0 from row 2, reaching two rows past the T's box, so
        // the boxes overlap without either containing the other.
        cells.extend((2..=12).map(|r| (0, r)));
        let boxes = coalesce(&cells, MAX_STREAMS);
        assert_eq!(boxes, vec![boxed(0, 0, 10, 10)], "the union was taken");
        for outside in [(1, 11), (10, 11), (10, 12), (0, 11), (0, 12)] {
            assert!(!boxes.iter().any(|b| b.contains(outside)), "{outside:?} is outside both boxes and was streamed");
        }
    }

    #[test]
    fn a_contiguous_block_is_one_region() {
        let cells = block(1, 2, 3, 5);
        assert_eq!(coalesce(&cells, MAX_STREAMS), vec![boxed(1, 2, 3, 5)]);
    }

    /// The fault this is here to prevent, one level up from comparing each tile: two
    /// things moving at opposite ends of the screen are two regions, not one box with
    /// the desktop inside it.
    #[test]
    fn two_separated_blocks_stay_two_regions() {
        let mut cells = block(0, 0, 2, 1);
        cells.extend(block(10, 12, 12, 13));
        let regions = coalesce(&cells, MAX_STREAMS);
        assert_eq!(regions.len(), 2, "{regions:?}");
        assert!(regions.contains(&boxed(0, 0, 2, 1)));
        assert!(regions.contains(&boxed(10, 12, 12, 13)));
    }

    /// Touching at a corner is not touching: 4-connectivity, so a diagonal pair is
    /// two regions rather than one box twice their size.
    #[test]
    fn cells_that_only_touch_at_a_corner_are_two_regions() {
        let mut cells = block(0, 0, 4, 0);
        cells.extend(block(5, 1, 9, 1));
        assert_eq!(coalesce(&cells, MAX_STREAMS).len(), 2);
        // Side by side is one, which is the same statement from the other direction.
        let mut cells = block(0, 0, 4, 0);
        cells.extend(block(5, 0, 9, 0));
        assert_eq!(coalesce(&cells, MAX_STREAMS), vec![boxed(0, 0, 9, 0)]);
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

    /// The pair queues must not turn a cost tie into a policy change: component
    /// order has always picked the first pair, so the top two bars merge rather
    /// than the bottom two.
    #[test]
    fn equal_cost_merges_keep_component_order() {
        let mut cells = block(0, 0, 4, 0);
        cells.extend(block(0, 2, 4, 2));
        cells.extend(block(0, 4, 4, 4));
        assert_eq!(coalesce(&cells, 2), vec![boxed(0, 0, 4, 2), boxed(0, 4, 4, 4)]);
    }

    /// And when no merge is cheap enough, the smallest region loses its stream rather
    /// than the screen being swallowed. Its tiles go as PNG, which is exact and
    /// merely expensive — the safe direction.
    #[test]
    fn a_merge_that_would_swallow_the_screen_drops_the_smallest_region_instead() {
        let mut cells = block(0, 0, 2, 2);
        cells.extend(block(30, 20, 34, 20));
        let regions = coalesce(&cells, 1);
        assert_eq!(regions, vec![boxed(0, 0, 2, 2)], "one far strip took the whole screen into a stream");
    }

    /// Fewer than `MIN_STREAM_CELLS` moving tiles is a spinner, not a video: it stays
    /// PNG however far it is from anything else, and a cap that has room for it does
    /// not change that.
    #[test]
    fn a_component_too_small_to_be_a_video_is_not_streamed() {
        // A 2×2 block is the biggest thing the gate holds back.
        assert!(coalesce(&block(0, 0, 1, 1), MAX_STREAMS).is_empty());
        assert!(coalesce(&[(0, 0), (1, 0), (2, 0), (3, 0)], MAX_STREAMS).is_empty());
        // One more tile and it is a region.
        assert_eq!(coalesce(&block(0, 0, 4, 0), MAX_STREAMS), vec![boxed(0, 0, 4, 0)]);
    }

    /// The case the gate exists for: a spinner in one corner while a video plays in
    /// the other. The video streams, the spinner is PNG, and however many spinners
    /// there are they neither fill the cap nor drag the video's box out to meet them.
    #[test]
    fn spinners_beside_a_video_cost_it_neither_a_slot_nor_a_merge() {
        let video = block(0, 0, 4, 2);
        let mut cells = video.clone();
        for spinner in [(20, 0), (20, 10), (0, 20), (20, 20), (10, 10)] {
            cells.extend(block(spinner.0, spinner.1, spinner.0 + 1, spinner.1 + 1));
        }
        for max in 1..=MAX_STREAMS {
            assert_eq!(coalesce(&cells, max), vec![boxed(0, 0, 4, 2)], "at max {max}");
        }
    }

    /// A spinner inside a video's box is carried by the video's stream, exactly as a
    /// still tile there would be: the gate leaves it out of the decision, and the box
    /// that covers it is the same box either way.
    #[test]
    fn a_small_component_inside_a_larger_box_is_carried_by_that_box() {
        // A hollow frame, and a spinner in the hole.
        let mut cells: Vec<(u16, u16)> = block(0, 0, 5, 0);
        cells.extend(block(0, 4, 5, 4));
        cells.extend([(0, 1), (0, 2), (0, 3), (5, 1), (5, 2), (5, 3)]);
        cells.push((2, 2));
        assert_eq!(coalesce(&cells, MAX_STREAMS), vec![boxed(0, 0, 5, 4)]);
    }

    /// Whatever the cap does, the result is a set of disjoint boxes — which is what
    /// makes "a tile is in at most one stream" true by construction.
    #[test]
    fn regions_never_overlap_each_other() {
        let mut cells = Vec::new();
        for (c, r) in [(0, 0), (5, 1), (6, 9), (12, 3), (13, 3), (2, 7)] {
            cells.extend(block(c, r, c + 1, r + 2));
        }
        for max in 1..=MAX_STREAMS {
            let regions = coalesce(&cells, max);
            assert!(regions.len() <= max);
            for (i, a) in regions.iter().enumerate() {
                for b in &regions[i + 1..] {
                    assert!(!a.overlaps(b), "at max {max}, {a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn nothing_moving_is_no_regions() {
        assert!(coalesce(&[], MAX_STREAMS).is_empty());
    }

    /// A tile tucked into an L's corner is its own region inside the L's box, and the
    /// L's stream would carry it whatever was decided — so the pair is merged at
    /// every cap, and the veto is never asked about a union that adds no tile.
    #[test]
    fn a_nested_box_is_merged_whatever_the_cap() {
        let mut cells = block(0, 0, 6, 0);
        cells.extend(block(0, 1, 0, 6));
        // A strip a tile in from the L's arms, so 4-connected to none of it, and
        // inside its bounding box.
        cells.extend(block(2, 2, 6, 2));
        for max in 1..=MAX_STREAMS {
            assert_eq!(coalesce(&cells, max), vec![boxed(0, 0, 6, 6)], "at max {max}");
        }
    }

    // ---- the live table ------------------------------------------------------
    //
    // Every instant below is made up rather than waited for, so nothing here changes
    // if the machine is twice as slow.

    /// A 320×128 desktop — five tiles across, two down — with its mirror already
    /// built, which is what a first blit does. Five across because a row of it is
    /// the smallest region the gate lets stream.
    fn regions() -> Regions {
        sized(320, 128)
    }

    /// The top row of [`regions`]: one region, the smallest there is.
    fn top_row() -> Vec<(u16, u16)> {
        block(0, 0, 4, 0)
    }

    /// Both rows of [`regions`]: the same region grown to the whole desktop.
    fn both_rows() -> Vec<(u16, u16)> {
        block(0, 0, 4, 1)
    }

    /// Two 3×2 blocks with a gap between them on a 1600×128 desktop, so they
    /// coalesce as two regions rather than one.
    fn two_blocks() -> Vec<(u16, u16)> {
        let mut cells = block(0, 0, 2, 1);
        cells.extend(block(5, 0, 7, 1));
        cells
    }

    /// The same, at whatever size a test needs tiles for. A region needs
    /// `MIN_STREAM_CELLS` tiles, so a test about two *separate* regions needs room
    /// for two blocks that size with a quiet tile between: neighbouring tiles
    /// coalesce into one.
    fn sized(w: u16, h: u16) -> Regions {
        let mut regions = Regions::new(60, false, None);
        regions.want(w, h);
        let bytes = usize::from(w) * usize::from(h) * 3;
        regions.blit(Rect { left: 0, top: 0, right: w - 1, bottom: h - 1 }, &vec![0; bytes]).expect("a full-desktop blit");
        regions
    }

    /// The rectangle of one live stream, so a test can say whether geometry moved.
    fn only_rect(regions: &Regions) -> Rect {
        assert_eq!(regions.live.len(), 1, "expected exactly one stream");
        regions.live[0].rect
    }

    /// The first sight covers the whole desktop, and motion inside it is not a
    /// reason to keep it: a region that qualifies ends it and streams on its own, so
    /// a video playing at the start does not keep the whole desktop lossy.
    #[test]
    fn motion_during_the_first_sight_ends_it_for_a_region_of_its_own() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.first_sight(t0).expect("a first sight");
        assert_eq!(only_rect(&regions), Rect::from_size(0, 0, 320, 128).unwrap());
        let first = regions.live[0].id;
        regions.retune(&[], t0).expect("nothing moving");
        assert_eq!(only_rect(&regions).h(), 128, "the first sight ended with nothing moving");

        regions.retune(&top_row(), t0 + RETUNE).expect("a stream for the moving tiles");
        assert_eq!(only_rect(&regions).h(), 64, "the first sight was kept for the motion inside it");
        assert!(!regions.live[0].first);
        // Its id may be handed straight back to the region, which the page tells
        // from a continuation by the rectangle; otherwise it is ended.
        assert!(regions.live[0].id == first || regions.drain_ended().contains(&first));
    }

    /// The first sight ends once a round has carried it with nothing waiting, not
    /// while something is, and never later than a stream idle after it started.
    #[test]
    fn the_first_sight_ends_once_carried_and_never_past_the_stream_idle() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.first_sight(t0).expect("a first sight");
        regions.end_first_sight();
        assert!(regions.covering(), "a first sight ended before it was carried");
        let mut round = regions.take_round().expect("a first sight is dirty");
        round.encode().expect("an encode");
        regions.put_back(round, t0);
        regions.end_first_sight();
        assert!(!regions.covering(), "a first sight outlived the round that carried it");
        assert!(regions.owes(), "and its tiles are owed exactly");

        let mut regions = self::regions();
        regions.first_sight(t0).expect("a first sight");
        regions.expire(t0 + STREAM_IDLE);
        assert!(!regions.covering(), "a first sight outlived the stream idle");
    }

    #[test]
    fn geometry_does_not_move_inside_the_retune_interval() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&top_row(), t0).expect("a stream for the moving tiles");
        let first = only_rect(&regions);

        // Twice as much is moving, but not yet.
        regions.retune(&both_rows(), t0 + RETUNE / 2).expect("no work");
        assert_eq!(only_rect(&regions), first, "geometry moved inside the interval");
    }

    /// The interval guards a live stream's geometry. With none live there is nothing
    /// to guard, and a region that has just qualified streams at the next round
    /// rather than at the next tick of a clock started when nothing was moving.
    #[test]
    fn a_first_region_does_not_wait_for_the_retune_interval() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&[], t0).expect("nothing to do");
        assert!(regions.live.is_empty());

        regions.retune(&top_row(), t0 + RETUNE / 4).expect("a stream for the moving tiles");
        assert_eq!(only_rect(&regions).h(), 64, "the first region waited for the interval");

        // Now something is live, and the interval holds again.
        regions.retune(&both_rows(), t0 + RETUNE / 2).expect("no work");
        assert_eq!(only_rect(&regions).h(), 64, "geometry moved inside the interval");
    }

    /// While a round is away the live table is on the worker. A stream built into the
    /// empty one left behind would be dropped when the round came back — with the id
    /// it took and the debts it recorded — so a retune waits for the return.
    #[test]
    fn nothing_is_retuned_while_a_round_is_out() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&top_row(), t0).expect("a stream");
        let first = only_rect(&regions);
        let round = regions.take_round().expect("a new stream is dirty");

        regions.retune(&both_rows(), t0 + RETUNE).expect("no work");
        assert!(regions.live.is_empty(), "a stream was built beside a round that is out");
        assert_eq!(regions.retuned_at, Some(t0), "the retune counted");

        regions.put_back(round, t0 + RETUNE);
        assert_eq!(only_rect(&regions), first, "the round did not bring its stream back");
        regions.retune(&both_rows(), t0 + RETUNE).expect("a taller stream");
        assert_eq!(only_rect(&regions).h(), 128, "the retune after the return was skipped");
    }

    /// Shrinking is free: the idle margin codes as skipped macroblocks, where a
    /// restart costs an encoder and a keyframe.
    #[test]
    fn a_shrinking_region_keeps_its_stream() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&both_rows(), t0).expect("a stream");
        let tall = only_rect(&regions);
        assert_eq!(tall.h(), 128);

        regions.retune(&top_row(), t0 + RETUNE).expect("no restart");
        assert_eq!(only_rect(&regions), tall, "a shrinking region paid for a new encoder");
    }

    /// The other side of shrinking: motion that would not start a stream does not
    /// keep one alive. A video that pauses under a spinner ends its stream, its tiles
    /// come due, and the spinner is PNG — the same as a pause with no spinner.
    #[test]
    fn motion_below_the_gate_does_not_keep_a_stream_alive() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&both_rows(), t0).expect("a stream");
        let id = regions.live[0].id;

        // Only a 2×2 block still moving inside it, for as long as a stream may idle.
        regions.retune(&block(0, 0, 1, 1), t0 + STREAM_IDLE).expect("no work");
        assert!(regions.live.is_empty(), "a spinner kept a video's stream alive");
        assert_eq!(regions.drain_ended(), vec![id]);
        assert!(!regions.covers((0, 0)), "a tile no stream carries is still covered");
    }

    /// One rectangle that has split into two regions keeps one stream, carrying
    /// both. Building a second stream for the second region would put it inside the
    /// first — two streams over one tile, the delivery rule broken.
    #[test]
    fn a_kept_stream_carries_every_region_inside_it() {
        // Five tiles across, three down.
        let mut regions = sized(320, 192);
        let t0 = Instant::now();
        regions.retune(&block(0, 0, 4, 2), t0).expect("a stream");
        let whole = only_rect(&regions);
        let id = regions.live[0].id;

        // The middle row goes quiet, leaving a region in each of the outer rows.
        let mut split = block(0, 0, 4, 0);
        split.extend(block(0, 2, 4, 2));
        regions.retune(&split, t0 + RETUNE).expect("the same stream");
        assert_eq!(only_rect(&regions), whole, "the split rebuilt the stream");
        assert_eq!(regions.live[0].id, id);
        assert!(regions.drain_ended().is_empty(), "nothing ended");
    }

    /// A region that straddles a kept stream's rectangle cannot share the screen
    /// with it, so the stream ends and the regions get streams of their own — every
    /// one disjoint from every other.
    #[test]
    fn a_region_straddling_a_kept_stream_ends_it() {
        // Ten tiles across, three down; the first stream covers the top two rows.
        let mut regions = sized(640, 192);
        let t0 = Instant::now();
        regions.retune(&block(0, 0, 9, 1), t0).expect("a stream");
        let id = regions.live[0].id;

        // One region inside the old rectangle, and one reaching out of it into the
        // third row, with a quiet column between them.
        let mut split = block(0, 0, 4, 0);
        split.extend(block(6, 1, 9, 2));
        regions.retune(&split, t0 + RETUNE).expect("two streams");
        let rects: Vec<Rect> = regions.live.iter().map(|live| live.rect).collect();
        assert_eq!(rects.len(), 2, "{rects:?}");
        assert!(rects[0].intersect(&rects[1]).is_none(), "{rects:?} overlap");
        assert!(
            rects.contains(&boxed(6, 1, 9, 2).to_rect(640, 192).expect("a rectangle")),
            "the straddling region did not get its own stream: {rects:?}"
        );
        assert!(regions.live.iter().all(|live| live.keyframe_owed), "a page cannot start on either new picture");
        // The old stream ended, unless its id was handed straight back to one of
        // the new ones — in which case it is not an end, and must not be said as one.
        let ended = regions.drain_ended();
        let reused = regions.live.iter().any(|live| live.id == id);
        assert_eq!(ended.contains(&id), !reused, "ended {ended:?}, reused {reused}");
        assert!(
            ended.iter().all(|e| regions.live.iter().all(|live| live.id != *e)),
            "an id in use was said to have ended: {ended:?}"
        );
    }

    /// Growing is not free, and must not be: the stream's rectangle is fixed for its
    /// life, so a region that outgrows it needs a new one — and a keyframe.
    #[test]
    fn a_growing_region_starts_a_new_stream() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&top_row(), t0).expect("a stream");
        assert_eq!(only_rect(&regions).h(), 64);

        regions.retune(&both_rows(), t0 + RETUNE).expect("a taller stream");
        let grown = only_rect(&regions);
        assert_eq!(grown.h(), 128, "the stream kept a rectangle its region outgrew");
        assert!(regions.live[0].keyframe_owed, "a page cannot start on the new picture");
    }

    /// A stream is otherwise indistinguishable, from the page's side, from one whose
    /// region is merely still — so the end is said. What it buys is the decoder, and
    /// with it whatever the platform gave the decoder.
    #[test]
    fn a_stream_that_ends_names_its_id() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&top_row(), t0).expect("a stream");
        let id = regions.live[0].id;
        assert!(regions.drain_ended().is_empty(), "a stream that just started has not ended");

        regions.expire(t0 + STREAM_IDLE);
        assert!(regions.live.is_empty(), "the idle stream should have ended");
        assert_eq!(regions.drain_ended(), vec![id]);
        assert!(regions.drain_ended().is_empty(), "an end is said once");
    }

    /// The exception: an id this retune both freed and handed back is a new region on
    /// a decoder that is still the right decoder for that id. Ending it would close a
    /// session the page could have kept.
    #[test]
    fn an_id_handed_straight_back_is_not_an_end() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&top_row(), t0).expect("a stream");
        let id = regions.live[0].id;
        regions.drain_ended();

        regions.retune(&both_rows(), t0 + RETUNE).expect("a taller stream");
        assert_eq!(regions.live.len(), 1);
        assert_eq!(regions.live[0].id, id, "the replacement took the id back");
        assert!(regions.drain_ended().is_empty(), "the page was told to close a live decoder");
    }

    /// A resize drops every stream, so the ids have to be said, or a desktop that
    /// resizes and then settles into fewer regions leaves the rest of them holding a
    /// decode session for the rest of the session.
    #[test]
    fn a_resize_names_the_streams_it_dropped() {
        let mut regions = sized(1600, 128);
        // Two blocks with a gap, so they coalesce as two regions rather than one.
        regions.retune(&two_blocks(), Instant::now()).expect("two streams");
        let mut ids: Vec<u8> = regions.live.iter().map(|live| live.id).collect();
        assert_eq!(ids.len(), 2);
        regions.drain_ended();

        regions.want(1600, 256);
        assert!(regions.live.is_empty(), "the resize kept a stream on the old framebuffer");
        ids.sort_unstable();
        let mut said = regions.drain_ended();
        said.sort_unstable();
        assert_eq!(said, ids, "the decoders those ids built were never handed back");
    }

    /// The same, for the streams a resize cannot see: a round that is out has taken
    /// the live table with it, and its epoch says not to bring it back.
    #[test]
    fn a_resize_over_a_round_in_flight_names_that_round_s_streams() {
        let mut regions = sized(1600, 128);
        let t0 = Instant::now();
        regions.retune(&two_blocks(), t0).expect("two streams");
        regions.drain_ended();
        let round = regions.take_round().expect("dirty streams");
        let mut ids: Vec<u8> = round.live.iter().map(|live| live.id).collect();
        assert_eq!(ids.len(), 2);

        regions.want(1600, 256);
        assert!(regions.drain_ended().is_empty(), "the resize saw a live table it did not have");
        regions.put_back(round, t0 + RETUNE);
        ids.sort_unstable();
        let mut said = regions.drain_ended();
        said.sort_unstable();
        assert_eq!(said, ids, "a discarded round's decoders were never handed back");
    }

    /// Every live stream needs an id of its own, because a page keys its decoders
    /// by it: two regions sharing one would feed two chains to one decoder and
    /// neither would decode. The trap is that a retune holds the live table outside
    /// `self`, so an allocator reading `self.live` sees nothing in use.
    #[test]
    fn two_regions_born_in_one_retune_get_different_ids() {
        let mut regions = sized(1600, 128);
        regions.retune(&two_blocks(), Instant::now()).expect("two streams");
        let ids: Vec<u8> = regions.live.iter().map(|live| live.id).collect();
        assert_eq!(regions.live.len(), 2, "expected two regions, got {ids:?}");
        assert_ne!(ids[0], ids[1], "both regions were sent as the same stream");
    }

    /// A round with several dirty streams encodes all of them, **concurrently** —
    /// the rendezvous holds each encode until every one has started, which a serial
    /// loop can never satisfy — and the contract that survives any scheduling still
    /// holds: every stream produces its unit, the units keep stream order whichever
    /// encode finished first, and each announces its format ahead of its first unit.
    #[test]
    fn a_round_with_two_streams_produces_both_units_in_stream_order() {
        let mut regions = sized(1600, 128);
        regions.retune(&two_blocks(), Instant::now()).expect("two streams");
        let ids: Vec<u8> = regions.live.iter().map(|live| live.id).collect();
        assert_eq!(ids.len(), 2, "expected two regions, got {ids:?}");

        let mut round = regions.take_round().expect("both streams are dirty from birth");
        round.expect_overlap(2);
        let records = round.encode().expect("an encode");
        assert_eq!(round.skipped(), 0, "a stream's pixels were lost to the fan-out");
        let formats = records.iter().take_while(|r| matches!(r, ServerMsg::RegionFormat { .. })).count();
        assert_eq!(formats, 2, "each stream announces before every unit");
        let units: Vec<&RegionUnit> = records
            .iter()
            .filter_map(|r| match r {
                ServerMsg::Region(unit) => Some(unit),
                _ => None,
            })
            .collect();
        let streams: Vec<u8> = units.iter().map(|unit| unit.stream).collect();
        assert_eq!(streams, ids, "units must keep stream order whichever encode finishes first");
        assert!(units.iter().all(|unit| unit.keyframe), "each stream's first unit");
        regions.put_back(round, Instant::now());
        assert!(!regions.dirty(), "both streams were carried");
    }

    /// And a stream that survives a retune keeps the id it had, whatever is built
    /// alongside it: a new id would read as a new region, and the page would start
    /// a decoder with none of the history the next frame is expressed against.
    #[test]
    fn a_carried_stream_keeps_its_id_when_another_is_built_beside_it() {
        let mut regions = sized(1600, 128);
        let t0 = Instant::now();
        regions.retune(&block(0, 0, 2, 1), t0).expect("one stream");
        let first = regions.live[0].id;

        // The first region keeps moving and its rectangle still fits; the second is
        // new, and must not be handed the id the first is still using.
        regions.retune(&two_blocks(), t0 + RETUNE).expect("a second stream");
        let ids: Vec<u8> = regions.live.iter().map(|live| live.id).collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(ids.contains(&first), "the surviving region was renumbered");
        assert_ne!(ids[0], ids[1]);
    }

    /// What the cleanup's idle threshold is in [`crate::motion`], where it lives
    /// with the rest of the cleanup policy.
    const CLEANUP_IDLE_FOR_TESTS: Duration = Duration::from_millis(500);

    /// The stream ends, and every tile it covered is owed an exact re-send.
    #[test]
    fn a_region_that_stops_moving_ends_and_its_cells_come_due() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&top_row(), t0).expect("a stream");
        assert!(regions.covers((0, 0)));
        assert!(
            regions.due(t0 + CLEANUP_IDLE_FOR_TESTS, CLEANUP_IDLE_FOR_TESTS, 8).is_empty(),
            "a tile a live stream is carrying was cleaned up underneath it"
        );

        // Nothing moving, and long enough for the stream to have gone quiet.
        let later = t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS;
        regions.expire(later);
        assert!(regions.live.is_empty(), "a region with nothing moving kept its stream");
        assert!(!regions.covers((0, 0)));
        let due = regions.due(later, CLEANUP_IDLE_FOR_TESTS, 8);
        assert_eq!(due.len(), 1, "the tiles it streamed are owed nothing");
        assert_eq!(due[0], Rect { left: 0, top: 0, right: 319, bottom: 63 });
    }

    /// Only in full. Damage is clipped to what changed rather than snapped out to the
    /// grid, so a sliver of a tile says nothing about the rest of it.
    #[test]
    fn only_a_whole_cell_discharges_what_that_cell_owes() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&top_row(), t0).expect("a stream");
        regions.expire(t0 + STREAM_IDLE);
        let later = t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS;

        // The whole row is owed, first tile included: a run from the left edge.
        regions.discharge(Rect { left: 0, top: 0, right: 15, bottom: 15 });
        assert_eq!(
            regions.due(later, CLEANUP_IDLE_FOR_TESTS, 8),
            vec![Rect { left: 0, top: 0, right: 319, bottom: 63 }],
            "a sliver discharged the whole tile's debt"
        );

        // And a send that covers the tile outright does discharge it: the run now
        // starts one tile in.
        regions.retune(&top_row(), later).expect("a stream again");
        regions.expire(later + STREAM_IDLE);
        regions.discharge(Rect { left: 0, top: 0, right: 63, bottom: 63 });
        assert_eq!(
            regions.due(later + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS, CLEANUP_IDLE_FOR_TESTS, 8),
            vec![Rect { left: 64, top: 0, right: 319, bottom: 63 }],
            "a whole tile went out exact and is still owed"
        );
    }

    /// Every tile a stream covers is owed, not only the ones that were moving: the
    /// stream codes them lossily whether they change or not, and nothing else is
    /// going to send them.
    #[test]
    fn a_still_cell_swept_into_a_region_is_owed_too() {
        let mut regions = regions();
        let t0 = Instant::now();
        // The top row and one tile of the bottom row, so the box covers four tiles
        // that never moved at all.
        let mut cells = top_row();
        cells.push((4, 1));
        regions.retune(&cells, t0).expect("a stream");
        assert_eq!(only_rect(&regions).h(), 128);
        regions.expire(t0 + STREAM_IDLE);
        let mut due = regions.due(t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS, CLEANUP_IDLE_FOR_TESTS, 16);
        due.sort_unstable_by_key(|rect| rect.top);
        assert_eq!(
            due,
            vec![Rect { left: 0, top: 0, right: 319, bottom: 63 }, Rect { left: 0, top: 64, right: 319, bottom: 127 }],
            "a tile inside the stream's rectangle was owed nothing"
        );
    }

    /// Tiles that come due together and sit side by side in one row are one
    /// rectangle: the tile is the unit of debt, not of the PNG that pays it.
    #[test]
    fn due_cells_in_one_row_are_restored_as_a_run() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&both_rows(), t0).expect("a stream over every tile");
        assert_eq!(only_rect(&regions), Rect { left: 0, top: 0, right: 319, bottom: 127 });
        regions.expire(t0 + STREAM_IDLE);
        let settled = t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS;
        let mut due = regions.due(settled, CLEANUP_IDLE_FOR_TESTS, 16);
        due.sort_unstable_by_key(|rect| rect.top);
        assert_eq!(
            due,
            vec![Rect { left: 0, top: 0, right: 319, bottom: 63 }, Rect { left: 0, top: 64, right: 319, bottom: 127 }],
            "ten tiles in two rows should be two stripes"
        );
        assert!(regions.due(settled, CLEANUP_IDLE_FOR_TESTS, 16).is_empty());
    }

    /// The budget cuts the batch at a row, not at a column: the tiles a stream
    /// carried all came due at once, and a batch that can pay five of ten must pay
    /// the top stripe whole rather than the left half of both — which is five PNGs
    /// of a tile each, and a video that sharpens sideways in slivers.
    #[test]
    fn a_short_budget_takes_whole_stripes_first() {
        let mut regions = regions();
        let t0 = Instant::now();
        regions.retune(&both_rows(), t0).expect("a stream over every tile");
        regions.expire(t0 + STREAM_IDLE);
        let settled = t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS;
        assert_eq!(
            regions.due(settled, CLEANUP_IDLE_FOR_TESTS, 5),
            vec![Rect { left: 0, top: 0, right: 319, bottom: 63 }],
            "five of ten tiles should be the top stripe"
        );
        assert_eq!(
            regions.due(settled, CLEANUP_IDLE_FOR_TESTS, 5),
            vec![Rect { left: 0, top: 64, right: 319, bottom: 127 }],
            "the next five should be the bottom one"
        );
        assert!(regions.due(settled, CLEANUP_IDLE_FOR_TESTS, 5).is_empty());
    }

    /// The geometry theorem, from the coalescing side: a box becomes a rectangle
    /// whose origin is on the grid and whose size is odd only where the desktop is.
    #[test]
    fn a_region_is_even_unless_the_desktop_is_odd_at_that_edge() {
        let interior = boxed(1, 1, 2, 2).to_rect(1919, 1079).expect("a rectangle");
        assert_eq!((interior.left, interior.top), (64, 64));
        assert_eq!((interior.w() % 2, interior.h() % 2), (0, 0));

        let edge = boxed(29, 16, 29, 16).to_rect(1919, 1079).expect("a rectangle");
        assert_eq!((edge.right, edge.bottom), (1918, 1078), "clipped to the desktop");
        assert_eq!((edge.w() % 2, edge.h() % 2), (1, 1), "odd exactly where the desktop is");

        // A box entirely off the desktop is not a rectangle at all, which is what a
        // stale churn key after a resize would produce.
        assert!(boxed(40, 0, 40, 0).to_rect(1919, 1079).is_none());
    }

    fn flat(w: u16, h: u16, value: u8) -> Vec<u8> {
        vec![value; usize::from(w) * usize::from(h) * 3]
    }

    fn placed(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    /// `rect` cut into bands 64 rows tall, from its own top, as an engine that
    /// reports one rectangle a piece at a time hands it over.
    fn bands(rect: Rect) -> Vec<Rect> {
        (rect.top..=rect.bottom)
            .step_by(64)
            .map(|top| Rect { top, bottom: (top + 63).min(rect.bottom), ..rect })
            .collect()
    }

    /// Cutting a report into bands must not cost the staged list anything: the bands
    /// of one rectangle meet edge to edge, so they merge back into the rectangle they
    /// came from, and the spare-mirror sync copies what it copied when the whole
    /// rectangle arrived in one blit.
    #[test]
    fn bands_of_one_rectangle_stage_as_that_rectangle() {
        let mut regions = sized(320, 256);
        regions.staged.clear();
        let whole = placed(0, 0, 320, 256);
        for band in bands(whole) {
            regions.blit(band, &flat(band.w(), band.h(), 7)).expect("a band blit");
        }
        assert_eq!(regions.staged, vec![whole], "four bands are the one rectangle");
    }

    /// And so the cap still counts reports rather than bands: [`STAGED_CAP`]
    /// reports that each straddle a band line would be sixty-four entries without the
    /// merge, and the list would have collapsed to a bounding box spanning the whole
    /// desktop, syncing a screenful of pixels that never changed.
    #[test]
    fn banding_a_report_does_not_spend_the_staged_cap() {
        // As tall as a picture may be, the reports a little apart.
        let mut regions = sized(64, 3840);
        regions.staged.clear();
        let reports: Vec<Rect> = (0..STAGED_CAP).map(|k| placed(0, 32 + k as u16 * 116, 64, 96)).collect();
        for report in &reports {
            assert_eq!(bands(*report).len(), 2, "a report that straddles a band line");
            for band in bands(*report) {
                regions.blit(band, &flat(band.w(), band.h(), 7)).expect("a band blit");
            }
        }
        assert_eq!(regions.staged, reports, "one entry per report, none of them collapsed");
    }

    /// A 320×64 desktop streamed whole: one row of five tiles, the smallest region.
    fn streamed() -> (Regions, Rect) {
        let mut regions = sized(320, 64);
        regions.retune(&top_row(), Instant::now()).expect("a stream over the desktop");
        (regions, placed(0, 0, 320, 64))
    }

    /// The double buffer, end to end: rounds stay serial, damage that lands while a
    /// round is away re-dirties the returning stream, and the pixels it carried
    /// reach the next round's mirror — including through the spare-sync at the swap.
    #[test]
    fn damage_during_a_round_survives_into_the_next() {
        let (mut regions, whole) = streamed();
        regions.blit(whole, &flat(320, 64, 10)).expect("a blit");
        let first = regions.take_round().expect("a dirty stream means a round");
        assert!(regions.take_round().is_none(), "two rounds out would race one chain");

        // Damage lands while the round is away...
        regions.blit(whole, &flat(320, 64, 20)).expect("a blit mid-round");
        assert!(regions.take_round().is_none(), "still away");
        regions.put_back(first, Instant::now());

        // ...and the returning stream is dirty with it, over the newer pixels.
        let second = regions.take_round().expect("the staged damage re-dirtied the stream");
        let mut out = Vec::new();
        second.mirror().crop_into(whole, &mut out).expect("a crop");
        assert!(out.iter().all(|&b| b == 20), "the double buffer lost mid-round damage");
        regions.put_back(second, Instant::now());

        // The third round encodes from the spare that was synced at the last swap:
        // anything still 10 in it would be the sync not happening.
        let corner = placed(0, 0, 4, 4);
        regions.blit(corner, &flat(4, 4, 30)).expect("a corner blit");
        let third = regions.take_round().expect("dirty again");
        third.mirror().crop_into(corner, &mut out).expect("a crop");
        assert!(out.iter().all(|&b| b == 30));
        let elsewhere = placed(32, 32, 4, 4);
        third.mirror().crop_into(elsewhere, &mut out).expect("a crop");
        assert!(out.iter().all(|&b| b == 20), "the spare was not synced before the swap");
    }

    /// A resize while a round is away: the returning round is state for a desktop
    /// that no longer exists, and none of it may come back.
    #[test]
    fn a_round_that_outlives_its_desktop_is_dropped_on_return() {
        let (mut regions, whole) = streamed();
        regions.blit(whole, &flat(320, 64, 10)).expect("a blit");
        let stale = regions.take_round().expect("a round");
        regions.want(32, 32);
        regions.put_back(stale, Instant::now());
        assert!(regions.take_round().is_none(), "a stale round was restored");
        assert!(regions.live.is_empty(), "a stale round's streams came back");
    }

    /// A tile the remote redrew with the pixels it had is not a tile that changed,
    /// and a tile a rectangle's box only reached over is not either.
    #[test]
    fn only_the_tiles_whose_pixels_differ_have_changed() {
        let mut regions = sized(320, 64);
        let band = placed(0, 0, 320, 64);
        assert!(regions.changed(band, &flat(320, 64, 0)).expect("a comparison").is_empty(), "a repeat");

        // One band across five tiles: the ends differ, the middle is as it was.
        let mut rgb = flat(320, 64, 0);
        for y in 0..64usize {
            rgb[(y * 320 + 3) * 3] = 9;
            rgb[(y * 320 + 300) * 3] = 9;
        }
        let changed: Vec<(u16, u16)> = regions.changed(band, &rgb).expect("a comparison").into_iter().map(|(cell, _)| cell).collect();
        assert_eq!(changed, vec![(0, 0), (4, 0)], "a tile inside the box was taken as changed");
    }
}
