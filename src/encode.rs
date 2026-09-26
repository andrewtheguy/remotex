//! Ordered video encoding outside the RDP and VNC protocol-read loops.
//!
//! ```text
//! read loop:  pack → Shadow::accept → VideoSink::damage() → mirror
//!             VideoSink::frame() ── take the round ──┐
//!                                                    │ spawn_blocking(encode)
//!             VideoSink::msg()  ── ServerMsg ────────┤ handle pushed, in order
//!                                                    ▼ mpsc, cap ENCODE_DEPTH
//!                                order task: await handles FIFO → frame_tx
//!                                            ⤷ settle tick → re-encode at the dial
//! ```
//!
//! FIFO collection keeps control messages in source order with the access units
//! around them, so a resize cannot overtake the frame before it. A full queue
//! backpressures the engine because its shadow has already recorded submitted
//! pixels.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::config::RenderPlan;
use crate::feedback::LinkFeedback;
use crate::protocol::{Held, ServerMsg, Tile, VideoUnit};
use crate::stream::{DesktopStream, Produced, Round};
use crate::shadow::Rect;
use crate::video;

/// Maximum queued items ahead of the one currently collected: rounds, which are
/// serial with each other, and the control messages between them.
const ENCODE_DEPTH: usize = 16;

/// Encoded bytes allowed between an engine and the browser's socket.
///
/// Every queue on that path is bounded by *messages* — [`ENCODE_DEPTH`] here,
/// [`crate::session::FRAME_BUFFER`] twice in series behind it — and a message
/// is whatever an access unit happened to compress to. On a link that slows down
/// those counts are no bound on what matters, which is how old the newest pixel is
/// by the time it is drawn: measured against a throttled link with a busy desktop,
/// the queues held 30 MB, and at 4 Mbit/s that is a picture 63 s behind its
/// desktop. Input still reaches the remote; what it did arrives a minute later,
/// which reads as a session that has stopped responding until a fresh engine
/// throws the queues away.
///
/// So a round's size comes out of this budget before it is encoded, and the share
/// rides inside the unit ([`Held`]) until the unit is dropped anywhere on the way or
/// its batch leaves: written to the socket, for a client that is keeping up, and
/// *received* by it, for one that is behind — where a written batch is only backlog
/// that has moved into the kernel's send buffer (`ws.rs` decides which, and says why
/// both). With the budget spent the *engine* waits, in [`VideoSink::frame`], and
/// stops reading its remote — the backpressure the counts were meant to be,
/// arriving while the backlog is still short. The order task never waits on it:
/// everything queued behind the order task holds a share, so a wait there could be
/// for room only it can free.
///
/// A size is not known until the encode is done, so the engine takes an estimate —
/// the size of the last round — and the order task settles it ([`Held::settle`]).
/// An estimate that was short is over-committed rather than waited for, which
/// bounds the error at [`ENCODE_DEPTH`] rounds and corrects itself within as many.
///
/// Two full batches ([`crate::wire`] caps one at 256 KiB): one being written and
/// one ready behind it, so a fast link never waits on the encoder for want of
/// room, and a slow one is never more than this far behind. Against the same
/// throttled link and an incompressible 12 Mbit/s of damage, the picture ran 4 s
/// behind at 1 Mbit/s where the message counts alone left it 23 s, and 0.6 s
/// behind at 4 Mbit/s; an unthrottled link 100 ms away carried what it did before.
const QUEUE_BUDGET: u32 = 512 * 1024;

/// How long the stream must have gone out below the dial and then sat quiet before
/// it is settled back at the dial. Long enough that a brief pause in motion is not
/// chased with a redundant re-encode, short enough that a settled screen sharpens
/// while the eye is still on it.
const SETTLE_IDLE: Duration = Duration::from_millis(500);

/// How often the order task wakes to look for a quiet stream to settle. It has to
/// be its own timer rather than something the next frame does, because a screen
/// that stops changing produces no next frame — which is exactly the case a settle
/// is for.
const SETTLE_TICK: Duration = Duration::from_millis(250);

/// How long queueing an access unit may block before the frame counts as one the link
/// could not keep up with.
///
/// More than half a frame at 30 Hz. The queues between here and the socket are
/// deliberately shallow ([`crate::session::FRAME_BUFFER`]), so
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

/// How much of the dial one step down gives up, and how much one step back up
/// reclaims. Bigger down than up, for the same reason [`CLEAR_FRAMES`] is bigger than
/// [`BEHIND_FRAMES`].
///
/// The loop speaks quality rather than a quantizer because a quantizer is the codec's
/// own scale — VP9's is 0–63 — while the dial is the gateway's; the mapping is
/// [`crate::vp9`]'s. Ten points down and three back is roughly four quantizer steps
/// against one, on the dial's scale.
const QUALITY_STEP_DOWN: u8 = 10;
/// See [`QUALITY_STEP_DOWN`].
const QUALITY_STEP_UP: u8 = 3;

/// Queueing lag past which an *adaptive* plan counts a frame as one the link
/// could not keep up with, beside [`BEHIND_BLOCK`] — the paint window's own
/// signal, measured by [`crate::feedback::LinkFeedback`] as how long the oldest
/// unacknowledged batch has been owed beyond the link's floor.
///
/// Well under [`crate::ws`]'s 150 ms lag gate on purpose: by the time that gate
/// parks the window the backpressure chain will reach [`BEHIND_BLOCK`] on its
/// own, so a threshold up there would never fire first. This one moves quality
/// while the window is still open — before the stall, which is the point of
/// asking for `render_adaptive` at all.
const LAG_BEHIND: Duration = Duration::from_millis(60);

/// Queueing lag below which an adaptive plan counts a frame as clear. The gap
/// between this and [`LAG_BEHIND`] is hysteresis: a link hovering between the
/// two earns neither a coarser picture nor its quality back.
const LAG_CLEAR: Duration = Duration::from_millis(30);

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
/// 33_333 µs is the interval `VIDEO_FRAME_US` in `frontend/src/videoDecoder.ts`
/// already stamps access units with, so the timestamps stop being a fiction.
const VIDEO_FRAME_INTERVAL: Duration = Duration::from_micros(33_333);

/// What the link will bear, on the 1–100 dial.
///
/// One-directional by construction, and that is the whole design rather than a
/// simplification: `dial` is a *ceiling*, so this can only ever make the picture
/// coarser than the operator asked for and never finer. What TCP hides is headroom —
/// you can tell you are behind, never how far ahead you could be — and this never needs
/// to know, because exceeding the configured quality was never a goal. The worst it can
/// do is coarsen a link that was already struggling.
///
/// In dial units rather than a quantizer: a quantizer is the codec module's own scale
/// (VP9's runs 0–63), and the mapping lives there, so nothing here knows one.
///
/// Pure, and takes `now` rather than reading a clock, so every one of its decisions is
/// testable without waiting for one.
struct Congestion {
    /// The configured quality: the finest this will ever ask for.
    dial: u8,
    /// The coarsest the walk may go. [`video::QUALITY_MIN`] historically, and the
    /// plan's floor when the target asked for `render_adaptive` — an operator who
    /// named a floor has said how much picture they are willing to trade.
    floor: u8,
    /// Whether the client's lag is a signal this walk listens to. Only an
    /// adaptive plan's; without it the walk keeps its historical shape, pressure
    /// only, and the lag handed to [`Self::observe`] is ignored.
    lag_aware: bool,
    /// The quality in force.
    quality: u8,
    /// Consecutive frames whose queueing blocked, and consecutive frames whose did
    /// not. Only one is ever non-zero.
    behind: u32,
    clear: u32,
    /// When the quality last moved, for [`ADJUST_COOLDOWN`]. `None` before it ever
    /// has, so the first verdict does not have to wait out a cooldown that never ran.
    changed_at: Option<tokio::time::Instant>,
}

impl Congestion {
    fn new(dial: u8, adaptive: Option<u8>) -> Self {
        // The floor cannot sit above the ceiling. A configured plan arrives with
        // that already settled — `TargetConfig::render_plan` clamps the floor to the
        // dial, so the card states the floor the walk really holds to — and this
        // keeps the invariant for a plan built by hand: the walk a dial admits is
        // the widest one under it.
        let floor = adaptive.map_or(video::QUALITY_MIN, |floor| floor.min(dial));
        Self {
            dial,
            floor,
            lag_aware: adaptive.is_some(),
            quality: dial,
            behind: 0,
            clear: 0,
            changed_at: None,
        }
    }

    /// Take the dial back outright, for a stream that has gone quiet: an idle link is
    /// the clearest evidence of room it will ever get, and one frame at the dial is
    /// what sharpens everything a coarser quality left behind. The cooldown restarts
    /// from here, like any other move.
    fn settle(&mut self, now: tokio::time::Instant) {
        self.quality = self.dial;
        self.behind = 0;
        self.clear = 0;
        self.changed_at = Some(now);
    }

    /// Record how long queueing a frame blocked and how far behind the client's
    /// paint window is, and return a new quality if that changes the verdict.
    fn observe(&mut self, blocked: Duration, lag: Duration, now: tokio::time::Instant) -> Option<u8> {
        let lag = if self.lag_aware { lag } else { Duration::ZERO };
        if blocked >= BEHIND_BLOCK || lag >= LAG_BEHIND {
            self.behind += 1;
            self.clear = 0;
        } else if lag <= LAG_CLEAR {
            self.clear += 1;
            self.behind = 0;
        } else {
            // Between the two lag thresholds: not a reason to coarsen, not
            // evidence of room either. Both counters start over.
            self.behind = 0;
            self.clear = 0;
        }
        if self.changed_at.is_some_and(|at| now.saturating_duration_since(at) < ADJUST_COOLDOWN) {
            return None;
        }
        let wanted = if self.behind >= BEHIND_FRAMES {
            self.quality.saturating_sub(QUALITY_STEP_DOWN).max(self.floor)
        } else if self.clear >= CLEAR_FRAMES {
            // Stops at the dial, never above it. A link with room to spare does not
            // earn a better picture than the one that was configured.
            self.quality.saturating_add(QUALITY_STEP_UP).min(self.dial)
        } else {
            return None;
        };
        if wanted == self.quality {
            return None;
        }
        self.quality = wanted;
        self.behind = 0;
        self.clear = 0;
        self.changed_at = Some(now);
        Some(wanted)
    }
}

