//! Ordered tile encoding outside the RDP and VNC protocol-read loops.
//!
//! ```text
//! read loop:  pack → Shadow::accept → TileSink::damage()
//!                                       │  bands, cut at the cell grid where it matters
//!                                       ▼
//!                        spawn_blocking(encode) ─────────┐
//!                                                        │ handle pushed, in order
//!             TileSink::msg()  ── ServerMsg ─────────────┤  mpsc, cap ENCODE_DEPTH
//!                                                        ▼
//!                                order task: await handles FIFO → frame_tx
//!                                            ⤷ cleanup tick → base re-encode
//! ```
//!
//! FIFO collection preserves source order even when encodes finish out of order.
//! Control messages share the queue so a resize cannot overtake related tiles.
//! A full queue backpressures the engine because its shadow has already recorded
//! submitted pixels.
//!
//! This is also where the render dial's `motion` strategy lives, because it is the
//! one place both engines already funnel their damage through. See [`Motion`].

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;


use crate::config::{MotionEncode, RenderPlan, TileCodec};
use crate::h264;
use crate::protocol::{ServerMsg, Tile, VideoUnit};
use crate::regions::{Policy, Regions};
use crate::tiles::{Changed, Rect};

/// Maximum queued encodes ahead of the handle currently collected. This covers
/// roughly one 1280×800 repaint while bounding memory and worker pressure.
const ENCODE_DEPTH: usize = 16;

/// The granularity churn is counted at. Several changes inside one slot count
/// once, so churn measures how much of a stretch of *time* a cell was busy for.
///
/// Time rather than frames, because neither engine has a frame worth counting.
/// RDP's outer loop turns once per PDU received, most of which redraw nothing, so
/// a counter driven by it races ahead of the repaints and a cell's history ages
/// out between its own changes. VNC's turns once per `FramebufferUpdate`, which is
/// damage-driven and so much closer, but its rate is set by the update-request
/// loop rather than by the remote: a cell changing in every update looks identical
/// whether that is sixty times a second or twice. A slot means the same thing on
/// both, and "in motion" stays one statement about the remote rather than two
/// about the transports.
const CHURN_SLOT: Duration = Duration::from_millis(100);

/// Slots of change history each cell keeps — the width of the `u8` shift register
/// in [`ChurnCell`], so it may not exceed 8. With [`CHURN_SLOT`] that is a window
/// of 800ms.
const CHURN_WINDOW: u64 = 8;

/// Churn — how many of a cell's last [`CHURN_WINDOW`] slots changed it — at which
/// the cell is taken to be in motion and switches to the motion encode.
///
/// A hard switch rather than a ramp between the two encodes. Which of those is
/// right is a question for measurement, and the switch is the one worth measuring
/// first: it makes the detection legible in the totals, where a ramp would smear
/// the answer across every quality in between.
///
/// Half the window, so one isolated change is never motion — a popup, an image
/// that just finished loading, a cursor — and a cell has to keep changing for
/// 400ms of the last 800ms before its quality drops. Anything less would only draw
/// a pointless cleanup a moment later.
const CHURN_MOVING: u32 = 4;

/// How long a cell sent at the motion encode must sit unchanged before it is
/// re-sent at the base one. Long enough that a brief pause in motion is not chased
/// with a redundant re-encode, short enough that a settled region sharpens while
/// the eye is still on it.
const CLEANUP_IDLE: Duration = Duration::from_millis(500);

/// How often the order task wakes to look for settled cells. It has to be its own
/// timer rather than something the next frame does, because a screen that stops
/// changing produces no next frame — which is exactly the case a cleanup is for.
const CLEANUP_TICK: Duration = Duration::from_millis(250);

/// Cleanups per tick, so a whole stopped video settles over a few ticks rather
/// than in one burst competing with live motion for the socket.
const MAX_CLEANUPS_PER_TICK: usize = 8;

/// Colour of a `render_motion_debug` outline on a piece sent at the motion encode.
///
/// Magenta, cyan and green rather than anything subtler: these survive a JPEG at
/// quality 10, which is the encode whose extent they are drawn to show, and none of
/// them is a colour a desktop produces in a straight line by accident.
const MARK_MOTION: [u8; 3] = [255, 0, 255];
/// Colour of a `render_motion_debug` outline on a piece sent at the base encode
/// from inside a split band — the quiet side of the split.
const MARK_CRISP: [u8; 3] = [0, 255, 255];
/// Colour of a `render_motion_debug` outline on a cleanup tile.
const MARK_CLEANUP: [u8; 3] = [0, 255, 0];
/// Thickness of a `render_motion_debug` outline. Two pixels, because one does not
/// reliably survive chroma subsampling at the quality a moving cell is sent at.
const MARK_PX: u16 = 2;

/// Cap on the source pixels held for cleanups at once. Past it a cell keeps the
/// motion encode until it next changes — safe, just not as crisp — rather than the
/// stash growing without bound under a full-screen video.
const MAX_STASH_BYTES: usize = 8 * 1024 * 1024;

/// How long queueing an access unit may block before the frame counts as one the link
/// could not keep up with.
///
/// More than half a frame at 30 Hz. Under a video plan the queues between here and
/// the socket are deliberately shallow ([`crate::session::VIDEO_FRAME_BUFFER`]), so
/// this stays at zero while the link has room and becomes obvious the moment it does
/// not — which is the whole reason those queues are shallow.
const BEHIND_BLOCK: Duration = Duration::from_millis(20);

/// Consecutive slow frames before quality is given up. Two rather than one, so a
/// single unlucky frame — a keyframe going out, a scheduler hiccup — is not a verdict
/// about the link.
const BEHIND_FRAMES: u32 = 2;

/// Consecutive clear frames before quality is taken back — about a second at 30 Hz,
/// and deliberately far more than [`BEHIND_FRAMES`]. Quick to give up, slow to
/// reclaim, so a link that is intermittently bad settles at a quality it can hold
/// instead of oscillating around one it cannot.
const CLEAR_FRAMES: u32 = 30;

/// The least time between two adjustments, so a burst of slow frames is one decision
/// rather than one per frame.
const ADJUST_COOLDOWN: Duration = Duration::from_secs(1);

/// How much coarser one step down is. Bigger than the step back up (one), for the
/// same reason [`CLEAR_FRAMES`] is bigger than [`BEHIND_FRAMES`].
const QP_STEP: u8 = 4;

/// The shortest gap between two access units.
///
/// The engines' frame boundaries are not a frame *rate*: RDP's is one `outputs`
/// batch, and a busy desktop produces those far faster than anything presents
/// them — 126 a second, measured, against a 60 Hz screen and a 30 Hz stream. Every
/// one of them cost a full encode of the mirror, which is how a stream carrying
/// under 800 kbit/s came to spend 88% of a session inside the encoder: the cost
/// scaled with how *often* the remote reported damage rather than with how much
/// had changed.
///
/// So the boundary proposes and this disposes. Damage between two access units
/// accumulates in the mirror and rides the next one as an ordinary, slightly
/// larger delta — which is cheaper than coding the same movement across four
/// frames, not merely fewer frames of it.
///
/// 33_333 µs is the interval `VIDEO_FRAME_US` in `frontend/src/tilePainter.ts`
/// already stamps access units with, so the timestamps stop being a fiction.
const VIDEO_FRAME_INTERVAL: Duration = Duration::from_micros(33_333);

/// What the link will bear, as a quantizer.
///
/// One-directional by construction, and that is the whole design rather than a
/// simplification: `dial` is a *ceiling* on quality, so this can only ever make the
/// picture coarser than the operator asked for and never finer. What TCP hides is
/// headroom — you can tell you are behind, never how far ahead you could be — and
/// this never needs to know, because exceeding the configured quality was never a
/// goal. The worst it can do is coarsen a link that was already struggling.
///
/// Pure, and takes `now` rather than reading a clock, so every one of its decisions
/// is testable without waiting for one.
struct Congestion {
    /// The configured quality as a quantizer: the finest this will ever ask for.
    dial: u8,
    /// The quantizer in force.
    qp: u8,
    /// Consecutive frames whose queueing blocked, and consecutive frames whose did
    /// not. Only one is ever non-zero.
    behind: u32,
    clear: u32,
    /// When the quantizer last moved, for [`ADJUST_COOLDOWN`]. `None` before it ever
    /// has, so the first verdict does not have to wait out a cooldown that never ran.
    changed_at: Option<tokio::time::Instant>,
}

impl Congestion {
    fn new(dial: u8) -> Self {
        Self { dial, qp: dial, behind: 0, clear: 0, changed_at: None }
    }

    /// Record how long queueing a frame blocked, and return a new quantizer if that
    /// changes the verdict.
    fn observe(&mut self, blocked: Duration, now: tokio::time::Instant) -> Option<u8> {
        if blocked >= BEHIND_BLOCK {
            self.behind += 1;
            self.clear = 0;
        } else {
            self.clear += 1;
            self.behind = 0;
        }
        if self.changed_at.is_some_and(|at| now.saturating_duration_since(at) < ADJUST_COOLDOWN) {
            return None;
        }
        let wanted = if self.behind >= BEHIND_FRAMES {
            self.qp.saturating_add(QP_STEP).min(h264::QP_COARSEST)
        } else if self.clear >= CLEAR_FRAMES {
            // Stops at the dial, never below it. A link with room to spare does not
            // earn a better picture than the one that was configured.
            self.qp.saturating_sub(1).max(self.dial)
        } else {
            return None;
        };
        if wanted == self.qp {
            return None;
        }
        self.qp = wanted;
        self.behind = 0;
        self.clear = 0;
        self.changed_at = Some(now);
        Some(wanted)
    }
}

/// The streams, the mirror behind them, and what the link will bear.
///
/// One type for both dials that produce access units: [`crate::regions`] holds the
/// difference between "the whole desktop, always" and "a region per thing that is
/// moving", and everything here is the same either way.
struct Video {
    regions: Regions,
    congestion: Congestion,
    /// The earliest the next round may be encoded — see [`VIDEO_FRAME_INTERVAL`].
    /// `None` before the first one, so a freshly connected desktop paints without
    /// waiting out an interval.
    due_at: Option<tokio::time::Instant>,
}

impl Video {
    fn new(policy: Policy, quality: u8, mark: Option<h264::Mark>) -> Self {
        Self {
            regions: Regions::new(policy, quality, mark),
            congestion: Congestion::new(h264::qp_for(quality)),
            due_at: None,
        }
    }
}

/// One item in the ordered queue.
///
/// A tile arrives as the *handle* to work already running, which is what buys the
/// parallelism: the engine pushed it and moved on. Everything else is already
/// finished and only needs its place in the order kept.
enum Pending {
    /// An encode in flight, yielding the tile and the microseconds it cost.
    Tile(JoinHandle<anyhow::Result<(Tile, u64)>>),
    /// One round of the video streams: every access unit produced at one frame
    /// boundary, already encoded, and what the round cost.
    ///
    /// The streams' shape, and the one thing they cannot borrow from the still path:
    /// their frames are links in a chain, each encoded from a mirror the read loop is
    /// writing, so they cannot be raced across workers the way independent stills can.
    /// A round is encoded under a lock on the caller's own task and only its *place in
    /// the order* is queued — the same contract [`Pending::Tile`] has, minus the
    /// parallelism there is nothing here to overlap with.
    ///
    /// One queue slot per round rather than per unit, because a round is one frame of
    /// the desktop however many regions it took: that is what [`ENCODE_DEPTH`] should
    /// be counting, and it is what makes the congestion loop's one measurement of how
    /// long a push blocked mean one thing.
    Round { units: Vec<VideoUnit>, encode_micros: u64 },
    Msg(ServerMsg),
    /// A caller waiting for everything pushed before it to have reached `frame_tx`.
    Flush(oneshot::Sender<()>),
}

/// One cell's recent history: bit 0 is the slot it was last seen changing,
/// shifted left one place for every slot since. Its churn is the number of set
/// bits — how many of the last [`CHURN_WINDOW`] slots it changed in.
#[derive(Clone, Copy)]
struct ChurnCell {
    history: u8,
    last_slot: u64,
}

/// A piece sent at the motion encode, held so it can be re-sent at the base one
/// once its cell stops moving.
///
/// It keeps the *source* pixels, so the re-encode needs no fresh frame from the
/// remote — which is the whole point, since a still screen produces none. `rgb` is
/// shared with the encode worker rather than copied for it: the two want the same
/// bytes and neither writes them.
struct Stashed {
    rect: Rect,
    rgb: Arc<Vec<u8>>,
    /// Tokio's clock rather than the standard library's, so that it is the same
    /// clock [`CLEANUP_TICK`] runs on. That keeps the two from disagreeing under a
    /// test that pauses time, which is the only way to assert the cleanup path
    /// without asserting how fast the machine is.
    sent_at: tokio::time::Instant,
}

/// Which cells are changing fast enough to take the cheap encode, and which of
/// them are still owed a re-send at the base one.
///
/// Keyed by [`Rect::cell_key`], which is the reason the grid exists: RDP and VNC
/// describe the same moving region with different rectangles from one frame to the
/// next, and a key that moved with them would count no churn at all.
///
/// Guarded by a mutex on [`Shared`] because [`TileSink`] is `Clone` and VNC pushes
/// from two tasks. Every critical section here is a hash lookup and some
/// arithmetic; nothing holds the lock across an await.
#[derive(Default)]
struct Motion {
    /// Where slot numbering starts, taken from the first observation and dropped
    /// by [`Self::clear`].
    ///
    /// Slots have to be numbered against one origin every cell shares rather than
    /// measured as a delta per cell: a cell changing faster than [`CHURN_SLOT`]
    /// would otherwise carry its own last-seen instant forward with every change,
    /// never advance its register, and so never be in motion — which is the case
    /// this exists to catch.
    origin: Option<tokio::time::Instant>,
    churn: HashMap<(u16, u16), ChurnCell>,
    stash: HashMap<(u16, u16), Stashed>,
    stash_bytes: usize,
}

impl Motion {
    /// Record that `key` changed at `now`, and return its churn.
    ///
    /// Aging is lazy: a cell that did not change is not touched, and its history is
    /// shifted by the whole gap when it is next seen. That is also what keeps the
    /// shift from overflowing — a gap of a full window empties the history instead.
    fn observe(&mut self, key: (u16, u16), now: tokio::time::Instant) -> u32 {
        let origin = *self.origin.get_or_insert(now);
        let slot = u64::try_from(
            now.saturating_duration_since(origin).as_millis() / CHURN_SLOT.as_millis(),
        )
        .unwrap_or(u64::MAX);
        let cell = self
            .churn
            .entry(key)
            .or_insert(ChurnCell { history: 0, last_slot: slot });
        let elapsed = slot.saturating_sub(cell.last_slot);
        cell.history = if elapsed >= CHURN_WINDOW {
            0
        } else {
            cell.history << elapsed
        };
        cell.history |= 1;
        cell.last_slot = slot;
        cell.history.count_ones()
    }

