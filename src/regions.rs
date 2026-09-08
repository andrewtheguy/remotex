//! Which parts of the framebuffer get a video stream, when one starts and stops,
//! and what a client is owed when one does.
//!
//! Two dials arrive here and they differ only in how many rectangles they ask for.
//! `render_type = "video"` asks for one covering the whole desktop and never changes
//! its mind ([`Policy::Whole`]). `render_motion = true` asks for one per
//! coalesced moving region, with the still codecs carrying everything else
//! ([`Policy::Moving`]). A codec module knows how to encode a rectangle; this module
//! is every decision about *which*.
//!
//! Three things hold the whole design up:
//!
//! - **One mirror, several encoders.** [`crate::video::Mirror`] holds the exact
//!   current source for every pixel, moving or not, and each stream encodes a crop
//!   of it. That is what lets a stream start in the middle of a session — its
//!   region's pixels are already there — and it is why the debts below hold no
//!   pixels at all.
//! - **A debt is a cell key, not a picture.** A cell whose only delivery was through
//!   a lossy stream is owed a crisp re-send, because
//!   [`crate::tiles::Shadow::accept`] already recorded those pixels as delivered and
//!   nothing else will send them again. The still motion path has to *remember the
//!   pixels* it approximated, with all the staleness that implies; here the cleanup
//!   crops the mirror and is the newest truth by construction.
//! - **Live regions are pairwise disjoint, and a cell in one is never also a tile.**
//!   Two deliveries of one cell in one frame is how a debt gets discharged by pixels
//!   that did not discharge it.
//!
//! A stream is built over exactly its region's cell bounding box. Nothing outside
//! what [`coalesce`] chose is streamed when a stream starts: the still pixels beside
//! a moving region keep whatever crisp tile last painted them, and are owed nothing.
//!
//! Geometry moves at most once per `RETUNE`, so a stream is not restarted for
//! every twitch: a region that shrinks keeps its stream (the idle margin costs
//! almost nothing to code), and only a region that grows past its stream's rectangle
//! pays for a new encoder and a keyframe. A kept stream keeps its whole rectangle,
//! and every cell of it — the margin the region has left behind included — stays
//! covered and owed a cleanup until the stream ends.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use crate::config::Chroma;
use crate::protocol::{TileGrid, VideoUnit, batch};
use crate::tiles::Rect;
use crate::video::{AccessUnit, Mark, Mirror};
use crate::vp9::Stream;

/// The most streams one session runs at once.
///
/// Four rather than one per moving region without limit: each is an encoder with its
/// own reference frame and its own decoder at the far end, and a desktop with five
/// genuinely independent moving regions is a desktop where the whole screen is
/// moving — which is what merging produces anyway.
pub const MAX_STREAMS: usize = 4;

/// Most rectangles the staged-damage list holds before collapsing to a bounding
/// box — see [`Regions::stage`].
const STAGED_CAP: usize = 32;

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

/// The fewest moving cells a component needs before it is worth a stream.
///
/// Five, so a 2×2 block of the grid — 128×128 points, the most anything up to a
/// cell wide can touch however it straddles the lines — is never streamed on its
/// own. What churns in that few cells is a spinner, a progress bar, a typing
/// indicator, a blinking badge: not a video, and small enough that the base codec
/// carries it for a few kilobytes a change, where a stream costs an encoder, a
/// keyframe, a decoder at the far end and a cleanup when it stops — and a spinner
/// stops and starts with every page load, so all of that is spent and torn down
/// again each time. A component this small goes to the still codecs the way one the
/// cap dropped does, and for the same reason: crisp and merely expensive is the
/// safe direction. It is not the same as being out of a stream altogether — a
/// larger component whose box covers its cells carries them, as any box carries
/// every cell inside it.
const MIN_STREAM_CELLS: u32 = 5;

/// Note that the client is owed an end for `id`, once: an end is said once however
/// many paths notice it before [`Regions::drain_ended`] takes it.
fn record_end(ended: &mut Vec<u8>, id: u8) {
    if !ended.contains(&id) {
        ended.push(id);
    }
}

/// How many rectangles this target streams, and how they are chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// `render_type = "video"`: one stream over the whole desktop, for the whole
    /// session. No cells, no debts, no cleanup — nothing else is being sent.
    Whole,
    /// `render_motion = true`: a stream per coalesced moving region, with
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
    /// The dial this stream's encoder is *known* to be running at — recorded only on
    /// a successful `set_quality`, unlike the stream's own notion, which is updated
    /// before the calls that can fail. What [`Regions::put_back`] compares
    /// against, so an unchanged dial costs a returned stream nothing and a failed
    /// retune is retried instead of believed.
    quality: u8,
    /// Whether anything has been blitted into this region since its last access unit.
    dirty: bool,
    /// Whether the next access unit must be one a decoder can start from.
    keyframe_owed: bool,
    /// Whether the round just encoded produced a unit for this stream — which is what
    /// says its cells have just been carried and are not idle.
    carried: bool,
    /// The configuration string this stream has already announced to the client, if any.
    ///
    /// Cleared by [`Regions::force_keyframes`], which is what makes a reattach
    /// re-announce: the browser that just arrived never saw the original text frame, and its
    /// decoder cannot be configured from the units alone.
    announced: Option<String>,
    /// When something inside this region was last seen moving. What [`STREAM_IDLE`]
    /// is measured from.
    moving_at: Instant,
}