/// The stream, and what the link will bear.
struct Video {
    stream: DesktopStream,
    congestion: Congestion,
    /// The earliest the next round may be encoded — see [`VIDEO_FRAME_INTERVAL`].
    /// `None` before the first one, so a freshly connected desktop paints without
    /// waiting out an interval.
    due_at: Option<tokio::time::Instant>,
    /// When the last round went out coarser than the dial, if it did — the picture
    /// the client is holding is then below the configured quality, and a screen that
    /// stops changing would keep it that way. `None` once a round at the dial has
    /// gone out, which sharpens every block, moved or not. What [`settle_stream`]
    /// comes back for.
    coarse_at: Option<tokio::time::Instant>,
}

/// Whether a source can carry its picture as tiles, for a desktop too large for a
/// video stream ([`video::within_ceiling`]).
///
/// A desktop within the ceiling is always one VP9 stream. Past it, a source whose
/// own updates are rectangles has each one sent as it came, one PNG [`Tile`] each,
/// and any other ends the session with [`video::check_picture`]'s refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileSupport {
    /// The picture is video or nothing.
    None,
    /// The source's updates are rectangles, which it hands to [`VideoSink::damage`]
    /// whole while [`VideoSink::tiling`] says so.
    Rects,
    /// The picture is the remote's own stream passed through ([`VideoSink::pass_hevc`]),
    /// and the source's rectangles fill its gaps — before it flows and across a display
    /// change — as tiles, whatever the desktop's size: nothing here encodes video.
    Gaps,
}

/// One item in the ordered queue.
enum Pending {
    /// One round in flight: an encode on a blocking worker, yielding the round itself
    /// (to be put back), what it produced, and the microseconds it cost.
    ///
    /// The frames are links in a chain, so rounds stay serial with each other —
    /// [`DesktopStream::round_out`] refuses a second while one is here — but the
    /// engine's read loop does not wait the encode out: the spare mirror takes its
    /// blits meanwhile, and the order task puts the round back when it lands.
    ///
    /// With the share of [`QUEUE_BUDGET`] taken for it, at the last round's size.
    Round(JoinHandle<(Round, anyhow::Result<Produced>, u64)>, Held),
    /// One remote update's rectangles being PNG-encoded on a blocking worker, with
    /// the share of [`QUEUE_BUDGET`] taken for them, at the last update's size.
    Tiles(JoinHandle<(anyhow::Result<Vec<Tile>>, u64)>, Held),
    Msg(ServerMsg),
    /// A caller waiting for everything pushed before it to have reached `frame_tx`.
    Flush(oneshot::Sender<()>),
}

/// State the sink and its order task both touch.
///
/// The counters live here rather than in the order task because the engine is what
/// reports them: an engine's `run` returning drops the thread's whole runtime, so a
/// line the task logged on its own way out would be cancelled before it printed.
struct Shared {
    tile_support: TileSupport,
    /// Whether the desktop is past the ceiling and carried as tiles — see
    /// [`TileSupport`]. Decided by each `Resize` ([`VideoSink::msg`]).
    tiling: AtomicBool,
    /// While [`Self::tiling`], the rectangles [`VideoSink::damage`] has taken since
    /// the last [`VideoSink::frame`], in the order they came.
    rects: Mutex<Vec<(Rect, Vec<u8>)>>,
    /// Why the order task gave up, so the engine's next push can report it rather
    /// than a bare closed channel. The error itself, so the cause chain survives
    /// the hop from the task to the push. See [`VideoSink::closed`].
    failure: Mutex<Option<anyhow::Error>>,
    /// A `tokio` mutex rather than a `std` one because several of its critical
    /// sections span awaits. It is *not* held across the encode: a round owns its
    /// mirror and encoder outright for the duration ([`DesktopStream::take_round`]),
    /// the spare mirror takes the blits meanwhile, and "one round at a time, in
    /// order" is `DesktopStream::round_out`'s guarantee.
    video: tokio::sync::Mutex<Video>,
    /// Signalled by the order task when a pipelined round has come back with pixels
    /// (or a keyframe) still waiting, so an engine parked on a clean `due_at` finds
    /// out the mirror is dirty again. See [`VideoSink::round_returned`].
    round_returned: Notify,
    /// Whether the picture is the remote's own stream, passed through
    /// ([`VideoSink::pass`]) rather than encoded here. Set by the first frame passed,
    /// cleared when the desktop goes to tiles.
    passing: AtomicBool,
    /// The browser must start the passed stream over: drop what is not a keyframe,
    /// and announce the configuration again ahead of the one that is. Set from the
    /// start and by [`VideoSink::reset_render`], so a reattach, a takeover and a
    /// resize each begin where a decoder can.
    pass_restart: AtomicBool,
    /// The configuration string last announced for the passed stream.
    pass_announced: Mutex<Option<String>>,
    /// Set by [`VideoSink::reset_render`], consumed by [`VideoSink::frame`]. An atomic
    /// rather than a field on [`Video`] so that resetting stays synchronous: its call
    /// sites are already awaiting other things, and none of them should have to wait
    /// out an encode to say "the client needs to start again".
    keyframe_owed: AtomicBool,
    /// The link as the attached browser's paint window measures it — see
    /// [`crate::feedback`]. [`VideoSink::adjust`] hands its lag to an adaptive
    /// congestion walk beside the push-blocked signal, and the settle tick asks it
    /// whether the client has room for what it is about to send.
    feedback: Arc<LinkFeedback>,
    units: AtomicU64,
    encoded_bytes: AtomicU64,
    /// Tiles sent while [`Self::tiling`], and their PNG bytes.
    tiles: AtomicU64,
    tile_bytes: AtomicU64,
    /// Of [`Self::units`], those a decoder could start from, and what they cost. Read
    /// together: see [`Totals`].
    keyframes: AtomicU64,
    keyframe_bytes: AtomicU64,
    /// Frames the encoder produced no bitstream for. Must stay zero.
    skipped: AtomicU64,
    /// Frames sent coarser than the dial asked for, and the lowest quality the link
    /// ever forced. The whole measurement of [`Congestion`].
    coarsened: AtomicU64,
    /// Starts at [`video::QUALITY_MAX`] and only ever falls, because "the worst" is a
    /// minimum on this scale.
    worst_quality: AtomicU64,
    encode_micros: AtomicU64,
    /// Wall time the order task spent waiting for encodes to finish.
    waited_micros: AtomicU64,
    /// Time the engine spent blocked pushing into a full queue.
    stalled_micros: AtomicU64,
    /// The bound on encoded bytes queued towards the browser — see [`QUEUE_BUDGET`].
    budget: Arc<Semaphore>,
    /// What the last round came to, which the next is taken at.
    round_bytes: AtomicU64,
    /// How long the engine has waited on the budget since the last round was
    /// queued. Read and cleared by [`VideoSink::frame`]: a link that is behind now
    /// holds the engine here rather than at a full queue, and the congestion walk
    /// reads blocking wherever it happens.
    held_micros: AtomicU64,
}

impl Shared {
    fn new(plan: RenderPlan, feedback: Arc<LinkFeedback>, tile_support: TileSupport) -> Self {
        let RenderPlan { quality, adaptive, chroma, .. } = plan;
        Self {
            tile_support,
            tiling: AtomicBool::new(tile_support == TileSupport::Gaps),
            rects: Mutex::default(),
            failure: Mutex::default(),
            video: tokio::sync::Mutex::new(Video {
                stream: DesktopStream::new(quality, chroma),
                congestion: Congestion::new(quality, adaptive),
                due_at: None,
                coarse_at: None,
            }),
            round_returned: Notify::new(),
            passing: AtomicBool::new(false),
            pass_restart: AtomicBool::new(true),
            pass_announced: Mutex::default(),
            keyframe_owed: AtomicBool::new(false),
            feedback,
            units: AtomicU64::new(0),
            encoded_bytes: AtomicU64::new(0),
            tiles: AtomicU64::new(0),
            tile_bytes: AtomicU64::new(0),
            keyframes: AtomicU64::new(0),
            keyframe_bytes: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            coarsened: AtomicU64::new(0),
            worst_quality: AtomicU64::new(u64::from(video::QUALITY_MAX)),
            encode_micros: AtomicU64::new(0),
            waited_micros: AtomicU64::new(0),
            stalled_micros: AtomicU64::new(0),
            budget: Arc::new(Semaphore::new(QUEUE_BUDGET as usize)),
            round_bytes: AtomicU64::new(0),
            held_micros: AtomicU64::new(0),
        }
    }
}

/// The engine's handle on the encoder.
///
/// `Clone` because the VNC engine drives its read loop as a separate task while
/// its input side keeps sending control messages; both push into the same queue,
/// and only the read loop pushes pixels.
#[derive(Clone)]
pub struct VideoSink {
    engine: &'static str,
    tx: mpsc::Sender<Pending>,
    shared: Arc<Shared>,
}

impl VideoSink {
    /// Start an encoder for one engine. `engine` prefixes its log lines. `plan` is
    /// the resolved render dial ([`crate::config::TargetConfig::render_plan`]).
    /// `feedback` is the session's link measurement ([`crate::feedback`]), read by
    /// an adaptive plan. `tiles` says whether the source can carry a desktop past the
    /// video ceiling as tiles.
    pub fn new(
        engine: &'static str,
        frame_tx: mpsc::Sender<ServerMsg>,
        plan: RenderPlan,
        feedback: Arc<LinkFeedback>,
        tiles: TileSupport,
    ) -> Self {
        let (tx, rx) = mpsc::channel(ENCODE_DEPTH);
        let shared = Arc::new(Shared::new(plan, feedback, tiles));
        tokio::spawn(order_loop(engine, rx, frame_tx, Arc::clone(&shared)));
        Self { engine, tx, shared }
    }

    /// Copy one changed rectangle of packed RGB888 into the mirror.
    ///
    /// Nothing is sent until the engine says a frame has ended ([`Self::frame`]):
    /// the unit of the encoder is the whole framebuffer, and a rectangle is only a
    /// part of the next one.
    ///
    /// While [`Self::tiling`] the rectangle is kept whole instead, to go out as one
    /// tile of its own at the next [`Self::frame`].
    pub async fn damage(&self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        if self.tiling() {
            self.shared.rects.lock().unwrap().push((rect, rgb.to_vec()));
            return Ok(());
        }
        self.shared.video.lock().await.stream.blit(rect, rgb)
    }

    /// Whether the desktop is past the video ceiling and carried as tiles: then
    /// [`Self::damage`] wants each rectangle whole, as the remote sent it, and a
    /// source must not trim it. Changes only with a `Resize` through [`Self::msg`].
    pub fn tiling(&self) -> bool {
        self.shared.tiling.load(Ordering::Relaxed)
    }