    /// The cells in motion as of `now`.
    ///
    /// [`Self::observe`] ages a cell's history only when it changes, which is what
    /// keeps the hot path cheap; this is the other side of that bargain, and the one
    /// caller who needs the answer for cells that are *not* changing. A cell whose
    /// history has emptied is dropped outright, so the map stays the size of the
    /// screen's recent activity rather than of the session.
    ///
    /// Only the region policy asks (`render_motion_subtype = "h264"`), and only once
    /// per retune.
    fn moving(&mut self, now: tokio::time::Instant) -> Vec<(u16, u16)> {
        let Some(origin) = self.origin else {
            return Vec::new();
        };
        let slot = u64::try_from(
            now.saturating_duration_since(origin).as_millis() / CHURN_SLOT.as_millis(),
        )
        .unwrap_or(u64::MAX);
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

    /// Remember a piece about to be sent at the motion encode, so a later tick can
    /// settle it. Reports whether the debt was recorded.
    ///
    /// A `false` return has to be honoured by the caller, which is why this is
    /// `#[must_use]`: `Shadow` records the *source* pixels as delivered the moment a
    /// rectangle is accepted, so a cell sent at the motion encode with no debt
    /// recorded is one the client holds a lossy copy of while the gateway believes
    /// it holds the exact pixels — and nothing will ever re-send it, because nothing
    /// knows it is owed. That is permanent, not merely coarse. Sending such a cell
    /// at the base encode instead is the only safe reading of a full stash.
    ///
    /// Over [`MAX_STASH_BYTES`] the *existing* entry is kept and the new one
    /// dropped. Keeping the older one is the point: it is the one closer to coming
    /// due, and a cell whose debt was dropped instead would never clean up at all
    /// until it next moved.
    ///
    /// A cell holds one debt, so a debt already there may only be *replaced by a
    /// rectangle covering it*. That is the same rule for the same reason: damage is
    /// sent as it is reported and clipped to the cell, so two sends can be two
    /// different slivers of one cell, and overwriting the first debt with the
    /// second would leave the first sliver lossy with nothing left that knows it is
    /// owed. It is the trail a pointer drags across a cell — a run of small
    /// rectangles, of which only the last would ever be cleaned up.
    #[must_use]
    fn stash(
        &mut self,
        key: (u16, u16),
        rect: Rect,
        rgb: Arc<Vec<u8>>,
        now: tokio::time::Instant,
    ) -> bool {
        let replaced = match self.stash.get(&key) {
            Some(owed) if !rect.contains(&owed.rect) => return false,
            Some(owed) => owed.rgb.len(),
            None => 0,
        };
        let after = self.stash_bytes - replaced + rgb.len();
        if after > MAX_STASH_BYTES {
            return false;
        }
        self.stash_bytes = after;
        self.stash.insert(key, Stashed { rect, rgb, sent_at: now });
        true
    }

    /// Discharge what `key` owes as far as `sent` reaches — a rectangle whose exact
    /// source pixels, `rgb`, have just gone out at the base encode.
    ///
    /// Covering all of what is owed cancels the debt outright. Covering *part* of it
    /// does not, because damage is sent as it is reported, clipped to the cell rather
    /// than snapped out to it: a cell can owe a cleanup for a region wider than the
    /// piece that just went out crisp, and cancelling on that would strand the rest
    /// at the motion encode with nothing left to remember it.
    ///
    /// So a partial cover writes those newer pixels into the debt instead. That is
    /// not an optimisation — it is what stops a cleanup from *undoing* a later send.
    /// A debt holds the pixels of the frame it was recorded on; when something inside
    /// it changes and goes out crisp, the debt still holds the older version, and the
    /// cleanup would faithfully restore it over the newer one. On RDP, where the
    /// pointer is composited into the framebuffer, that is a cursor painted back onto
    /// a spot it has already left — wrong content rather than coarse content, and
    /// permanent, since the shadow has recorded the newer pixels as delivered and
    /// nothing will send them twice.
    fn settle(&mut self, key: (u16, u16), sent: Rect, rgb: &[u8]) {
        let Some(owed) = self.stash.get_mut(&key) else {
            return;
        };
        if !sent.contains(&owed.rect) {
            match sent.intersect(&owed.rect) {
                // Nothing in common: what the debt holds is still true.
                None => return,
                // A debt that cannot be brought up to date has to go, and the cell
                // keeps the motion encode until it next changes. Losing crispness is
                // recoverable; restoring superseded pixels is not.
                Some(over) if patch(owed, sent, rgb, over) => return,
                Some(_) => {}
            }
        }
        let owed = self.stash.remove(&key).expect("just found it");
        self.stash_bytes -= owed.rgb.len();
    }

    /// Take up to `max` of the cells that have sat unchanged for [`CLEANUP_IDLE`],
    /// oldest first, for the caller to re-encode at the base quality.
    ///
    /// Oldest first so a backlog larger than one tick drains in the order it
    /// accrued; picking arbitrarily lets a long-settled cell be starved by newer
    /// ones for as long as the motion lasts.
    fn take_due(&mut self, now: tokio::time::Instant, max: usize) -> Vec<Stashed> {
        let mut due: Vec<((u16, u16), tokio::time::Instant)> = self
            .stash
            .iter()
            .filter(|(_, s)| now.saturating_duration_since(s.sent_at) >= CLEANUP_IDLE)
            .map(|(key, s)| (*key, s.sent_at))
            .collect();
        due.sort_unstable_by_key(|(_, sent_at)| *sent_at);
        due.truncate(max);
        due.into_iter()
            .map(|(key, _)| {
                let stashed = self.stash.remove(&key).expect("key came from the map");
                self.stash_bytes -= stashed.rgb.len();
                stashed
            })
            .collect()
    }

    /// Drop everything. A resize changes what every key means, and a repaint has
    /// already re-sent every pixel the stash was holding.
    fn clear(&mut self) {
        self.origin = None;
        self.churn.clear();
        self.stash.clear();
        self.stash_bytes = 0;
    }
}

/// Write `over` — the part of `sent` that lies inside what `owed` is holding — over
/// the debt's own pixels, so that what a cleanup restores is the newest source and
/// not the frame the debt happened to be recorded on. Reports whether it could.
///
/// Copy-on-write through the `Arc`: the encoder task may still be holding the buffer
/// this debt was recorded from, and it must go out as it was measured.
fn patch(owed: &mut Stashed, sent: Rect, rgb: &[u8], over: Rect) -> bool {
    let (dw, dh) = (usize::from(owed.rect.w()), usize::from(owed.rect.h()));
    let (sw, sh) = (usize::from(sent.w()), usize::from(sent.h()));
    if owed.rgb.len() != dw * dh * 3 || rgb.len() != sw * sh * 3 {
        return false;
    }
    let run = usize::from(over.w()) * 3;
    let dst = Arc::make_mut(&mut owed.rgb);
    for y in over.top..=over.bottom {
        let d = (usize::from(y - owed.rect.top) * dw + usize::from(over.left - owed.rect.left)) * 3;
        let s = (usize::from(y - sent.top) * sw + usize::from(over.left - sent.left)) * 3;
        dst[d..d + run].copy_from_slice(&rgb[s..s + run]);
    }
    true
}

/// `rgb` with the border of `rect` painted `colour`, for `render_motion_debug`.
///
/// A copy, and the copy is what goes to the encoder: the original is what
/// [`crate::tiles::Shadow`] has already recorded as delivered and what the stash
/// owes, so painting it in place would make the outline permanent — the cleanup
/// would faithfully restore the mark along with the pixels, and nothing after that
/// would ever take it off again.
fn marked(rgb: &Arc<Vec<u8>>, rect: Rect, colour: [u8; 3]) -> Arc<Vec<u8>> {
    let (w, h) = (usize::from(rect.w()), usize::from(rect.h()));
    if rgb.len() != w * h * 3 {
        // Not the pixels this rectangle describes. A debug aid is never the thing
        // that corrupts a frame, so it declines rather than indexes on a guess.
        return Arc::clone(rgb);
    }
    let t = usize::from(MARK_PX);
    let mut out = rgb.as_ref().clone();
    let mut paint = |row: usize, from: usize, to: usize| {
        for x in from..to {
            out[(row * w + x) * 3..][..3].copy_from_slice(&colour);
        }
    };
    for y in 0..h {
        if y < t || y + t >= h {
            paint(y, 0, w);
        } else {
            paint(y, 0, t.min(w));
            paint(y, w.saturating_sub(t), w);
        }
    }
    Arc::new(out)
}

/// State the sink and its order task both touch.
///
/// The counters live here rather than in the order task because the engine is what
/// reports them: an engine's `run` returning drops the thread's whole runtime, so a
/// line the task logged on its own way out would be cancelled before it printed.
struct Shared {
    /// Why the order task gave up, so the engine's next push can report it rather
    /// than a bare closed channel. See [`TileSink::closed`].
    failure: Mutex<Option<String>>,
    motion: Mutex<Motion>,
    /// A `tokio` mutex rather than a `std` one because its guard is held across the
    /// `spawn_blocking` that encodes. That is not incidental — it is what makes "one
    /// access unit at a time, in push order, over a mirror nobody else is writing" a
    /// fact about the type instead of a hope about the callers.
    video: tokio::sync::Mutex<Video>,
    /// Set by [`TileSink::reset_render`], consumed by [`TileSink::frame`]. An atomic
    /// rather than a field on [`Video`] so that resetting stays synchronous: its four
    /// call sites are already awaiting other things, and none of them should have to
    /// wait out an encode to say "the client needs to start again".
    keyframe_owed: AtomicBool,
    tiles: AtomicU64,
    encoded_bytes: AtomicU64,
    /// Of [`Self::tiles`], those sent at the motion encode rather than the base.
    motion_tiles: AtomicU64,
    /// Of [`Self::tiles`], those that are a settled cell being re-sent crisp.
    cleanups: AtomicU64,
    cleanup_bytes: AtomicU64,
    /// Access units, counted apart from tiles because they are not one: a tile is a
    /// picture and a unit is a link in a chain, and one number for both would compare
    /// a repaint's bands with a video's frames.
    units: AtomicU64,
    /// Of [`Self::units`], those a decoder could start from, and what they cost. Read
    /// together: see [`Totals`].
    keyframes: AtomicU64,
    keyframe_bytes: AtomicU64,
    /// Frames the encoder produced no bitstream for. Must stay zero.
    skipped: AtomicU64,
    /// Frames sent coarser than the dial asked for, and the coarsest quantizer the
    /// link ever forced. The whole measurement of [`Congestion`].
    coarsened: AtomicU64,
    coarsest_qp: AtomicU64,
    /// Summed across workers, so it may exceed the wall clock.
    encode_micros: AtomicU64,
    /// Wall time the order task spent waiting for encodes to finish.
    waited_micros: AtomicU64,
    /// Time the engine spent blocked pushing into a full queue.
    stalled_micros: AtomicU64,
}

impl Shared {
    fn new(plan: RenderPlan) -> Self {
        // Which dial produces access units, and at what quality. A plan that produces
        // none still builds a `Video` — never touched, and holding no mirror until
        // something is blitted into it — so that the streaming paths need no
        // unwrapping once they have established which plan they are on.
        let (policy, quality, mark) = match plan {
            RenderPlan::Video { quality } => (Policy::Whole, quality, None),
            RenderPlan::Tiles { motion: Some(MotionEncode::Stream { quality }), debug, .. } => (
                Policy::Moving,
                quality,
                debug.then_some(h264::Mark { colour: MARK_MOTION, px: MARK_PX }),
            ),
            RenderPlan::Tiles { .. } => (Policy::Whole, 1, None),
        };
        Self {
            failure: Mutex::default(),
            motion: Mutex::default(),
            video: tokio::sync::Mutex::new(Video::new(policy, quality, mark)),
            keyframe_owed: AtomicBool::new(false),
            tiles: AtomicU64::new(0),
            encoded_bytes: AtomicU64::new(0),
            motion_tiles: AtomicU64::new(0),
            cleanups: AtomicU64::new(0),
            cleanup_bytes: AtomicU64::new(0),
            units: AtomicU64::new(0),
            keyframes: AtomicU64::new(0),
            keyframe_bytes: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            coarsened: AtomicU64::new(0),
            coarsest_qp: AtomicU64::new(0),
            encode_micros: AtomicU64::new(0),
            waited_micros: AtomicU64::new(0),
            stalled_micros: AtomicU64::new(0),
        }
    }
}

/// The engine's handle on the encoder.
///
/// `Clone` because the VNC engine drives its read loop as a separate task while
/// its input side keeps sending control messages; both push into the same queue,
/// and only the read loop pushes tiles.
#[derive(Clone)]
pub struct TileSink {
    engine: &'static str,
    tx: mpsc::Sender<Pending>,
    shared: Arc<Shared>,
    /// The resolved render dial, read once from the target in `rdp::run` /
    /// `vnc::run` (see [`crate::config::TargetConfig::render_plan`]).
    plan: RenderPlan,
}

impl TileSink {
    /// Start an encoder for one engine. `engine` prefixes its log lines. `plan` is
    /// the resolved render dial — which of the two ways this gateway can put a
    /// desktop on a wire the target asked for, and how it is tuned.
    pub fn new(engine: &'static str, frame_tx: mpsc::Sender<ServerMsg>, plan: RenderPlan) -> Self {
        let (tx, rx) = mpsc::channel(ENCODE_DEPTH);
        let shared = Arc::new(Shared::new(plan));
        tokio::spawn(order_loop(engine, rx, frame_tx, Arc::clone(&shared), plan));
        Self {
            engine,
            tx,
            shared,
            plan,
        }
    }

