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

use screen_vp9::walk::{LAG_CLEAR, QualityWalk};
#[cfg(test)]
use screen_vp9::walk::SETTLE_IDLE;

use crate::config::RenderPlan;
use crate::feedback::LinkFeedback;
use crate::protocol::{GraphicsUnit, Held, HoldCause, Painted, ServerMsg, VideoUnit};
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

/// How often the order task wakes to look for a quiet stream to settle. It has to
/// be its own timer rather than something the next frame does, because a screen
/// that stops changing produces no next frame — which is exactly the case a settle
/// is for.
const SETTLE_TICK: Duration = Duration::from_millis(250);

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
/// already stamps access units with, so the timestamps stop being a fiction. It is
/// the interval a link with room gets; one that is behind on the floor gets it
/// doubled, up to three times ([`QualityWalk::interval`]).
const VIDEO_FRAME_INTERVAL: Duration = Duration::from_micros(33_333);


/// The stream, and what the link will bear.
struct Video {
    stream: DesktopStream,
    /// screen-vp9's walk of the dial, with the plan's quality as its ceiling and
    /// [`VIDEO_FRAME_INTERVAL`] as the frame it slows from. Its verdicts are the
    /// push's blocking ([`VideoSink::adjust`]) and, on an adaptive plan, the paint
    /// window's lag ([`LinkFeedback::lag`]) — whose threshold sits well under
    /// [`crate::ws`]'s 150 ms lag gate on purpose: by the time that gate parks the
    /// window the backpressure chain blocks the push on its own, so the walk
    /// moves quality while the window is still open, before the stall.
    ///
    /// It is also what knows a settle is owed: told what every round left of the
    /// picture the client holds ([`QualityWalk::sent`]), it says when a screen
    /// that stops changing below the dial is to be sharpened there
    /// ([`QualityWalk::settle_at`]), which is what [`settle_stream`] comes back for.
    congestion: QualityWalk,
    /// The earliest the next round may be encoded — see [`VIDEO_FRAME_INTERVAL`].
    /// `None` before the first one, so a freshly connected desktop paints without
    /// waiting out an interval.
    due_at: Option<tokio::time::Instant>,
}