    /// Queue the rectangles taken since the last call as tiles, PNG-encoded on a
    /// blocking worker, in their place among the messages around them. Every one goes
    /// out and none waits for an interval: nothing re-sends a rectangle.
    async fn queue_tiles(&self) -> anyhow::Result<()> {
        let rects = std::mem::take(&mut *self.shared.rects.lock().unwrap());
        if rects.is_empty() {
            return Ok(());
        }
        let handle = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let tiles = rects
                .iter()
                .map(|(rect, rgb)| Tile::from_rgb(rect.left, rect.top, rect.w(), rect.h(), rgb))
                .collect();
            (tiles, micros(started))
        });
        let estimate = usize::try_from(self.shared.round_bytes.load(Ordering::Relaxed)).unwrap_or(usize::MAX);
        let held = self.hold(estimate).await;
        self.push(Pending::Tiles(handle, held)).await
    }

    /// One remote frame has ended: encode everything [`Self::damage`] has blitted
    /// since the last call, and queue the access unit.
    ///
    /// It has to be driven by the engines because neither protocol hands its damage
    /// over a frame at a time — `damage` is called once per *rectangle*, and RDP's
    /// loop turns once per PDU, most of which redraw nothing. So this is a no-op when
    /// nothing was blitted: without that, a still screen would encode a frame per PDU.
    ///
    /// The encode runs on a blocking worker with only the *round* — mirror and
    /// encoder, taken outright — and its place in the order queued: the read loop
    /// keeps decoding into the spare mirror while the worker encodes, and the order
    /// task puts the round back when it lands. Rounds stay serial with each other
    /// ([`DesktopStream::round_out`]), which is what an inter-frame stream requires;
    /// what does not happen is the engine waiting the encode out.
    ///
    /// **Calling it is a proposal, not an instruction.** At most one round is produced
    /// per `VIDEO_FRAME_INTERVAL`; a call inside that window leaves the mirror dirty
    /// and returns. So an engine may call this as often as its boundaries occur, but
    /// must also call it when [`Self::due_at`] says to — see there for why that second
    /// half is not optional.
    pub async fn frame(&self) -> anyhow::Result<()> {
        if self.tiling() {
            return self.queue_tiles().await;
        }
        let mut video = self.shared.video.lock().await;
        if video.stream.round_out() {
            // The previous round is still encoding, and it *is* this call's answer:
            // its return re-arms the engine (`Self::round_returned`), and taking
            // anything now — even the keyframe flag — would act on an encoder that
            // is away on the worker.
            return Ok(());
        }
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
            // It stays armed until the stream actually produces one — so an encode
            // that yields no bitstream cannot lose the ask, and a client is never
            // left waiting for a keyframe nothing will send again.
            video.stream.force_keyframe();
        }
        let Some(mut round) = video.stream.take_round()? else {
            return Ok(());
        };
        video.due_at = Some(now + VIDEO_FRAME_INTERVAL);
        // What the round's encoder really runs at, not the table: a stream that
        // refused a retune is still coarse, and the settle it owes must not be
        // cleared by a table that has already reached the dial.
        let quality = round.quality();
        video.coarse_at = (quality < video.congestion.dial).then_some(now);
        // Dropped before the spawn and the push: the whole point is that `damage`
        // gets the lock back while the worker encodes.
        drop(video);

        let handle = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let produced = round.encode();
            (round, produced, micros(started))
        });
        let queued = Instant::now();
        let round_bytes = usize::try_from(self.shared.round_bytes.load(Ordering::Relaxed)).unwrap_or(usize::MAX);
        let held = self.hold(round_bytes).await;
        let pushed = self.push(Pending::Round(handle, held)).await;
        // How long that took is the congestion signal, and it is read whether or not
        // the push succeeded: a push that failed waited just as long, and the verdict
        // is about the link rather than about this round. Waiting on the budget and
        // the queue backing up far enough to block both mean the socket is not
        // draining what this sends.
        let held_micros = self.shared.held_micros.swap(0, Ordering::Relaxed);
        self.adjust(queued.elapsed().max(Duration::from_micros(held_micros)), quality).await;
        pushed
    }

    /// Completes when a pipelined round has come back with pixels or a keyframe
    /// still waiting — the engines' third wake-up source, beside their own frame
    /// boundaries and [`Self::due_at`].
    ///
    /// Needed because `due_at` reads the live stream, and while a round is out the
    /// encoder is away: an engine that went idle then would park on a clean mirror
    /// and never hear that the returning round re-dirtied it. A permit is stored if
    /// nobody is waiting, so the signal cannot be lost to timing; a spurious
    /// wake-up costs one no-op [`Self::frame`].
    pub async fn round_returned(&self) {
        self.shared.round_returned.notified().await;
    }

    /// When the engine must call [`Self::frame`] again, whatever its own boundaries
    /// are doing — or `None` when there is nothing waiting.
    ///
    /// **This is the correctness half of pacing, not a convenience.**
    /// [`crate::shadow::Shadow::accept`] records source pixels as delivered to the
    /// client the moment `damage` blits them into the mirror, and nothing re-sends
    /// them. A deferred frame that is never followed by another encode is therefore
    /// permanently wrong pixels — and "the motion stopped right after one" is the
    /// ordinary case rather than a corner: a video paused, a pointer come to rest, a
    /// window settled. The deadline is what comes back for them.
    ///
    /// `None` while the mirror is clean, so an idle stream parks on
    /// [`std::future::pending`] instead of waking an engine to encode nothing.
    pub async fn due_at(&self) -> Option<tokio::time::Instant> {
        let video = self.shared.video.lock().await;
        if !video.stream.dirty() {
            return None;
        }
        // A dirty mirror with no deadline yet is one whose pixels arrived before any
        // access unit went out; it is owed one now rather than in an interval.
        Some(video.due_at.unwrap_or_else(tokio::time::Instant::now))
    }

    /// Let the congestion policy see how long that frame's push blocked, and move the
    /// dial if it has changed its mind.
    ///
    /// A failure to re-tune is logged and dropped rather than ending the session: the
    /// stream is still perfectly good at the quality it already had, and losing the
    /// ability to *degrade* is not a reason to stop.
    async fn adjust(&self, blocked: Duration, quality: u8) {
        self.shared.worst_quality.fetch_min(u64::from(quality), Ordering::Relaxed);
        let mut video = self.shared.video.lock().await;
        if quality < video.congestion.dial {
            self.shared.coarsened.fetch_add(1, Ordering::Relaxed);
        }
        let dial = video.congestion.dial;
        let now = tokio::time::Instant::now();
        // The client's own half of the verdict. Free to read whether or not the
        // walk is lag-aware; `observe` is what knows.
        let lag = self.shared.feedback.lag(now);
        let before = video.congestion.quality;
        let Some(wanted) = video.congestion.observe(blocked, lag, now) else {
            return;
        };
        if let Err(e) = video.stream.set_quality(wanted) {
            // The stream kept the quality it had, so the walk does too: its next
            // verdict starts from what is actually in force.
            video.congestion.quality = before;
            warn!("{}: could not move the video quality to {wanted}: {e:#}", self.engine);
        } else {
            debug!("{}: video quality now {wanted} (the dial asks for {dial})", self.engine);
        }
    }

    /// Start the stream over for a client that has to be able to decode from here.
    ///
    /// For a resize, where the picture size has changed, and for a repaint, where
    /// leaving the stream mid-chain would ask a client to decode from a frame it
    /// never saw. Its call sites are exactly the moments a client's decoder has to be
    /// able to start over, which is why the keyframe is armed here.
    pub fn reset_render(&self) {
        self.shared.keyframe_owed.store(true, Ordering::Relaxed);
        self.shared.pass_restart.store(true, Ordering::Relaxed);
    }

    /// Queue a `w`×`h` frame the remote encoded itself as the next access unit,
    /// untouched: wlshare's VP9 encoding, which is the 4:4:4 stream this gateway would
    /// have encoded from the same pixels ([`crate::stream::pass_444`]).
    ///
    /// None of the stream's own machinery applies. There is no mirror, round,
    /// interval, quality walk or settle: the remote paces, codes and sharpens its
    /// stream itself, and learns how the browser is keeping up from the fences the
    /// engine echoes once [`Self::drained`] says so. What is shared is the queue: the
    /// frame takes its size out of [`QUEUE_BUDGET`] like an encoded unit, and goes out
    /// in order with the messages around it.
    ///
    /// A browser that must start over ([`Self::reset_render`]) is sent nothing until a
    /// keyframe, which the full update the engine asks for at the same moment brings,
    /// and the keyframe goes out behind a fresh announcement.
    pub async fn pass(&self, w: u16, h: u16, frame: Vec<u8>) -> anyhow::Result<()> {
        let passed = crate::stream::pass_444(w, h, &frame)?;
        self.forward(w, h, frame, passed).await.map(drop)
    }

    /// Queue one access unit of a High Performance Mac's HEVC, `w`×`h`, as the next
    /// unit, untouched: an Annex B picture that `passed` describes, read from the
    /// stream's own parameter sets ([`crate::vnc_apple_media`]).
    ///
    /// As [`Self::pass`] in everything the queue does — the budget, the order, the
    /// restart at a keyframe behind a fresh announcement — and nothing else: the Mac
    /// paces and codes its stream, and the browser's queueing reaches it as no
    /// report, so there is no fence to echo. A browser that must start over is sent
    /// nothing until an IDR, and `false` says this unit was dropped for one: only the
    /// Mac can send it, and a screen that stays still would never bring one unasked.
    pub async fn pass_hevc(
        &self,
        w: u16,
        h: u16,
        frame: Vec<u8>,
        passed: crate::stream::Passed,
    ) -> anyhow::Result<bool> {
        video::check_picture((w, h))?;
        self.forward(w, h, frame, passed).await
    }

    /// Queue a checked, passed unit: see [`Self::pass`]. `false` when it was dropped
    /// while the browser waits for a keyframe.
    async fn forward(&self, w: u16, h: u16, frame: Vec<u8>, passed: crate::stream::Passed) -> anyhow::Result<bool> {
        self.shared.passing.store(true, Ordering::Relaxed);
        let restart = if passed.keyframe {
            self.shared.pass_restart.swap(false, Ordering::Relaxed)
        } else if self.shared.pass_restart.load(Ordering::Relaxed) {
            debug!("{}: dropping a passed frame until the keyframe a restart needs", self.engine);
            return Ok(false);
        } else {
            false
        };
        let announce = {
            let mut announced = self.shared.pass_announced.lock().unwrap();
            (restart || announced.as_deref() != Some(passed.decode.as_str())).then(|| {
                *announced = Some(passed.decode.clone());
                passed.decode
            })
        };
        let bytes = frame.len();
        let held = self.hold(bytes).await;
        self.shared.units.fetch_add(1, Ordering::Relaxed);
        self.shared.encoded_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        if passed.keyframe {
            self.shared.keyframes.fetch_add(1, Ordering::Relaxed);
            self.shared.keyframe_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        }
        if let Some(decode) = announce {
            self.push(Pending::Msg(ServerMsg::VideoFormat { decode })).await?;
        }
        let unit = VideoUnit { w, h, keyframe: passed.keyframe, data: frame, held };
        self.push(Pending::Msg(ServerMsg::Video(unit))).await?;
        Ok(true)
    }

    /// Whether the picture is the remote's stream passed through ([`Self::pass`]).
    pub fn passing(&self) -> bool {
        self.shared.passing.load(Ordering::Relaxed)
    }

    /// Wait until everything queued towards the browser has given its share of
    /// [`QUEUE_BUDGET`] back: written to a socket that keeps up, or received by a
    /// client that is behind (`ws.rs` decides which). Immediate on a link with room.
    ///
    /// What a passed stream's fence waits on before it is echoed. The remote keeps one
    /// frame in flight and times its fence, so an echo held here puts the browser's
    /// queueing inside the round trip its quality walk reads, where an immediate echo
    /// would time only the hop to this gateway.
    pub async fn drained(&self) {
        // Every permit at once, handed straight back: nothing is taken, only waited
        // for. Only a closed semaphore refuses, and nothing closes this one.
        let _ = self.shared.budget.acquire_many(QUEUE_BUDGET).await;
    }

    /// Take `bytes` of [`QUEUE_BUDGET`], waiting for the browser's socket to make
    /// room. Counted as the engine stalling, which it is, and towards the next
    /// round's congestion verdict.
    async fn hold(&self, bytes: usize) -> Held {
        let started = Instant::now();
        let held = Held::take(&self.shared.budget, bytes, QUEUE_BUDGET).await;
        let waited = micros(started);
        self.shared.stalled_micros.fetch_add(waited, Ordering::Relaxed);
        self.shared.held_micros.fetch_add(waited, Ordering::Relaxed);
        held
    }

    /// Queue anything that is not pixels, keeping it behind the frames it follows.
    ///
    /// A resize is also how the stream learns how big the desktop is. That is one
    /// interception rather than a size threaded through every place a stream has to
    /// be rebuilt, and it cannot be forgotten by a place added later: telling the
    /// client its framebuffer changed and telling the encoder are the same event, and
    /// the first already happens everywhere the second must.
    ///
    /// Bookkeeping only — nothing here can fail, deliberately. This method's error is
    /// read by every caller as "the browser has gone", answered by returning without
    /// a word (`rdp::run`, `vnc::run`), so a desktop the encoder cannot handle would
    /// end the session silently on the picker. Building the mirror and the stream
    /// waits for [`Self::damage`], which is on the engines' `?` path and ends the
    /// session with the message attached.
    ///
    /// A resize is also where the picture moves between video and tiles, for a source
    /// that has them ([`TileSupport`]): past the ceiling it is tiles, within it video.
    /// Either way the remote repaints the resized desktop in full, which is what a
    /// browser changing carriage starts from; back on video, the stream starts over
    /// from a keyframe as a browser that attached would.
    pub async fn msg(&self, msg: ServerMsg) -> anyhow::Result<()> {
        if self.tiling() {
            // Rectangles taken before this message are drawn before it.
            self.queue_tiles().await?;
        }
        if let ServerMsg::Resize { w, h, .. } = &msg {
            let (w, h) = (*w, *h);
            let tiling = match self.shared.tile_support {
                TileSupport::None => false,
                TileSupport::Rects => !video::within_ceiling((u32::from(w), u32::from(h))),
                TileSupport::Gaps => true,
            };
            if self.shared.tiling.swap(tiling, Ordering::Relaxed) != tiling {
                if tiling {
                    self.shared.passing.store(false, Ordering::Relaxed);
                    info!(
                        "{}: a {w}x{h} desktop is past what a video stream encodes; \
                         its rectangles go to the browser as tiles",
                        self.engine
                    );
                } else {
                    info!("{}: a {w}x{h} desktop goes to the browser as video again", self.engine);
                    self.reset_render();
                }
            }
            self.shared.video.lock().await.stream.want(w, h);
            if self.shared.tile_support == TileSupport::Rects {
                self.push(Pending::Msg(msg)).await?;
                return self.push(Pending::Msg(ServerMsg::Tiling { active: tiling })).await;
            }
        }
        self.push(Pending::Msg(msg)).await
    }

    /// Tell the stream the desktop is `w`×`h` without putting a `Resize` on the
    /// channel, for an engine test that reads the channel and never sent one.
    #[cfg(test)]
    pub(crate) fn presize(&self, w: u16, h: u16) {
        self.shared.video.try_lock().expect("a fresh sink").stream.want(w, h);
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
    /// zeroes says nothing.
    fn report(&self) {
        let totals = Totals::of(&self.shared);
        if totals.units > 0 || totals.tiles > 0 {
            info!("{}: encode totals: {totals}", self.engine);
        }
    }

    /// The error a push reports once the order task has stopped.
    ///
    /// An encode failure lands in the order task, so it is recorded there and
    /// surfaces here on the next push — which is what stops the shadow from
    /// believing the client holds pixels that were never sent.
    fn closed(&self) -> anyhow::Error {
        match self.shared.failure.lock().unwrap().take() {
            Some(error) => error,
            None => anyhow::anyhow!("frame channel closed"),
        }
    }
}