    /// Queue everything a changed rectangle owes the client.
    ///
    /// `pack` copies a sub-rectangle of the engine's framebuffer out as packed
    /// RGB888. It is a callback rather than one buffer because the two engines hold
    /// those pixels differently — RDP repacks out of the decoded image, VNC crops
    /// out of the rectangle it just read — and because how a rectangle is *cut* is
    /// the one thing both should agree on, which is what this method is.
    ///
    /// Without a motion plan the cut is [`Rect::bands`] and nothing else, which is
    /// what the gateway has always sent. With one, a band whose cells are all quiet
    /// is still sent whole and at the base encode — a target with nothing moving is
    /// byte-for-byte what the same target sends today — and only a band containing a
    /// moving cell is cut at the grid, so a video in a window costs its own cells
    /// their quality and costs the text beside it nothing.
    ///
    /// Under `render_motion_debug` every piece a *split* band produces is outlined
    /// in the pixels sent, so which cells the detection put in motion is something
    /// QA reads off the screen rather than infers from how blurry a region looks:
    ///
    /// - [`MARK_MOTION`] (magenta) — sent at the motion encode. Magenta over
    ///   something that is not moving is the detection reaching too far.
    /// - [`MARK_CRISP`] (cyan) — sent at the base encode from inside a split band:
    ///   a quiet cell beside a moving one. This is the boundary of the split.
    /// - [`MARK_CLEANUP`] (green), drawn by [`flush_cleanups`] — a settled cell
    ///   restored to the base encode.
    ///
    /// An unmarked region was sent whole at the base encode, which is the quiet
    /// path and the great majority of a still screen. So blur with no mark on it is
    /// not a live decision at all: it is a stale one nothing has replaced.
    pub async fn damage<F>(&self, changed: &Changed, pack: F) -> anyhow::Result<()>
    where
        F: Fn(Rect) -> Vec<u8>,
    {
        // Video first, because it is not a variation on the rest of this method: no
        // bands, no cells, no tile. The unit of that encoder is the whole
        // framebuffer, so a rectangle is copied into the mirror and nothing is sent
        // until the engine says a frame has ended. See [`Self::frame`].
        let (base, motion, debug) = match self.plan {
            RenderPlan::Video { .. } => {
                let rgb = pack(changed.rect);
                return self.shared.video.lock().await.regions.blit(changed.rect, &rgb);
            }
            RenderPlan::Tiles { base, motion, debug } => (base, motion, debug),
        };

        let motion_codec = match motion {
            None => {
                for band in changed.rect.bands() {
                    self.encode(band, Arc::new(pack(band)), base).await?;
                }
                return Ok(());
            }
            Some(MotionEncode::Stream { .. }) => {
                return self.damage_streaming(changed, pack, base, debug).await;
            }
            Some(MotionEncode::Tile(codec)) => codec,
        };

        // One reading for the whole rectangle: the pieces of one report of damage
        // arrived together and belong in the same slot, however long the cutting
        // and encoding below take.
        let now = tokio::time::Instant::now();
        for band in changed.rect.bands() {
            // Churn is recorded for the cells that *changed*, not for every cell the
            // band covers. `Changed::rect` is one box round everything that
            // differed, so a video and a banner ad at opposite ends of the screen
            // put every cell between them inside it — and arming those was what put
            // a still sidebar, a menu bar and a taskbar into motion because
            // something else was moving on the same screen.
            //
            // A cell only along for the ride is left at the base encode and settled.
            // Its pixels are being re-sent anyway, so crisp costs only bytes; and
            // crisp is what discharges whatever it was owed.
            let cells: Vec<(Rect, bool)> = {
                let mut motion = self.shared.motion.lock().unwrap();
                band.cells()
                    .map(|cell| {
                        let key = cell.cell_key();
                        let moving = changed.has(key) && motion.observe(key, now) >= CHURN_MOVING;
                        (cell, moving)
                    })
                    .collect()
            };

            if !cells.iter().any(|(_, moving)| *moving) {
                let rgb = Arc::new(pack(band));
                {
                    let mut motion = self.shared.motion.lock().unwrap();
                    for (cell, _) in &cells {
                        motion.settle(cell.cell_key(), band, &rgb);
                    }
                }
                self.encode(band, rgb, base).await?;
                continue;
            }

            for (cell, moving) in cells {
                let rgb = Arc::new(pack(cell));
                // A cell takes the motion encode only if its cleanup was recorded.
                // Past the stash cap it stays crisp instead: the alternative is a
                // client left holding a lossy copy that nothing is owed and so
                // nothing will ever replace. Costing bytes is recoverable; that is
                // not.
                let took_the_discount = moving && {
                    // Timed at dispatch rather than when the encode lands, which is
                    // what keeps a cleanup from overtaking fresher pixels: a cell
                    // with a tile still in the queue has just been touched, so it
                    // cannot also be idle.
                    self.shared
                        .motion
                        .lock()
                        .unwrap()
                        .stash(cell.cell_key(), cell, Arc::clone(&rgb), now)
                };
                let codec = if took_the_discount {
                    self.shared.motion_tiles.fetch_add(1, Ordering::Relaxed);
                    motion_codec
                } else {
                    // Crisp pixels discharge whatever this cell was owed, whether it
                    // is quiet or only crisp because the stash is full.
                    self.shared.motion.lock().unwrap().settle(cell.cell_key(), cell, &rgb);
                    base
                };
                // Only a *split* band is marked. A quiet band goes out whole and
                // untouched, which is the byte-identity claim the strategy rests
                // on, and marking it would flood a still screen with outlines that
                // say nothing.
                let rgb = if debug {
                    let colour = if took_the_discount { MARK_MOTION } else { MARK_CRISP };
                    marked(&rgb, cell, colour)
                } else {
                    rgb
                };
                self.encode(cell, rgb, codec).await?;
            }
        }
        Ok(())
    }

    /// [`Self::damage`] for a target whose moving encode is a stream per region.
    ///
    /// The same cut as the still motion path — bands, and cells only where it matters
    /// — with one difference that decides everything: a cell a live stream covers is
    /// **not sent at all**. Its pixels reach the client through that stream, and
    /// sending them as a tile as well would discharge a debt the stream has not paid.
    ///
    /// Everything goes into the mirror first, moving or not. That copy is what a
    /// stream starting later reads and what a cleanup crops, so it has to be the whole
    /// truth rather than the parts that happened to be busy.
    ///
    /// Which cells are streamed is read once, here. A stream can only ever *end*
    /// underneath this — the cleanup tick expires idle ones and never starts any, and
    /// starting is [`Self::frame`]'s job on this same task. That direction is the safe
    /// one: a cell whose stream ended after the reading is simply not sent this frame,
    /// its debt still stands, and the cleanup carries it from the mirror. The other
    /// direction would be a cell delivered twice, once crisp and once inside a
    /// keyframe, with the crisp one discharging a debt the stream had not paid.
    async fn damage_streaming<F>(
        &self,
        changed: &Changed,
        pack: F,
        base: TileCodec,
        debug: bool,
    ) -> anyhow::Result<()>
    where
        F: Fn(Rect) -> Vec<u8>,
    {
        let streamed: HashSet<(u16, u16)> = {
            let mut video = self.shared.video.lock().await;
            video.regions.blit(changed.rect, &pack(changed.rect))?;
            changed
                .rect
                .cells()
                .map(|cell| cell.cell_key())
                .filter(|key| video.regions.covers(*key))
                .collect()
        };

        // One reading for the whole rectangle: the pieces of one report of damage
        // arrived together and belong in the same slot.
        let now = tokio::time::Instant::now();
        // What went out crisp, to discharge in one critical section at the end.
        let mut crisp: Vec<Rect> = Vec::new();
        for band in changed.rect.bands() {
            let cells: Vec<Rect> = band.cells().collect();
            {
                // Churn is recorded for the cells that *changed*, not for every cell
                // the band covers — `Changed::rect` is one box round everything that
                // differed. Recorded whether or not a stream is already carrying the
                // cell, because that is what keeps the stream alive.
                let mut motion = self.shared.motion.lock().unwrap();
                for cell in &cells {
                    let key = cell.cell_key();
                    if changed.has(key) {
                        motion.observe(key, now);
                    }
                }
            }

            if !cells.iter().any(|cell| streamed.contains(&cell.cell_key())) {
                // The quiet path, and the great majority of a screen: one whole band
                // at the base encode, byte for byte what this target would send with
                // no motion strategy at all.
                crisp.push(band);
                self.encode(band, Arc::new(pack(band)), base).await?;
                continue;
            }

            for cell in cells {
                if streamed.contains(&cell.cell_key()) {
                    continue;
                }
                let rgb = Arc::new(pack(cell));
                // Only a *split* band is marked, for the reason the still path gives.
                // A region's own outline is drawn by its encoder, on the crop rather
                // than on the mirror — see `h264::Mark`.
                let rgb = if debug { marked(&rgb, cell, MARK_CRISP) } else { rgb };
                crisp.push(cell);
                self.encode(cell, rgb, base).await?;
            }
        }

        if !crisp.is_empty() {
            let mut video = self.shared.video.lock().await;
            for sent in crisp {
                video.regions.discharge(sent);
            }
        }
        Ok(())
    }

    /// One remote frame has ended: choose the regions, encode everything
    /// [`Self::damage`] has blitted since the last call, and queue the access units.
    ///
    /// A no-op for a target with no streams, where a rectangle is already a message.
    /// Otherwise it is the only thing that sends one, and it has to be driven by the
    /// engines because neither protocol hands its damage over a frame at a time —
    /// `damage` is called once per *rectangle*, and RDP's loop turns once per PDU,
    /// most of which redraw nothing. So this is also a no-op when nothing was blitted:
    /// without that, a still screen would encode a frame per PDU.
    ///
    /// The encode runs on a blocking worker with the streams' lock held across it —
    /// serial, which is what an inter-frame stream requires and what the still path
    /// deliberately is not. Only the finished round's place in the order is queued.
    ///
    /// **Calling it is a proposal, not an instruction.** At most one round is produced
    /// per [`VIDEO_FRAME_INTERVAL`]; a call inside that window leaves the mirror dirty
    /// and returns. So an engine may call this as often as its boundaries occur, but
    /// must also call it when [`Self::due_at`] says to — see there for why that second
    /// half is not optional.
    pub async fn frame(&self) -> anyhow::Result<()> {
        if !self.streaming() {
            return Ok(());
        }
        let mut video = self.shared.video.lock().await;
        // Read before it is consumed, because it decides whether the interval below
        // applies at all. A forced keyframe is never deferred: `reset_render` arms it
        // for a repaint, a reattach, a takeover or a resize, and every one of those is
        // a client sitting in front of nothing until the keyframe arrives. Holding one
        // back to keep a frame rate would be keeping time with an empty window.
        let owed = self.shared.keyframe_owed.load(Ordering::Relaxed);
        let now = tokio::time::Instant::now();
        if !owed && video.due_at.is_some_and(|due| now < due) {
            // Deliberately before the round is taken: the mirror keeps what was
            // blitted into it and the next round carries it as an ordinary delta.
            // Something must come back for it, which is what `due_at` tells the
            // engines.
            return Ok(());
        }
        if self.shared.keyframe_owed.swap(false, Ordering::Relaxed) {
            // Per stream, and it stays armed until that stream actually produces one
            // — so an encode that yields no bitstream cannot lose the ask, and a
            // client is never left waiting for a keyframe nothing will send again.
            video.regions.force_keyframes();
        }
        // Geometry moves here, on the engine's own task, before anything is encoded:
        // that is what makes the set of streamed cells `damage` reads a stable thing
        // rather than a race.
        if let RenderPlan::Tiles { .. } = self.plan {
            let moving = self.shared.motion.lock().unwrap().moving(now);
            video.regions.retune(&moving, now)?;
        }
        let Some(mut round) = video.regions.take_round() else {
            return Ok(());
        };
        video.due_at = Some(now + VIDEO_FRAME_INTERVAL);

        let started = Instant::now();
        let (round, units) = tokio::task::spawn_blocking(move || {
            let units = round.encode();
            (round, units)
        })
        .await
        .map_err(|e| {
            // Not recoverable by rebuilding: a fresh stream would hand a blank mirror
            // to a keyframe, which is wrong pixels rather than coarse ones, and the
            // shadow already counts the real ones as delivered.
            let message = format!("h264 encoder stopped: {e}");
            give_up(self.engine, &self.shared, message.clone());
            anyhow::anyhow!(message)
        })?;
        let encode_micros = micros(started);
        let qp = video.regions.qp();
        // Streams that produced nothing keep their dirty flag and their keyframe, so
        // those pixels ride the next round. `skip_frames(false)` should make that
        // unreachable; the counter is how we would find out that it is not.
        if round.skipped() > 0 {
            self.shared.skipped.fetch_add(round.skipped(), Ordering::Relaxed);
            warn!(
                "{}: {} h264 frame(s) encoded to nothing; their pixels wait for the next",
                self.engine,
                round.skipped()
            );
        }
        video.regions.put_back(round, now);
        let units = units?;
        if units.is_empty() {
            return Ok(());
        }
        // Dropped before the push, which may block: holding the streams' lock while
        // waiting on a full queue would make every later `damage` wait on the socket.
        drop(video);

        let queued = Instant::now();
        let pushed = self.push(Pending::Round { units, encode_micros }).await;
        // How long that took is the congestion signal, and it is read whether or not
        // the push succeeded: a push that failed waited just as long, and the verdict
        // is about the link rather than about this round.
        self.adjust(queued.elapsed(), qp).await;
        pushed
    }

    /// Whether this target's moving pixels go out as access units — either the whole
    /// desktop (`render_type = "video"`) or a region at a time
    /// (`render_motion_subtype = "h264"`).
    fn streaming(&self) -> bool {
        matches!(
            self.plan,
            RenderPlan::Video { .. }
                | RenderPlan::Tiles { motion: Some(MotionEncode::Stream { .. }), .. }
        )
    }

    /// When the engine must call [`Self::frame`] again, whatever its own boundaries
    /// are doing — or `None` when there is nothing waiting.
    ///
    /// **This is the correctness half of pacing, not a convenience.**
    /// [`crate::tiles::Shadow::accept`] records source pixels as delivered to the
    /// client the moment `damage` blits them into the mirror, and nothing re-sends
    /// them. A deferred frame that is never followed by another encode is therefore
    /// permanently wrong pixels — and "the motion stopped right after one" is the
    /// ordinary case rather than a corner: a video paused, a pointer come to rest, a
    /// window settled. The deadline is what comes back for them.
    ///
    /// `None` while the mirror is clean, so an idle stream parks on
    /// [`std::future::pending`] instead of waking an engine to encode nothing.
    pub async fn due_at(&self) -> Option<tokio::time::Instant> {
        if !self.streaming() {
            return None;
        }
        let video = self.shared.video.lock().await;
        if !video.regions.dirty() {
            return None;
        }
        // A dirty mirror with no deadline yet is one whose pixels arrived before any
        // access unit went out; it is owed one now rather than in an interval.
        Some(video.due_at.unwrap_or_else(tokio::time::Instant::now))
    }

    /// Let the congestion policy see how long that frame's push blocked, and move the
    /// quantizer if it has changed its mind.
    ///
    /// A failure to re-tune is logged and dropped rather than ending the session: the
    /// stream is still perfectly good at the quality it already had, and losing the
    /// ability to *degrade* is not a reason to stop.
    async fn adjust(&self, blocked: Duration, qp: u8) {
        self.shared.coarsest_qp.fetch_max(u64::from(qp), Ordering::Relaxed);
        let mut video = self.shared.video.lock().await;
        if qp > video.congestion.dial {
            self.shared.coarsened.fetch_add(1, Ordering::Relaxed);
        }
        let dial = video.congestion.dial;
        let Some(wanted) = video.congestion.observe(blocked, tokio::time::Instant::now()) else {
            return;
        };
        // Every live stream, and every one started afterwards: one link, one verdict.
        // A region that appears while the link is behind has no more room than the
        // ones already running.
        if let Err(e) = video.regions.set_qp(wanted) {
            warn!("{}: could not move the h264 quantizer to {wanted}: {e:#}", self.engine);
        } else {
            debug!("{}: h264 quantizer now {wanted} (the dial asks for {dial})", self.engine);
        }
    }