/// A rectangle of the cell grid, in cell coordinates. What [`coalesce`] works in,
/// because a region is a union of whole cells by construction — which is what makes
/// [`crate::video::coded_rect`]'s evenness a theorem rather than a check.
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

    fn overlaps(&self, other: &Self) -> bool {
        self.c0 <= other.c1 && other.c0 <= self.c1 && self.r0 <= other.r1 && other.r0 <= self.r1
    }

    fn contains(&self, (c, r): (u16, u16)) -> bool {
        self.c0 <= c && c <= self.c1 && self.r0 <= r && r <= self.r1
    }

    /// This box in framebuffer pixels, clipped to a `w`×`h` desktop cut at `grid`.
    ///
    /// The clip is the only place a region's size can come out odd, and the only
    /// place it may: a cell column starts at a multiple of `grid.w` and a row at a
    /// multiple of `grid.h`, both even ([`TileGrid`]), so an interior box is even on
    /// both axes and an edge one is odd exactly where the desktop is.
    fn to_rect(self, w: u16, h: u16, grid: TileGrid) -> Option<Rect> {
        let left = self.c0.checked_mul(grid.w)?;
        let top = self.r0.checked_mul(grid.h)?;
        if left >= w || top >= h {
            return None;
        }
        Some(Rect {
            left,
            top,
            right: (self.c1.saturating_mul(grid.w).saturating_add(grid.w - 1)).min(w - 1),
            bottom: (self.r1.saturating_mul(grid.h).saturating_add(grid.h - 1)).min(h - 1),
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
/// are two regions), then each component's bounding box. A component with fewer
/// than [`MIN_STREAM_CELLS`] moving cells is not a video and is left to the still
/// codecs before anything else is decided: it neither takes a stream of its own nor
/// counts towards the cap, so a spinner in one corner does not cost the video in the
/// other a merge or a slot. Two components' *cells*
/// cannot overlap, but their boxes can — an L and a cell tucked into its corner, or
/// two L's interlocked — and a cell inside two live regions is a cell two streams
/// both carry, which is the one delivery rule this module exists to keep. So an
/// overlapping pair is merged into its union, and that merge is judged like any
/// other: a union that adds no cell (one box inside the other) is always taken, and
/// one that would cover more than [`MERGE_WASTE`] times the cells actually moving
/// inside it is refused, with the component that has fewer moving cells dropped to
/// the still codecs instead.
///
/// If that leaves more boxes than `max`, the cheapest pair — the one whose merged
/// box adds the fewest cells — is merged, and so on, under the same veto; when
/// nothing may be merged and there are still too many, the region with the fewest
/// moving cells is dropped. Every pass removes a component, which is what makes
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
/// and how many cells are moving inside it, with every component under
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
    // Before any merging, so a component too small to stream is also too small to
    // fill a slot or to pull a larger region's box out to meet it. Its cells inside
    // some larger component's box are carried by that stream whatever is decided
    // later; the rest go crisp.
    components.retain(|c| c.moving >= MIN_STREAM_CELLS);
    components
}

/// The second half of [`coalesce`]: at most `max` pairwise disjoint boxes from the
/// components, merging and dropping under [`MERGE_WASTE`] as described there.
fn merge(mut components: Vec<Component>, max: usize) -> Vec<CellBox> {
    if max == 0 {
        return Vec::new();
    }
    loop {
        // Overlaps first, because they are forced rather than chosen: the delivery
        // rule leaves no option of keeping both boxes as they are.
        let overlapping = (0..components.len())
            .flat_map(|i| (i + 1..components.len()).map(move |j| (i, j)))
            .find(|&(i, j)| components[i].bbox.overlaps(&components[j].bbox));
        if let Some((i, j)) = overlapping {
            let merged = components[i].bbox.union(&components[j].bbox);
            let moving = components[i].moving + components[j].moving;
            // A nested pair's union is the larger box itself. Refusing that would
            // drop the inner component to the still codecs while the larger stream
            // carried its cells anyway — the delivery rule broken from the other
            // side — so it is always taken.
            let nested = merged == components[i].bbox || merged == components[j].bbox;
            if nested || merged.area() <= MERGE_WASTE * moving {
                components.remove(j);
                components[i] = Component { bbox: merged, moving };
            } else {
                // The union would swallow still cells outside both boxes, so the
                // smaller component loses its stream instead: its cells inside the
                // survivor's box are carried by the survivor, and the rest are crisp.
                let loser = if (components[j].moving, components[j].bbox.area())
                    < (components[i].moving, components[i].bbox.area())
                {
                    j
                } else {
                    i
                };
                components.remove(loser);
            }
            continue;
        }
        if components.len() <= max {
            break;
        }
        let mut best: Option<(usize, usize, u32)> = None;
        for i in 0..components.len() {
            for j in i + 1..components.len() {
                let merged = components[i].bbox.union(&components[j].bbox);
                let moving = components[i].moving + components[j].moving;
                if merged.area() > MERGE_WASTE * moving {
                    continue;
                }
                // No two boxes overlap by the time this runs, so the union is at
                // least the sum of the parts and the subtraction cannot wrap.
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

/// The lowest stream id none of `taken` is using, or `None` when the wire has none
/// left.
///
/// The range is the *wire's* [`batch::MAX_STREAMS`] rather than the policy's
/// [`MAX_STREAMS`], and the difference is what makes a retune safe: a stream being
/// replaced keeps its id until the moment it is dropped, so a retune can have both
/// the outgoing and the incoming set in hand at once — more ids in play than there
/// are streams alive at the end of it.
fn free_id(taken: &[u8]) -> Option<u8> {
    (0..batch::MAX_STREAMS).find(|id| !taken.contains(id))
}

/// The mirror, the live streams, and the cells they owe.
pub struct Regions {
    policy: Policy,
    /// The 1–100 dial a new stream starts at — the config's, unless the congestion loop
    /// has moved it. Without that a region appearing on a struggling link would start at
    /// full quality and make the struggle worse.
    quality: u8,
    /// The chroma sampling every stream is built with — the target's, for its whole
    /// session; nothing moves it.
    chroma: Chroma,
    /// The desktop, learned from [`crate::protocol::ServerMsg::Resize`]. `None` until
    /// the engine has announced one, which it always does before any damage.
    size: Option<(u16, u16)>,
    /// The lattice every cell key here is on, learned with `size` from the same
    /// announcement: [`TileGrid::at`] its scale.
    grid: TileGrid,
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
    /// Stream ids whose region has ended and that the client has not been told about
    /// yet — see [`Self::drain_ended`].
    ended: Vec<u8>,
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
    pub fn new(policy: Policy, quality: u8, chroma: Chroma, mark: Option<Mark>) -> Self {
        Self {
            policy,
            quality,
            chroma,
            size: None,
            grid: TileGrid::ONE,
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

    /// Adopt the desktop the client is about to be told about.
    ///
    /// Cannot fail, and is all that happens on the message path. A different size
    /// drops everything: every cell key means somewhere else on a new framebuffer,
    /// and an encoder cannot change picture size without starting over anyway. The
    /// debts go with them because the shadow is resized in the same breath — nothing
    /// is owed on a framebuffer that no longer exists. A new `grid` at the same size
    /// is the same event: a density change re-keys every cell.
    pub fn want(&mut self, w: u16, h: u16, grid: TileGrid) {
        if self.size != Some((w, h)) || self.grid != grid {
            self.size = Some((w, h));
            self.grid = grid;
            self.mirror = None;
            self.spare = None;
            self.staged.clear();
            self.forget();
        }
    }

    /// Drop every stream and every debt, keeping the mirror.
    ///
    /// For a repaint or a reattach: every pixel is about to be re-sent at
    /// the base encode, which discharges anything that was owed, and a client with
    /// nothing on screen cannot decode from the middle of a stream.
    pub fn forget(&mut self) {
        // A round away on a worker carries streams and a mirror this is dropping;
        // the epoch is what tells `put_back` not to bring them back from the dead.
        self.epoch += 1;
        // The streams this drops are ends the client is owed like any other, and the
        // only ones nothing else is ever going to say. A resize is not an attachment
        // boundary — the page keeps its decoder table across one, and only `clear`
        // empties it — so an id that was live here and is not handed back afterwards
        // would sit on a decode session for the rest of the session. Which is the
        // case this dial cannot afford: the ids that come back cost nothing, and the
        // ones that do not are exactly what an end is for.
        //
        // Recorded here, delivered later. `want` runs this before the `Resize` is
        // even queued, but what it records is drained by the next frame's `retune`
        // or the next cleanup tick's `expire`, both behind that `Resize` on the
        // ordered path — so the client hears the ends after the resize, against a
        // decoder table the resize left intact.
        //
        // Under [`Policy::Whole`] there is one stream, it is rebuilt on the same id
        // by the next blit, and nothing ever drains this — so nothing is recorded.
        if self.policy == Policy::Moving {
            for live in &self.live {
                record_end(&mut self.ended, live.id);
            }
        }
        self.live.clear();
        self.covered.clear();
        self.debts.clear();
        self.retuned_at = None;
    }

    /// Stream ids whose region is over, taken once.
    ///
    /// A client keys its decoders by id and is otherwise never told that one is
    /// finished: an id simply stops arriving, and the decoder sits there holding
    /// whatever the platform gave it — which for a hardware decoder is a decode
    /// session, a resource there are few of and which the next stream to start will
    /// be asking for. So the end is said, from the side that knows it, rather than
    /// guessed at by a timer on the side that does not.
    ///
    /// **Ordered against the units, or it is worse than nothing.** Ids are reused as
    /// regions come and go, so an end that overtakes the units of the next stream on
    /// that id would close a decoder that had just been built for a picture still
    /// arriving. Both callers drain this on the ordered path and ahead of the round
    /// they drain it for: [`Self::retune`] runs before its round is taken, and
    /// [`Self::expire`] inside the same task the units go out on.
    ///
    /// The ends [`Self::want`] records are recorded earlier still — before the resize
    /// that caused them is even queued — but delivered by the same two drains, behind
    /// it; and an id a later retune hands straight back is dropped from here by that
    /// retune rather than said.
    pub fn drain_ended(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.ended)
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

    /// Whether any cell at all is under a stream.
    ///
    /// The question [`Self::covers`] answers for a whole rectangle at once, and the
    /// only cheap answer available before a walk: no live stream means no cell can
    /// be covered, so a caller about to ask cell by cell may stop here.
    pub fn covering(&self) -> bool {
        !self.covered.is_empty()
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
                .ok_or_else(|| anyhow::anyhow!("the video mirror was asked for pixels before a desktop size"))?;
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
        self.stage(rect);
        // A whole-desktop stream is created here rather than in `retune`, because
        // there is no decision to make: the region is the desktop, and the first
        // pixels to arrive are the ones it exists to carry. Not while a round is
        // out, though — the live table is empty then because the stream is away
        // encoding, not because there is none.
        if self.policy == Policy::Whole && self.live.is_empty() && !self.round_out {
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

    /// Note that `rect` now differs between the current mirror and the spare.
    ///
    /// Capped: past [`STAGED_CAP`] the list collapses to one bounding box, so the
    /// sync copies some slop that did not change — bounded by what the shadow
    /// already paid to compare — where an unbounded list on a target that never
    /// takes a round (streams configured, nothing ever moving) would grow forever.
    fn stage(&mut self, rect: Rect) {
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
    /// A no-op under [`Policy::Whole`], and at most once per `RETUNE` otherwise.
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

        // A region with motion worth a stream in it is a region still in use, whatever
        // the cap and the veto decide to do about it. Motion *not* worth a stream is
        // not: what would not start a stream does not keep one alive either, so a
        // video that pauses under a buffering spinner ends its stream and sharpens,
        // exactly as it would with no spinner — and the spinner goes to the tiles,
        // where the gate would have put it anyway.
        let worthy = components(moving);
        let busy: HashSet<(u16, u16)> = moving
            .iter()
            .copied()
            .filter(|cell| worthy.iter().any(|c| c.bbox.contains(*cell)))
            .collect();
        for live in &mut self.live {
            if live.cells.iter().any(|cell| busy.contains(cell)) {
                live.moving_at = now;
            }
        }

        // The exact boxes, and nothing around them: a stream built here covers only
        // what is moving, and the still tiles beside it are never re-sent lossily. A
        // stream kept from an earlier retune is the exception — it keeps its whole
        // rectangle, margin included, and every cell of it stays covered and owed.
        let wanted: Vec<Rect> = merge(worthy, MAX_STREAMS)
            .into_iter()
            .filter_map(|bbox| bbox.to_rect(w, h, self.grid))
            .collect();

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
                // it must keep it, because a client would read a new id as a new
                // region and start a decoder with no history to decode from.
                let taken: Vec<u8> =
                    next.iter().chain(old.iter()).map(|live| live.id).collect();
                next.push(self.build(rect, now, &taken)?);
            }
        }
        debug_assert!(
            next.iter().enumerate().all(|(i, a)| {
                next[i + 1..].iter().all(|b| a.rect.intersect(&b.rect).is_none())
            }),
            "two live streams overlap"
        );
        // A region that has stopped moving keeps its stream for a moment — a video
        // pauses, a pointer comes to rest — and then ends, which is what makes its
        // cells due for a crisp re-send.
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

        if self.live.len() != next.len()
            || self.live.iter().zip(&next).any(|(a, b)| a.rect != b.rect)
        {
            log::debug!(
                "video regions: {} cell(s) moving -> {}",
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
        // picture about to arrive. Telling the client to close it would throw away a
        // hardware decode session it could have kept.
        self.ended.retain(|id| !self.live.iter().any(|live| live.id == *id));
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

    /// Start a stream over `rect` and record what it will owe.
    fn start(&mut self, rect: Rect, now: Instant) -> anyhow::Result<()> {
        let taken: Vec<u8> = self.live.iter().map(|live| live.id).collect();
        let live = self.build(rect, now, &taken)?;
        self.covered.extend(live.cells.iter().copied());
        self.live.push(live);
        Ok(())
    }

    /// Build a stream over `rect`, with an id none of `taken` is using.
    ///
    /// The ids in use are passed in rather than read off `self.live`, because the
    /// caller that matters — [`Self::retune`] — has taken the live table *out* of
    /// `self` for the duration, so a stream built from what is left there would
    /// always be handed id 0. Two live regions sharing an id is not a cosmetic
    /// fault: a client keys its decoders by it, so both chains would be fed to one
    /// decoder and neither would decode.
    fn build(&mut self, rect: Rect, now: Instant, taken: &[u8]) -> anyhow::Result<Live> {
        let id = free_id(taken).ok_or_else(|| {
            anyhow::anyhow!(
                "video regions: no stream id left for a {}x{} region; {} are in use and \
                 the wire allows {}",
                rect.w(),
                rect.h(),
                taken.len(),
                batch::MAX_STREAMS
            )
        })?;
        let mirror = self.mirror_mut()?.coded();
        // At `self.quality` rather than the config's: a region that appears while the
        // link is behind starts where the link left off.
        let stream = Stream::new(rect, mirror, self.quality, self.chroma)?;
        let grid = self.grid;
        let cells: Vec<(u16, u16)> = rect.cells(grid).map(|cell| cell.cell_key(grid)).collect();
        // Every cell of the region is owed from the moment it is streamed, including
        // the ones that are not moving: the stream codes them lossily whether they
        // change or not, and nothing else is going to send them.
        if self.policy == Policy::Moving {
            for cell in &cells {
                self.debts.entry(*cell).or_insert(now);
            }
        }
        Ok(Live {
            id,
            rect,
            cells,
            stream,
            // What the encoder was just built at, per the comment above.
            quality: self.quality,
            // Its whole region is owed: nothing has carried these pixels yet.
            dirty: true,
            keyframe_owed: true,
            carried: false,
            announced: None,
            moving_at: now,
        })
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
        for piece in sent.cells(self.grid) {
            let key = piece.cell_key(self.grid);
            if CellBox::of(key).to_rect(w, h, self.grid).is_some_and(|cell| sent.contains(&cell)) {
                self.debts.remove(&key);
            }
        }
    }

    /// Take up to `max` cells that have been owed for `idle`, oldest first, as
    /// rectangles for the caller to re-encode at the base quality — one per run of
    /// neighbouring cells in a row, not one per cell.
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
        // Oldest first, and among cells owed since the same instant — every cell a
        // stream carried, once it ends — row by row rather than column by column, so
        // that what the budget cuts off is whole stripes: a stopped video sharpens
        // top-down a stripe at a time, not left-to-right in slivers a cell wide.
        ready.sort_unstable_by_key(|((col, row), at)| (*at, *row, *col));
        ready.truncate(max);
        let mut cells: Vec<(u16, u16)> = ready
            .into_iter()
            .map(|(cell, _)| {
                self.debts.remove(&cell);
                cell
            })
            .collect();
        // The tickful goes out as runs: consecutive columns of one row are one
        // rectangle, so a stopped video is restored a stripe at a time rather than a
        // cell at a time, and pays PNG's fixed cost once per stripe. Age chose the
        // cells; it does not also have to order the tiles, which all leave together.
        cells.sort_unstable_by_key(|(col, row)| (*row, *col));
        let mut runs: Vec<CellBox> = Vec::new();
        for (col, row) in cells {
            match runs.last_mut() {
                Some(run) if run.r0 == row && run.c1.checked_add(1) == Some(col) => run.c1 = col,
                _ => runs.push(CellBox::of((col, row))),
            }
        }
        runs.into_iter().filter_map(|run| run.to_rect(w, h, self.grid)).collect()
    }

    /// Take the mirror and every stream, for an encode on a blocking worker.
    ///
    /// `None` when no stream has anything waiting — so a still screen costs neither
    /// the hand-off nor the encode — and while a previous round is still away, which
    /// is what keeps the inter-frame chain serial now that the caller no longer
    /// waits the encode out. Everything is taken, not just the dirty ones: an
    /// encoder cannot be borrowed across a `spawn_blocking`, and leaving half of
    /// them behind would make [`Self::covers`] answer differently depending on
    /// whether a round is out.
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

    /// Put back what [`Self::take_round`] took, and note that the cells those streams
    /// cover have just been carried.
    ///
    /// The debts are refreshed here rather than where the units are pushed, which is
    /// the conservative side of the same argument the still path's stash makes: a
    /// cell whose stream produced a frame this round cannot also be idle, so nothing
    /// can clean it up from underneath a unit still on its way to the socket.
    pub fn put_back(&mut self, round: Round, now: Instant) {
        self.round_out = false;
        if round.epoch != self.epoch {
            // The desktop was resized, or everything forgotten, while this round was
            // encoding: its mirror and streams describe a framebuffer that no longer
            // exists. Its access units were still delivered — the ordered queue puts
            // them ahead of the resize the client hears about — but nothing here is
            // worth keeping.
            //
            // Except the ids. `forget` records the ends it can see, and a round that
            // is out has taken the live table with it, so these are the streams it
            // could not: this is the last place that knows a client is holding a
            // decoder for them.
            if self.policy == Policy::Moving {
                for live in &round.live {
                    record_end(&mut self.ended, live.id);
                }
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
            // And the congestion loop may have moved the dial while the streams were
            // out of reach — compared against what each encoder is *known* to run at,
            // so an unchanged dial costs nothing here. A failure to retune keeps the
            // quality the stream already has, the same answer `adjust` gives, and
            // leaves `live.quality` alone so the next round tries again.
            if live.quality != self.quality {
                match live.stream.set_quality(self.quality) {
                    Ok(()) => live.quality = self.quality,
                    Err(e) => log::warn!(
                        "video regions: a returned stream refused quality {}: {e:#}",
                        self.quality
                    ),
                }
            }
        }
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

    /// Move every live stream's quality, for the congestion loop.
    ///
    /// One link, one verdict: the streams share a socket, and a policy that treated
    /// them separately would be reasoning about a bottleneck none of them can see on
    /// its own.
    pub fn set_quality(&mut self, quality: u8) -> anyhow::Result<()> {
        self.quality = quality;
        for live in &mut self.live {
            live.stream.set_quality(quality)?;
            live.quality = quality;
        }
        Ok(())
    }

    /// The dial every live stream is encoding at, for the totals.
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// Arm a keyframe on every live stream. Its callers are exactly the moments a
    /// client's decoder has to be able to start over.
    pub fn force_keyframes(&mut self) {
        for live in &mut self.live {
            live.keyframe_owed = true;
            // And the format is owed again, for the same reason the keyframe is: the client this
            // is for may be one that has never seen either. A `VideoFormat` costs a short text
            // frame, against a keyframe's hundreds of kilobytes, so there is nothing to weigh.
            live.announced = None;
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
/// One round is one frame of the desktop however many regions it took, and what the
/// pipelined queue pays for it is its **wall-clock**, not its total CPU — no new
/// round can be taken while one is out, so a slow round is what caps the frame
/// rate. The dirty streams therefore encode concurrently ([`Self::encode`]): with
/// cores free to run them, a round costs its slowest stream rather than the sum of
/// them, and under CPU contention the scheduler serializes some of that overlap and
/// the cost degrades back toward the sum — which is what the serial loop always
/// cost, so the bound never inverts.
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
        assert!(
            !waited.timed_out(),
            "the round's streams did not encode concurrently: a sibling never started"
        );
    }
}

/// [`Rendezvous::arrive`] when one is armed; nothing otherwise. A function rather
/// than an inline `if let`, so the call sites inside `encode`'s closures stay one
/// `cfg`-gated line each.
#[cfg(test)]
fn rendezvous_arrive(rendezvous: &Option<std::sync::Arc<Rendezvous>>) {
    if let Some(rendezvous) = rendezvous {
        rendezvous.arrive();
    }
}

impl Round {
    /// Encode every stream with pixels waiting. Blocking: call it on a worker.
    ///
    /// The dirty streams encode **concurrently** — one scoped thread per stream past
    /// the first, which runs on the worker itself, since most rounds have exactly
    /// one. The sharing is what the types already promise: every `Stream` is `Send`
    /// (the whole round crosses a `spawn_blocking`), each thread holds one stream
    /// `&mut`, and the mirror is read everywhere and written nowhere, so
    /// `std::thread::scope` proves the whole arrangement at compile time with
    /// nothing `unsafe`. Units keep stream order whichever encode finishes first,
    /// and an error is returned only after every stream has had its attempt — the
    /// session ends on it either way, but no stream is left half-done by a
    /// sibling's failure.
    ///
    /// A stream the encoder produced no bitstream for keeps its dirty flag and its
    /// keyframe, so those pixels ride the next round — which is what stops a frame
    /// that produced nothing from becoming pixels the client never gets.
    pub fn encode(&mut self) -> anyhow::Result<Produced> {
        self.mirror.pad_edges();
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

        let outcomes: Vec<(usize, anyhow::Result<Option<AccessUnit>>)> =
            std::thread::scope(|scope| {
                let mut waiting = waiting.into_iter();
                let first = waiting.next();
                let spawned: Vec<_> = waiting
                    .map(|(at, live)| {
                        #[cfg(test)]
                        let rendezvous = rendezvous.clone();
                        let handle = scope.spawn(move || {
                            #[cfg(test)]
                            rendezvous_arrive(&rendezvous);
                            live.stream.encode(mirror, mark)
                        });
                        (at, handle)
                    })
                    .collect();
                let mut outcomes = Vec::with_capacity(spawned.len() + 1);
                if let Some((at, live)) = first {
                    #[cfg(test)]
                    rendezvous_arrive(&rendezvous);
                    outcomes.push((at, live.stream.encode(mirror, mark)));
                }
                for (at, handle) in spawned {
                    // A panicking encode is re-raised rather than absorbed: release
                    // builds abort on panic anyway, and a debug run should die where
                    // the fault is.
                    let outcome =
                        handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                    outcomes.push((at, outcome));
                }
                outcomes
            });

        let mut produced =
            Produced { formats: Vec::new(), units: Vec::with_capacity(outcomes.len()) };
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
            // The announcement goes out ahead of every unit in this round, which is the
            // contract `ServerMsg::VideoFormat` states.
            if let Some(decode) = live.stream.decode_string()
                && live.announced.as_deref() != Some(decode)
            {
                live.announced = Some(decode.to_owned());
                produced.formats.push(Format {
                    stream: live.id,
                    decode: decode.to_owned(),
                });
            }
            produced.units.push(VideoUnit {
                stream: live.id,
                x: live.rect.left,
                y: live.rect.top,
                w: live.rect.w(),
                h: live.rect.h(),
                keyframe: unit.keyframe,
                data: unit.data,
            });
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(produced),
        }
    }

    /// Streams whose encode yielded no bitstream. Must stay zero: `skip_frames(false)`
    /// should make it unreachable, and a non-zero count is how we would find out that
    /// it is not.
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

/// What one round produced: the announcements owed, and then the access units.
///
/// Two lists rather than one interleaved sequence, because the order that matters is only
/// "every announcement before every unit" — which is stronger than the contract needs and
/// simpler than tracking which unit each one belongs in front of.
pub struct Produced {
    pub formats: Vec<Format>,
    pub units: Vec<VideoUnit>,
}

/// One stream's `ServerMsg::VideoFormat`, owed because it is new or because its configuration
/// string changed.
pub struct Format {
    pub stream: u8,
    pub decode: String,
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

    /// Two components' cells cannot overlap, but their boxes can: a cell tucked into
    /// an L's corner is its own region inside the L's box. A cell inside two live
    /// regions is a cell two streams both carry, so the overlap is merged away —
    /// whatever the cap — and separated blocks are left exactly as they are.
    #[test]
    fn regions_are_disjoint_even_where_their_boxes_nest() {
        let mut cells = vec![(0, 0), (0, 1), (0, 2), (1, 2), (2, 2), (2, 0)];
        cells.extend(block(4, 0, 5, 2));
        cells.extend(block(0, 8, 2, 9));
        let inside = |boxes: &[CellBox], cell: (u16, u16)| {
            boxes.iter().any(|b| {
                b.c0 <= cell.0 && cell.0 <= b.c1 && b.r0 <= cell.1 && cell.1 <= b.r1
            })
        };
        for max in 1..=MAX_STREAMS {
            let boxes = coalesce(&cells, max);
            assert!(boxes.len() <= max, "the merge invented a stream at max {max}");
            for (i, a) in boxes.iter().enumerate() {
                for b in &boxes[i + 1..] {
                    assert!(!a.overlaps(b), "at max {max}, {a:?} overlaps {b:?}");
                }
            }
            // The corner cell is inside the L's box whichever way the cap goes, so
            // it is streamed at every cap.
            assert!(inside(&boxes, (2, 0)), "at max {max}, the corner cell lost its region");
        }
        // And the boxes are exactly the regions' own: the L with its corner cell, and
        // the two blocks, none of them grown by a cell.
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

    /// And past the veto the smaller component goes to the still codecs, so no cell
    /// outside both original boxes is streamed — the union would have swallowed a
    /// screenful of still ones for the sake of a line.
    #[test]
    fn a_partial_overlap_past_the_waste_veto_drops_the_smaller_region() {
        // A T: eleven cells along row 0 and ten down column 5, in a 11×11 box.
        let mut cells: Vec<(u16, u16)> = (0..=10).map(|c| (c, 0)).collect();
        cells.extend((1..=10).map(|r| (5, r)));
        // A line down column 0 from row 2, reaching two rows past the T's box, so
        // the boxes overlap without either containing the other.
        cells.extend((2..=12).map(|r| (0, r)));
        let boxes = coalesce(&cells, MAX_STREAMS);
        assert_eq!(boxes, vec![boxed(0, 0, 10, 10)], "the union was taken");
        let inside = |cell: (u16, u16)| {
            boxes.iter().any(|b| {
                b.c0 <= cell.0 && cell.0 <= b.c1 && b.r0 <= cell.1 && cell.1 <= b.r1
            })
        };
        for outside in [(1, 11), (10, 11), (10, 12), (0, 11), (0, 12)] {
            assert!(!inside(outside), "{outside:?} is outside both boxes and was streamed");
        }
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

    /// And when no merge is cheap enough, the smallest region loses its stream rather
    /// than the screen being swallowed. Its cells take the base encode, which is
    /// crisp and merely expensive — the safe direction.
    #[test]
    fn a_merge_that_would_swallow_the_screen_drops_the_smallest_region_instead() {
        let mut cells = block(0, 0, 2, 2);
        cells.extend(block(30, 20, 34, 20));
        let regions = coalesce(&cells, 1);
        assert_eq!(
            regions,
            vec![boxed(0, 0, 2, 2)],
            "one far strip took the whole screen into a stream"
        );
    }

    /// Fewer than `MIN_STREAM_CELLS` moving cells is a spinner, not a video: it stays
    /// on the still codecs however far it is from anything else, and a cap that has
    /// room for it does not change that.
    #[test]
    fn a_component_too_small_to_be_a_video_is_not_streamed() {
        // A 2×2 block is the biggest thing the gate holds back.
        assert!(coalesce(&block(0, 0, 1, 1), MAX_STREAMS).is_empty());
        assert!(coalesce(&[(0, 0), (1, 0), (2, 0), (3, 0)], MAX_STREAMS).is_empty());
        // One more cell and it is a region.
        assert_eq!(coalesce(&block(0, 0, 4, 0), MAX_STREAMS), vec![boxed(0, 0, 4, 0)]);
    }

    /// The case the gate exists for: a spinner in one corner while a video plays in
    /// the other. The video streams, the spinner is a tile, and however many spinners
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
    /// still cell there would be: the gate leaves it out of the decision, and the box
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
    /// makes "a cell is in at most one stream" true by construction.
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

    /// A cell tucked into an L's corner is its own region inside the L's box, and the
    /// L's stream would carry it whatever was decided — so the pair is merged at
    /// every cap, and the veto is never asked about a union that adds no cell.
    #[test]
    fn a_nested_box_is_merged_whatever_the_cap() {
        let mut cells = block(0, 0, 6, 0);
        cells.extend(block(0, 1, 0, 6));
        // A strip a cell in from the L's arms, so 4-connected to none of it, and
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

    /// A 320×128 desktop — five cells across, two down — with its mirror already
    /// built, which is what a first blit does. Five across because a row of it is
    /// the smallest region the gate lets stream.
    async fn regions() -> Regions {
        sized(320, 128).await
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

    /// The same, at whatever size a test needs cells for. A cell is 64×64 at 1× and a
    /// region needs `MIN_STREAM_CELLS` of them, so a test about two *separate*
    /// regions needs room for two blocks that size with a quiet cell between:
    /// neighbouring cells coalesce into one.
    async fn sized(w: u16, h: u16) -> Regions {
        let mut regions = Regions::new(Policy::Moving, 60, Chroma::Subsampled, None);
        regions.want(w, h, TileGrid::ONE);
        let bytes = usize::from(w) * usize::from(h) * 3;
        regions
            .blit(Rect { left: 0, top: 0, right: w - 1, bottom: h - 1 }, &vec![0; bytes])
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
        regions.retune(&top_row(), t0).expect("a stream for the moving cells");
        let first = only_rect(&regions);

        // Twice as much is moving, but not yet.
        regions.retune(&both_rows(), t0 + RETUNE / 2).expect("no work");
        assert_eq!(only_rect(&regions), first, "geometry moved inside the interval");
    }

    /// Shrinking is free: the idle margin codes as skipped macroblocks, where a
    /// restart costs an encoder and a keyframe.
    #[tokio::test]
    async fn a_shrinking_region_keeps_its_stream() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&both_rows(), t0).expect("a stream");
        let tall = only_rect(&regions);
        assert_eq!(tall.h(), 128);

        regions.retune(&top_row(), t0 + RETUNE).expect("no restart");
        assert_eq!(only_rect(&regions), tall, "a shrinking region paid for a new encoder");
    }

    /// The other side of shrinking: motion that would not start a stream does not
    /// keep one alive. A video that pauses under a spinner ends its stream, its cells
    /// come due, and the spinner is a tile — the same as a pause with no spinner.
    #[tokio::test]
    async fn motion_below_the_gate_does_not_keep_a_stream_alive() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&both_rows(), t0).expect("a stream");
        let id = regions.live[0].id;

        // Only a 2×2 block still moving inside it, for as long as a stream may idle.
        regions.retune(&block(0, 0, 1, 1), t0 + STREAM_IDLE).expect("no work");
        assert!(regions.live.is_empty(), "a spinner kept a video's stream alive");
        assert_eq!(regions.drain_ended(), vec![id]);
        assert!(!regions.covers((0, 0)), "a cell no stream carries is still covered");
    }

    /// One rectangle that has split into two regions keeps one stream, carrying
    /// both. Building a second stream for the second region would put it inside the
    /// first — two streams over one cell, the delivery rule broken.
    #[tokio::test]
    async fn a_kept_stream_carries_every_region_inside_it() {
        // Five cells across, three down.
        let mut regions = sized(320, 192).await;
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
    #[tokio::test]
    async fn a_region_straddling_a_kept_stream_ends_it() {
        // Ten cells across, three down; the first stream covers the top two rows.
        let mut regions = sized(640, 192).await;
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
            rects.contains(&boxed(6, 1, 9, 2).to_rect(640, 192, TileGrid::ONE).expect("a rectangle")),
            "the straddling region did not get its own stream: {rects:?}"
        );
        assert!(
            regions.live.iter().all(|live| live.keyframe_owed),
            "a client cannot start on either new picture"
        );
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
    #[tokio::test]
    async fn a_growing_region_starts_a_new_stream() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&top_row(), t0).expect("a stream");
        assert_eq!(only_rect(&regions).h(), 64);

        regions.retune(&both_rows(), t0 + RETUNE).expect("a taller stream");
        let grown = only_rect(&regions);
        assert_eq!(grown.h(), 128, "the stream kept a rectangle its region outgrew");
        assert!(regions.live[0].keyframe_owed, "a client cannot start on the new picture");
    }

    /// A stream is otherwise indistinguishable, from the client's side, from one whose
    /// region is merely still — so the end is said. What it buys is the decoder, and
    /// with it whatever the platform gave the decoder.
    #[tokio::test]
    async fn a_stream_that_ends_names_its_id() {
        let mut regions = regions().await;
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
    /// session the client could have kept.
    #[tokio::test]
    async fn an_id_handed_straight_back_is_not_an_end() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&top_row(), t0).expect("a stream");
        let id = regions.live[0].id;
        regions.drain_ended();

        regions.retune(&both_rows(), t0 + RETUNE).expect("a taller stream");
        assert_eq!(regions.live.len(), 1);
        assert_eq!(regions.live[0].id, id, "the replacement took the id back");
        assert!(regions.drain_ended().is_empty(), "the client was told to close a live decoder");
    }

    /// A resize drops every stream, and a client does *not* drop its decoders for one
    /// — only an attachment boundary does that. So the ids have to be said, or a
    /// desktop that resizes and then settles into fewer regions leaves the rest of
    /// them holding a decode session for the rest of the session.
    #[tokio::test]
    async fn a_resize_names_the_streams_it_dropped() {
        let mut regions = sized(1600, 128).await;
        // Two blocks with a gap, so they coalesce as two regions rather than one.
        regions.retune(&two_blocks(),Instant::now()).expect("two streams");
        let mut ids: Vec<u8> = regions.live.iter().map(|live| live.id).collect();
        assert_eq!(ids.len(), 2);
        regions.drain_ended();

        regions.want(1600, 256, TileGrid::ONE);
        assert!(regions.live.is_empty(), "the resize kept a stream on the old framebuffer");
        ids.sort_unstable();
        let mut said = regions.drain_ended();
        said.sort_unstable();
        assert_eq!(said, ids, "the decoders those ids built were never handed back");
    }

    /// The same, for the streams a resize cannot see: a round that is out has taken
    /// the live table with it, and its epoch says not to bring it back.
    #[tokio::test]
    async fn a_resize_over_a_round_in_flight_names_that_round_s_streams() {
        let mut regions = sized(1600, 128).await;
        let t0 = Instant::now();
        regions.retune(&two_blocks(),t0).expect("two streams");
        regions.drain_ended();
        let round = regions.take_round().expect("dirty streams");
        let mut ids: Vec<u8> = round.live.iter().map(|live| live.id).collect();
        assert_eq!(ids.len(), 2);

        regions.want(1600, 256, TileGrid::ONE);
        assert!(regions.drain_ended().is_empty(), "the resize saw a live table it did not have");
        regions.put_back(round, t0 + RETUNE);
        ids.sort_unstable();
        let mut said = regions.drain_ended();
        said.sort_unstable();
        assert_eq!(said, ids, "a discarded round's decoders were never handed back");
    }

    /// Every live stream needs an id of its own, because a client keys its decoders
    /// by it: two regions sharing one would feed two chains to one decoder and
    /// neither would decode. The trap is that a retune holds the live table outside
    /// `self`, so an allocator reading `self.live` sees nothing in use.
    #[tokio::test]
    async fn two_regions_born_in_one_retune_get_different_ids() {
        let mut regions = sized(1600, 128).await;
        // Two blocks with a gap between them, so they coalesce as two regions.
        regions.retune(&two_blocks(),Instant::now()).expect("two streams");
        let ids: Vec<u8> = regions.live.iter().map(|live| live.id).collect();
        assert_eq!(regions.live.len(), 2, "expected two regions, got {ids:?}");
        assert_ne!(ids[0], ids[1], "both regions were sent as the same stream");
    }

    /// A round with several dirty streams encodes all of them, **concurrently** —
    /// the rendezvous holds each encode until every one has started, which a serial
    /// loop can never satisfy — and the contract that survives any scheduling still
    /// holds: every stream produces its unit, the units keep stream order whichever
    /// encode finished first, and each announces its format ahead of its first unit.
    #[tokio::test]
    async fn a_round_with_two_streams_produces_both_units_in_stream_order() {
        let mut regions = sized(1600, 128).await;
        regions.retune(&two_blocks(),Instant::now()).expect("two streams");
        let ids: Vec<u8> = regions.live.iter().map(|live| live.id).collect();
        assert_eq!(ids.len(), 2, "expected two regions, got {ids:?}");

        let mut round = regions.take_round().expect("both streams are dirty from birth");
        round.expect_overlap(2);
        let produced = round.encode().expect("an encode");
        assert_eq!(round.skipped(), 0, "a stream's pixels were lost to the fan-out");
        let streams: Vec<u8> = produced.units.iter().map(|unit| unit.stream).collect();
        assert_eq!(streams, ids, "units must keep stream order whichever encode finishes first");
        assert!(produced.units.iter().all(|unit| unit.keyframe), "each stream's first unit");
        assert_eq!(produced.formats.len(), 2, "each stream announces before its first unit");
        regions.put_back(round, Instant::now());
        assert!(!regions.dirty(), "both streams were carried");
    }

    /// And a stream that survives a retune keeps the id it had, whatever is built
    /// alongside it: a new id would read as a new region, and the client would start
    /// a decoder with none of the history the next frame is expressed against.
    #[tokio::test]
    async fn a_carried_stream_keeps_its_id_when_another_is_built_beside_it() {
        let mut regions = sized(1600, 128).await;
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

    /// The roadmap's third question, answered: the stream ends, and every cell it
    /// covered is owed a crisp re-send.
    #[tokio::test]
    async fn a_region_that_stops_moving_ends_and_its_cells_come_due() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&top_row(), t0).expect("a stream");
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
        regions.retune(&top_row(), t0).expect("a stream");
        regions.expire(t0 + STREAM_IDLE);
        let later = t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS;

        // The whole row is owed, first cell included: a run from the left edge.
        regions.discharge(Rect { left: 0, top: 0, right: 15, bottom: 15 });
        assert_eq!(
            regions.due(later, CLEANUP_IDLE_FOR_TESTS, 8),
            vec![Rect { left: 0, top: 0, right: 319, bottom: 63 }],
            "a sliver discharged the whole cell's debt"
        );

        // And a send that covers the cell outright does discharge it: the run now
        // starts one cell in.
        regions.retune(&top_row(), later).expect("a stream again");
        regions.expire(later + STREAM_IDLE);
        regions.discharge(Rect { left: 0, top: 0, right: 63, bottom: 63 });
        assert_eq!(
            regions.due(later + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS, CLEANUP_IDLE_FOR_TESTS, 8),
            vec![Rect { left: 64, top: 0, right: 319, bottom: 63 }],
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
        // The top row and one cell of the bottom row, so the box covers four cells
        // that never moved at all.
        let mut cells = top_row();
        cells.push((4, 1));
        regions.retune(&cells, t0).expect("a stream");
        assert_eq!(only_rect(&regions).h(), 128);
        regions.expire(t0 + STREAM_IDLE);
        let mut due =
            regions.due(t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS, CLEANUP_IDLE_FOR_TESTS, 16);
        due.sort_unstable_by_key(|rect| rect.top);
        assert_eq!(
            due,
            vec![
                Rect { left: 0, top: 0, right: 319, bottom: 63 },
                Rect { left: 0, top: 64, right: 319, bottom: 127 },
            ],
            "a cell inside the stream's rectangle was owed nothing"
        );
    }

    /// Cells that come due together and sit side by side in one row are one
    /// rectangle: the cell is the unit of debt, not of the tile that pays it.
    #[tokio::test]
    async fn due_cells_in_one_row_are_restored_as_a_run() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&both_rows(), t0).expect("a stream over every cell");
        assert_eq!(only_rect(&regions), Rect { left: 0, top: 0, right: 319, bottom: 127 });
        regions.expire(t0 + STREAM_IDLE);
        let mut due =
            regions.due(t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS, CLEANUP_IDLE_FOR_TESTS, 16);
        due.sort_unstable_by_key(|rect| rect.top);
        assert_eq!(
            due,
            vec![
                Rect { left: 0, top: 0, right: 319, bottom: 63 },
                Rect { left: 0, top: 64, right: 319, bottom: 127 },
            ],
            "ten cells in two rows should be two stripes"
        );
        assert!(
            regions
                .due(t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS, CLEANUP_IDLE_FOR_TESTS, 16)
                .is_empty()
        );
    }

    /// The budget cuts the tickful at a row, not at a column: the cells a stream
    /// carried all came due at once, and a tick that can pay five of ten must pay
    /// the top stripe whole rather than the left half of both — which is five
    /// tiles of a cell each, and a video that sharpens sideways in slivers.
    #[tokio::test]
    async fn a_short_budget_takes_whole_stripes_first() {
        let mut regions = regions().await;
        let t0 = Instant::now();
        regions.retune(&both_rows(), t0).expect("a stream over every cell");
        regions.expire(t0 + STREAM_IDLE);
        let settled = t0 + STREAM_IDLE + CLEANUP_IDLE_FOR_TESTS;
        assert_eq!(
            regions.due(settled, CLEANUP_IDLE_FOR_TESTS, 5),
            vec![Rect { left: 0, top: 0, right: 319, bottom: 63 }],
            "five of ten cells should be the top stripe"
        );
        assert_eq!(
            regions.due(settled, CLEANUP_IDLE_FOR_TESTS, 5),
            vec![Rect { left: 0, top: 64, right: 319, bottom: 127 }],
            "the next five should be the bottom one"
        );
        assert!(regions.due(settled, CLEANUP_IDLE_FOR_TESTS, 5).is_empty());
    }

    /// What `crate::encode` passes as the cleanup's idle threshold. Its own constant
    /// lives there, with the rest of the cleanup policy.
    const CLEANUP_IDLE_FOR_TESTS: Duration = Duration::from_millis(500);

    /// The geometry theorem, from the coalescing side: a box becomes a rectangle
    /// whose origin is on the grid and whose size is odd only where the desktop is.
    #[test]
    fn a_region_is_even_unless_the_desktop_is_odd_at_that_edge() {
        let interior = boxed(1, 1, 2, 2).to_rect(1919, 1079, TileGrid::ONE).expect("a rectangle");
        assert_eq!((interior.left, interior.top), (64, 64));
        assert_eq!((interior.w() % 2, interior.h() % 2), (0, 0));

        let edge = boxed(29, 16, 29, 16).to_rect(1919, 1079, TileGrid::ONE).expect("a rectangle");
        assert_eq!((edge.right, edge.bottom), (1918, 1078), "clipped to the desktop");
        assert_eq!((edge.w() % 2, edge.h() % 2), (1, 1), "odd exactly where the desktop is");

        // A box entirely off the desktop is not a rectangle at all, which is what a
        // stale churn key after a resize would produce.
        assert!(boxed(40, 0, 40, 0).to_rect(1919, 1079, TileGrid::ONE).is_none());

        // The same theorem at 2x: the grid is 128 pixels and still even.
        let retina = boxed(1, 1, 2, 2).to_rect(3839, 2159, TileGrid::at(2.0)).expect("a rectangle");
        assert_eq!((retina.left, retina.top, retina.w(), retina.h()), (128, 128, 256, 256));
    }

    /// A whole-desktop target with pixels in it, the pipelined shape.
    fn whole_regions(w: u16, h: u16) -> Regions {
        let mut regions = Regions::new(Policy::Whole, 60, Chroma::Subsampled, None);
        regions.want(w, h, TileGrid::ONE);
        regions
    }

    fn flat(w: u16, h: u16, value: u8) -> Vec<u8> {
        vec![value; usize::from(w) * usize::from(h) * 3]
    }

    fn placed(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    /// The double buffer, end to end: rounds stay serial, damage that lands while a
    /// round is away re-dirties the returning stream, and the pixels it carried
    /// reach the next round's mirror — including through the spare-sync at the swap.
    #[test]
    fn damage_during_a_round_survives_into_the_next() {
        let mut regions = whole_regions(64, 64);
        let whole = placed(0, 0, 64, 64);
        regions.blit(whole, &flat(64, 64, 10)).expect("a blit");
        let first = regions.take_round().expect("a dirty stream means a round");
        assert!(regions.take_round().is_none(), "two rounds out would race one chain");

        // Damage lands while the round is away...
        regions.blit(whole, &flat(64, 64, 20)).expect("a blit mid-round");
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
        let mut regions = whole_regions(64, 64);
        regions.blit(placed(0, 0, 64, 64), &flat(64, 64, 10)).expect("a blit");
        let stale = regions.take_round().expect("a round");
        regions.want(32, 32, TileGrid::ONE);
        regions.put_back(stale, Instant::now());
        assert!(regions.take_round().is_none(), "a stale round was restored");

        // The new desktop starts clean and streams its own pixels.
        let small = placed(0, 0, 32, 32);
        regions.blit(small, &flat(32, 32, 40)).expect("a blit at the new size");
        let fresh = regions.take_round().expect("a stream over the new desktop");
        let mut out = Vec::new();
        fresh.mirror().crop_into(small, &mut out).expect("a crop");
        assert!(out.iter().all(|&b| b == 40));
    }
}