/// Sharpen a stream that went quiet below the dial.
///
/// The congestion walk only runs when a round is taken, and a round is only taken
/// when something changed — so a screen that stops right after the link coarsened
/// it would keep that picture for good, with the walk frozen below the dial. Once
/// the stream has been idle [`SETTLE_IDLE`] and the client's lag has cleared, this
/// takes the dial back and marks the unchanged mirror dirty; the engine, woken the
/// way a returning round wakes it, encodes it as one inter frame, which sharpens
/// every block and costs no keyframe.
///
/// It waits for [`LAG_CLEAR`] and not merely for the lag to stop counting as
/// behind: settling while the link is still behind would only be walked back down
/// again. It can afford to wait without a deadline — a stream that has gone quiet
/// is a link with nothing on it, so the lag this waits on drains.
async fn settle_stream(engine: &'static str, shared: &Shared) {
    let now = tokio::time::Instant::now();
    let mut video = shared.video.lock().await;
    let Some(coarse_at) = video.coarse_at else {
        return;
    };
    if video.stream.round_out()
        || video.stream.dirty()
        || now.saturating_duration_since(coarse_at) < SETTLE_IDLE
        || (video.congestion.lag_aware && shared.feedback.lag(now) > LAG_CLEAR)
    {
        return;
    }
    let dial = video.congestion.dial;
    // The encoder first and the walk only after it: on failure nothing is recorded,
    // the settle stays owed, and the next tick tries again. The picture on screen is
    // still a good one, only a coarser one.
    if let Err(e) = video.stream.set_quality(dial) {
        warn!("{engine}: could not take the video quality back to {dial}: {e:#}");
        return;
    }
    video.congestion.settle(now);
    video.stream.refresh();
    video.coarse_at = None;
    drop(video);
    debug!("{engine}: the desktop went quiet below the dial; settling it at {dial}");
    shared.round_returned.notify_one();
}

/// Collect finished rounds in push order and forward them, and settle a stream that
/// went quiet below its dial.
///
/// The settle timer belongs here rather than anywhere the frames arrive, because
/// the case it exists for is a screen that has stopped producing them.
async fn order_loop(
    engine: &'static str,
    mut rx: mpsc::Receiver<Pending>,
    frame_tx: mpsc::Sender<ServerMsg>,
    shared: Arc<Shared>,
) {
    let mut settle = tokio::time::interval(SETTLE_TICK);
    // Delay rather than Burst: a tick missed while the queue was busy is a tick
    // whose settle is still owed, and firing the backlog at once would only stack
    // re-encodes behind whatever made it late.
    settle.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let item = tokio::select! {
            item = rx.recv() => match item {
                Some(item) => item,
                None => break,
            },
            _ = settle.tick() => {
                settle_stream(engine, &shared).await;
                continue;
            }
        };
        let (handle, mut held) = match item {
            Pending::Msg(msg) => {
                if frame_tx.send(msg).await.is_err() {
                    break; // browser gone; the engine learns it from its own next push
                }
                continue;
            }
            Pending::Flush(ack) => {
                // Everything before this is already through, which is the whole
                // claim; the ack costs nothing and asks for no ordering of its own.
                let _ = ack.send(());
                continue;
            }
            Pending::Round(handle, held) => (handle, held),
            Pending::Tiles(handle, held) => {
                if !forward_tiles(engine, &shared, &frame_tx, handle, held).await {
                    break;
                }
                continue;
            }
        };
        // `waiting` accrues only while the handle is found unfinished, so a round
        // already encoded when its turn comes adds encode time and no waiting — the
        // read loop overlapped it.
        let started = Instant::now();
        let joined = handle.await;
        shared.waited_micros.fetch_add(micros(started), Ordering::Relaxed);
        let (round, produced, encode_micros) = match joined {
            Ok(finished) => finished,
            // Only reachable by cancellation — `panic = "abort"` in release means a
            // panicking worker never gets this far.
            Err(e) => {
                give_up(engine, &shared, anyhow::Error::new(e).context("video encoder stopped"));
                break;
            }
        };
        shared.encode_micros.fetch_add(encode_micros, Ordering::Relaxed);
        // A stream that produced nothing keeps its dirty flag and its keyframe, so
        // those pixels ride the next round. `skip_frames(false)` should make that
        // unreachable; the counter is how we would find out that it is not.
        if round.skipped() > 0 {
            shared.skipped.fetch_add(round.skipped(), Ordering::Relaxed);
            warn!("{engine}: a video frame encoded to nothing; its pixels wait for the next");
        }
        let dirty = {
            let mut video = shared.video.lock().await;
            video.stream.put_back(round);
            video.stream.dirty()
        };
        // Damage may have landed while the round was out, and the engine may be
        // parked on a `due_at` computed when the encoder was away — this is what
        // tells it the mirror is dirty again. The keyframe flag is the same shape:
        // armed while nothing was home to take it.
        if dirty || shared.keyframe_owed.load(Ordering::Relaxed) {
            shared.round_returned.notify_one();
        }
        let produced = match produced {
            Ok(produced) => produced,
            // Not recoverable by rebuilding: a fresh stream would hand a blank mirror
            // to a keyframe, which is wrong pixels rather than coarse ones, and the
            // shadow already counts the real ones as delivered.
            Err(e) => {
                give_up(engine, &shared, e.context("video encode failed"));
                break;
            }
        };
        // The announcement first, so a client's decoder is configured before the unit
        // that needs it arrives. Sent here rather than pushed, because this *is* the
        // ordered task: nothing queued behind this round can overtake it.
        if let Some(decode) = produced.format {
            let msg = ServerMsg::VideoFormat { decode };
            if frame_tx.send(msg).await.is_err() {
                break;
            }
        }
        let Some(mut unit) = produced.unit else {
            continue;
        };
        let bytes = unit.data.len();
        shared.round_bytes.store(bytes as u64, Ordering::Relaxed);
        held.settle(bytes);
        unit.held = held;
        shared.units.fetch_add(1, Ordering::Relaxed);
        shared.encoded_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        if unit.keyframe {
            shared.keyframes.fetch_add(1, Ordering::Relaxed);
            shared.keyframe_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        }
        debug!(
            "{engine}: {} {}x{}: {bytes} bytes",
            if unit.keyframe { "keyframe" } else { "frame" },
            unit.w,
            unit.h
        );
        if frame_tx.send(ServerMsg::Video(unit)).await.is_err() {
            break; // browser gone; the engine learns it from its own next push
        }
    }
}