    /// Forget everything the render dial remembers about this framebuffer.
    ///
    /// For a resize, where a cell key no longer names the same pixels and a stream's
    /// picture size has changed, and for a repaint, where every pixel is about to be
    /// re-sent anyway — carrying churn across either would put cells in motion that
    /// are only being redrawn once, and leaving a video stream mid-chain would ask a
    /// client to decode from a frame it never saw.
    ///
    /// Its call sites are exactly the moments a client's decoder has to be able to
    /// start over, which is why the keyframe is armed here rather than anywhere that
    /// knows about H.264.
    pub fn reset_render(&self) {
        self.shared.motion.lock().unwrap().clear();
        self.shared.keyframe_owed.store(true, Ordering::Relaxed);
    }

    /// Queue one rectangle of packed RGB888 at the base encode.
    ///
    /// The primitive under [`Self::damage`], and what a caller with a rectangle
    /// already cut to its liking wants.
    pub async fn tile(&self, x: u16, y: u16, w: u16, h: u16, rgb: Vec<u8>) -> anyhow::Result<()> {
        let Some(rect) = Rect::from_size(x, y, w, h) else {
            return Ok(());
        };
        match self.plan {
            // Same reason as in `damage`: under video there is no such thing as one
            // rectangle's worth of message.
            RenderPlan::Video { .. } => self.shared.video.lock().await.regions.blit(rect, &rgb),
            RenderPlan::Tiles { base, .. } => self.encode(rect, Arc::new(rgb), base).await,
        }
    }

    /// Start an encode and queue its place in the order.
    ///
    /// `rgb` is owned rather than borrowed because the engine's framebuffer is
    /// gone by the time a worker reads it — the read loop has moved on to the next
    /// PDU. The encode starts immediately; only its *place in the order* is queued.
    async fn encode(
        &self,
        rect: Rect,
        rgb: Arc<Vec<u8>>,
        codec: TileCodec,
    ) -> anyhow::Result<()> {
        let handle = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let tile = encode_tile(rect, &rgb, codec)?;
            Ok((tile, micros(started)))
        });
        self.push(Pending::Tile(handle)).await
    }

    /// Queue anything that is not a tile, keeping it behind the tiles it follows.
    ///
    /// A resize is also how the streams learn how big the desktop is. That is one
    /// interception rather than a size threaded through every place a stream has to be
    /// rebuilt, and it cannot be forgotten by a fifth such place added later: telling
    /// the client its framebuffer changed and telling the encoder are the same event,
    /// and the first already happens everywhere the second must.
    ///
    /// Bookkeeping only — nothing here can fail, deliberately. This method's error is
    /// read by every caller as "the browser has gone", answered by returning without
    /// a word (`rdp::run`, `vnc::run`), so a desktop the encoder cannot handle would
    /// end the session silently on the picker. Building the mirror and the streams
    /// waits for [`Self::damage`] or [`Self::frame`], which are on the engines' `?`
    /// path and end the session with the message attached.
    pub async fn msg(&self, msg: ServerMsg) -> anyhow::Result<()> {
        if let ServerMsg::Resize { w, h, .. } = &msg
            && self.streaming()
        {
            self.shared.video.lock().await.regions.want(*w, *h);
        }
        self.push(Pending::Msg(msg)).await
    }

    /// Shut the encoder down: deliver what is still in flight, then log what it cost.
    ///
    /// The one thing an engine's `run` must do before returning, and one call rather
    /// than two because the order cannot be got wrong this way. Both halves matter and
    /// for different reasons: the runtime is dropped when `run` returns, so anything
    /// the order task still held would be cancelled — including the `ServerMsg::Error`
    /// that explains why the session ended, leaving the browser on the picker with
    /// nothing to show — and the totals are only complete once it has stopped adding
    /// to them.
    pub async fn finish(&self) {
        self.flush().await;
        self.report();
    }

    /// Wait until everything pushed so far has reached the frame channel.
    ///
    /// Returns early if the order task is already gone; there is then nothing left to
    /// wait for. Shutdown wants [`Self::finish`] instead; this is for a caller that
    /// needs to *read* what it pushed, which in practice means a test.
    pub async fn flush(&self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(Pending::Flush(ack_tx)).await.is_ok() {
            let _ = ack_rx.await;
        }
    }

    async fn push(&self, item: Pending) -> anyhow::Result<()> {
        let started = Instant::now();
        let result = self.tx.send(item).await;
        // Sub-microsecond sends truncate to zero, so this counts only real waiting.
        self.shared
            .stalled_micros
            .fetch_add(micros(started), Ordering::Relaxed);
        result.map_err(|_| self.closed())
    }

    /// Log what this engine's encoder cost. Private: [`Self::finish`] is the only
    /// caller, so it cannot be run before the flush that completes the numbers.
    ///
    /// Explicit rather than emitted by the order task on its way out, for the reason
    /// [`Shared`] gives. Silent when an engine encoded nothing, since a line of
    /// zeroes says nothing — though both current engines, RDP and VNC, do encode.
    fn report(&self) {
        let totals = Totals::of(&self.shared);
        if totals.tiles > 0 || totals.units > 0 {
            info!("{}: encode totals: {totals}", self.engine);
        }
    }

    /// The error a push reports once the order task has stopped.
    ///
    /// An encode failure used to `?` straight out of the engine's own loop and end
    /// the session, which is what stops the shadow from believing the client holds
    /// pixels that were never sent. Deferred, the failure lands in the order task
    /// instead, so it is recorded there and surfaces here on the next push — one
    /// message later than before, with the same outcome.
    fn closed(&self) -> anyhow::Error {
        match self.shared.failure.lock().unwrap().take() {
            Some(message) => anyhow::anyhow!(message),
            None => anyhow::anyhow!("frame channel closed"),
        }
    }
}

/// Encode one rectangle of packed RGB888 with the given codec.
fn encode_tile(rect: Rect, rgb: &[u8], codec: TileCodec) -> anyhow::Result<Tile> {
    let (x, y, w, h) = (rect.left, rect.top, rect.w(), rect.h());
    match codec {
        TileCodec::Png => Tile::from_rgb(x, y, w, h, rgb),
        TileCodec::Jpeg(q) => Tile::from_rgb_jpeg(x, y, w, h, rgb, q),
        TileCodec::Webp(q) => Tile::from_rgb_webp(x, y, w, h, rgb, q),
    }
}