/// What a source's desktop too large for a video stream ([`video::within_ceiling`])
/// comes to.
///
/// A desktop within the ceiling is always one VP9 stream. Past it there is no
/// picture: a source the gateway cannot size holds the session open and says so,
/// for the browser to offer a smaller desktop, and any other ends the session with
/// [`video::check_picture`]'s refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Oversize {
    /// The session ends: the gateway sized the remote, and a remote that answers
    /// past the ceiling has refused what it was asked.
    Refuse,
    /// The picture stops until a `Resize` brings the desktop within the ceiling,
    /// which only the remote can do: [`VideoSink::damage`] drops what it is handed
    /// while [`VideoSink::oversized`] says so, and the browser is told
    /// ([`ServerMsg::Oversize`]). A source that holds is also held over too many
    /// screens when its engine says so ([`VideoSink::hold_screens`]).
    Hold,
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
    oversize: Oversize,
    /// Whether the session's pages decode a Mac's passed HEVC themselves, which
    /// each `VideoFormat` of it says ([`RenderPlan::software`]).
    software: bool,
    /// Whether the desktop's picture is held — see [`Oversize`]. Decided by each
    /// `Resize` ([`VideoSink::msg`]).
    oversized: AtomicBool,
    /// Whether the desktop spans more screens than one view shows, which holds its
    /// picture whatever its size. Set by the engine ahead of the `Resize` it comes
    /// with ([`VideoSink::hold_screens`]).
    too_many_screens: AtomicBool,
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
    /// cleared when the desktop goes past the ceiling and by a rectangle damaged while
    /// it is set, which takes the picture back to the stream encoded here.
    passing: AtomicBool,
    /// The browser must start the passed stream over: drop what is not a keyframe,
    /// and announce the configuration again ahead of the one that is. Set from the
    /// start and by [`VideoSink::reset_render`], so a reattach and a
    /// resize each begin where a decoder can.
    pass_restart: AtomicBool,
    /// The configuration string last announced for the passed stream.
    pass_announced: Mutex<Option<(String, u8)>>,
    /// The browser's notice that the screen is not available is to come down
    /// behind the next unit queued — see [`VideoSink::uncover`].
    uncover_owed: AtomicBool,
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
    /// Runs of a graphics pipeline passed ([`VideoSink::pass_graphics`]), and their
    /// bytes.
    graphics: AtomicU64,
    graphics_bytes: AtomicU64,
    /// Of [`Self::units`], those a decoder could start from, and what they cost. Read
    /// together: see [`Totals`].
    keyframes: AtomicU64,
    keyframe_bytes: AtomicU64,
    /// Frames the encoder produced no bitstream for. Must stay zero.
    skipped: AtomicU64,
    /// Frames sent coarser than the dial asked for, and the lowest quality the link
    /// ever forced. The whole measurement of the walk.
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
    fn new(plan: RenderPlan, feedback: Arc<LinkFeedback>, oversize: Oversize) -> Self {
        let RenderPlan { quality, adaptive, software, .. } = plan;
        Self {
            oversize,
            software,
            oversized: AtomicBool::new(false),
            too_many_screens: AtomicBool::new(false),
            failure: Mutex::default(),
            video: tokio::sync::Mutex::new(Video {
                stream: DesktopStream::new(quality),
                congestion: QualityWalk::new(quality, VIDEO_FRAME_INTERVAL, adaptive),
                due_at: None,
            }),
            round_returned: Notify::new(),
            passing: AtomicBool::new(false),
            pass_restart: AtomicBool::new(true),
            pass_announced: Mutex::default(),
            uncover_owed: AtomicBool::new(false),
            keyframe_owed: AtomicBool::new(false),
            feedback,
            units: AtomicU64::new(0),
            encoded_bytes: AtomicU64::new(0),
            graphics: AtomicU64::new(0),
            graphics_bytes: AtomicU64::new(0),
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
    /// an adaptive plan. `oversize` says what a desktop past the video ceiling comes
    /// to.
    pub fn new(
        engine: &'static str,
        frame_tx: mpsc::Sender<ServerMsg>,
        plan: RenderPlan,
        feedback: Arc<LinkFeedback>,
        oversize: Oversize,
    ) -> Self {
        let (tx, rx) = mpsc::channel(ENCODE_DEPTH);
        let shared = Arc::new(Shared::new(plan, feedback, oversize));
        tokio::spawn(order_loop(engine, rx, frame_tx, Arc::clone(&shared)));
        Self { engine, tx, shared }
    }

    /// Copy one changed rectangle of packed RGB888 into the mirror.
    ///
    /// Nothing is sent until the engine says a frame has ended ([`Self::frame`]):
    /// the unit of the encoder is the whole framebuffer, and a rectangle is only a
    /// part of the next one.
    ///
    /// While [`Self::oversized`] the rectangle is dropped: there is no picture to put
    /// it in, and the resize that ends the hold is repainted in full.
    ///
    /// A rectangle while a passed stream is the picture ([`Self::passing`]) is the gap
    /// after it, on a source whose rectangles come back — wlshare's VP9 on a generic
    /// VNC target: they carry the picture again, as the stream encoded here, which
    /// starts at a keyframe behind its announcement for a browser whose decoder was
    /// the passed stream's. The passed stream starts over the same way when it comes
    /// back. A Mac's media stream has no such gap: its rectangles never reach here
    /// ([`crate::vnc::DesktopState::media_stream`]).
    pub async fn damage(&self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        if self.oversized() {
            return Ok(());
        }
        if self.shared.passing.swap(false, Ordering::Relaxed) {
            debug!("{}: the source's rectangles carry the picture again, as video encoded here", self.engine);
            self.reset_render();
        }
        self.shared.video.lock().await.stream.blit(rect, rgb)
    }

    /// Whether the desktop's picture is held ([`Oversize::Hold`]): past the video
    /// ceiling, or over too many screens. Then [`Self::damage`] and [`Self::frame`]
    /// do nothing, and a frame the remote coded itself is not to be passed either.
    /// Changes only with a `Resize` through [`Self::msg`].
    pub fn oversized(&self) -> bool {
        self.shared.oversized.load(Ordering::Relaxed)
    }

    /// Say whether the desktop the next `Resize` describes spans more screens than
    /// one view shows: a Mac's Combined Display over more than
    /// [`crate::vnc_apple::MAX_COMBINED_SCREENS`]. Read at that `Resize` and every
    /// one after it, a reattach's included, on a source that holds; a count that
    /// changes always changes the combined desktop's size with it.
    pub fn hold_screens(&self, too_many: bool) {
        self.shared.too_many_screens.store(too_many, Ordering::Relaxed);
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
        if self.oversized() {
            return Ok(());
        }
        // The browser is decoding the passed stream: a unit coded here would be one
        // its decoder was not built for. What the mirror holds waits for the gap
        // [`Self::damage`] opens, which starts the stream here over at a keyframe.
        if self.passing() {
            return Ok(());
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
        // for a repaint, a reattach or a resize, and every one of those is
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
        video.due_at = Some(now + video.congestion.interval());
        // What the round's encoder really runs at, not the table: a stream that
        // refused a retune is still coarse, and the settle it owes must not be
        // cleared by a table that has already reached the dial. Nor by a round at
        // the dial that encodes only where the picture changed, which leaves the
        // rest as coarse as it was.
        let quality = round.quality();
        video.congestion.sent(round.coarsest(), now.into_std());
        // The settle's one frame at the dial ([`settle_stream`]), which leaves the
        // encoder where the walk had it for the rounds after.
        let settling = round.settling();
        let keyframe = round.keyframe();
        if keyframe {
            video.congestion.keyframe(now.into_std());
        }
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
        self.adjust(queued.elapsed().max(Duration::from_micros(held_micros)), quality, settling || keyframe)
            .await;
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
    /// [`std::future::pending`] instead of waking an engine to encode nothing, and
    /// while a passed stream is the picture, when [`Self::frame`] encodes nothing.
    pub async fn due_at(&self) -> Option<tokio::time::Instant> {
        if self.passing() {
            return None;
        }
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
    /// `exempt` is a round that is no verdict about the link: a keyframe, or the
    /// settle's frame at the dial. Both are the whole picture and large by their
    /// nature, so the time they take says what they are, not what the link bears;
    /// the frames queued behind them carry any lag they cause into the next verdicts.
    ///
    /// A failure to re-tune is logged and dropped rather than ending the session: the
    /// stream is still perfectly good at the quality it already had, and losing the
    /// ability to *degrade* is not a reason to stop.
    async fn adjust(&self, blocked: Duration, quality: u8, exempt: bool) {
        self.shared.worst_quality.fetch_min(u64::from(quality), Ordering::Relaxed);
        let mut video = self.shared.video.lock().await;
        if quality < video.congestion.ceiling() {
            self.shared.coarsened.fetch_add(1, Ordering::Relaxed);
        }
        if exempt {
            return;
        }
        let dial = video.congestion.ceiling();
        let now = tokio::time::Instant::now();
        // The client's own half of the verdict. Free to read whether or not the
        // walk is lag-aware; `observe` is what knows.
        let lag = self.shared.feedback.lag(now);
        let before = video.congestion.quality();
        let Some(pace) = video.congestion.observe(blocked, lag, now.into_std()) else {
            return;
        };
        if pace.quality != before && let Err(e) = video.stream.set_quality(pace.quality) {
            // The stream kept the quality it had, so the walk does too: its next
            // verdict starts from what is actually in force.
            video.congestion.stays_at(before);
            warn!("{}: could not move the video quality to {}: {e:#}", self.engine, pace.quality);
        } else {
            debug!(
                "{}: video quality now {} at {:.1} frames/s (the dial asks for {dial}; lag {}ms, blocked {}ms)",
                self.engine,
                pace.quality,
                1.0 / pace.interval.as_secs_f64(),
                lag.as_millis(),
                blocked.as_millis()
            );
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
        self.restart_pass();
    }

    /// Start only the passed stream over, at its next keyframe: for a unit the engine
    /// dropped, which the ones after it predict from. The stream encoded here is not
    /// touched, since the browser's picture is not in question.
    pub fn restart_pass(&self) {
        self.shared.pass_restart.store(true, Ordering::Relaxed);
    }

    /// Queue a `w`×`h` frame the remote encoded itself as the next access unit,
    /// untouched: wlshare's VP9 encoding, which is the stream this gateway would have
    /// encoded from the same pixels ([`crate::stream::pass`]).
    ///
    /// None of the stream's own machinery applies. There is no mirror, round,
    /// interval, quality walk or settle: the remote paces, codes and sharpens its
    /// stream itself, and learns how the browser is keeping up from the fences the
    /// engine echoes once [`Self::fence_hold`] says so. What is shared is the queue: the
    /// frame takes its size out of [`QUEUE_BUDGET`] like an encoded unit, and goes out
    /// in order with the messages around it.
    ///
    /// A browser that must start over ([`Self::reset_render`]) is sent nothing until a
    /// keyframe, which the full update the engine asks for at the same moment brings,
    /// and the keyframe goes out behind a fresh announcement.
    pub async fn pass(&self, w: u16, h: u16, frame: Vec<u8>) -> anyhow::Result<()> {
        let passed = crate::stream::pass(w, h, &frame)?;
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
    ///
    /// Its gaps — before it flows and across a display change — show nothing new:
    /// the Mac's rectangles are never encoded on a session with a media stream, so
    /// one that passes the stream builds no encoder at all. The stream coming back
    /// after a gap starts over at an IDR, announced again.
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
        // A stream that comes in strips says so with its format.
        let strips = if passed.strip.is_some() { crate::protocol::Strip::COUNT } else { 1 };
        let announce = {
            let mut announced = self.shared.pass_announced.lock().unwrap();
            let format = (passed.decode, strips);
            (restart || announced.as_ref() != Some(&format)).then(|| {
                *announced = Some(format.clone());
                format.0
            })
        };
        let bytes = frame.len();
        let held = self.hold(bytes).await;
        // Before the push: the batch that carries the frame is written after it,
        // which is what tells [`Self::fence_hold`] the batches ahead from its own.
        self.shared.feedback.handed(tokio::time::Instant::now());
        self.shared.units.fetch_add(1, Ordering::Relaxed);
        self.shared.encoded_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        if passed.keyframe {
            self.shared.keyframes.fetch_add(1, Ordering::Relaxed);
            self.shared.keyframe_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        }
        if let Some(decode) = announce {
            let software = crate::config::page_decodes(self.shared.software, &decode);
            self.push(Pending::Msg(ServerMsg::VideoFormat { decode, passthrough: true, software, strips })).await?;
        }
        let unit = VideoUnit { w, h, strip: passed.strip, keyframe: passed.keyframe, data: frame, held };
        self.push(Pending::Msg(ServerMsg::Video(unit))).await?;
        self.uncover_behind().await?;
        Ok(true)
    }

    /// Bring the browser's notice that the screen is not available down behind the
    /// next unit sent, encoded here or passed: a
    /// `ScreenUnavailable { active: false }` follows that unit on the channel, so
    /// the notice never lifts on a canvas with nothing new on it. For an engine
    /// whose picture is a stream that has just delivered its first picture of a
    /// display ([`crate::vnc::DesktopState::canvas_live`]): the engine cannot tell
    /// when that picture is queued, since [`Self::frame`] defers it while a round is
    /// out or the interval has not passed, and a passed unit may be dropped for a
    /// keyframe.
    pub fn uncover(&self) {
        self.shared.uncover_owed.store(true, Ordering::Relaxed);
    }

    /// The stream stopped before the notice [`Self::uncover`] owes came down: it
    /// stays up for the next stream's first unit.
    pub fn cover(&self) {
        self.shared.uncover_owed.store(false, Ordering::Relaxed);
    }

    /// The notice [`Self::uncover`] owes, behind a passed unit just queued. An
    /// encoded round's follows the unit it produces, in the order task, since a
    /// round may produce none.
    async fn uncover_behind(&self) -> anyhow::Result<()> {
        if self.shared.uncover_owed.swap(false, Ordering::Relaxed) {
            self.push(Pending::Msg(ServerMsg::ScreenUnavailable { active: false })).await?;
        }
        Ok(())
    }

    /// An RDP host's graphics pipeline starts here, from nothing, and is the picture
    /// from now on ([`Self::pass_graphics`]): said ahead of its first command, and
    /// again for a pipeline the host closed and opened anew.
    ///
    /// What the mirror holds of a picture encoded here is left where it is. Such a
    /// picture is a host's bitmap updates, which a host that has confirmed its
    /// pipeline sends no more of.
    pub async fn graphics_start(&self) -> anyhow::Result<()> {
        self.shared.passing.store(true, Ordering::Relaxed);
        self.push(Pending::Msg(ServerMsg::GraphicsStart)).await
    }

    /// Queue a run of an RDP host's graphics pipeline as the next record, untouched:
    /// whole commands out of their bulk compression, for the browser to compose
    /// ([`crate::rdp_client::Event::Graphics`]).
    ///
    /// As [`Self::pass`] in what the queue does — the run takes its size out of
    /// [`QUEUE_BUDGET`] and goes out in order with the messages around it — and
    /// unlike it in having no restart: nothing in a pipeline is a keyframe, and a
    /// run is never dropped for one. A browser that needs the picture from the
    /// start is given a session that starts.
    ///
    /// `frame` is the acknowledgement the host is owed for the frame the run ends,
    /// if it ends one, said once the browser has painted it. That, and not the
    /// queue, is what paces the host: no frame can be dropped between it and the
    /// page, so a page that composes more slowly than the host draws has to be what
    /// the host hears from. The budget is what is left for a browser that says
    /// nothing at all.
    pub async fn pass_graphics(&self, commands: Vec<u8>, frame: Option<Painted>) -> anyhow::Result<()> {
        self.shared.passing.store(true, Ordering::Relaxed);
        let bytes = commands.len();
        let held = self.hold(bytes).await;
        self.shared.graphics.fetch_add(1, Ordering::Relaxed);
        self.shared.graphics_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.push(Pending::Msg(ServerMsg::Graphics(GraphicsUnit { data: commands, frame, held }))).await
    }

    /// Whether the picture is the remote's stream passed through ([`Self::pass`]).
    pub fn passing(&self) -> bool {
        self.shared.passing.load(Ordering::Relaxed)
    }

    /// How long a passed stream's fence is held before it is echoed: the queue
    /// ahead of the frame on the browser's link
    /// ([`crate::feedback::LinkFeedback::hold`]). The remote keeps one frame in
    /// flight and times its fence, so an echo held for this puts the browser's
    /// queueing inside the round trip its quality walk reads, where an immediate
    /// echo would time only the hop to this gateway.
    pub fn fence_hold(&self) -> Duration {
        self.shared.feedback.hold(tokio::time::Instant::now())
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
    /// A resize is also where the picture is held or resumed, for a source that holds
    /// it ([`Oversize::Hold`]): past the ceiling or over too many screens there is
    /// none, and otherwise video. Back from a hold, the remote repaints the resized
    /// desktop in full as after any resize, and the stream starts over from a
    /// keyframe as a browser that attached would.
    pub async fn msg(&self, msg: ServerMsg) -> anyhow::Result<()> {
        if let ServerMsg::Resize { w, h, .. } = &msg {
            let (w, h) = (*w, *h);
            let cause = match self.shared.oversize {
                Oversize::Refuse => None,
                Oversize::Hold if self.shared.too_many_screens.load(Ordering::Relaxed) => {
                    Some(HoldCause::Screens)
                }
                Oversize::Hold if !video::within_ceiling((u32::from(w), u32::from(h))) => {
                    Some(HoldCause::Size)
                }
                Oversize::Hold => None,
            };
            let oversized = cause.is_some();
            if self.shared.oversized.swap(oversized, Ordering::Relaxed) != oversized {
                if let Some(cause) = cause {
                    self.shared.passing.store(false, Ordering::Relaxed);
                    let why = match cause {
                        HoldCause::Size => "is past what a video stream encodes",
                        HoldCause::Screens => "spans more screens than one view shows",
                    };
                    info!(
                        "{}: a {w}x{h} desktop {why}; the picture is held until the \
                         remote sends another",
                        self.engine
                    );
                } else {
                    info!("{}: a {w}x{h} desktop goes to the browser as video again", self.engine);
                    self.reset_render();
                }
            }
            self.shared.video.lock().await.stream.want(w, h);
            if self.shared.oversize == Oversize::Hold {
                self.push(Pending::Msg(msg)).await?;
                return self.push(Pending::Msg(ServerMsg::Oversize { cause })).await;
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
        if totals.units > 0 || totals.graphics > 0 {
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
/// it would keep that picture for good. Once the walk says the settle is owed
/// ([`QualityWalk::settle_at`], [`screen_vp9::walk::SETTLE_IDLE`] after the round
/// that left the picture coarse) and the client's lag has cleared, this marks the
/// unchanged mirror dirty, to be encoded whole at the dial; the engine, woken the
/// way a returning round wakes it, encodes it as one inter frame, which sharpens
/// every block and costs no keyframe. The walk keeps its place and so does the
/// encoder's dial: the frame is `screen-vp9`'s, which leaves the stream where the
/// link had it for the rounds after that one, and the one frame is no verdict.
///
/// It waits for [`LAG_CLEAR`] and not merely for the lag to stop counting as
/// behind: settling while the link is still behind would only be walked back down
/// again. It can afford to wait without a deadline — a stream that has gone quiet
/// is a link with nothing on it, so the lag this waits on drains.
async fn settle_stream(engine: &'static str, shared: &Shared) {
    let now = tokio::time::Instant::now();
    let mut video = shared.video.lock().await;
    let Some(owed_at) = video.congestion.settle_at() else {
        return;
    };
    if video.stream.round_out()
        || video.stream.dirty()
        || now.into_std() < owed_at
        || (video.congestion.lag_aware() && shared.feedback.lag(now) > LAG_CLEAR)
    {
        return;
    }
    let dial = video.congestion.ceiling();
    video.congestion.settle(now.into_std());
    video.stream.settle(dial);
    let walk = video.congestion.quality();
    drop(video);
    debug!("{engine}: the desktop went quiet below the dial; settling it at {dial} (motion stays at {walk})");
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
            // The settle was judged owed before the round was encoded, by the worst
            // it could leave. What it did leave is known now: a round at the dial
            // over the last coarse blocks owes none.
            let left = round.left();
            if left >= video.congestion.ceiling() {
                video.congestion.sent(left, Instant::now());
            }
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
            let software = crate::config::page_decodes(shared.software, &decode);
            let msg = ServerMsg::VideoFormat { decode, passthrough: false, software, strips: 1 };
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
        // Here and not where the round was queued: a round that produced no unit
        // has put no picture ahead of the notice, and its pixels ride the next.
        if shared.uncover_owed.swap(false, Ordering::Relaxed)
            && frame_tx.send(ServerMsg::ScreenUnavailable { active: false }).await.is_err()
        {
            break;
        }
    }
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
    graphics: u64,
    graphics_bytes: u64,
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
            graphics: shared.graphics.load(Ordering::Relaxed),
            graphics_bytes: shared.graphics_bytes.load(Ordering::Relaxed),
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
            "{} access unit(s) / {} bytes, \
             {} graphics run(s) / {} bytes passed, \
             {} keyframe(s) / {} bytes, {} skipped, \
             {} round(s) coarsened (lowest quality {}), \
             {}µs encoding in {}µs of waiting, engine stalled {}µs",
            self.units,
            self.encoded_bytes,
            self.graphics,
            self.graphics_bytes,
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

    const VIDEO: RenderPlan = RenderPlan { quality: 60, adaptive: false, apple_media: false, rdp_graphics: false, rdp_h264: false, software: false };

    /// A video sink that has been told how big the desktop is, which is the one thing
    /// it needs before it will accept any pixels.
    async fn video_sink(w: u16, h: u16) -> (VideoSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), Oversize::Refuse);
        sink.msg(ServerMsg::Resize { w, h, scale: UNSCALED }).await.unwrap();
        sink.flush().await;
        // The resize itself, so a test can count what follows.
        assert!(matches!(frame_rx.recv().await, Some(ServerMsg::Resize { .. })));
        (sink, frame_rx)
    }

    /// A sink for a source that holds an oversize desktop, told the desktop is
    /// `w`×`h`.
    async fn holding_sink(w: u16, h: u16) -> (VideoSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), Oversize::Hold);
        sink.msg(ServerMsg::Resize { w, h, scale: UNSCALED }).await.unwrap();
        sink.flush().await;
        assert!(matches!(frame_rx.recv().await, Some(ServerMsg::Resize { .. })));
        let cause = (!video::within_ceiling((u32::from(w), u32::from(h)))).then_some(HoldCause::Size);
        assert!(
            matches!(frame_rx.recv().await, Some(ServerMsg::Oversize { cause: said }) if said == cause),
            "a resize of a source that holds did not say whether the picture follows"
        );
        (sink, frame_rx)
    }

    /// A passed graphics pipeline is announced where it starts, goes out run for run in
    /// order with the messages around it, and stops the stream encoded here: nothing of
    /// the mirror is encoded while it is the picture.
    #[tokio::test]
    async fn a_graphics_pipeline_is_passed_in_order_and_nothing_is_encoded_beside_it() {
        let (sink, mut rx) = video_sink(64, 32).await;
        sink.graphics_start().await.unwrap();
        sink.pass_graphics(vec![1; 40], None).await.unwrap();
        sink.msg(ServerMsg::Resize { w: 32, h: 32, scale: UNSCALED }).await.unwrap();
        sink.pass_graphics(vec![2; 9], Some(Painted::default())).await.unwrap();
        assert!(sink.passing());
        assert!(sink.due_at().await.is_none(), "there is nothing to come back and encode");
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut rx, 4).await;
        assert!(matches!(out[0], ServerMsg::GraphicsStart));
        assert!(matches!(&out[1], ServerMsg::Graphics(run) if run.data == vec![1; 40] && run.frame.is_none()));
        assert!(matches!(out[2], ServerMsg::Resize { w: 32, h: 32, .. }));
        assert!(matches!(&out[3], ServerMsg::Graphics(run) if run.data == vec![2; 9] && run.frame.is_some()));
        assert!(rx.try_recv().is_err(), "and no access unit beside them");
    }

    /// A passed run takes its size out of the queue's budget, so a browser that is
    /// behind holds the engine — and through it the host — rather than the queue
    /// growing.
    #[tokio::test]
    async fn a_passed_graphics_run_holds_its_share_of_the_budget() {
        let (sink, mut rx) = video_sink(64, 32).await;
        sink.pass_graphics(vec![0; QUEUE_BUDGET as usize], None).await.unwrap();
        sink.flush().await;
        let held = drain(&mut rx, 1).await;
        let waiting = tokio::time::timeout(Duration::from_millis(50), sink.pass_graphics(vec![0; 16], None)).await;
        assert!(waiting.is_err(), "the budget is spent, so the next run waits");
        drop(held);
        tokio::time::timeout(Duration::from_secs(1), sink.pass_graphics(vec![0; 16], None))
            .await
            .expect("the share came back with the run")
            .unwrap();
    }

    /// The opening byte of a profile 1 VP9 frame — the 4:4:4 every stream is:
    /// `frame_marker` 2, profile 1, not a repeat, then `frame_type`. What `pass`
    /// reads; the rest is the remote's.
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
            matches!(&out[0], ServerMsg::VideoFormat { decode, passthrough: true, .. } if decode == "vp09.01.40.08.03.06.06.06.00"),
            "{:?}",
            out[0]
        );
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe && unit.data == passed_frame(true, 900)));
        assert!(matches!(&out[2], ServerMsg::Video(unit) if !unit.keyframe && unit.data.len() == 50));
        assert!(frame_rx.try_recv().is_err(), "a frame before the first keyframe went out");
    }

    /// A reset — a reattach, a resize — restarts the passed stream the
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

    /// The notice that the screen is not available comes down behind the next unit
    /// queued, not when the engine asks: a clean mirror queues nothing, and the
    /// notice waits with it.
    #[tokio::test(start_paused = true)]
    async fn the_notice_comes_down_behind_the_next_unit() {
        let (sink, mut frame_rx) = video_sink(64, 48).await;
        let rect = Rect::from_size(0, 0, 64, 48).unwrap();

        sink.uncover();
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "nothing to queue, nothing to follow");

        sink.damage(rect, &[7; 64 * 48 * 3]).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 3).await;
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe), "{:?}", out[1]);
        assert!(matches!(&out[2], ServerMsg::ScreenUnavailable { active: false }), "{:?}", out[2]);

        // Owed once: the next unit brings no second notice.
        tokio::time::sleep(VIDEO_FRAME_INTERVAL).await;
        sink.damage(rect, &[8; 64 * 48 * 3]).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 1).await;
        assert!(matches!(&out[0], ServerMsg::Video(_)), "{:?}", out[0]);
        assert!(frame_rx.try_recv().is_err());

        // A passed unit dropped for a keyframe brings it no sooner than the one sent.
        sink.uncover();
        let hevc = |keyframe| crate::stream::Passed { decode: "hev1.4.10.L150.BE.8".to_owned(), keyframe, strip: None };
        sink.reset_render();
        assert!(!sink.pass_hevc(64, 48, vec![1; 30], hevc(false)).await.unwrap());
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "dropped, so nothing follows");
        assert!(sink.pass_hevc(64, 48, vec![2; 900], hevc(true)).await.unwrap());
        sink.flush().await;
        let out = drain(&mut frame_rx, 3).await;
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe), "{:?}", out[1]);
        assert!(matches!(&out[2], ServerMsg::ScreenUnavailable { active: false }), "{:?}", out[2]);

        // A stream that stopped before its unit went leaves the notice up.
        sink.uncover();
        sink.cover();
        assert!(sink.pass_hevc(64, 48, vec![3; 900], hevc(true)).await.unwrap());
        sink.flush().await;
        let out = drain(&mut frame_rx, 1).await;
        assert!(matches!(&out[0], ServerMsg::Video(_)), "{:?}", out[0]);
        assert!(frame_rx.try_recv().is_err());
    }

    /// The sink can switch from passed HEVC back to encoded rectangles for a source
    /// that uses both, each beginning at a keyframe behind its announcement. A High
    /// Performance Mac does not use that capability: its rectangles never reach
    /// [`VideoSink::damage`]. A unit dropped for a keyframe says so to the engine.
    #[tokio::test]
    async fn a_source_can_switch_between_passed_hevc_and_encoded_rectangles() {
        const HEVC: &str = "hev1.4.10.L150.BE.8";
        let (sink, mut frame_rx) = video_sink(64, 48).await;
        let rect = Rect::from_size(0, 0, 64, 48).unwrap();
        let hevc = |keyframe| crate::stream::Passed { decode: HEVC.to_owned(), keyframe, strip: None };
        let is_vp9 = |msg: &ServerMsg| matches!(msg, ServerMsg::VideoFormat { decode, passthrough: false, .. } if decode.starts_with("vp09"));
        let is_hevc = |msg: &ServerMsg| matches!(msg, ServerMsg::VideoFormat { decode, passthrough: true, .. } if decode == HEVC);

        // Before the passed stream flows: VP9 from the rectangles.
        sink.damage(rect, &[7; 64 * 48 * 3]).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 2).await;
        assert!(is_vp9(&out[0]), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe));

        // The passed stream takes over at its IDR, announced.
        assert!(!sink.pass_hevc(64, 48, vec![1; 30], hevc(false)).await.unwrap(), "dropped for a keyframe");
        assert!(sink.pass_hevc(64, 48, vec![2; 900], hevc(true)).await.unwrap());
        assert!(sink.pass_hevc(64, 48, vec![3; 40], hevc(false)).await.unwrap());
        // A repaint while it passes encodes nothing here.
        sink.reset_render();
        sink.frame().await.unwrap();
        assert_eq!(sink.due_at().await, None);
        assert!(sink.pass_hevc(64, 48, vec![4; 800], hevc(true)).await.unwrap());
        sink.flush().await;
        let out = drain(&mut frame_rx, 5).await;
        assert!(is_hevc(&out[0]), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe && unit.data.len() == 900));
        assert!(matches!(&out[2], ServerMsg::Video(unit) if !unit.keyframe && unit.data.len() == 40));
        assert!(is_hevc(&out[3]), "a repaint announces again: {:?}", out[3]);
        assert!(matches!(&out[4], ServerMsg::Video(unit) if unit.keyframe && unit.data.len() == 800));
        assert!(frame_rx.try_recv().is_err(), "nothing encoded here while the stream passed");

        // The source switches back to rectangles: VP9 again, from a keyframe,
        // although its encoder was built before.
        sink.damage(rect, &[9; 64 * 48 * 3]).await.unwrap();
        assert!(!sink.passing());
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 2).await;
        assert!(is_vp9(&out[0]), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe));

        // And the passed stream back, from an IDR, announced for the decoder VP9 displaced.
        assert!(!sink.pass_hevc(64, 48, vec![5; 30], hevc(false)).await.unwrap());
        assert!(sink.pass_hevc(64, 48, vec![6; 700], hevc(true)).await.unwrap());
        sink.flush().await;
        let out = drain(&mut frame_rx, 2).await;
        assert!(is_hevc(&out[0]), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe && unit.data.len() == 700));
        assert!(frame_rx.try_recv().is_err());
    }

    /// Only a 4:4:4 stream — the one the announcement describes — is passed.
    #[tokio::test]
    async fn a_passed_frame_of_another_profile_is_refused() {
        let (sink, _frame_rx) = video_sink(1280, 800).await;
        let mut frame = passed_frame(true, 10);
        frame[0] = 0x80; // profile 0
        let error = sink.pass(1280, 800, frame).await.unwrap_err();
        assert!(format!("{error:#}").contains("profile 0, not the 4:4:4"), "{error:#}");
    }

    /// Past the ceiling, a source that holds sends nothing of the picture: its
    /// rectangles are dropped, no stream is built, and nothing waits to go out.
    #[tokio::test]
    async fn an_oversize_desktop_holds_the_picture() {
        let (sink, mut frame_rx) = holding_sink(5376, 2288).await;
        assert!(sink.oversized());

        for (seed, area) in [rect(5000, 2000, 7, 5), rect(3, 4, 64, 2)].iter().enumerate() {
            sink.damage(*area, &rgb(area.w(), area.h(), seed as u8)).await.unwrap();
        }
        sink.frame().await.unwrap();
        sink.flush().await;

        assert!(frame_rx.try_recv().is_err(), "an oversize desktop sent a picture");
        assert!(sink.due_at().await.is_none(), "an oversize desktop is owed a frame");
    }

    /// Within the ceiling a source that holds is video like any other, and a source
    /// that refuses past the ceiling fails as it always has.
    #[tokio::test]
    async fn only_a_desktop_video_cannot_carry_is_held() {
        let (sink, mut frame_rx) = holding_sink(1280, 800).await;
        assert!(!sink.oversized());
        let area = rect(0, 0, 64, 64);
        sink.damage(area, &rgb(64, 64, 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;

        let (sink, _frame_rx) = video_sink(5376, 2288).await;
        assert!(!sink.oversized());
        let error = sink.damage(area, &rgb(64, 64, 1)).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("will not encode a 5376x2288 picture"),
            "unexpected refusal: {error:#}"
        );
    }

    /// A desktop that shrinks back under the ceiling is video again, and its stream
    /// starts where a decoder can: an announcement and a keyframe. What came while it
    /// was past the ceiling is never sent.
    #[tokio::test]
    async fn crossing_the_ceiling_holds_and_resumes_at_the_resize() {
        let (sink, mut frame_rx) = holding_sink(1280, 800).await;
        let area = rect(0, 0, 64, 64);
        sink.damage(area, &rgb(64, 64, 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_units(&mut frame_rx, 1).await;

        sink.msg(ServerMsg::Resize { w: 5376, h: 2288, scale: UNSCALED }).await.unwrap();
        sink.damage(area, &rgb(64, 64, 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.msg(ServerMsg::Resize { w: 1280, h: 800, scale: UNSCALED }).await.unwrap();
        assert!(!sink.oversized());
        sink.damage(area, &rgb(64, 64, 3)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 6).await;
        assert!(matches!(out[0], ServerMsg::Resize { w: 5376, .. }));
        assert!(matches!(out[1], ServerMsg::Oversize { cause: Some(HoldCause::Size) }));
        assert!(matches!(out[2], ServerMsg::Resize { w: 1280, .. }), "a held desktop sent a picture");
        assert!(matches!(out[3], ServerMsg::Oversize { cause: None }));
        assert!(matches!(out[4], ServerMsg::VideoFormat { .. }), "video came back unannounced");
        assert!(matches!(&out[5], ServerMsg::Video(unit) if unit.keyframe));
    }

    /// Too many screens hold a desktop well within the ceiling, and say so as their
    /// own cause; a desktop the engine says is back to few enough is video again.
    #[tokio::test]
    async fn too_many_screens_hold_a_desktop_of_any_size() {
        let (sink, mut frame_rx) = holding_sink(1280, 800).await;
        sink.hold_screens(true);
        sink.msg(ServerMsg::Resize { w: 3840, h: 800, scale: UNSCALED }).await.unwrap();
        assert!(sink.oversized());
        let area = rect(0, 0, 64, 64);
        sink.damage(area, &rgb(64, 64, 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.hold_screens(false);
        sink.msg(ServerMsg::Resize { w: 1280, h: 800, scale: UNSCALED }).await.unwrap();
        sink.damage(area, &rgb(64, 64, 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 6).await;
        assert!(matches!(out[0], ServerMsg::Resize { w: 3840, .. }));
        assert!(matches!(out[1], ServerMsg::Oversize { cause: Some(HoldCause::Screens) }));
        assert!(matches!(out[2], ServerMsg::Resize { w: 1280, .. }), "a held desktop sent a picture");
        assert!(matches!(out[3], ServerMsg::Oversize { cause: None }));
        assert!(matches!(out[4], ServerMsg::VideoFormat { .. }));
        assert!(matches!(&out[5], ServerMsg::Video(unit) if unit.keyframe));
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

    /// The VP9 encoded here is each page's own to decode, so its formats never say the
    /// session's pages decode it, even in a session whose pages decode a Mac's HEVC.
    #[tokio::test]
    async fn the_vp9_encoded_here_is_never_the_sessions_to_decode() {
        let plan = RenderPlan { apple_media: true, software: true, ..VIDEO };
        let (frame_tx, mut frame_rx) = mpsc::channel(64);
        let sink = VideoSink::new("test", frame_tx, plan, feedback(), Oversize::Refuse);
        sink.msg(ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED }).await.unwrap();
        let area = rect(0, 0, 320, 64);
        for round in 1..=2 {
            sink.reset_render();
            sink.damage(area, &rgb(area.w(), area.h(), round)).await.unwrap();
            sink.frame().await.unwrap();
            sink.flush().await;
        }
        let formats: Vec<_> = std::iter::from_fn(|| frame_rx.try_recv().ok())
            .filter_map(|msg| match msg {
                ServerMsg::VideoFormat { decode, software, .. } => Some((decode.starts_with("vp09.01."), software)),
                _ => None,
            })
            .collect();
        assert_eq!(formats, [(true, false), (true, false)]);
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
        let ServerMsg::VideoFormat { decode, passthrough: false, .. } = &out[0] else {
            panic!("the first thing a stream sends must be its format, encoded here, got {:?}", out[0]);
        };
        assert!(decode.starts_with("vp09.01."), "not a VP9 profile-1 configuration: {decode}");
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

        // A repaint does. This is the reattach: `reset_render` is what
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
    /// reattach or a resize, and every one of those is a client sitting in
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
        video.congestion.stays_at(quality);
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
        sink.frame().await.unwrap();
        sink.flush().await;
        let units = drain_units(&mut frame_rx, 1).await;
        assert!(!units[0].keyframe, "a settle is an inter frame, not a keyframe");
        assert_eq!(sink.shared.video.lock().await.stream.quality(), 20, "the settle moved the dial the link had walked");

        // Settled at the dial, so there is nothing more to come back for.
        tokio::time::sleep(SETTLE_IDLE * 4).await;
        assert!(sink.due_at().await.is_none(), "a settled stream was settled again");
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "a settled stream kept sending");
    }

    /// A round back at the dial over the very blocks a coarse round encoded leaves
    /// nothing coarse, and a stream that then goes quiet is not settled: the settle
    /// is the whole picture, for a client that already holds it sharp.
    #[tokio::test(start_paused = true)]
    async fn a_coarse_band_encoded_again_at_the_dial_owes_no_settle() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        coarse_round(&sink, &mut frame_rx, 1).await;
        coarsen(&sink, 20).await;
        coarse_round(&sink, &mut frame_rx, 2).await;
        coarsen(&sink, 60).await;
        coarse_round(&sink, &mut frame_rx, 3).await;

        tokio::time::sleep(SETTLE_IDLE * 4).await;
        assert!(sink.due_at().await.is_none(), "a picture sharp everywhere was owed a settle");
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "a picture sharp everywhere was settled");
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
            video.congestion.stays_at(60);
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
        let plan = RenderPlan { quality: 60, adaptive: true, apple_media: false, rdp_graphics: false, rdp_h264: false, software: false };
        let sink = VideoSink::new("test", frame_tx, plan, Arc::clone(&link), Oversize::Refuse);
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
        assert!(sink.due_at().await.is_some(), "the settle marked nothing to encode");
        assert_eq!(sink.shared.video.lock().await.stream.quality(), 20, "the settle moved the dial the link had walked");
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
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), Oversize::Refuse);

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
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), Oversize::Refuse);
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

    /// dial.
    #[test]
    fn an_adaptive_plan_makes_the_walk_lag_aware() {
        let plan = RenderPlan { quality: 60, adaptive: true, apple_media: false, rdp_graphics: false, rdp_h264: false, software: false };
        let shared = Shared::new(plan, feedback(), Oversize::Refuse);
        let video = shared.video.try_lock().expect("nothing else holds the stream");
        assert!(video.congestion.lag_aware(), "the walk ignores lag");
        assert_eq!(video.congestion.quality(), 60, "the walk starts on the dial");
    }
}