/// Collect one update's tiles and forward them, each with its part of the share
/// taken for them all. `false` when the queue is done: the encode failed, or the
/// browser has gone.
async fn forward_tiles(
    engine: &'static str,
    shared: &Shared,
    frame_tx: &mpsc::Sender<ServerMsg>,
    handle: JoinHandle<(anyhow::Result<Vec<Tile>>, u64)>,
    mut held: Held,
) -> bool {
    let started = Instant::now();
    let joined = handle.await;
    shared.waited_micros.fetch_add(micros(started), Ordering::Relaxed);
    let mut tiles = match joined {
        Ok((Ok(tiles), encode_micros)) => {
            shared.encode_micros.fetch_add(encode_micros, Ordering::Relaxed);
            tiles
        }
        Ok((Err(e), _)) => {
            give_up(engine, shared, e.context("tile encode failed"));
            return false;
        }
        Err(e) => {
            give_up(engine, shared, anyhow::Error::new(e).context("tile encoder stopped"));
            return false;
        }
    };
    let bytes: usize = tiles.iter().map(|tile| tile.data.len()).sum();
    shared.round_bytes.store(bytes as u64, Ordering::Relaxed);
    held.settle(bytes);
    for tile in &mut tiles {
        tile.held = held.split(tile.data.len());
    }
    shared.tiles.fetch_add(tiles.len() as u64, Ordering::Relaxed);
    shared.tile_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    debug!("{engine}: {} tile(s): {bytes} bytes", tiles.len());
    frame_tx.send(ServerMsg::Tiles(tiles)).await.is_ok()
}

/// Record why the queue stopped, for the engine's next push to report.
fn give_up(engine: &str, shared: &Shared, error: anyhow::Error) {
    warn!("{engine}: {error:#}");
    *shared.failure.lock().unwrap() = Some(error);
}

fn micros(since: Instant) -> u64 {
    since.elapsed().as_micros() as u64
}

/// A snapshot of what one engine's encoder cost, for [`VideoSink::report`].
///
/// The repo has no benchmark harness, so — like `wire::Totals` for the browser
/// link — this line is the only measurement of the encoder that exists in
/// production. Each number earns its place by answering something the others
/// cannot:
///
/// - `unit`, `keyframe` and `coarsened` are read together. A keyframe count
///   rivalling `unit` means the stream is getting no inter-frame compression at all,
///   which is the entire claim it makes; subtracting the keyframe bytes from `bytes`
///   gives the average delta. `coarsened` is how many rounds went out below the
///   configured quality because the link could not carry it, with the worst quality
///   reached — zero there means the dial was the only thing deciding, and the
///   measurement is clean.
/// - `encode` against `waiting` says whether the encodes overlapped the read loop:
///   `waiting` accrues only while the order task finds a round *unfinished*.
/// - `stalled` is what the read loop still pays, waiting on the queue or the budget.
/// - `bytes` cross-checks against the `ws: outbound totals` line.
/// - `skipped` must be zero. It counts frames the encoder produced no bitstream for,
///   whose pixels are carried by the next frame instead. Non-zero means a hazard that
///   is supposed to be unreachable is not.
struct Totals {
    units: u64,
    encoded_bytes: u64,
    tiles: u64,
    tile_bytes: u64,
    keyframes: u64,
    keyframe_bytes: u64,
    skipped: u64,
    coarsened: u64,
    worst_quality: u64,
    encode_micros: u64,
    waited_micros: u64,
    stalled_micros: u64,
}

impl Totals {
    fn of(shared: &Shared) -> Self {
        // Relaxed throughout, and read while the order task may still be running:
        // this is a log line, not a decision, and the counters only ever grow.
        Self {
            units: shared.units.load(Ordering::Relaxed),
            encoded_bytes: shared.encoded_bytes.load(Ordering::Relaxed),
            tiles: shared.tiles.load(Ordering::Relaxed),
            tile_bytes: shared.tile_bytes.load(Ordering::Relaxed),
            keyframes: shared.keyframes.load(Ordering::Relaxed),
            keyframe_bytes: shared.keyframe_bytes.load(Ordering::Relaxed),
            skipped: shared.skipped.load(Ordering::Relaxed),
            coarsened: shared.coarsened.load(Ordering::Relaxed),
            worst_quality: shared.worst_quality.load(Ordering::Relaxed),
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
            "{} access unit(s) / {} bytes, {} tile(s) / {} bytes, \
             {} keyframe(s) / {} bytes, {} skipped, \
             {} round(s) coarsened (lowest quality {}), \
             {}µs encoding in {}µs of waiting, engine stalled {}µs",
            self.units,
            self.encoded_bytes,
            self.tiles,
            self.tile_bytes,
            self.keyframes,
            self.keyframe_bytes,
            self.skipped,
            self.coarsened,
            self.worst_quality,
            self.encode_micros,
            self.waited_micros,
            self.stalled_micros
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Chroma;
    use crate::protocol::{UNSCALED, VideoUnit};

    /// A fresh, never-written link measurement: what every sink here runs on, so
    /// nothing in these tests depends on a lag that was never the subject.
    fn feedback() -> Arc<LinkFeedback> {
        Arc::new(LinkFeedback::new())
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a non-empty rectangle")
    }

    /// Packed RGB888 for a `w`x`h` rectangle, filled so no two seeds share bytes.
    fn rgb(w: u16, h: u16, seed: u8) -> Vec<u8> {
        (0..usize::from(w) * usize::from(h) * 3)
            .map(|i| seed.wrapping_add((i % 251) as u8))
            .collect()
    }

    /// The assertion most tests here make: what came out, in the order it came.
    async fn drain(rx: &mut mpsc::Receiver<ServerMsg>, count: usize) -> Vec<ServerMsg> {
        let mut out = Vec::new();
        for _ in 0..count {
            out.push(rx.recv().await.expect("frame channel closed early"));
        }
        out
    }

    const VIDEO: RenderPlan = RenderPlan { quality: 60, adaptive: None, chroma: Chroma::Subsampled, apple_hevc: false };

    /// A video sink that has been told how big the desktop is, which is the one thing
    /// it needs before it will accept any pixels.
    async fn video_sink(w: u16, h: u16) -> (VideoSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), TileSupport::None);
        sink.msg(ServerMsg::Resize { w, h, scale: UNSCALED }).await.unwrap();
        sink.flush().await;
        // The resize itself, so a test can count what follows.
        assert!(matches!(frame_rx.recv().await, Some(ServerMsg::Resize { .. })));
        (sink, frame_rx)
    }