/// Re-send cells that have stopped moving at the base encode, so a paused screen
/// sharpens on its own. `false` means the browser is gone.
///
/// A cleanup that fails to encode is dropped rather than ending the session, which
/// is the one place this path differs from an ordinary tile: a tile that never
/// arrives leaves the shadow claiming the client has pixels it never got, while a
/// cleanup that never arrives leaves the client with pixels that are correct and
/// merely coarser.
///
/// Dropped rather than put back, because by then the pixels may be stale. The
/// encode runs across an `await`, and the engine's read loop stashes and settles
/// without passing through this task, so the cell can have moved on — and
/// [`Motion::take_due`] has already forgotten the old debt, so neither a new
/// [`Motion::stash`] nor a [`Motion::settle`] in that window leaves anything here
/// could test. Putting the old pixels back would let a later cleanup paint them
/// over newer ones the shadow already counts as delivered, which is wrong content
/// rather than coarse content, and wrong until that region happens to change again.
/// Dropping leaves the region at the motion encode until it next changes, which is
/// the same state the stash cap already produces and is accepted there.
async fn flush_cleanups(
    engine: &'static str,
    shared: &Arc<Shared>,
    base: TileCodec,
    debug: bool,
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> bool {
    let due = shared
        .motion
        .lock()
        .unwrap()
        .take_due(tokio::time::Instant::now(), MAX_CLEANUPS_PER_TICK);
    // Started together and collected in order, which is how the ordinary tile path
    // already works and for the same reason: this is the one task everything else
    // is forwarded through, so awaiting a tickful of encodes one after another
    // holds up whatever is still moving elsewhere on the screen for the sum of them
    // rather than for the longest. A cleanup usually arrives on a quiet screen, but
    // `CLEANUP_IDLE` is per cell — one region settles while another is still going.
    //
    // `sent_at` has done its work in `take_due`; nothing past here cares how old the
    // debt was, only that it is being discharged now.
    let started: Vec<(Rect, JoinHandle<anyhow::Result<Tile>>)> = due
        .into_iter()
        .map(|Stashed { rect, rgb, sent_at: _ }| {
            let rgb = if debug { marked(&rgb, rect, MARK_CLEANUP) } else { rgb };
            (rect, tokio::task::spawn_blocking(move || encode_tile(rect, &rgb, base)))
        })
        .collect();

    for (rect, handle) in started {
        let tile = match handle.await {
            Ok(Ok(tile)) => tile,
            Ok(Err(e)) => {
                warn!(
                    "{engine}: dropping a cleanup for {}x{} at ({},{}) that would not encode; \
                     it stays at the motion encode until it changes again: {e:#}",
                    rect.w(),
                    rect.h(),
                    rect.left,
                    rect.top
                );
                continue;
            }
            Err(e) => {
                give_up(engine, shared, format!("tile encoder stopped: {e}"));
                return false;
            }
        };
        debug!(
            "{engine}: cleanup {}x{} at ({},{}): {} bytes",
            tile.w,
            tile.h,
            tile.x,
            tile.y,
            tile.data.len()
        );
        let bytes = tile.data.len() as u64;
        shared.tiles.fetch_add(1, Ordering::Relaxed);
        shared.cleanups.fetch_add(1, Ordering::Relaxed);
        shared.encoded_bytes.fetch_add(bytes, Ordering::Relaxed);
        shared.cleanup_bytes.fetch_add(bytes, Ordering::Relaxed);
        if frame_tx.send(ServerMsg::Tile(tile)).await.is_err() {
            return false;
        }
    }
    true
}

/// [`flush_cleanups`] for a target whose moving encode is a stream per region.
///
/// The same job with a much shorter argument behind it. The still path has to
/// remember the pixels it approximated, and everything hard about it follows from
/// those pixels going stale: whether a debt may be replaced, what a partial cover
/// does, what happens when the stash is full. Here the mirror already holds the exact
/// current source for every pixel, so a cleanup is a crop of it and is the newest
/// truth by construction. The debt is two words — which cell, and when a unit last
/// carried it.
///
/// A cell a live stream still covers is never due, so nothing here can overtake a
/// stream that is still running.
async fn flush_stream_cleanups(
    engine: &'static str,
    shared: &Arc<Shared>,
    base: TileCodec,
    debug: bool,
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> bool {
    let mut due: Vec<(Rect, Vec<u8>)> = Vec::new();
    {
        // One critical section for the whole tickful: `due` and the crops have to
        // agree about the mirror, and holding the lock across the encodes below would
        // make every `damage` wait on them.
        let mut video = shared.video.lock().await;
        let now = tokio::time::Instant::now();
        // A screen that has stopped changing produces no frame boundary, so this is
        // the only thing that will ever notice its streams have gone quiet — and a
        // stream that never ends is a region that never comes due.
        video.regions.expire(now);
        let rects = video.regions.due(now, CLEANUP_IDLE, MAX_CLEANUPS_PER_TICK);
        for rect in rects {
            let mut rgb = Vec::new();
            match video.regions.crop(rect, &mut rgb) {
                Ok(()) => due.push((rect, rgb)),
                // The debt is gone with the read that failed, and that is the safe
                // direction: the cell keeps the stream's rendition until it changes
                // again, which is the same state a dropped cleanup already leaves.
                Err(e) => warn!("{engine}: dropping a cleanup that would not crop: {e:#}"),
            }
        }
    }

    let started: Vec<(Rect, JoinHandle<anyhow::Result<Tile>>)> = due
        .into_iter()
        .map(|(rect, rgb)| {
            let rgb = Arc::new(rgb);
            let rgb = if debug { marked(&rgb, rect, MARK_CLEANUP) } else { rgb };
            (rect, tokio::task::spawn_blocking(move || encode_tile(rect, &rgb, base)))
        })
        .collect();

    for (rect, handle) in started {
        let tile = match handle.await {
            Ok(Ok(tile)) => tile,
            Ok(Err(e)) => {
                warn!(
                    "{engine}: dropping a cleanup for {}x{} at ({},{}) that would not encode; \
                     it stays at the stream's quality until it changes again: {e:#}",
                    rect.w(),
                    rect.h(),
                    rect.left,
                    rect.top
                );
                continue;
            }
            Err(e) => {
                give_up(engine, shared, format!("tile encoder stopped: {e}"));
                return false;
            }
        };
        let bytes = tile.data.len() as u64;
        shared.tiles.fetch_add(1, Ordering::Relaxed);
        shared.cleanups.fetch_add(1, Ordering::Relaxed);
        shared.encoded_bytes.fetch_add(bytes, Ordering::Relaxed);
        shared.cleanup_bytes.fetch_add(bytes, Ordering::Relaxed);
        if frame_tx.send(ServerMsg::Tile(tile)).await.is_err() {
            return false;
        }
    }
    true
}

/// Collect finished encodes in push order and forward them, and — for a target on
/// the `motion` strategy — settle what has stopped moving.
///
/// The cleanup timer belongs here rather than anywhere the frames arrive, because
/// the case it exists for is a screen that has stopped producing them.
async fn order_loop(
    engine: &'static str,
    mut rx: mpsc::Receiver<Pending>,
    frame_tx: mpsc::Sender<ServerMsg>,
    shared: Arc<Shared>,
    plan: RenderPlan,
) {
    // Only the motion strategy has anything to settle, and its two encodes settle
    // differently: a still remembers the pixels it approximated, a stream leaves the
    // debt to the mirror. A whole-desktop video stream has no cells and no debts —
    // its next frame carries whatever the last one approximated — and a plain tiles
    // plan sends everything crisp the first time.
    let settling = match plan {
        RenderPlan::Tiles { base, motion: Some(motion), debug } => Some((base, motion, debug)),
        _ => None,
    };
    let mut cleanup = tokio::time::interval(CLEANUP_TICK);
    // Delay rather than Burst: a tick missed while the queue was busy is a tick
    // whose cells are still there to settle, and firing the backlog at once would
    // only stack re-encodes behind whatever made it late.
    cleanup.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let item = tokio::select! {
            item = rx.recv() => match item {
                Some(item) => item,
                None => break,
            },
            _ = cleanup.tick(), if settling.is_some() => {
                let (base, motion, debug) = settling.expect("the arm is guarded on it");
                let settled = match motion {
                    MotionEncode::Tile(_) => {
                        flush_cleanups(engine, &shared, base, debug, &frame_tx).await
                    }
                    MotionEncode::Stream { .. } => {
                        flush_stream_cleanups(engine, &shared, base, debug, &frame_tx).await
                    }
                };
                if settled {
                    continue;
                }
                break;
            }
        };
        let msg = match item {
            Pending::Msg(msg) => msg,
            Pending::Flush(ack) => {
                // Everything before this is already through, which is the whole
                // claim; the ack costs nothing and asks for no ordering of its own.
                let _ = ack.send(());
                continue;
            }
            Pending::Round { units, encode_micros } => {
                shared.encode_micros.fetch_add(encode_micros, Ordering::Relaxed);
                // Counted as waiting too, and honestly so: this round was encoded on
                // the engine's own task, so the read loop really did wait the whole of
                // it. That keeps `encode` against `waiting` meaning what its doc says
                // — and it reads 1.0 for a session that is all stream, which is the
                // truth: an inter-frame stream has nothing to overlap with.
                shared.waited_micros.fetch_add(encode_micros, Ordering::Relaxed);
                let mut gone = false;
                for unit in units {
                    let bytes = unit.data.len() as u64;
                    shared.units.fetch_add(1, Ordering::Relaxed);
                    shared.encoded_bytes.fetch_add(bytes, Ordering::Relaxed);
                    if unit.keyframe {
                        shared.keyframes.fetch_add(1, Ordering::Relaxed);
                        shared.keyframe_bytes.fetch_add(bytes, Ordering::Relaxed);
                    }
                    debug!(
                        "{engine}: stream {} {} {}x{} at ({},{}): {bytes} bytes",
                        unit.stream,
                        if unit.keyframe { "keyframe" } else { "frame" },
                        unit.w,
                        unit.h,
                        unit.x,
                        unit.y
                    );
                    if frame_tx.send(ServerMsg::Video(unit)).await.is_err() {
                        gone = true;
                        break;
                    }
                }
                if gone {
                    break; // browser gone; the engine learns it from its own next push
                }
                continue;
            }
            Pending::Tile(handle) => {
                let started = Instant::now();
                let joined = handle.await;
                shared.waited_micros.fetch_add(micros(started), Ordering::Relaxed);
                match joined {
                    Ok(Ok((tile, encode_micros))) => {
                        shared.tiles.fetch_add(1, Ordering::Relaxed);
                        shared
                            .encoded_bytes
                            .fetch_add(tile.data.len() as u64, Ordering::Relaxed);
                        shared.encode_micros.fetch_add(encode_micros, Ordering::Relaxed);
                        debug!(
                            "{engine}: tile {}x{} at ({},{}): {} -> {} bytes",
                            tile.w,
                            tile.h,
                            tile.x,
                            tile.y,
                            usize::from(tile.w) * usize::from(tile.h) * 3,
                            tile.data.len()
                        );
                        ServerMsg::Tile(tile)
                    }
                    // Ends the session, as encoding on the read loop did: the
                    // shadow already counts those pixels as delivered.
                    Ok(Err(e)) => {
                        give_up(engine, &shared, format!("tile encode failed: {e}"));
                        break;
                    }
                    // Only reachable by cancellation — `panic = "abort"` in release
                    // means a panicking worker never gets this far.
                    Err(e) => {
                        give_up(engine, &shared, format!("tile encoder stopped: {e}"));
                        break;
                    }
                }
            }
        };
        if frame_tx.send(msg).await.is_err() {
            break; // browser gone; the engine learns it from its own next push
        }
    }
}

/// Record why the queue stopped, for the engine's next push to report.
fn give_up(engine: &str, shared: &Shared, message: String) {
    warn!("{engine}: {message}");
    *shared.failure.lock().unwrap() = Some(message);
}

fn micros(since: Instant) -> u64 {
    since.elapsed().as_micros() as u64
}

/// A snapshot of what one engine's encoder cost, for [`TileSink::report`].
///
/// The repo has no benchmark harness, so — like `wire::Totals` for the browser
/// link — this line is the only measurement of the encoder that exists in
/// production. Each number earns its place by answering something the others
/// cannot:
///
/// - `encode` against `waiting` says whether bands overlapped, and it is **not** the
///   parallelism achieved — read as that it flatters itself. `waiting` accrues only
///   while the order task finds a handle *unfinished*, so a band already encoded when
///   its turn came adds encode time and no waiting at all. The ratio is an upper bound
///   on the concurrency and can exceed [`ENCODE_DEPTH`] outright. Its low end is the
///   sound part: 1.0 means every collect blocked for the whole of its band, so nothing
///   overlapped.
/// - `stalled` is what the read loop still pays. It is the number that says whether
///   [`ENCODE_DEPTH`] is the binding constraint: zero means the engine never waited
///   for the encoder at all, which is the point of the whole module.
/// - `bytes` cross-checks against the `ws: outbound totals` line, and must not move
///   when the depth does — the same pixels are encoded either way.
/// - `motion` and `cleanup` are the whole measurement of the `motion` strategy, and
///   they are read together. `motion` at zero on a target that has one configured
///   is the claim that a still screen is untouched, and it is the number to check
///   before believing any saving. `cleanup` against it is what the discount cost:
///   every cleanup is a tile sent twice, so a scheme that pays more in re-sends than
///   it saves in motion shows up here as a `cleanup` byte count rivalling the
///   saving — and both are already inside `tiles` and `bytes`, which stay the
///   totals for the link.
/// - `unit`, `keyframe` and `coarsened` are the same job for the streams, and the
///   same warning applies to reading any of them alone. A keyframe count rivalling
///   `unit` means the streams are getting no inter-frame compression at all, which is
///   the entire claim they make; subtracting the keyframe bytes from `bytes` gives the
///   average delta, which is the number that says whether they are winning.
///   `coarsened` is how many rounds went out below the configured quality because the
///   link could not carry it, with the worst quantizer reached — zero there means the
///   dial was the only thing deciding, and the measurement is clean.
/// - `skipped` must be zero. It counts frames the encoder produced no bitstream for,
///   whose pixels are carried by the next frame instead. Non-zero means a hazard that
///   is supposed to be unreachable is not.
///
/// `tiles` counts still tiles and `unit` counts access units, so both are comparable
/// across every dial: a `motion` session's tiles are the same kind of thing as a
/// `motion` + `h264` session's, and only `bytes` compares the two transports.
struct Totals {
    tiles: u64,
    encoded_bytes: u64,
    motion_tiles: u64,
    cleanups: u64,
    cleanup_bytes: u64,
    units: u64,
    keyframes: u64,
    keyframe_bytes: u64,
    skipped: u64,
    coarsened: u64,
    coarsest_qp: u64,
    encode_micros: u64,
    waited_micros: u64,
    stalled_micros: u64,
}

impl Totals {
    fn of(shared: &Shared) -> Self {
        // Relaxed throughout, and read while the order task may still be running:
        // this is a log line, not a decision, and the counters only ever grow.
        Self {
            tiles: shared.tiles.load(Ordering::Relaxed),
            encoded_bytes: shared.encoded_bytes.load(Ordering::Relaxed),
            motion_tiles: shared.motion_tiles.load(Ordering::Relaxed),
            cleanups: shared.cleanups.load(Ordering::Relaxed),
            cleanup_bytes: shared.cleanup_bytes.load(Ordering::Relaxed),
            units: shared.units.load(Ordering::Relaxed),
            keyframes: shared.keyframes.load(Ordering::Relaxed),
            keyframe_bytes: shared.keyframe_bytes.load(Ordering::Relaxed),
            skipped: shared.skipped.load(Ordering::Relaxed),
            coarsened: shared.coarsened.load(Ordering::Relaxed),
            coarsest_qp: shared.coarsest_qp.load(Ordering::Relaxed),
            encode_micros: shared.encode_micros.load(Ordering::Relaxed),
            waited_micros: shared.waited_micros.load(Ordering::Relaxed),
            stalled_micros: shared.stalled_micros.load(Ordering::Relaxed),
        }
    }
}

impl fmt::Display for Totals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} tile(s) / {} bytes ({} in motion, {} cleanup / {} bytes), \
             {} access unit(s), {} keyframe(s) / {} bytes, {} skipped, \
             {} round(s) coarsened (worst qp {}), \
             {}µs encoding across workers in {}µs of waiting, engine stalled {}µs",
            self.tiles,
            self.encoded_bytes,
            self.motion_tiles,
            self.cleanups,
            self.cleanup_bytes,
            self.units,
            self.keyframes,
            self.keyframe_bytes,
            self.skipped,
            self.coarsened,
            self.coarsest_qp,
            self.encode_micros,
            self.waited_micros,
            self.stalled_micros
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::UNSCALED;

    fn plan(base: TileCodec, motion: Option<TileCodec>) -> RenderPlan {
        RenderPlan::Tiles { base, motion: motion.map(MotionEncode::Tile), debug: false }
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a non-empty rectangle")
    }

    /// Packed RGB888 for a `w`x`h` band, filled so no two bands share bytes.
    fn rgb(w: u16, h: u16, seed: u8) -> Vec<u8> {
        (0..usize::from(w) * usize::from(h) * 3)
            .map(|i| seed.wrapping_add((i % 251) as u8))
            .collect()
    }

    /// The assertion every test here makes: what came out, in the order it came.
    async fn drain(rx: &mut mpsc::Receiver<ServerMsg>, count: usize) -> Vec<ServerMsg> {
        let mut out = Vec::new();
        for _ in 0..count {
            out.push(rx.recv().await.expect("frame channel closed early"));
        }
        out
    }

    /// Sizes deliberately unequal and descending, so a sink that forwarded
    /// whatever finished first would almost certainly interleave them. The
    /// assertion is on order alone, which holds however fast the machine is.
    #[tokio::test]
    async fn tiles_reach_the_frame_channel_in_push_order() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Png, None));

        for i in 0..64u16 {
            let (w, h) = (320 - i * 4, 64);
            sink.tile(0, i * 64, w, h, rgb(w, h, i as u8)).await.unwrap();
        }
        sink.flush().await;

        for (i, msg) in drain(&mut frame_rx, 64).await.into_iter().enumerate() {
            let ServerMsg::Tile(tile) = msg else {
                panic!("expected a tile at {i}");
            };
            assert_eq!(tile.y, i as u16 * 64, "tiles left the sink out of order");
            assert_eq!(tile.format, Tile::FORMAT_PNG);
        }
    }

    /// A sink built with a JPEG quality encodes its tiles as JPEG; the default
    /// (`None`, asserted above) stays PNG. The one bit the render dial threads
    /// all the way to the wire.
    #[tokio::test]
    async fn a_jpeg_quality_makes_tiles_jpeg() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Jpeg(60), None));

        sink.tile(0, 0, 320, 64, rgb(320, 64, 1)).await.unwrap();
        sink.flush().await;

        let ServerMsg::Tile(tile) = &drain(&mut frame_rx, 1).await[0] else {
            panic!("expected a tile");
        };
        assert_eq!(tile.format, Tile::FORMAT_JPEG);
    }

    /// Same for the other lossy codec: a WebP-configured sink emits WebP tiles.
    #[tokio::test]
    async fn a_webp_quality_makes_tiles_webp() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Webp(60), None));

        sink.tile(0, 0, 320, 64, rgb(320, 64, 1)).await.unwrap();
        sink.flush().await;

        let ServerMsg::Tile(tile) = &drain(&mut frame_rx, 1).await[0] else {
            panic!("expected a tile");
        };
        assert_eq!(tile.format, Tile::FORMAT_WEBP);
    }

    /// The hazard a side channel for control messages would create: the client
    /// must learn a new size before a tile in the new coordinate space arrives.
    #[tokio::test]
    async fn a_control_message_cannot_overtake_the_tiles_before_it() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Png, None));

        for i in 0..8u16 {
            sink.tile(0, i * 64, 320, 64, rgb(320, 64, i as u8)).await.unwrap();
        }
        sink.msg(ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED }).await.unwrap();
        sink.tile(0, 0, 16, 16, rgb(16, 16, 9)).await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 10).await;
        for (i, msg) in out.iter().take(8).enumerate() {
            let ServerMsg::Tile(tile) = msg else {
                panic!("expected a tile at {i}");
            };
            assert_eq!(tile.y, i as u16 * 64);
        }
        assert!(
            matches!(out[8], ServerMsg::Resize { w: 640, .. }),
            "the resize did not keep its place"
        );
        assert!(matches!(out[9], ServerMsg::Tile(_)));
    }

    #[tokio::test]
    async fn flush_waits_for_everything_pushed_before_it() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Png, None));

        for i in 0..16u16 {
            sink.tile(0, i * 64, 320, 64, rgb(320, 64, i as u8)).await.unwrap();
        }
        sink.flush().await;

        // Everything is already queued on the frame channel, so this is a
        // synchronous drain rather than a wait — which is the claim.
        for i in 0..16u16 {
            match frame_rx.try_recv().expect("flush returned with tiles still in flight") {
                ServerMsg::Tile(tile) => assert_eq!(tile.y, i * 64),
                other => panic!("expected a tile, got {other:?}"),
            }
        }
    }

    /// A failing encode ends the session, as it did when it `?`-ed out of the
    /// engine's own loop — and it says why, rather than reading as a closed channel.
    #[tokio::test]
    async fn an_encode_failure_stops_the_sink_and_reports_itself() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Png, None));

        // A payload one byte short of the geometry: `Tile::from_rgb` rejects it on
        // its length check rather than handing a short buffer to the PNG encoder.
        let short = rgb(32, 32, 0)[1..].to_vec();
        // Accepted: the queue takes work without running it, so the engine's own
        // push cannot be what reports this.
        sink.tile(0, 0, 32, 32, short).await.unwrap();

        // A flush is how a test reaches the failure at any `ENCODE_DEPTH`: it makes
        // the order task get as far as the bad tile, and returns when the task drops
        // the ack rather than answering it.
        sink.flush().await;
        let error = sink
            .msg(ServerMsg::RemoteOs { macos: false })
            .await
            .expect_err("the sink kept accepting work after a failed encode");
        let text = format!("{error:#}");
        assert!(text.contains("tile encode failed"), "{text}");
        assert!(text.contains("expected 3072"), "the cause is lost: {text}");
        assert!(frame_rx.recv().await.is_none(), "nothing follows a failed encode");
    }

    /// Not an error: a browser that leaves mid-frame ends the sink the same way a
    /// full engine teardown does, and the engine hears about it on its next push.
    #[tokio::test]
    async fn a_dropped_frame_channel_is_reported_as_a_closed_channel() {
        let (frame_tx, frame_rx) = mpsc::channel(1);
        let sink = TileSink::new("test", frame_tx, plan(TileCodec::Png, None));
        drop(frame_rx);

        sink.tile(0, 0, 320, 64, rgb(320, 64, 0)).await.unwrap();
        sink.flush().await; // the order task discovers it has nowhere to forward to
        let error = sink
            .msg(ServerMsg::RemoteOs { macos: false })
            .await
            .expect_err("the sink accepted work with nowhere to put it");
        // No encode failed, so this is the plain closed-channel message the engines
        // reported before the sink existed.
        assert_eq!(format!("{error}"), "frame channel closed");
    }

    // ---- motion ----
    //
    // Churn is counted in wall-clock slots, so every test below either drives
    // `Motion` directly with instants it made up or runs under `start_paused` and
    // moves the clock itself. Nothing sleeps for real and nothing counts events, so
    // no assertion here changes if the machine is twice as slow.

    const MOTION: RenderPlan = RenderPlan::Tiles {
        base: TileCodec::Png,
        motion: Some(MotionEncode::Tile(TileCodec::Jpeg(10))),
        debug: false,
    };

    const MOTION_DEBUG: RenderPlan = RenderPlan::Tiles {
        base: TileCodec::Png,
        motion: Some(MotionEncode::Tile(TileCodec::Jpeg(10))),
        debug: true,
    };

    /// A report that every cell of `rect` really did change — damage whose bounding
    /// box is hiding nothing, which is what most tests here mean by a rectangle.
    fn all_of(rect: Rect) -> Changed {
        let mut cells: Vec<(u16, u16)> = rect.cells().map(|c| c.cell_key()).collect();
        cells.sort_unstable();
        cells.dedup();
        Changed { rect, cells }
    }

    /// Damage given to `sink` once per churn slot for `slots` slots — what a video
    /// playing in a window looks like from here. Requires a paused clock.
    async fn drive(sink: &TileSink, area: Rect, slots: u64) {
        for _ in 0..slots {
            sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 7)).await.unwrap();
            tokio::time::advance(CHURN_SLOT).await;
        }
    }

    fn formats(msgs: &[ServerMsg]) -> Vec<u8> {
        msgs.iter()
            .map(|m| match m {
                ServerMsg::Tile(tile) => tile.format,
                other => panic!("expected a tile, got {other:?}"),
            })
            .collect()
    }

    /// A slot's worth of instants, for driving `Motion` by hand.
    fn slots(base: tokio::time::Instant, n: u64) -> tokio::time::Instant {
        base + CHURN_SLOT * u32::try_from(n).unwrap()
    }

    #[test]
    fn churn_counts_recent_slots_and_ages_out() {
        let mut motion = Motion::default();
        let key = (0, 0);
        let base = tokio::time::Instant::now();
        // Changing every slot ramps churn up one per slot, to the window width.
        for slot in 0..CHURN_WINDOW {
            assert_eq!(
                u64::from(motion.observe(key, slots(base, slot))),
                slot + 1,
                "slot {slot}"
            );
        }
        // It saturates at the window rather than overflowing the register.
        assert_eq!(
            u64::from(motion.observe(key, slots(base, CHURN_WINDOW))),
            CHURN_WINDOW,
            "churn should cap at the window"
        );
        // A gap longer than the window empties the history: one recent change only.
        assert_eq!(
            motion.observe(key, slots(base, CHURN_WINDOW * 3)),
            1,
            "a long-idle cell reset"
        );
    }

    /// Churn is how much of a stretch of time a cell was busy for, not how many
    /// rectangles described it. This is the property that decides the whole scheme:
    /// a burst of damage in one instant is not motion, and an engine that reports
    /// the same change as ten rectangles must not read as ten times as busy.
    #[test]
    fn changes_inside_one_slot_count_once() {
        let mut motion = Motion::default();
        let base = tokio::time::Instant::now();
        for i in 0..40 {
            let churn = motion.observe((0, 0), base + Duration::from_millis(i));
            assert_eq!(churn, 1, "a flurry inside one slot counted as motion");
        }
        // And the next slot advances it by exactly one.
        assert_eq!(motion.observe((0, 0), slots(base, 1)), 2);
    }

    /// The claim the whole design rests on: a target with nothing moving sends
    /// exactly what it would send without a motion plan at all — same rectangles,
    /// same codec, same bytes. Asserted against a second sink rather than against a
    /// written-down expectation, so it cannot drift.
    #[tokio::test(start_paused = true)]
    async fn a_still_screen_is_byte_identical_to_its_base_configuration() {
        let area = rect(37, 41, 900, 200);
        let mut out = Vec::new();
        for plan in [plan(TileCodec::Png, None), MOTION] {
            let (frame_tx, mut frame_rx) = mpsc::channel(256);
            let sink = TileSink::new("test", frame_tx, plan);
            // A screen that changes now and then rather than continuously: the same
            // region redrawn four times, but with a full churn window of quiet
            // between each, so no cell is ever in motion. Four redraws rather than
            // one, so a plan that simply never split would not pass by never being
            // asked to.
            for _ in 0..4 {
                sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 7)).await.unwrap();
                tokio::time::advance(CHURN_SLOT * u32::try_from(CHURN_WINDOW).unwrap()).await;
            }
            sink.flush().await;

            let mut tiles = Vec::new();
            while let Ok(ServerMsg::Tile(tile)) = frame_rx.try_recv() {
                tiles.push((tile.format, tile.x, tile.y, tile.w, tile.h, tile.data));
            }
            out.push(tiles);
        }
        assert_eq!(out[0], out[1], "a motion plan changed a still screen's output");
        assert!(!out[0].is_empty(), "the test sent nothing");
    }

    /// A cell changing every slot switches to the motion encode once its churn
    /// reaches the threshold, and not one slot before.
    #[tokio::test(start_paused = true)]
    async fn a_cell_changing_every_slot_switches_at_the_threshold() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION);

        // One cell, so one tile per slot and the tile index is the slot index.
        let cell = rect(0, 0, 320, 64);
        drive(&sink, cell, u64::from(CHURN_MOVING) + 2).await;
        sink.flush().await;

        let sent = formats(&drain(&mut frame_rx, usize::try_from(CHURN_MOVING).unwrap() + 2).await);
        let (before, after) = sent.split_at(usize::try_from(CHURN_MOVING).unwrap() - 1);
        assert!(
            before.iter().all(|&f| f == Tile::FORMAT_PNG),
            "a cell went to the motion encode before it had the churn for it: {sent:?}"
        );
        assert!(
            after.iter().all(|&f| f == Tile::FORMAT_JPEG),
            "a cell at the threshold stayed on the base encode: {sent:?}"
        );
    }

    /// The payoff the grid buys: a video in a window costs its own cells their
    /// quality and costs the text beside it nothing. The band spans four cells and
    /// only the first keeps changing.
    #[tokio::test(start_paused = true)]
    async fn only_the_cells_in_motion_lose_their_quality() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION);

        let moving = rect(0, 0, 320, 64);
        let band = rect(0, 0, 1280, 64);
        // Enough slots of the small region alone to put its cell in motion.
        drive(&sink, moving, u64::from(CHURN_MOVING)).await;
        // Then one report of the whole band, which now contains one moving cell and
        // three quiet ones.
        sink.damage(&all_of(band), |piece| rgb(piece.w(), piece.h(), 7)).await.unwrap();
        sink.flush().await;

        let all = drain(&mut frame_rx, usize::try_from(CHURN_MOVING).unwrap() + 4).await;
        let split = &all[usize::try_from(CHURN_MOVING).unwrap()..];
        assert_eq!(
            formats(split),
            vec![
                Tile::FORMAT_JPEG,
                Tile::FORMAT_PNG,
                Tile::FORMAT_PNG,
                Tile::FORMAT_PNG
            ],
            "the band was not cut at the cell the motion is in"
        );
        let ServerMsg::Tile(first) = &split[0] else { unreachable!() };
        assert_eq!((first.x, first.w), (0, 320), "the moving piece is one cell wide");
        let ServerMsg::Tile(last) = &split[3] else { unreachable!() };
        assert_eq!((last.x, last.w), (960, 320), "the quiet pieces cover the rest");
    }

    #[test]
    fn a_settled_cell_is_cleaned_up_once_then_forgotten() {
        let mut motion = Motion::default();
        let cell = rect(0, 0, 320, 64);
        let base = tokio::time::Instant::now();
        assert!(motion.stash((0, 0), cell, Arc::new(rgb(320, 64, 1)), base));

        assert!(motion.take_due(base, 8).is_empty(), "settled before it was idle");
        assert!(
            motion.take_due(base + CLEANUP_IDLE - Duration::from_millis(1), 8).is_empty(),
            "settled a millisecond early"
        );
        let due = motion.take_due(base + CLEANUP_IDLE, 8);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].rect, cell);
        assert_eq!(motion.stash_bytes, 0, "the debt was not refunded");
        assert!(
            motion.take_due(base + CLEANUP_IDLE * 4, 8).is_empty(),
            "a cell was cleaned up twice"
        );
    }

    /// Damage is sent as reported, clipped to the cell rather than snapped out to
    /// it, so a piece that goes out crisp may cover less than the cell owes. Only
    /// full cover settles the debt; anything less would strand a sliver at the
    /// motion encode with nothing left to remember it.
    #[test]
    fn only_full_cover_cancels_a_pending_cleanup() {
        let mut motion = Motion::default();
        let owed = rect(0, 0, 320, 64);
        let stash = |m: &mut Motion| {
            assert!(m.stash((0, 0), owed, Arc::new(rgb(320, 64, 1)), tokio::time::Instant::now()))
        };

        stash(&mut motion);
        let half = rect(0, 0, 160, 64);
        motion.settle((0, 0), half, &rgb(160, 64, 2));
        assert_eq!(motion.stash.len(), 1, "a partial cover cancelled the whole debt");

        motion.settle((0, 0), owed, &rgb(320, 64, 2));
        assert!(motion.stash.is_empty(), "an exact cover left the debt standing");
        assert_eq!(motion.stash_bytes, 0);

        stash(&mut motion);
        motion.settle((0, 0), rect(0, 0, 1280, 64), &rgb(1280, 64, 2));
        assert!(motion.stash.is_empty(), "a band covering the cell left the debt standing");
    }

    /// The cursor bug: a debt holds the frame it was recorded on, so anything that
    /// changed inside it since — the pointer RDP composites into the framebuffer,
    /// having moved on — would be painted back by the cleanup, over the crisp send
    /// that replaced it. Wrong content, not coarse content, and permanent: the shadow
    /// has already recorded the newer pixels as delivered.
    #[test]
    fn a_partial_cover_brings_the_debt_it_leaves_standing_up_to_date() {
        let mut motion = Motion::default();
        let owed = rect(0, 0, 320, 64);
        // Flat colours rather than `rgb`'s gradient, so which pixels came from which
        // send is readable a byte at a time.
        let flat = |w: usize, h: usize, v: u8| vec![v; w * h * 3];
        assert!(motion.stash(
            (0, 0),
            owed,
            Arc::new(flat(320, 64, 1)),
            tokio::time::Instant::now()
        ));

        // The left half changes and goes out crisp, which settles nothing: the right
        // half is still owed, and is still worth a cleanup.
        motion.settle((0, 0), rect(0, 0, 160, 64), &flat(160, 64, 2));
        let due = motion.take_due(tokio::time::Instant::now() + CLEANUP_IDLE, 8);
        assert_eq!(due.len(), 1, "the debt did not survive a partial cover");

        let row = |y: usize| &due[0].rgb[y * 320 * 3..][..320 * 3];
        assert!(
            row(0)[..160 * 3].iter().all(|&b| b == 2),
            "the cleanup would have repainted the half that changed under it"
        );
        assert!(
            row(0)[160 * 3..].iter().all(|&b| b == 1),
            "the cleanup lost the half it is actually owed for"
        );
        assert!(row(63)[..160 * 3].iter().all(|&b| b == 2), "only the first row was patched");
    }

    /// A debt is patched only where the newer pixels reach. A send that misses it
    /// entirely says nothing about it.
    #[test]
    fn a_send_that_misses_a_debt_leaves_it_alone() {
        let mut motion = Motion::default();
        let owed = rect(0, 0, 320, 64);
        let flat = |v: u8| vec![v; 320 * 64 * 3];
        assert!(motion.stash((0, 0), owed, Arc::new(flat(1)), tokio::time::Instant::now()));

        // The cell next door, which is a different key anyway, and a strip below.
        motion.settle((0, 0), rect(320, 0, 320, 64), &flat(2));
        motion.settle((0, 0), rect(0, 64, 320, 64), &flat(2));
        let due = motion.take_due(tokio::time::Instant::now() + CLEANUP_IDLE, 8);
        assert_eq!(due.len(), 1);
        assert!(due[0].rgb.iter().all(|&b| b == 1), "a debt was patched from outside itself");
    }

    /// A whole stopped video settles over a few ticks rather than in one burst, and
    /// in the order the debts accrued — otherwise a long-settled cell can be
    /// starved by newer ones for as long as the motion lasts.
    #[test]
    fn cleanups_are_bounded_per_tick_and_drain_oldest_first() {
        let mut motion = Motion::default();
        let base = tokio::time::Instant::now();
        let count = MAX_CLEANUPS_PER_TICK + 3;
        for i in 0..count {
            let key = (i as u16, 0);
            let cell = rect(i as u16 * 320, 0, 320, 64);
            // Staggered, so "oldest" is a fact about the data rather than about
            // whichever order the map happens to iterate in.
            let sent_at = base + Duration::from_millis(i as u64);
            assert!(motion.stash(key, cell, Arc::new(rgb(320, 64, 1)), sent_at));
        }

        let now = base + Duration::from_millis(count as u64) + CLEANUP_IDLE;
        let first = motion.take_due(now, MAX_CLEANUPS_PER_TICK);
        assert_eq!(first.len(), MAX_CLEANUPS_PER_TICK);
        assert_eq!(
            first.iter().map(|s| s.rect.left).collect::<Vec<_>>(),
            (0..MAX_CLEANUPS_PER_TICK).map(|i| i as u16 * 320).collect::<Vec<_>>(),
            "the backlog did not drain oldest first"
        );
        let second = motion.take_due(now, MAX_CLEANUPS_PER_TICK);
        assert_eq!(second.len(), 3, "the rest did not follow on the next tick");
        assert_eq!(motion.stash_bytes, 0);
    }

    /// Past the cap a cell keeps the motion encode until it next changes — safe,
    /// just not as crisp. The *existing* entry is what survives: it is the one
    /// closer to coming due, and dropping it would leave that cell owed nothing.
    #[test]
    fn a_full_stash_keeps_the_debt_it_already_has() {
        let mut motion = Motion::default();
        let now = tokio::time::Instant::now();
        let cell = |i: u16| rect(i * 320, 0, 320, 64);
        // 320x64 RGB is 61,440 bytes, so this fills 8 MiB in 136 cells and change.
        let per_cell = 320 * 64 * 3;
        let fits = MAX_STASH_BYTES / per_cell;
        for i in 0..fits {
            assert!(motion.stash((i as u16, 0), cell(i as u16), Arc::new(rgb(320, 64, 1)), now));
        }
        let held = motion.stash_bytes;
        assert!(held + per_cell > MAX_STASH_BYTES, "the stash did not fill");

        // A new cell past the cap is dropped rather than admitted.
        assert!(
            !motion.stash((9000, 0), cell(0), Arc::new(rgb(320, 64, 1)), now),
            "a stash past the cap must report that it recorded nothing"
        );
        assert_eq!(motion.stash.len(), fits, "the cap did not hold");
        assert_eq!(motion.stash_bytes, held);

        // A cell that already owes one is still admitted at the cap, because what it
        // replaces is its own entry: the bytes come back before the new ones are
        // counted, so a same-sized redraw of a cell already in the stash always
        // fits. It has to be admitted, too — a refusal here would send that cell
        // crisp for no reason, since the debt is unchanged either way.
        let older = now - Duration::from_secs(1);
        assert!(motion.stash((0, 0), cell(0), Arc::new(rgb(320, 64, 2)), older));
        assert_eq!(motion.stash.len(), fits);
        assert_eq!(
            motion.take_due(now + CLEANUP_IDLE, 1).len(),
            1,
            "the cell that was already owed a cleanup lost it"
        );
    }

    /// Past the stash cap a cell stays on the base encode rather than taking a
    /// discount nothing is owed for.
    ///
    /// `Shadow` records the *source* pixels as delivered the moment a rectangle is
    /// accepted, so a cell sent lossy with no debt recorded is one the client holds
    /// a worse copy of than the gateway believes, with nothing left that would ever
    /// re-send it. An HP virtual display is big enough to reach this: 8 MiB holds
    /// 136 cells, and 2560×1600 is 200 of them.
    #[tokio::test(start_paused = true)]
    async fn a_cell_past_the_stash_cap_stays_on_the_base_encode() {
        let (frame_tx, mut frame_rx) = mpsc::channel(4096);
        let sink = TileSink::new("test", frame_tx, MOTION);

        // 61,440 bytes a cell, so the stash holds 136 and this asks for 140.
        let (across, down) = (10u16, 14u16);
        let fits = MAX_STASH_BYTES / (320 * 64 * 3);
        let total = usize::from(across) * usize::from(down);
        assert!(total > fits, "the area has to outgrow the stash for this to test it");

        let area = rect(0, 0, across * 320, down * 64);
        drive(&sink, area, u64::from(CHURN_MOVING)).await;
        sink.flush().await;

        // Every slot before the last is under the threshold and goes out as whole
        // bands; the last one splits into cells.
        let bands = usize::from(down) * (usize::try_from(CHURN_MOVING).unwrap() - 1);
        let sent = formats(&drain(&mut frame_rx, bands + total).await);
        let cells = &sent[bands..];
        assert_eq!(
            cells.iter().filter(|&&f| f == Tile::FORMAT_JPEG).count(),
            fits,
            "the discount was handed out to more cells than the stash could record"
        );
        assert!(
            cells[fits..].iter().all(|&f| f == Tile::FORMAT_PNG),
            "a cell went out lossy with no cleanup recorded for it"
        );
    }

    /// The cleanup path end to end, through the order task's own timer: a cell
    /// that stops moving is re-sent crisp with nothing to prompt it, because the
    /// case it exists for is a remote that has stopped sending frames entirely.
    ///
    /// Time is paused, so the sleep costs nothing and the assertion is about
    /// [`CLEANUP_IDLE`] rather than about how fast the machine is.
    #[tokio::test(start_paused = true)]
    async fn a_screen_that_stops_moving_sharpens_on_its_own() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION);

        let cell = rect(0, 0, 320, 64);
        drive(&sink, cell, u64::from(CHURN_MOVING)).await;
        sink.flush().await;
        let moving = drain(&mut frame_rx, usize::try_from(CHURN_MOVING).unwrap()).await;
        assert_eq!(*formats(&moving).last().unwrap(), Tile::FORMAT_JPEG);

        // Nothing is pushed from here on: the remote has gone quiet, and the tick
        // is the only thing left that can act.
        tokio::time::sleep(CLEANUP_IDLE + CLEANUP_TICK * 2).await;

        let ServerMsg::Tile(settled) = &drain(&mut frame_rx, 1).await[0] else {
            panic!("a settled cell was never re-sent");
        };
        assert_eq!(settled.format, Tile::FORMAT_PNG, "it settled to the wrong encode");
        assert_eq!(
            (settled.x, settled.y, settled.w, settled.h),
            (cell.left, cell.top, cell.w(), cell.h()),
            "the cleanup covered something other than the cell that settled"
        );

        // Once, and only once: the debt is discharged, not standing.
        tokio::time::sleep(CLEANUP_IDLE * 4).await;
        assert!(frame_rx.try_recv().is_err(), "a settled cell was cleaned up twice");
    }

    /// A cell the bounding box merely reached over is not a cell that changed, and
    /// must not accrue churn.
    ///
    /// This is what put a still sidebar, a menu bar and a taskbar at quality 10
    /// because a video was playing elsewhere on the same screen: `Shadow::accept`
    /// returns one box round everything that differs, so a video at one end and an
    /// animated banner at the other swept up every cell between them, and four
    /// reports like that in 800ms was the whole screen in motion.
    #[tokio::test(start_paused = true)]
    async fn a_cell_the_bounding_box_only_reached_over_never_goes_into_motion() {
        let (frame_tx, mut frame_rx) = mpsc::channel(4096);
        let sink = TileSink::new("test", frame_tx, MOTION);

        // One band, four cells. The video is at one end, the banner at the other,
        // and the two quiet cells between them are only inside the box.
        let band = rect(0, 0, 320 * 4 - 1, 63);
        let report = Changed { rect: band, cells: vec![(0, 0), (3, 0)] };
        for _ in 0..CHURN_WINDOW {
            sink.damage(&report, |piece| rgb(piece.w(), piece.h(), 7)).await.unwrap();
            tokio::time::advance(CHURN_SLOT).await;
        }
        sink.flush().await;

        {
            let motion = sink.shared.motion.lock().unwrap();
            let churn = |key| motion.churn.get(&key).map_or(0, |c| c.history.count_ones());
            assert_eq!(churn((0, 0)), u32::try_from(CHURN_WINDOW).unwrap());
            assert_eq!(churn((3, 0)), u32::try_from(CHURN_WINDOW).unwrap());
            assert_eq!(churn((1, 0)), 0, "a cell inside the box accrued churn");
            assert_eq!(churn((2, 0)), 0, "a cell inside the box accrued churn");
        }

        // And the split follows: the ends go lossy, the middle stays exact.
        let quiet = usize::try_from(CHURN_MOVING).unwrap() - 1;
        let sent = formats(&drain(&mut frame_rx, quiet + (usize::try_from(CHURN_WINDOW).unwrap() - quiet) * 4).await);
        for (i, chunk) in sent[quiet..].chunks(4).enumerate() {
            assert_eq!(
                chunk,
                [Tile::FORMAT_JPEG, Tile::FORMAT_PNG, Tile::FORMAT_PNG, Tile::FORMAT_JPEG],
                "split {i} put the wrong cells in motion"
            );
        }
    }

    /// A cell holds one cleanup debt, so a second, differently-shaped piece of the
    /// same cell may not quietly replace the first — it takes the base encode
    /// instead.
    ///
    /// This is the pointer trail: a cursor crossing a cell leaves a run of small
    /// rectangles, and overwriting each debt with the next left every one but the
    /// last permanently lossy, with nothing that knew it was owed.
    #[tokio::test(start_paused = true)]
    async fn a_second_piece_of_one_cell_cannot_overwrite_the_debt_of_the_first() {
        let now = tokio::time::Instant::now();
        let mut motion = Motion::default();
        let key = (0, 0);

        let left = rect(0, 0, 99, 63);
        assert!(motion.stash(key, left, Arc::new(rgb(100, 64, 1)), now));

        // A disjoint sliver of the same cell: admitting it would strand `left`.
        let right = rect(200, 0, 299, 63);
        assert!(!motion.stash(key, right, Arc::new(rgb(100, 64, 2)), now));
        assert_eq!(motion.stash.get(&key).map(|s| s.rect), Some(left));

        // One that covers what is owed may replace it: nothing is stranded, and the
        // cleanup that follows restores strictly more.
        let both = rect(0, 0, 299, 63);
        assert!(motion.stash(key, both, Arc::new(rgb(300, 64, 3)), now));
        assert_eq!(motion.stash.get(&key).map(|s| s.rect), Some(both));
    }

    /// The same rule through the sink: the second piece goes out crisp rather than
    /// lossy-and-forgotten.
    #[tokio::test(start_paused = true)]
    async fn a_piece_with_no_debt_of_its_own_goes_out_at_the_base_encode() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION);

        // Put the cell in motion with one sliver of it.
        let left = rect(0, 0, 99, 63);
        drive(&sink, left, u64::from(CHURN_MOVING)).await;
        sink.flush().await;
        let seen = formats(&drain(&mut frame_rx, usize::try_from(CHURN_MOVING).unwrap()).await);
        assert_eq!(*seen.last().unwrap(), Tile::FORMAT_JPEG);

        // A different sliver of the same cell, which is now in motion. It cannot
        // take the discount without stranding the first, so it stays exact.
        let right = rect(200, 0, 299, 63);
        sink.damage(&all_of(right), |piece| rgb(piece.w(), piece.h(), 9)).await.unwrap();
        sink.flush().await;
        assert_eq!(formats(&drain(&mut frame_rx, 1).await), vec![Tile::FORMAT_PNG]);

        // And the first sliver is still owed its cleanup.
        assert_eq!(
            sink.shared.motion.lock().unwrap().stash.get(&(0, 0)).map(|s| s.rect),
            Some(left),
            "the debt for the first piece was lost"
        );
    }

    /// The debug outline is a border and nothing else: the pixels inside it are
    /// what the encoder would have been given anyway, so a marked region is still
    /// legible and QA is reading the real screen with a frame drawn round it.
    #[test]
    fn a_debug_mark_borders_a_piece_and_leaves_its_middle_alone() {
        let piece = rect(320, 64, 320, 64);
        let source = Arc::new(rgb(320, 64, 7));
        let out = marked(&source, piece, MARK_MOTION);

        assert_eq!(**source, rgb(320, 64, 7), "the source pixels were painted over");
        let px = |buf: &[u8], x: usize, y: usize| buf[(y * 320 + x) * 3..][..3].to_vec();
        for (x, y) in [(0, 0), (319, 0), (0, 63), (319, 63), (160, 1), (1, 32), (318, 32)] {
            assert_eq!(px(&out, x, y), MARK_MOTION, "({x},{y}) is on the border");
        }
        for (x, y) in [(2, 2), (160, 32), (317, 61)] {
            assert_eq!(px(&out, x, y), px(&source, x, y), "({x},{y}) is inside it");
        }
    }

    /// The mark goes on the copy handed to the encoder and never on the pixels the
    /// stash owes. Otherwise the cleanup would restore the outline along with the
    /// pixels and the region would keep a magenta frame round it for the rest of
    /// the session, with nothing left that could take it off.
    #[tokio::test(start_paused = true)]
    async fn a_debug_mark_never_reaches_the_pixels_a_cleanup_owes() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION_DEBUG);

        let cell = rect(0, 0, 320, 64);
        drive(&sink, cell, u64::from(CHURN_MOVING)).await;
        sink.flush().await;
        assert_eq!(
            *formats(&drain(&mut frame_rx, usize::try_from(CHURN_MOVING).unwrap()).await)
                .last()
                .unwrap(),
            Tile::FORMAT_JPEG,
            "the marking changed which encode the cell took"
        );

        let owed = {
            let motion = sink.shared.motion.lock().unwrap();
            Arc::clone(&motion.stash.get(&cell.cell_key()).expect("a cleanup is owed").rgb)
        };
        assert_eq!(*owed, rgb(320, 64, 7), "the stash is holding marked pixels");
    }

    /// A resize makes every key name somewhere else, and a repaint re-sends every
    /// pixel at the base encode. Either way nothing carries across.
    #[tokio::test(start_paused = true)]
    async fn a_reset_drops_every_history_and_every_debt() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION);

        let cell = rect(0, 0, 320, 64);
        drive(&sink, cell, u64::from(CHURN_MOVING)).await;
        sink.flush().await;
        drain(&mut frame_rx, usize::try_from(CHURN_MOVING).unwrap()).await;
        assert!(!sink.shared.motion.lock().unwrap().stash.is_empty(), "nothing was owed");

        sink.reset_render();
        {
            let motion = sink.shared.motion.lock().unwrap();
            assert!(motion.churn.is_empty() && motion.stash.is_empty());
            assert_eq!(motion.stash_bytes, 0);
        }

        // The next change is a first change, not the continuation of a motion.
        sink.damage(&all_of(cell), |piece| rgb(piece.w(), piece.h(), 7)).await.unwrap();
        sink.flush().await;
        assert_eq!(formats(&drain(&mut frame_rx, 1).await), vec![Tile::FORMAT_PNG]);
    }

    // ---- the video transport ------------------------------------------------

    const VIDEO: RenderPlan = RenderPlan::Video { quality: 60 };

    /// A video sink that has been told how big the desktop is, which is the one thing
    /// it needs before it will accept any pixels.
    async fn video_sink(w: u16, h: u16) -> (TileSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx, VIDEO);
        sink.msg(ServerMsg::Resize { w, h, scale: UNSCALED }).await.unwrap();
        sink.flush().await;
        // The resize itself, so a test can count what follows.
        assert!(matches!(frame_rx.recv().await, Some(ServerMsg::Resize { .. })));
        (sink, frame_rx)
    }

    /// The test for the whole design: `damage` is called once per damage
    /// *rectangle*, and a frame is what the engine says it is. Three rectangles
    /// between two frame boundaries have to be one access unit, not three — and not
    /// one per band either, which is what the tiles path would have made of them.
    #[tokio::test]
    async fn one_access_unit_per_frame_not_per_damage() {
        let (sink, mut frame_rx) = video_sink(640, 480).await;

        for y in [0, 64, 128] {
            let area = rect(0, y, 320, 64);
            sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 3)).await.unwrap();
        }
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 1).await;
        let ServerMsg::Video(unit) = &out[0] else {
            panic!("expected an access unit");
        };
        assert_eq!(unit.stream, 0, "one desktop, one stream");
        assert!(frame_rx.try_recv().is_err(), "three rectangles produced more than one frame");
    }

    /// The tile header is the *desktop*, not the picture. H.264 needs even sides and
    /// a desktop need not have them, so the two differ — and it is the desktop a
    /// client has a canvas for.
    #[tokio::test]
    async fn an_access_unit_covers_the_whole_desktop_at_its_true_size() {
        let (sink, mut frame_rx) = video_sink(1919, 1079).await;

        let area = rect(17, 33, 100, 50);
        sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 5)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 1).await;
        let ServerMsg::Video(unit) = &out[0] else {
            panic!("expected an access unit");
        };
        assert_eq!((unit.x, unit.y, unit.w, unit.h), (0, 0, 1919, 1079));
    }

    /// RDP's loop turns once per PDU and most redraw nothing, so a frame boundary
    /// with nothing behind it has to cost nothing. Without this a still screen would
    /// encode a whole-framebuffer frame per PDU.
    #[tokio::test]
    async fn a_frame_boundary_with_no_damage_sends_nothing() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;

        sink.frame().await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "an untouched framebuffer was encoded");

        let area = rect(0, 0, 320, 64);
        sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 9)).await.unwrap();
        sink.frame().await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain(&mut frame_rx, 1).await;
        assert!(frame_rx.try_recv().is_err(), "the same pixels were encoded twice");
    }

    /// The pacing, from the engine's side: a boundary is a proposal.
    ///
    /// RDP produced 126 of these a second on a busy desktop, each costing a full
    /// encode of the mirror — 88% of a session inside the encoder for a stream
    /// carrying under 800 kbit/s. Three boundaries inside one interval are one
    /// access unit, and the two that did not encode must have left their pixels in
    /// the mirror rather than dropped them.
    #[tokio::test(start_paused = true)]
    async fn several_frame_boundaries_inside_one_interval_are_one_access_unit() {
        let (sink, mut frame_rx) = video_sink(640, 480).await;

        for y in [0, 64, 128] {
            let area = rect(0, y, 320, 64);
            sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 3)).await.unwrap();
            sink.frame().await.unwrap();
        }
        sink.flush().await;

        drain(&mut frame_rx, 1).await;
        assert!(frame_rx.try_recv().is_err(), "the interval did not hold the later boundaries");
        assert!(
            sink.due_at().await.is_some(),
            "the deferred blits were dropped rather than held for the next access unit"
        );
    }

    /// The half that makes deferring safe.
    ///
    /// `Shadow::accept` counts pixels as delivered the moment `damage` blits them, so
    /// a deferral nobody comes back for is permanently wrong pixels — and motion
    /// stopping right after one is the ordinary case, not a corner. Nothing further is
    /// damaged here: the deadline alone has to collect them.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_collects_what_a_deferred_frame_held() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        let area = rect(0, 0, 320, 64);

        sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain(&mut frame_rx, 1).await;

        sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "the second frame was not held");

        // What an engine's flush arm does, and the only thing that happens here.
        let due = sink.due_at().await.expect("pixels the shadow calls delivered are owed");
        tokio::time::sleep_until(due).await;
        sink.frame().await.unwrap();
        sink.flush().await;
        drain(&mut frame_rx, 1).await;
        assert!(
            sink.due_at().await.is_none(),
            "the mirror is still holding pixels nothing will send again"
        );
    }

    /// A forced keyframe is never deferred. `reset_render` arms one for a repaint, a
    /// reattach, a takeover or a resize, and every one of those is a client sitting in
    /// front of nothing until it arrives — keeping time there would be keeping time
    /// with an empty window.
    #[tokio::test(start_paused = true)]
    async fn a_forced_keyframe_does_not_wait_for_the_interval() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        let area = rect(0, 0, 320, 64);

        sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain(&mut frame_rx, 1).await;
        let before = sink.shared.keyframes.load(Ordering::Relaxed);

        // Inside the interval, so without the bypass this would be held.
        sink.reset_render();
        sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 1).await;
        assert!(matches!(out[0], ServerMsg::Video(_)), "the repaint waited out the interval");
        assert_eq!(
            sink.shared.keyframes.load(Ordering::Relaxed),
            before + 1,
            "the client was sent a delta to start its decoder from"
        );
    }

    /// What the engines park on. `None` has to mean "nothing is owed", or a still
    /// target's loop would wake to encode nothing and a video stream with an empty
    /// mirror would spin.
    #[tokio::test(start_paused = true)]
    async fn nothing_is_due_while_the_mirror_is_clean() {
        let (tiles_tx, _tiles_rx) = mpsc::channel(8);
        let tiles = TileSink::new("test", tiles_tx, plan(TileCodec::Png, None));
        assert!(tiles.due_at().await.is_none(), "a still target has no frame to owe");

        let (sink, mut frame_rx) = video_sink(320, 240).await;
        assert!(sink.due_at().await.is_none(), "an untouched mirror owes nothing");

        let area = rect(0, 0, 320, 64);
        sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 4)).await.unwrap();
        assert!(sink.due_at().await.is_some(), "blitted pixels are owed an access unit");

        sink.frame().await.unwrap();
        sink.flush().await;
        drain(&mut frame_rx, 1).await;
        assert!(sink.due_at().await.is_none(), "the encode settled the debt");
    }

    /// A resize is the client reallocating its canvas, so the stream has to start
    /// over: a new picture size, and an access unit the new decoder can begin from.
    #[tokio::test]
    async fn a_resize_starts_the_stream_again() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        let area = rect(0, 0, 320, 64);
        sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain(&mut frame_rx, 1).await;

        sink.reset_render();
        sink.msg(ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED }).await.unwrap();
        sink.damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 2).await;
        assert!(matches!(out[0], ServerMsg::Resize { w: 640, .. }), "the resize lost its place");
        let ServerMsg::Video(unit) = &out[1] else {
            panic!("expected an access unit after the resize");
        };
        assert_eq!((unit.w, unit.h), (640, 480), "the stream kept the old picture size");
    }

    /// Where a desktop the encoder cannot handle has to surface. Learning the size
    /// must not fail — `msg`'s error means "the browser has gone" to every caller,
    /// and answering it by returning would end the session silently on the picker.
    /// The failure belongs on the next call that is on the engine's `?` path.
    #[tokio::test]
    async fn a_desktop_too_large_fails_on_the_pixel_path_not_the_message_path() {
        let (frame_tx, _frame_rx) = mpsc::channel(64);
        let sink = TileSink::new("test", frame_tx, VIDEO);

        sink.msg(ServerMsg::Resize { w: 5120, h: 2880, scale: UNSCALED })
            .await
            .expect("learning a size must not be the thing that fails");

        let area = rect(0, 0, 320, 64);
        let refused = sink
            .damage(&all_of(area), |piece| rgb(piece.w(), piece.h(), 1))
            .await
            .expect_err("a 5K desktop was accepted");
        assert!(format!("{refused:#}").contains("3840"), "{refused:#}");
    }

    // ---- a stream per moving region -----------------------------------------

    const MOTION_STREAM: RenderPlan = RenderPlan::Tiles {
        base: TileCodec::Png,
        motion: Some(MotionEncode::Stream { quality: 60 }),
        debug: false,
    };

    /// A sink whose moving encode is a stream, told how big the desktop is.
    async fn stream_sink(w: u16, h: u16) -> (TileSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = TileSink::new("test", frame_tx, MOTION_STREAM);
        sink.msg(ServerMsg::Resize { w, h, scale: UNSCALED }).await.unwrap();
        sink.flush().await;
        assert!(matches!(frame_rx.recv().await, Some(ServerMsg::Resize { .. })));
        (sink, frame_rx)
    }

    /// A flat colour, so which send a pixel came from is readable a byte at a time.
    fn flat(w: u16, h: u16, v: u8) -> Vec<u8> {
        vec![v; usize::from(w) * usize::from(h) * 3]
    }

    /// Damage `area` slot by slot, with the frame boundaries an engine would produce,
    /// until a stream is carrying it. Requires a paused clock.
    ///
    /// It takes a while by construction: `CHURN_MOVING` slots before the cells count
    /// as moving, and `RETUNE` before geometry may change again.
    async fn until_streamed(sink: &TileSink, area: Rect, colour: u8) {
        for _ in 0..40 {
            sink.damage(&all_of(area), |piece| flat(piece.w(), piece.h(), colour))
                .await
                .unwrap();
            sink.frame().await.unwrap();
            tokio::time::advance(CHURN_SLOT).await;
            if sink.shared.video.lock().await.regions.covers(area.cell_key()) {
                return;
            }
        }
        panic!("a region that changed every slot never got a stream");
    }

    /// The claim the whole strategy makes: what moves goes out as a stream, and the
    /// cells beside it are not touched. A cell a stream carries must not *also* be
    /// sent as a tile — that is a second delivery of pixels the stream has not paid
    /// for, and it is what would discharge a debt nothing had settled.
    #[tokio::test(start_paused = true)]
    async fn a_cell_a_stream_carries_is_not_also_sent_as_a_tile() {
        let (sink, mut frame_rx) = stream_sink(640, 128).await;
        let moving = rect(0, 0, 320, 64);
        until_streamed(&sink, moving, 40).await;
        sink.flush().await;
        while frame_rx.try_recv().is_ok() {}

        sink.damage(&all_of(moving), |piece| flat(piece.w(), piece.h(), 90)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 1).await;
        let ServerMsg::Video(unit) = &out[0] else {
            panic!("expected an access unit, got {:?}", out[0]);
        };
        assert_eq!((unit.x, unit.y, unit.w, unit.h), (0, 0, 320, 64));
        assert!(
            frame_rx.try_recv().is_err(),
            "a streamed cell was sent as a tile as well as in its stream"
        );
    }

    /// And the other half: a quiet cell beside a streamed one is still crisp, and a
    /// band with nothing streamed in it still goes out whole. This is what the
    /// strategy is *for* — the text beside a video costs nothing and stays exact.
    #[tokio::test(start_paused = true)]
    async fn a_quiet_cell_beside_a_streamed_one_is_still_a_crisp_tile() {
        let (sink, mut frame_rx) = stream_sink(640, 128).await;
        let moving = rect(0, 0, 320, 64);
        until_streamed(&sink, moving, 40).await;
        sink.flush().await;
        while frame_rx.try_recv().is_ok() {}

        // A band across both cells, one streamed and one not, and a band below with
        // neither.
        sink.damage(&all_of(rect(0, 0, 640, 64)), |piece| flat(piece.w(), piece.h(), 90))
            .await
            .unwrap();
        sink.damage(&all_of(rect(0, 64, 640, 64)), |piece| flat(piece.w(), piece.h(), 91))
            .await
            .unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 3).await;
        let tiles: Vec<(u16, u16, u16, u16)> = out
            .iter()
            .filter_map(|msg| match msg {
                ServerMsg::Tile(tile) => Some((tile.x, tile.y, tile.w, tile.h)),
                _ => None,
            })
            .collect();
        assert_eq!(
            tiles,
            vec![(320, 0, 320, 64), (0, 64, 640, 64)],
            "the split band should send only its quiet cell, and the quiet band whole"
        );
        assert!(
            out.iter().any(|msg| matches!(msg, ServerMsg::Video(_))),
            "the streamed cell was not carried at all"
        );
    }

    /// The answer to what happens when a region stops moving while its stream is
    /// still the truth on screen — and the reason the debt holds no pixels.
    ///
    /// The stream ends, the cells come due, and the cleanup is cropped from the
    /// mirror, which holds the *newest* source. The still path has to remember the
    /// pixels it approximated and can therefore restore a frame that has been
    /// overtaken; here that is not expressible.
    #[tokio::test(start_paused = true)]
    async fn a_settled_region_is_restored_from_the_newest_pixels_in_the_mirror() {
        let (sink, mut frame_rx) = stream_sink(640, 128).await;
        let moving = rect(0, 0, 320, 64);
        until_streamed(&sink, moving, 40).await;

        // The last thing the stream carried, and the only thing a cleanup may restore.
        sink.damage(&all_of(moving), |piece| flat(piece.w(), piece.h(), 200)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        while frame_rx.try_recv().is_ok() {}

        // Nothing more happens: no damage, so no frame boundary either. The cleanup
        // timer is the only thing running, which is the case it exists for.
        for _ in 0..12 {
            tokio::time::advance(CLEANUP_TICK).await;
            tokio::task::yield_now().await;
        }
        sink.flush().await;

        let out = drain(&mut frame_rx, 1).await;
        let ServerMsg::Tile(tile) = &out[0] else {
            panic!("the settled region was never restored: {:?}", out[0]);
        };
        assert_eq!((tile.x, tile.y, tile.w, tile.h), (0, 0, 320, 64));
        assert_eq!(tile.format, Tile::FORMAT_PNG, "a cleanup is the base encode");
        let newest = Tile::from_rgb(0, 0, 320, 64, &flat(320, 64, 200)).unwrap();
        assert_eq!(tile.data, newest.data, "the cleanup restored a frame that was overtaken");
    }

    // ---- congestion ---------------------------------------------------------

    /// A link with room to spare never moves the quantizer off the dial. The dial is
    /// a ceiling, so there is nothing above it to reclaim.
    #[test]
    fn a_clear_link_stays_on_the_dial() {
        let mut congestion = Congestion::new(30);
        let start = tokio::time::Instant::now();
        for i in 0..CLEAR_FRAMES * 4 {
            let at = start + Duration::from_millis(u64::from(i) * 33);
            assert_eq!(congestion.observe(Duration::ZERO, at), None);
        }
        assert_eq!(congestion.qp, 30);
    }

    #[test]
    fn a_backlog_gives_up_quality_and_a_clear_link_takes_it_back() {
        let mut congestion = Congestion::new(30);
        let start = tokio::time::Instant::now();

        // One slow frame is not a verdict.
        assert_eq!(congestion.observe(BEHIND_BLOCK, start), None);
        assert_eq!(congestion.observe(BEHIND_BLOCK, start), Some(34));

        // The cooldown holds the next ones off however bad the link is — but it does
        // not stop them being *counted*, so a link that never recovered acts the
        // moment it expires rather than starting its case over.
        assert_eq!(congestion.observe(BEHIND_BLOCK, start), None);
        assert_eq!(congestion.observe(BEHIND_BLOCK, start), None);
        let later = start + ADJUST_COOLDOWN;
        assert_eq!(congestion.observe(BEHIND_BLOCK, later), Some(38));

        // Now a link that has recovered: one step back per spell of clear frames,
        // and it stops at the dial rather than going past it.
        let mut at = later;
        for _ in 0..40 {
            at += ADJUST_COOLDOWN;
            for _ in 0..CLEAR_FRAMES {
                congestion.observe(Duration::ZERO, at);
            }
        }
        assert_eq!(congestion.qp, 30, "the link recovered past the quality that was asked for");
    }

    #[test]
    fn quality_bottoms_out_rather_than_wrapping() {
        let mut congestion = Congestion::new(30);
        let start = tokio::time::Instant::now();
        let mut at = start;
        for _ in 0..40 {
            at += ADJUST_COOLDOWN;
            for _ in 0..BEHIND_FRAMES {
                congestion.observe(BEHIND_BLOCK, at);
            }
        }
        assert_eq!(congestion.qp, h264::QP_COARSEST);
    }
}