    /// A sink for a source that can carry an oversize desktop as tiles, told the
    /// desktop is `w`×`h`.
    async fn tile_sink(w: u16, h: u16) -> (VideoSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), TileSupport::Rects);
        sink.msg(ServerMsg::Resize { w, h, scale: UNSCALED }).await.unwrap();
        sink.flush().await;
        assert!(matches!(frame_rx.recv().await, Some(ServerMsg::Resize { .. })));
        let tiling = !video::within_ceiling((u32::from(w), u32::from(h)));
        assert!(
            matches!(frame_rx.recv().await, Some(ServerMsg::Tiling { active }) if active == tiling),
            "a resize of a source that can tile did not say which carriage follows"
        );
        (sink, frame_rx)
    }

    /// The opening byte of a profile 1 VP9 frame: `frame_marker` 2, profile 1, not a
    /// repeat, then `frame_type`. What `pass` reads; the rest is the remote's.
    fn passed_frame(keyframe: bool, len: usize) -> Vec<u8> {
        let mut frame = vec![0u8; len];
        frame[0] = if keyframe { 0xa0 } else { 0xa4 };
        frame
    }

    /// A frame the remote encoded goes out as it came, behind the announcement a
    /// decoder needs, and the ones before the first keyframe are not sent at all.
    #[tokio::test]
    async fn a_passed_stream_starts_at_a_keyframe_behind_its_announcement() {
        let (sink, mut frame_rx) = video_sink(1280, 800).await;
        assert!(!sink.passing());
        sink.pass(1280, 800, passed_frame(false, 40)).await.unwrap();
        sink.pass(1280, 800, passed_frame(true, 900)).await.unwrap();
        sink.pass(1280, 800, passed_frame(false, 50)).await.unwrap();
        sink.flush().await;
        assert!(sink.passing());

        let out = drain(&mut frame_rx, 3).await;
        assert!(
            matches!(&out[0], ServerMsg::VideoFormat { decode } if decode == "vp09.01.40.08.03.06.06.06.00"),
            "{:?}",
            out[0]
        );
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe && unit.data == passed_frame(true, 900)));
        assert!(matches!(&out[2], ServerMsg::Video(unit) if !unit.keyframe && unit.data.len() == 50));
        assert!(frame_rx.try_recv().is_err(), "a frame before the first keyframe went out");
    }

    /// A reset — a reattach, a takeover, a resize — restarts the passed stream the
    /// way it restarts one coded here: nothing until a keyframe, which is announced
    /// again for the browser that has never seen the announcement.
    #[tokio::test]
    async fn a_reset_passed_stream_waits_for_a_keyframe_and_announces_it_again() {
        let (sink, mut frame_rx) = video_sink(1280, 800).await;
        sink.pass(1280, 800, passed_frame(true, 10)).await.unwrap();
        sink.flush().await;
        drain(&mut frame_rx, 2).await;

        sink.reset_render();
        sink.pass(1280, 800, passed_frame(false, 10)).await.unwrap();
        sink.pass(1280, 800, passed_frame(true, 20)).await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 2).await;
        assert!(matches!(&out[0], ServerMsg::VideoFormat { .. }), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe && unit.data.len() == 20));
        assert!(frame_rx.try_recv().is_err());
    }

    /// A passed High Performance stream: the Mac's rectangles are tiles whatever the
    /// desktop's size, with no `tiling` said, since the picture is still the stream;
    /// its units go out as passed ones do, and one dropped for a keyframe says so,
    /// for the engine to ask the Mac.
    #[tokio::test]
    async fn the_gaps_around_a_passed_hevc_stream_are_tiles() {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), TileSupport::Gaps);
        assert!(sink.tiling(), "tiles from the start, before any resize");
        sink.msg(ServerMsg::Resize { w: 1600, h: 1000, scale: UNSCALED }).await.unwrap();
        let rect = Rect::from_size(0, 0, 2, 2).unwrap();
        sink.damage(rect, &[7; 12]).await.unwrap();
        sink.frame().await.unwrap();
        let hevc = |keyframe| crate::stream::Passed { decode: "hev1.4.10.L150.BE.8".to_owned(), keyframe };
        assert!(!sink.pass_hevc(1600, 1000, vec![1; 30], hevc(false)).await.unwrap(), "dropped for a keyframe");
        assert!(sink.pass_hevc(1600, 1000, vec![2; 900], hevc(true)).await.unwrap());
        sink.flush().await;

        let out = drain(&mut frame_rx, 4).await;
        assert!(matches!(&out[0], ServerMsg::Resize { .. }), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Tiles(tiles) if tiles.len() == 1), "{:?}", out[1]);
        assert!(
            matches!(&out[2], ServerMsg::VideoFormat { decode } if decode == "hev1.4.10.L150.BE.8"),
            "{:?}",
            out[2]
        );
        assert!(matches!(&out[3], ServerMsg::Video(unit) if unit.keyframe && unit.data.len() == 900));
        assert!(frame_rx.try_recv().is_err(), "no `tiling` message and nothing else");
        assert!(sink.tiling(), "a resize leaves the gaps as tiles");
    }

    /// Only the 4:4:4 stream the announcement describes is passed.
    #[tokio::test]
    async fn a_passed_frame_of_another_profile_is_refused() {
        let (sink, _frame_rx) = video_sink(1280, 800).await;
        let mut frame = passed_frame(true, 10);
        frame[0] = 0x80; // profile 0
        let error = sink.pass(1280, 800, frame).await.unwrap_err();
        assert!(format!("{error:#}").contains("profile 0"), "{error:#}");
    }

    /// `drained` is what a passed stream's fence waits on: done at once with nothing
    /// queued, and not until the browser's side has let go of a frame that is.
    #[tokio::test]
    async fn drained_waits_for_the_frames_queued_towards_the_browser() {
        let (sink, mut frame_rx) = video_sink(1280, 800).await;
        tokio::time::timeout(Duration::from_secs(1), sink.drained())
            .await
            .expect("an empty queue is drained");

        sink.pass(1280, 800, passed_frame(true, 4096)).await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 2).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sink.drained()).await.is_err(),
            "drained while the browser's side still held the frame"
        );
        drop(out);
        tokio::time::timeout(Duration::from_secs(1), sink.drained())
            .await
            .expect("drained once the frame was let go");
    }

    /// Past the ceiling, a source with rectangles has each one sent as it came:
    /// one tile per rectangle, at its place and size, in order, and no stream.
    #[tokio::test]
    async fn an_oversize_desktop_goes_as_the_sources_own_rectangles() {
        let (sink, mut frame_rx) = tile_sink(5376, 2288).await;
        assert!(sink.tiling());

        let rects = [rect(5000, 2000, 7, 5), rect(3, 4, 64, 2)];
        for (seed, area) in rects.iter().enumerate() {
            sink.damage(*area, &rgb(area.w(), area.h(), seed as u8)).await.unwrap();
        }
        sink.frame().await.unwrap();
        sink.flush().await;

        let Some(ServerMsg::Tiles(tiles)) = frame_rx.recv().await else {
            panic!("an oversize desktop did not go as tiles");
        };
        let placed: Vec<_> = tiles.iter().map(|t| (t.x, t.y, t.w, t.h)).collect();
        assert_eq!(placed, vec![(5000, 2000, 7, 5), (3, 4, 64, 2)]);
        assert!(frame_rx.try_recv().is_err(), "an oversize desktop sent more than its tiles");
        assert!(sink.due_at().await.is_none(), "a tile waits for nothing");
    }

    /// Within the ceiling a source with rectangles is video like any other, and a
    /// source without them past the ceiling fails as it always has.
    #[tokio::test]
    async fn tiles_are_only_for_a_desktop_video_cannot_carry() {
        let (sink, mut frame_rx) = tile_sink(1280, 800).await;
        assert!(!sink.tiling());
        let area = rect(0, 0, 64, 64);
        sink.damage(area, &rgb(64, 64, 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;

        let (sink, _frame_rx) = video_sink(5376, 2288).await;
        assert!(!sink.tiling());
        let error = sink.damage(area, &rgb(64, 64, 1)).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("will not encode a 5376x2288 picture"),
            "unexpected refusal: {error:#}"
        );
    }

    /// A desktop that shrinks back under the ceiling is video again, and its stream
    /// starts where a decoder can: an announcement and a keyframe. A desktop that
    /// grows past it sends what it had pending first, then tiles.
    #[tokio::test]
    async fn crossing_the_ceiling_changes_the_carriage_at_the_resize() {
        let (sink, mut frame_rx) = tile_sink(1280, 800).await;
        let area = rect(0, 0, 64, 64);
        sink.damage(area, &rgb(64, 64, 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;

        sink.msg(ServerMsg::Resize { w: 5376, h: 2288, scale: UNSCALED }).await.unwrap();
        sink.damage(area, &rgb(64, 64, 2)).await.unwrap();
        sink.msg(ServerMsg::Resize { w: 1280, h: 800, scale: UNSCALED }).await.unwrap();
        assert!(!sink.tiling());
        sink.damage(area, &rgb(64, 64, 3)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 7).await;
        assert!(matches!(out[0], ServerMsg::Resize { w: 5376, .. }));
        assert!(matches!(out[1], ServerMsg::Tiling { active: true }));
        assert!(
            matches!(&out[2], ServerMsg::Tiles(tiles) if tiles.len() == 1),
            "the rectangle taken past the ceiling did not go out before the next resize"
        );
        assert!(matches!(out[3], ServerMsg::Resize { w: 1280, .. }));
        assert!(matches!(out[4], ServerMsg::Tiling { active: false }));
        assert!(matches!(out[5], ServerMsg::VideoFormat { .. }), "video came back unannounced");
        assert!(matches!(&out[6], ServerMsg::Video(unit) if unit.keyframe));
    }

    /// Take the next `units` access units, stepping over the format announcements among them.
    ///
    /// A stream announces its `VideoFormat` before its first unit and again after a repaint, so a
    /// test about the *units* has to allow for them. That the announcement really does come first
    /// is its own test below rather than an assertion buried in a helper, because it is a claim
    /// about the order of two messages and not about what a unit contains.
    async fn drain_units(rx: &mut mpsc::Receiver<ServerMsg>, units: usize) -> Vec<VideoUnit> {
        let mut out = Vec::new();
        while out.len() < units {
            match rx.recv().await.expect("frame channel closed early") {
                ServerMsg::VideoFormat { decode, .. } => {
                    assert!(
                        decode.starts_with("vp09."),
                        "a format named no WebCodecs configuration: {decode}"
                    );
                }
                ServerMsg::Video(unit) => out.push(unit),
                other => panic!("expected video, got {other:?}"),
            }
        }
        out
    }

    /// The `VideoFormat` contract, which is the whole of what a client needs to build a decoder:
    /// it arrives **before** the first unit, it is not repeated while nothing changes, and a
    /// repaint says it again — because a repaint is what a browser that just attached gets, and it
    /// never saw the first one.
    /// Paused, because the second round below is an *ordinary* one and an ordinary round waits
    /// out [`VIDEO_FRAME_INTERVAL`] — the third does not, since a repaint bypasses it.
    #[tokio::test(start_paused = true)]
    async fn a_stream_announces_its_format_before_its_first_unit() {
        let (sink, mut frame_rx) = video_sink(640, 480).await;
        let area = rect(0, 0, 320, 64);

        sink.damage(area, &rgb(area.w(), area.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 2).await;
        let ServerMsg::VideoFormat { decode } = &out[0] else {
            panic!("the first thing a stream sends must be its format, got {:?}", out[0]);
        };
        assert!(decode.starts_with("vp09.00."), "not a VP9 profile-0 configuration: {decode}");
        let announced = decode.clone();
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe));

        // A second round changes nothing about how to decode it, so it says nothing.
        sink.damage(area, &rgb(area.w(), area.h(), 2)).await.unwrap();
        tokio::time::sleep(VIDEO_FRAME_INTERVAL).await;
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 1).await;
        assert!(
            matches!(out[0], ServerMsg::Video(_)),
            "the format was repeated for a decoder that already has it"
        );

        // A repaint does. This is the reattach and the takeover: `reset_render` is what
        // `ClientMsg::Refresh` reaches, and the browser it is for has seen neither the format nor
        // a keyframe.
        sink.reset_render();
        sink.damage(area, &rgb(area.w(), area.h(), 3)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 2).await;
        let ServerMsg::VideoFormat { decode: again, .. } = &out[0] else {
            panic!("a repaint must re-announce the format, got {:?}", out[0]);
        };
        assert_eq!(again, &announced, "the same stream came back as a different configuration");
        assert!(
            matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe),
            "a re-announced stream owes a keyframe too"
        );
    }

    /// The wake-up contract both engines rely on now that the encode is pipelined:
    /// while a round is away, `frame()` is a no-op that consumes nothing — not even
    /// a keyframe ask — and the order task signals [`VideoSink::round_returned`] when
    /// the round lands with pixels still waiting, which is what re-arms an engine
    /// parked on a clean `due_at`.
    ///
    /// The round is taken and pushed by hand rather than through `frame()`, so it is
    /// *deterministically* in flight for the middle of the test: a real `frame()`
    /// races the encode worker, and this contract must not be asserted by timing. No
    /// paused clock is needed for the same reason — `due_at` is armed only by the
    /// `frame()` that takes a round, which this test never lets happen until the end,
    /// where the preserved keyframe ask bypasses the interval anyway. The timeout is
    /// a hang guard on a broken signal, not an assertion about speed.
    #[tokio::test]
    async fn a_round_in_flight_defers_frame_and_its_return_wakes_the_engine() {
        let (sink, mut frame_rx) = video_sink(64, 64).await;
        sink.damage(rect(0, 0, 64, 64), &rgb(64, 64, 1)).await.unwrap();
        let mut round =
            sink.shared.video.lock().await.stream.take_round().unwrap().expect("a dirty stream");

        // frame() while the round is out: early return, with the keyframe ask left
        // for a call that has streams to arm.
        sink.reset_render();
        sink.frame().await.unwrap();
        assert!(
            sink.shared.keyframe_owed.load(Ordering::Relaxed),
            "the keyframe ask was consumed while the live table was away"
        );

        // Damage lands while the round is out. Nothing can be dirty yet — the live
        // table is on the worker — which is exactly why the wake-up has to exist.
        sink.damage(rect(0, 0, 64, 64), &rgb(64, 64, 2)).await.unwrap();
        assert!(!sink.shared.video.lock().await.stream.dirty());

        // Hand the round to the real order task, the way frame() does.
        let handle = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let produced = round.encode();
            (round, produced, micros(started))
        });
        sink.push(Pending::Round(handle, Held::default())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), sink.round_returned())
            .await
            .expect("the order task never signalled the returning round");
        assert!(
            sink.shared.video.lock().await.stream.dirty(),
            "the wake-up promised pixels no access unit has carried"
        );

        // The woken engine's next frame() carries them, and the preserved ask makes
        // its unit one a decoder can start from.
        sink.frame().await.unwrap();
        sink.flush().await;
        let units = drain_units(&mut frame_rx, 2).await;
        assert!(units[0].keyframe, "the first unit of a stream starts its decoder");
        assert!(units[1].keyframe, "the preserved keyframe ask never reached the encoder");
        assert!(frame_rx.try_recv().is_err(), "two rounds produced more than two units");
    }

    /// The test for the whole design: `damage` is called once per damage
    /// *rectangle*, and a frame is what the engine says it is. Three rectangles
    /// between two frame boundaries have to be one access unit, not three — and not
    /// one per band either.
    #[tokio::test]
    async fn one_access_unit_per_frame_not_per_damage() {
        let (sink, mut frame_rx) = video_sink(640, 480).await;

        for y in [0, 64, 128] {
            let area = rect(0, y, 320, 64);
            sink.damage(area, &rgb(area.w(), area.h(), 3)).await.unwrap();
        }
        sink.frame().await.unwrap();
        sink.flush().await;

        drain_units(&mut frame_rx, 1).await;
        assert!(frame_rx.try_recv().is_err(), "three rectangles produced more than one frame");
    }

    /// The record header is the *desktop*, not the picture. The encoder is held to
    /// even sides and a desktop need not have them, so the two differ — and it is
    /// the desktop a client has a canvas for.
    #[tokio::test]
    async fn an_access_unit_covers_the_whole_desktop_at_its_true_size() {
        let (sink, mut frame_rx) = video_sink(1919, 1079).await;

        let area = rect(17, 33, 100, 50);
        sink.damage(area, &rgb(area.w(), area.h(), 5)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let units = drain_units(&mut frame_rx, 1).await;
        let unit = &units[0];
        assert_eq!((unit.w, unit.h), (1919, 1079));
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
        sink.damage(area, &rgb(area.w(), area.h(), 9)).await.unwrap();
        sink.frame().await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;
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
            sink.damage(area, &rgb(area.w(), area.h(), 3)).await.unwrap();
            sink.frame().await.unwrap();
        }
        sink.flush().await;

        drain_units(&mut frame_rx, 1).await;
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

        sink.damage(area, &rgb(area.w(), area.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;

        sink.damage(area, &rgb(area.w(), area.h(), 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "the second frame was not held");

        // What an engine's flush arm does, and the only thing that happens here.
        let due = sink.due_at().await.expect("pixels the shadow calls delivered are owed");
        tokio::time::sleep_until(due).await;
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;
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

        sink.damage(area, &rgb(area.w(), area.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;
        let before = sink.shared.keyframes.load(Ordering::Relaxed);

        // Inside the interval, so without the bypass this would be held.
        sink.reset_render();
        sink.damage(area, &rgb(area.w(), area.h(), 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let units = drain_units(&mut frame_rx, 1).await;
        assert!(units[0].keyframe, "the repaint waited out the interval, or was not a keyframe");
        assert_eq!(
            sink.shared.keyframes.load(Ordering::Relaxed),
            before + 1,
            "the client was sent a delta to start its decoder from"
        );
    }

    /// Stand in for the congestion walk having coarsened the stream to `quality`,
    /// the way [`VideoSink::adjust`] leaves it.
    async fn coarsen(sink: &VideoSink, quality: u8) {
        let mut video = sink.shared.video.lock().await;
        video.congestion.quality = quality;
        video.stream.set_quality(quality).expect("a live encoder takes a new quality");
    }

    /// One coarse round: damage, and the frame that carries it.
    async fn coarse_round(sink: &VideoSink, frame_rx: &mut mpsc::Receiver<ServerMsg>, seed: u8) {
        let area = rect(0, 0, 320, 64);
        sink.damage(area, &rgb(area.w(), area.h(), seed)).await.unwrap();
        tokio::time::sleep(VIDEO_FRAME_INTERVAL).await;
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(frame_rx, 1).await;
    }

    /// A whole-desktop stream the link coarsened, and that then stopped changing, is
    /// sharpened: the walk only runs when a round is taken, so without the settle the
    /// client would hold the coarse picture — and the walk stay below the dial — for
    /// as long as the screen stayed still. One inter frame at the dial, woken the way
    /// a returning round wakes the engine, and nothing after it.
    #[tokio::test(start_paused = true)]
    async fn a_quiet_stream_below_the_dial_is_settled_at_the_dial() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        coarse_round(&sink, &mut frame_rx, 1).await;
        coarsen(&sink, 20).await;
        coarse_round(&sink, &mut frame_rx, 2).await;
        assert!(sink.due_at().await.is_none(), "the coarse round left pixels uncarried");

        tokio::time::timeout(SETTLE_IDLE * 4, sink.round_returned())
            .await
            .expect("a quiet stream below the dial was never settled");
        assert_eq!(sink.shared.video.lock().await.stream.quality(), 60, "the dial was not taken back");
        sink.frame().await.unwrap();
        sink.flush().await;
        let units = drain_units(&mut frame_rx, 1).await;
        assert!(!units[0].keyframe, "a settle is an inter frame, not a keyframe");

        // Settled at the dial, so there is nothing more to come back for.
        tokio::time::sleep(SETTLE_IDLE * 4).await;
        assert!(sink.due_at().await.is_none(), "a settled stream was settled again");
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "a settled stream kept sending");
    }

    /// A stream that went out at the dial owes nothing when it goes quiet: a still
    /// screen must cost nothing, which is the whole-desktop stream's standing promise.
    #[tokio::test(start_paused = true)]
    async fn a_quiet_stream_at_the_dial_sends_nothing() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        coarse_round(&sink, &mut frame_rx, 1).await;
        tokio::time::sleep(SETTLE_IDLE * 4).await;
        assert!(sink.due_at().await.is_none(), "an idle stream at its dial was re-sent");
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "an idle stream at its dial was re-encoded");
    }

    /// The settle owed is judged by the quality a round's encoder really ran at, not the
    /// table's: a stream that refused the retune back to the dial while its round was
    /// out still encodes coarse, and must still be settled once its retry succeeds.
    #[tokio::test(start_paused = true)]
    async fn a_round_a_refused_retune_kept_coarse_is_still_settled() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        coarse_round(&sink, &mut frame_rx, 1).await;
        coarsen(&sink, 20).await;
        coarse_round(&sink, &mut frame_rx, 2).await;

        // The walk takes the dial back while a round is out, and the stream refuses it
        // when the round comes home.
        let area = rect(0, 0, 320, 64);
        sink.damage(area, &rgb(area.w(), area.h(), 3)).await.unwrap();
        {
            let mut video = sink.shared.video.lock().await;
            video.stream.refuse_retunes(1);
            let round = video.stream.take_round().unwrap().expect("damage makes a round");
            video.congestion.quality = 60;
            video.stream.set_quality(60).expect("a table with its streams out takes anything");
            video.stream.put_back(round);
            assert_eq!(video.stream.quality(), 60, "the table is at the dial");
        }
        // Encoded at the refused stream's 20; its put_back retries and succeeds.
        tokio::time::sleep(VIDEO_FRAME_INTERVAL).await;
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;
        assert!(sink.due_at().await.is_none(), "the coarse round left pixels uncarried");

        tokio::time::sleep(SETTLE_IDLE * 4).await;
        assert!(sink.due_at().await.is_some(), "a round encoded below the dial was never settled");
        sink.frame().await.unwrap();
        sink.flush().await;
        let units = drain_units(&mut frame_rx, 1).await;
        assert!(!units[0].keyframe, "a settle is an inter frame, not a keyframe");
    }

    /// Under `render_adaptive`, the settle waits for the client's lag to clear rather
    /// than sharpening onto a link that would walk it straight back down.
    #[tokio::test(start_paused = true)]
    async fn an_adaptive_settle_waits_for_the_lag_to_clear() {
        let link = feedback();
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let plan = RenderPlan { quality: 60, adaptive: Some(10), chroma: Chroma::Subsampled, apple_hevc: false };
        let sink = VideoSink::new("test", frame_tx, plan, Arc::clone(&link), TileSupport::None);
        sink.msg(ServerMsg::Resize { w: 320, h: 240, scale: UNSCALED }).await.unwrap();
        sink.flush().await;
        assert!(matches!(frame_rx.recv().await, Some(ServerMsg::Resize { .. })));
        coarse_round(&sink, &mut frame_rx, 1).await;
        coarsen(&sink, 20).await;
        coarse_round(&sink, &mut frame_rx, 2).await;

        // Behind: a batch owed for far longer than the link's floor.
        link.baseline(5);
        link.owed_since(Some(tokio::time::Instant::now()));
        tokio::time::sleep(SETTLE_IDLE * 4).await;
        assert!(
            sink.due_at().await.is_none(),
            "the stream was settled while the client was still behind"
        );
        assert_eq!(sink.shared.video.lock().await.stream.quality(), 20);

        link.owed_since(None);
        tokio::time::timeout(SETTLE_IDLE * 4, sink.round_returned())
            .await
            .expect("the settle never came once the lag cleared");
        assert_eq!(sink.shared.video.lock().await.stream.quality(), 60);
    }

    /// What the engines park on. `None` has to mean "nothing is owed", or a still
    /// target's loop would wake to encode nothing and a video stream with an empty
    /// mirror would spin.
    #[tokio::test(start_paused = true)]
    async fn nothing_is_due_while_the_mirror_is_clean() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        assert!(sink.due_at().await.is_none(), "an untouched mirror owes nothing");

        let area = rect(0, 0, 320, 64);
        sink.damage(area, &rgb(area.w(), area.h(), 4)).await.unwrap();
        assert!(sink.due_at().await.is_some(), "blitted pixels are owed an access unit");

        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;
        assert!(sink.due_at().await.is_none(), "the encode settled the debt");
    }

    /// A resize is the client reallocating its canvas, so the stream has to start
    /// over: a new picture size, and an access unit the new decoder can begin from.
    #[tokio::test]
    async fn a_resize_starts_the_stream_again() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        let area = rect(0, 0, 320, 64);
        sink.damage(area, &rgb(area.w(), area.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;

        sink.reset_render();
        sink.msg(ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED }).await.unwrap();
        sink.damage(area, &rgb(area.w(), area.h(), 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 1).await;
        assert!(matches!(out[0], ServerMsg::Resize { w: 640, .. }), "the resize lost its place");
        let units = drain_units(&mut frame_rx, 1).await;
        assert_eq!((units[0].w, units[0].h), (640, 480), "the stream kept the old picture size");
    }

    /// Where a desktop the encoder cannot handle has to surface. Learning the size
    /// must not fail — `msg`'s error means "the browser has gone" to every caller,
    /// and answering it by returning would end the session silently on the picker.
    /// The failure belongs on the next call that is on the engine's `?` path.
    #[tokio::test]
    async fn a_desktop_too_large_fails_on_the_pixel_path_not_the_message_path() {
        let (frame_tx, _frame_rx) = mpsc::channel(64);
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), TileSupport::None);

        sink.msg(ServerMsg::Resize { w: 5120, h: 2880, scale: UNSCALED })
            .await
            .expect("learning a size must not be the thing that fails");

        let area = rect(0, 0, 320, 64);
        let refused = sink
            .damage(area, &rgb(area.w(), area.h(), 1))
            .await
            .expect_err("a 5K desktop was accepted");
        assert!(format!("{refused:#}").contains("3840"), "{refused:#}");
    }

    /// The hazard a side channel for control messages would create: a message pushed
    /// after a frame must not overtake the access unit that frame is still encoding.
    #[tokio::test]
    async fn a_control_message_cannot_overtake_the_frame_before_it() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        let area = rect(0, 0, 320, 64);
        sink.damage(area, &rgb(area.w(), area.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.msg(ServerMsg::RemoteOs { macos: false }).await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 3).await;
        assert!(matches!(out[0], ServerMsg::VideoFormat { .. }));
        assert!(matches!(out[1], ServerMsg::Video(_)), "the unit lost its place");
        assert!(matches!(out[2], ServerMsg::RemoteOs { .. }), "the message overtook the unit");
    }

    /// Not an error: a browser that leaves mid-frame ends the sink the same way a
    /// full engine teardown does, and the engine hears about it on its next push.
    #[tokio::test]
    async fn a_dropped_frame_channel_is_reported_as_a_closed_channel() {
        let (frame_tx, frame_rx) = mpsc::channel(1);
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), TileSupport::None);
        drop(frame_rx);

        sink.msg(ServerMsg::RemoteOs { macos: false }).await.unwrap();
        sink.flush().await; // the order task discovers it has nowhere to forward to
        let error = sink
            .msg(ServerMsg::RemoteOs { macos: false })
            .await
            .expect_err("the sink accepted work with nowhere to put it");
        // No encode failed, so this is the plain closed-channel message.
        assert_eq!(format!("{error}"), "frame channel closed");
    }

    // ---- congestion ---------------------------------------------------------

    /// A link with room to spare never moves the dial. The dial is a ceiling, so there
    /// is nothing above it to reclaim.
    #[test]
    fn a_clear_link_stays_on_the_dial() {
        let mut congestion = Congestion::new(30, None);
        let start = tokio::time::Instant::now();
        for i in 0..CLEAR_FRAMES * 4 {
            let at = start + Duration::from_millis(u64::from(i) * 33);
            assert_eq!(congestion.observe(Duration::ZERO, Duration::ZERO, at), None);
        }
        assert_eq!(congestion.quality, 30);
    }

    #[test]
    fn a_backlog_gives_up_quality_and_a_clear_link_takes_it_back() {
        let mut congestion = Congestion::new(30, None);
        let start = tokio::time::Instant::now();

        // One slow frame is not a verdict.
        assert_eq!(congestion.observe(BEHIND_BLOCK, Duration::ZERO, start), None);
        assert_eq!(congestion.observe(BEHIND_BLOCK, Duration::ZERO, start), Some(20));

        // The cooldown holds the next ones off however bad the link is — but it does
        // not stop them being *counted*, so a link that never recovered acts the
        // moment it expires rather than starting its case over.
        assert_eq!(congestion.observe(BEHIND_BLOCK, Duration::ZERO, start), None);
        assert_eq!(congestion.observe(BEHIND_BLOCK, Duration::ZERO, start), None);
        let later = start + ADJUST_COOLDOWN;
        assert_eq!(congestion.observe(BEHIND_BLOCK, Duration::ZERO, later), Some(10));

        // Now a link that has recovered: one step back per spell of clear frames,
        // and it stops at the dial rather than going past it.
        let mut at = later;
        for _ in 0..40 {
            at += ADJUST_COOLDOWN;
            for _ in 0..CLEAR_FRAMES {
                congestion.observe(Duration::ZERO, Duration::ZERO, at);
            }
        }
        assert_eq!(
            congestion.quality, 30,
            "the link recovered past the quality that was asked for"
        );
    }

    #[test]
    fn quality_bottoms_out_rather_than_wrapping() {
        let mut congestion = Congestion::new(30, None);
        let start = tokio::time::Instant::now();
        let mut at = start;
        for _ in 0..40 {
            at += ADJUST_COOLDOWN;
            for _ in 0..BEHIND_FRAMES {
                congestion.observe(BEHIND_BLOCK, Duration::ZERO, at);
            }
        }
        assert_eq!(congestion.quality, video::QUALITY_MIN);
    }

    // ---- the adaptive walk ---------------------------------------------------

    /// Without `render_adaptive`, the client's lag is not a signal at all: the
    /// walk keeps its historical shape, pressure only.
    #[test]
    fn a_walk_that_is_not_lag_aware_ignores_lag() {
        let mut congestion = Congestion::new(30, None);
        let start = tokio::time::Instant::now();
        let mut at = start;
        for _ in 0..40 {
            at += Duration::from_millis(33);
            assert_eq!(congestion.observe(Duration::ZERO, LAG_BEHIND * 4, at), None);
        }
        assert_eq!(congestion.quality, 30);
    }

    /// An adaptive walk gives quality up on the client's lag alone — the queue
    /// behind the socket never blocked, which is exactly the video case the paint
    /// window measured (222 ms behind, 7 batches in flight, nothing parked).
    #[test]
    fn an_adaptive_walk_gives_up_quality_on_lag_alone() {
        let mut congestion = Congestion::new(30, Some(20));
        let start = tokio::time::Instant::now();
        assert_eq!(congestion.observe(Duration::ZERO, LAG_BEHIND, start), None);
        assert_eq!(congestion.observe(Duration::ZERO, LAG_BEHIND, start), Some(20));
    }

    /// The adaptive floor is the operator's, not [`video::QUALITY_MIN`] — and a
    /// default floor above a lower dial clamps to the dial rather than raising it.
    #[test]
    fn an_adaptive_walk_bottoms_out_on_its_configured_floor() {
        let mut congestion = Congestion::new(80, Some(40));
        let start = tokio::time::Instant::now();
        let mut at = start;
        for _ in 0..40 {
            at += ADJUST_COOLDOWN;
            for _ in 0..BEHIND_FRAMES {
                congestion.observe(BEHIND_BLOCK, Duration::ZERO, at);
            }
        }
        assert_eq!(congestion.quality, 40);

        // A floor of 20 over a dial of 10, which config never resolves but a plan
        // written by hand can hold: the walk's floor is the dial.
        let clamped = Congestion::new(10, Some(20));
        assert_eq!(clamped.floor, 10);
    }

    /// Between the two lag thresholds nothing accumulates: not evidence the link
    /// is behind, not proof of room either.
    #[test]
    fn lag_between_the_thresholds_earns_neither_direction() {
        let mut congestion = Congestion::new(30, Some(20));
        let start = tokio::time::Instant::now();
        // Walk down once so there is something to reclaim.
        congestion.observe(BEHIND_BLOCK, Duration::ZERO, start);
        assert_eq!(congestion.observe(BEHIND_BLOCK, Duration::ZERO, start), Some(20));
        // A link hovering between LAG_CLEAR and LAG_BEHIND, long past the cooldown.
        let hover = LAG_CLEAR + (LAG_BEHIND - LAG_CLEAR) / 2;
        let mut at = start;
        for _ in 0..CLEAR_FRAMES * 4 {
            at += ADJUST_COOLDOWN;
            assert_eq!(congestion.observe(Duration::ZERO, hover, at), None);
        }
        assert_eq!(congestion.quality, 20, "a hovering link earned quality back");
    }

    /// An adaptive plan's floor reaches the walk, which becomes lag-aware and starts on
    /// the dial.
    #[test]
    fn an_adaptive_plan_carries_its_floor_into_the_walk() {
        let plan = RenderPlan { quality: 60, adaptive: Some(25), chroma: Chroma::Subsampled, apple_hevc: false };
        let shared = Shared::new(plan, feedback(), TileSupport::None);
        let video = shared.video.try_lock().expect("nothing else holds the stream");
        assert!(video.congestion.lag_aware, "the walk ignores lag");
        assert_eq!(video.congestion.floor, 25);
        assert_eq!(video.congestion.quality, 60, "the walk starts on the dial");
    }
}
