//! Ordered picture output outside the RDP and VNC protocol-read loops.
//!
//! ```text
//! read loop:  pack → Shadow::accept → VideoSink::damage() → mirror, tiles owed
//!             VideoSink::frame() ── take the round ──┐
//!                                                    │ spawn_blocking(code)
//!             VideoSink::msg()  ── ServerMsg ────────┤ handle pushed, in order
//!                                                    ▼ mpsc, cap ENCODE_DEPTH
//!                                order task: await handles FIFO → frame_tx
//!                                            ⤷ settle tick → still JPEG tiles owed exact
//! ```
//!
//! FIFO collection keeps control messages in source order with the tiles around
//! them, so a resize cannot overtake the round before it. A full queue
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
use crate::protocol::{GraphicsUnit, Held, HoldCause, Painted, ServerMsg, StandardUnit, TileUnit, VideoUnit};
use crate::shadow::Rect;
use crate::tiles::{Coded, Format, LEVELS, Round, Tiles};
use crate::video;

/// Maximum queued items ahead of the one currently collected: rounds, which are
/// serial with each other, and the control messages between them.
const ENCODE_DEPTH: usize = 16;

/// Bytes allowed between an engine and the browser's socket.
///
/// Every queue on that path is bounded by *messages* — [`ENCODE_DEPTH`] here,
/// [`crate::session::FRAME_BUFFER`] twice in series behind it — and a message
/// is whatever a payload happened to come to. On a link that slows down those
/// counts are no bound on what matters, which is how old the newest pixel is by
/// the time it is drawn: measured against a throttled link with a busy desktop,
/// the queues held 30 MB, and at 4 Mbit/s that is a picture 63 s behind its
/// desktop. Input still reaches the remote; what it did arrives a minute later,
/// which reads as a session that has stopped responding until a fresh engine
/// throws the queues away.
///
/// So a payload's size comes out of this budget before it is queued, and the share
/// rides inside it ([`Held`]) until it is dropped anywhere on the way or its batch
/// leaves: written to the socket, for a client that is keeping up, and *received*
/// by it, for one that is behind — where a written batch is only backlog that has
/// moved into the kernel's send buffer (`ws.rs` decides which, and says why both).
/// With the budget spent the *engine* waits, and stops reading its remote — the
/// backpressure the counts were meant to be, arriving while the backlog is still
/// short. The order task never waits on it: everything queued behind the order task
/// holds a share, so a wait there could be for room only it can free.
///
/// A round's size is not known until it is coded, so the engine takes an estimate —
/// the size of the last round — and the order task settles it ([`Held::settle`]).
/// An estimate that was short is over-committed rather than waited for, which
/// bounds the error at [`ENCODE_DEPTH`] rounds and corrects itself within as many.
///
/// Two full batches ([`crate::wire`] caps one at 256 KiB): one being written and
/// one ready behind it, so a fast link never waits on the coder for want of room,
/// and a slow one is never more than this far behind.
const QUEUE_BUDGET: u32 = 512 * 1024;

/// The fewest bytes of a picture's rectangles, a Mac's own or the tiles coded here,
/// that may be sent and not yet painted by the browser, however slowly the browser
/// paints ([`PaintWindow`]).
///
/// A Mac decides what to send by its own connection: the refinement after a full
/// update waits for the motion when the connection is slow ("What a Mac sends a
/// viewer that lists them" in docs/apple-vnc-889.md), and its zlib is sent no faster
/// than the connection takes it. The tiles coded here are sent as the browser
/// paints them the same way, the damage in between merging in the mirror. The queue
/// budget cannot show either a slow browser: a burst of rectangles passes the paint
/// window together and waits in the kernel's send buffer, and against a 300 kbit/s
/// browser a Mac's picture ran 5.7 s behind, the Mac refining throughout. Held to
/// what the browser has painted, the picture waits for the browser instead.
///
/// The size of the receive buffer an RDP host's passed pipeline is held behind,
/// which the connection to the Mac has too.
const AHEAD: u64 = 32 * 1024;

/// The most bytes of a picture's rectangles that may be sent and not yet painted,
/// however fast the browser paints.
const AHEAD_MOST: u64 = 16 * 1024 * 1024;

/// How many times what the browser paints in its shortest round trip a picture's
/// rectangles may be ahead of its painting, between [`AHEAD`] and [`AHEAD_MOST`].
///
/// A fixed bound held a fast browser to one rectangle a paint round trip: a
/// Full-quality rectangle is larger than [`AHEAD`] alone, and a lossless Combined
/// Display of a 1x and a 2x screen went to a page 39 ms away at 1.6 MB/s while the
/// Mac would send 20. Twice what a round trip carries lets each round carry twice
/// the last, until the link or the painting is what holds it; then the rate stops
/// growing, and the bound holds about one round trip's painting queued, where a
/// bound in time would queue that long behind a page whose painting is what is
/// slow.
const AHEAD_GAIN: f64 = 2.0;

/// How long a sample of the browser's painting rate counts towards the bound: the
/// fastest within it is the rate, so a still screen's few small rectangles do not
/// narrow it while the screen moves.
const RATE_SPAN: Duration = Duration::from_secs(1);

/// The largest rectangle whose round trip counts towards the shortest: one small
/// enough that its own time on the link is not the trip. Twice what even a
/// 300 kbit/s link carries in that time is under [`AHEAD`], so a slow link's bound
/// stays at its floor; a large one's trip, a session's opening full update on such a
/// link taking seconds, would widen it by as much.
const TRIP_BYTES: u64 = 4 * 1024;

/// How long a round of changed tiles may take the link at the plan's quality before
/// it goes coarse ([`crate::tiles::Round::coarse`]), and at each coarse level before
/// it goes coarser, at what the link has been seen to carry
/// ([`PaintWindow::carries`]). A few tiles of typing stay sharp on any link a desktop
/// is usable over, and a whole new screen goes coarse below about 50 Mbit/s, where it
/// would otherwise arrive row by row for a quarter of a second or more.
const COARSE_SPAN: Duration = Duration::from_millis(250);

/// The fewest bytes a painting's sample of what the link carries must span to count
/// ([`PaintWindow::carries`]). A smaller one's time is the browser's painting, not
/// the link: a tile of a few hundred bytes takes a whole animation frame on any
/// link, and would read a LAN as a slow one. Half [`AHEAD`], because a slow link's
/// window holds no more than that in flight, so a larger floor never sees one.
const CARRY_BYTES: u64 = AHEAD / 2;

/// How long a sample of what the link carries counts: the most within it is the
/// rate. Longer than [`RATE_SPAN`], because only large paintings sample it, and a
/// second of typing would otherwise forget a LAN.
const CARRY_SPAN: Duration = Duration::from_secs(10);

/// How often the order task looks for tiles sent as JPEG that have gone still, to be
/// owed their exact picture ([`Tiles::tick`]): a tile is still once a whole tick has
/// gone by without it changing, so it is sent exactly 50–100 ms after it stopped.
/// TigerVNC's server refreshes what it sent lossy on the same 50 ms. It has to be its
/// own timer rather than something the next round does, because a screen that stops
/// changing takes no next round — which is exactly the case a refresh is for.
const SETTLE_TICK: Duration = Duration::from_millis(50);

/// The shortest gap between two rounds.
///
/// The engines' frame boundaries are not a frame *rate*: RDP's is one `outputs`
/// batch, and a busy desktop produces those far faster than anything presents
/// them — 126 a second, measured, against a 60 Hz screen. A round at every one
/// would make the coding cost scale with how *often* the remote reported damage
/// rather than with how much had changed.
///
/// So the boundary proposes and this disposes. Damage between two rounds
/// accumulates in the mirror and rides the next one, the tiles it touched coded once.
const ROUND_INTERVAL: Duration = Duration::from_micros(33_333);

/// The tiles, and when the next round may be taken.
struct Video {
    tiles: Tiles,
    /// The earliest the next round may be taken — see [`ROUND_INTERVAL`]. `None`
    /// before the first one, so a freshly connected desktop paints without waiting
    /// out an interval.
    due_at: Option<tokio::time::Instant>,
}

/// How many bytes of a picture's rectangles may be sent and not yet painted by the
/// browser: [`AHEAD_GAIN`] times what it paints in the shortest round trip of the
/// attachment, at the fastest rate it painted them within [`RATE_SPAN`], and never fewer
/// than [`AHEAD`] or more than [`AHEAD_MOST`]. A rectangle larger than the bound waits
/// for every one before it and holds the next until it is painted itself.
///
/// The shortest round trip is the attachment's, of a rectangle no larger than
/// [`TRIP_BYTES`], not a recent one's: under motion a slow link's queue never
/// empties, and a recent shortest would come to the queue's own length and double the
/// bound at each, the lag the floor keeps out. One that only falls can at worst hold
/// the bound to its floor, and so does none.
///
/// Both are sampled at each rectangle's painting: its round trip is the time from its
/// sending to its painting, and the rate what the browser painted since the last
/// painting before it was sent, over that time. Counted from that painting rather than
/// from its sending, so the rectangles of a batch painted together do not each read as
/// all of it painted in the trip of the last.
///
/// Every measure is of one attachment's link ([`Self::attach`]): a browser that
/// attaches, as a reload does, starts unmeasured, whatever the last one's link was.
///
/// Pure state with the clock passed in.
#[derive(Debug, Default)]
struct PaintWindow {
    /// The attachment its measures are of: [`LinkFeedback::attachment`].
    attachment: u64,
    /// Bytes of the bound taken by rectangles sent and not yet painted.
    ahead: u64,
    /// Bytes painted so far, which a rectangle's sample counts from.
    painted: u64,
    /// When the last of them was painted.
    painted_at: Option<Instant>,
    /// The recent rates, in bytes a second, with when each was sampled.
    rates: std::collections::VecDeque<(Instant, f64)>,
    /// The shortest round trip so far of a rectangle no larger than [`TRIP_BYTES`].
    shortest: Option<Duration>,
    /// What the link has been seen to carry, in bytes a second, with when each was
    /// sampled, and the latest of them, which outlives [`CARRY_SPAN`]: see
    /// [`Self::carries`].
    carried: std::collections::VecDeque<(Instant, f64)>,
    last_carried: Option<f64>,
}

/// One rectangle's place in a [`PaintWindow`], given back when it is painted.
#[derive(Debug)]
struct PaintTicket {
    /// What it took of the bound.
    taken: u64,
    /// Its own size.
    bytes: u64,
    /// The attachment it was sent to: one sent to an earlier one gives its room back
    /// and measures nothing.
    attachment: u64,
    passed: Instant,
    /// [`PaintWindow::painted`] when it was sent, and when that was painted: its
    /// sending, where nothing had been.
    painted_before: (u64, Instant),
}

impl PaintWindow {
    /// The link is `attachment`'s from here: where it was another's, everything
    /// measured of that one is forgotten, and only the room its rectangles still
    /// hold is kept, for their paintings to give back.
    fn attach(&mut self, attachment: u64) {
        if attachment != self.attachment {
            *self = Self { attachment, ahead: self.ahead, ..Self::default() };
        }
    }

    /// The bound at `now`.
    fn bound(&mut self, now: Instant) -> u64 {
        while self.rates.front().is_some_and(|(at, _)| now.duration_since(*at) > RATE_SPAN) {
            self.rates.pop_front();
        }
        let rate = self.rates.iter().map(|(_, rate)| *rate).fold(0.0, f64::max);
        let trip = self.shortest.unwrap_or_default();
        // In range by the clamp, so the cast cannot truncate anything kept.
        ((AHEAD_GAIN * rate * trip.as_secs_f64()) as u64).clamp(AHEAD, AHEAD_MOST)
    }

    /// A rectangle of `bytes` is to be sent: its ticket where there is room. As much
    /// of the bound as there is at most, so one larger than it goes alone.
    fn take(&mut self, bytes: u64, now: Instant) -> Option<PaintTicket> {
        let bound = self.bound(now);
        let taken = bytes.clamp(1, bound);
        (self.ahead + taken <= bound).then(|| self.ticket(taken, bytes, now))
    }

    /// What the link carries, in bytes a second: the most it was seen to within
    /// [`CARRY_SPAN`], or the latest it was before that, and `None` before it has been
    /// seen at all.
    ///
    /// A sample is what the browser painted from a rectangle's sending to its
    /// painting, over that time less the shortest round trip, the distance, but never
    /// less than half of it, which is close to what the link carried once it spans
    /// [`CARRY_BYTES`].
    fn carries(&mut self, now: Instant) -> Option<f64> {
        while self.carried.front().is_some_and(|(at, _)| now.duration_since(*at) > CARRY_SPAN) {
            self.carried.pop_front();
        }
        self.carried.iter().map(|(_, rate)| *rate).reduce(f64::max).or(self.last_carried)
    }

    /// The level a round of changed tiles that would come to `bytes` at each of the
    /// [`LEVELS`] goes at: the finest the link carries within [`COARSE_SPAN`], or the
    /// coarsest where it carries none in time. A link not yet seen to carry anything
    /// is taken as a slow one for a round larger than [`AHEAD`] at the plan's quality,
    /// which goes at the first coarse level, so a session's first picture on a slow
    /// link comes coarse and settles, rather than taking seconds to arrive.
    fn level(&mut self, bytes: [u64; LEVELS], now: Instant) -> usize {
        match self.carries(now) {
            Some(rate) => bytes.iter().position(|&bytes| bytes as f64 <= rate * COARSE_SPAN.as_secs_f64()).unwrap_or(LEVELS - 1),
            None => usize::from(bytes[0] > AHEAD),
        }
    }

    /// The room left under the bound at `now`, or `None` where there is none: what a
    /// round of tiles may be taken in, a round being taken only while there is some.
    fn room(&mut self, now: Instant) -> Option<u64> {
        let bound = self.bound(now);
        (self.ahead < bound).then(|| bound - self.ahead)
    }

    /// A coded tile of `bytes` is sent, room or none: its round was taken while there
    /// was room, and coded to what it came to. The next round waits until it is painted
    /// back under the bound.
    fn take_over(&mut self, bytes: u64, now: Instant) -> PaintTicket {
        self.ticket(bytes.max(1), bytes, now)
    }

    fn ticket(&mut self, taken: u64, bytes: u64, now: Instant) -> PaintTicket {
        self.ahead += taken;
        PaintTicket { taken, bytes, attachment: self.attachment, passed: now, painted_before: (self.painted, self.painted_at.unwrap_or(now)) }
    }

    /// The rectangle `ticket` was given for is painted.
    fn painted(&mut self, ticket: PaintTicket, now: Instant) {
        self.ahead -= ticket.taken;
        if ticket.attachment != self.attachment {
            // Painted, or let go with its socket, by a browser that is gone.
            return;
        }
        self.painted += ticket.bytes;
        self.painted_at = Some(now);
        let trip = now.duration_since(ticket.passed).max(Duration::from_millis(1));
        let (before, since) = ticket.painted_before;
        let span = now.duration_since(since).max(trip);
        self.rates.push_back((now, (self.painted - before) as f64 / span.as_secs_f64()));
        if ticket.bytes <= TRIP_BYTES {
            self.shortest = Some(self.shortest.map_or(trip, |shortest| shortest.min(trip)));
        }
        let carried = self.painted - before;
        if carried >= CARRY_BYTES {
            // At most half the trip: a shortest trip measured behind a queue, as an
            // attachment's first rectangles are, would otherwise leave next to no
            // time and read any link as a fast one.
            let distance = self.shortest.unwrap_or_default().min(trip / 2);
            let rate = carried as f64 / trip.saturating_sub(distance).max(Duration::from_millis(1)).as_secs_f64();
            self.carried.push_back((now, rate));
            self.last_carried = Some(rate);
        }
    }
}

/// A [`PaintWindow`] the sink waits on and a painting gives room back to, of the
/// attachment `link` says the link is now.
#[derive(Debug)]
struct PaintAhead {
    window: Mutex<PaintWindow>,
    room: Notify,
    link: Arc<LinkFeedback>,
}

impl PaintAhead {
    fn new(link: Arc<LinkFeedback>) -> Self {
        Self { window: Mutex::default(), room: Notify::new(), link }
    }

    /// The window, of the attachment the link is now.
    fn window(&self) -> std::sync::MutexGuard<'_, PaintWindow> {
        let mut window = self.window.lock().unwrap();
        window.attach(self.link.attachment());
        window
    }

    async fn acquire(&self, bytes: u64) -> PaintTicket {
        loop {
            // Enabled before the look, so a painting between the two is not missed.
            let room = self.room.notified();
            tokio::pin!(room);
            room.as_mut().enable();
            if let Some(ticket) = self.window().take(bytes, Instant::now()) {
                return ticket;
            }
            room.await;
        }
    }

    /// A ticket for a rectangle of a round already taken: see
    /// [`PaintWindow::take_over`].
    fn take_over(&self, bytes: u64) -> PaintTicket {
        self.window().take_over(bytes, Instant::now())
    }

    /// The room for a round of tiles now: see [`PaintWindow::room`].
    fn room(&self) -> Option<u64> {
        self.window().room(Instant::now())
    }

    /// The level a round of changed tiles of `bytes` at each level goes at now: see
    /// [`PaintWindow::level`].
    fn level(&self, bytes: [u64; LEVELS]) -> usize {
        self.window().level(bytes, Instant::now())
    }

    /// Never waits: a [`Painted`] runs it wherever it is dropped.
    fn release(&self, ticket: PaintTicket) {
        self.window().painted(ticket, Instant::now());
        self.room.notify_waiters();
    }
}

/// What a source's desktop past the picture ceiling ([`video::within_ceiling`])
/// comes to.
///
/// The ceiling is a passed video stream's ([`video::MAX_LONG_SIDE`]). A source
/// whose picture is one has none past it: the gateway sizes the remote under the
/// ceiling and ends the session on a remote that answers past it, or, where it
/// cannot size the remote, holds the session open and says so, for the browser to
/// offer a smaller desktop. A source whose picture is no stream has no ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Oversize {
    /// The session ends: the gateway sized the remote, and a remote that answers
    /// past the ceiling has refused what it was asked. A High Performance Mac, an
    /// RDP host with its graphics pipeline, and a `wlshare` target started with
    /// resize.
    Refuse,
    /// The picture stops until a `Resize` brings the desktop within the ceiling,
    /// which only the remote can do: [`VideoSink::damage`] drops what it is handed
    /// while [`VideoSink::oversized`] says so, and the browser is told
    /// ([`ServerMsg::Oversize`]). A `wlshare` target on a physical output, whose VP9
    /// comes off the encoding list past the ceiling.
    Hold,
    /// The desktop is shown at whatever size the remote has: the tiles coded here
    /// of a plain VNC server's desktop or an RDP host's bitmap path, and a
    /// Standard Mac's own rectangles, passed one by one. No picture of these is a
    /// stream of the whole desktop, so none has the stream's ceiling, and the
    /// browser is never told of a hold.
    Shown,
}

/// One item in the ordered queue.
enum Pending {
    /// One round in flight: its tiles coded on a blocking worker, yielding the round
    /// itself (to be brought back), what it coded, and the microseconds it cost.
    ///
    /// Rounds stay serial with each other — [`Tiles::take_round`] refuses a second
    /// while one is here — so a tile is marked by what was sent of it before the next
    /// round looks at it, but the engine's read loop does not wait the coding out: the
    /// mirror takes its blits meanwhile, and the order task brings the round back
    /// when it lands.
    ///
    /// With the share of [`QUEUE_BUDGET`] taken for it, at the last round's size.
    Round(JoinHandle<(Round, anyhow::Result<Vec<Coded>>, u64)>, Held),
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
    /// Whether the desktop's picture is held — see [`Oversize`]. Decided by each
    /// `Resize` ([`VideoSink::msg`]).
    oversized: AtomicBool,
    /// Why the order task gave up, so the engine's next push can report it rather
    /// than a bare closed channel. The error itself, so the cause chain survives
    /// the hop from the task to the push. See [`VideoSink::closed`].
    failure: Mutex<Option<anyhow::Error>>,
    /// A `tokio` mutex rather than a `std` one because several of its critical
    /// sections span awaits. It is *not* held across the coding: a round owns the
    /// pixels it took outright, the mirror takes the blits meanwhile, and "one round
    /// at a time, in order" is [`Tiles::round_out`]'s guarantee.
    video: tokio::sync::Mutex<Video>,
    /// Signalled when a round may now be taken that could not be before: a round came
    /// back with tiles still owed, the browser painted tiles back under the paint
    /// window, or tiles went still and are owed their exact picture. An engine parked
    /// on a clean `due_at` finds out from it. See [`VideoSink::round_returned`].
    round_returned: Arc<Notify>,
    /// Whether the picture is the remote's own, passed through ([`VideoSink::pass`])
    /// rather than coded here. Set by the first frame passed, cleared when the desktop
    /// goes past the ceiling and by a rectangle damaged while it is set, which takes
    /// the picture back to the tiles coded here.
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
    /// Set by [`VideoSink::reset_render`], consumed by [`VideoSink::frame`], which
    /// owes the browser every tile from the picture's start. An atomic rather than a
    /// field on [`Video`] so that resetting stays synchronous: its call sites are
    /// already awaiting other things, and none of them should have to wait out a round
    /// to say "the client needs to start again".
    restart_owed: AtomicBool,
    /// The link as the attached browser's paint window measures it — see
    /// [`crate::feedback`]: how long a passed stream's fence is held.
    feedback: Arc<LinkFeedback>,
    /// The plan's chroma, which a passed wlshare frame is held to ([`crate::vp9::pass`]).
    chroma: crate::vp9::Chroma,
    /// Units of a remote's stream passed, and a Mac's own rectangles, and their bytes.
    units: AtomicU64,
    passed_bytes: AtomicU64,
    /// Of the passed units, those a decoder could start from, and what they cost.
    keyframes: AtomicU64,
    keyframe_bytes: AtomicU64,
    /// Runs of a graphics pipeline passed ([`VideoSink::pass_graphics`]), and their
    /// bytes.
    graphics: AtomicU64,
    graphics_bytes: AtomicU64,
    /// Rounds of tiles sent, and of them the refreshes: tiles owed their exact picture,
    /// and the coarse ones ([`crate::tiles::Round::coarse`]).
    rounds: AtomicU64,
    refreshes: AtomicU64,
    coarse: AtomicU64,
    /// Rectangles of tiles sent as JPEG and as PNG, and their bytes.
    jpeg: AtomicU64,
    jpeg_bytes: AtomicU64,
    png: AtomicU64,
    png_bytes: AtomicU64,
    code_micros: AtomicU64,
    /// Wall time the order task spent waiting for rounds to be coded.
    waited_micros: AtomicU64,
    /// Time the engine spent blocked pushing into a full queue.
    stalled_micros: AtomicU64,
    /// The bound on bytes queued towards the browser — see [`QUEUE_BUDGET`].
    budget: Arc<Semaphore>,
    /// The bound on a picture's rectangles sent and not yet painted — see
    /// [`PaintWindow`].
    ahead: Arc<PaintAhead>,
    /// What the last round came to, which the next is taken at.
    round_bytes: AtomicU64,
}

impl Shared {
    fn new(plan: RenderPlan, feedback: Arc<LinkFeedback>, oversize: Oversize) -> Self {
        let RenderPlan { quality, chroma, .. } = plan;
        Self {
            oversize,
            oversized: AtomicBool::new(false),
            failure: Mutex::default(),
            video: tokio::sync::Mutex::new(Video { tiles: Tiles::new(quality), due_at: None }),
            round_returned: Arc::new(Notify::new()),
            passing: AtomicBool::new(false),
            pass_restart: AtomicBool::new(true),
            pass_announced: Mutex::default(),
            uncover_owed: AtomicBool::new(false),
            restart_owed: AtomicBool::new(false),
            feedback: Arc::clone(&feedback),
            chroma,
            units: AtomicU64::new(0),
            passed_bytes: AtomicU64::new(0),
            keyframes: AtomicU64::new(0),
            keyframe_bytes: AtomicU64::new(0),
            graphics: AtomicU64::new(0),
            graphics_bytes: AtomicU64::new(0),
            rounds: AtomicU64::new(0),
            refreshes: AtomicU64::new(0),
            coarse: AtomicU64::new(0),
            jpeg: AtomicU64::new(0),
            jpeg_bytes: AtomicU64::new(0),
            png: AtomicU64::new(0),
            png_bytes: AtomicU64::new(0),
            code_micros: AtomicU64::new(0),
            waited_micros: AtomicU64::new(0),
            stalled_micros: AtomicU64::new(0),
            budget: Arc::new(Semaphore::new(QUEUE_BUDGET as usize)),
            ahead: Arc::new(PaintAhead::new(feedback)),
            round_bytes: AtomicU64::new(0),
        }
    }
}

/// The engine's handle on the picture.
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
    /// Start the picture for one engine. `engine` prefixes its log lines. `plan` is
    /// the resolved render plan ([`crate::config::TargetConfig::render_plan`]), whose
    /// quality the tiles' JPEG is coded at. `feedback` is the session's link
    /// measurement ([`crate::feedback`]). `oversize` says what a desktop past the
    /// picture ceiling comes to.
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

    /// Copy one changed rectangle of packed RGB888 into the mirror, and owe the
    /// browser the tiles it touches.
    ///
    /// Nothing is sent until the engine says a frame has ended ([`Self::frame`]):
    /// a rectangle is only a part of the next round.
    ///
    /// While [`Self::oversized`] the rectangle is dropped: there is no picture to put
    /// it in, and the resize that ends the hold is repainted in full.
    ///
    /// On a source that refuses a desktop past the ceiling ([`Oversize::Refuse`]),
    /// this is where the refusal surfaces: learning the size ([`Self::msg`]) must not
    /// fail, and every caller of this is on the engine's `?` path, which ends the
    /// session with the message attached.
    ///
    /// A rectangle while a passed stream is the picture ([`Self::passing`]) is the gap
    /// after it, on a source whose rectangles come back — a `wlshare` target's server
    /// between its VP9 frames, across an output switch: they carry the picture again,
    /// as the tiles coded here, from the picture's start. The passed stream starts over
    /// at a keyframe when it comes back. A Mac's media stream has no such gap: its
    /// rectangles never reach here ([`crate::vnc::DesktopState::media_stream`]).
    pub async fn damage(&self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        if self.oversized() {
            return Ok(());
        }
        if self.shared.passing.swap(false, Ordering::Relaxed) {
            debug!("{}: the source's rectangles carry the picture again, as tiles coded here", self.engine);
            self.reset_render();
        }
        let mut video = self.shared.video.lock().await;
        if self.shared.oversize == Oversize::Refuse
            && let Some(size) = video.tiles.size()
        {
            video::check_picture(size)?;
        }
        video.tiles.blit(rect, rgb)
    }

    /// Whether the desktop's picture is held ([`Oversize::Hold`]): past the picture
    /// ceiling. Then [`Self::damage`] and [`Self::frame`] do nothing, and a frame the
    /// remote coded itself is not to be passed either. Changes only with a `Resize`
    /// through [`Self::msg`].
    pub fn oversized(&self) -> bool {
        self.shared.oversized.load(Ordering::Relaxed)
    }

    /// One remote frame has ended: take a round of the tiles [`Self::damage`] has owed
    /// since the last, and queue it.
    ///
    /// It has to be driven by the engines because neither protocol hands its damage
    /// over a frame at a time — `damage` is called once per *rectangle*, and RDP's
    /// loop turns once per PDU, most of which redraw nothing. So this is a no-op when
    /// nothing is owed: without that, a still screen would take a round per PDU.
    ///
    /// The round's tiles are coded on a blocking worker, and its place in the order
    /// queued: the read loop keeps blitting into the mirror while the worker codes, and
    /// the order task brings the round back when it lands. Rounds stay serial with each
    /// other ([`Tiles::round_out`]).
    ///
    /// **Calling it is a proposal, not an instruction.** At most one round is taken per
    /// [`ROUND_INTERVAL`], and only while the browser has painted what it was sent back
    /// under the paint window ([`PaintWindow::room`]); a call outside either leaves the
    /// tiles owed and returns. So an engine may call this as often as its boundaries
    /// occur, but must also call it when [`Self::due_at`] or [`Self::round_returned`]
    /// says to — see there for why that second half is not optional.
    pub async fn frame(&self) -> anyhow::Result<()> {
        if self.oversized() {
            return Ok(());
        }
        // The browser is decoding the passed stream: a tile drawn now would be under
        // its next picture. What the mirror holds waits for the gap [`Self::damage`]
        // opens, which starts the tiles over from the picture's start.
        if self.passing() {
            return Ok(());
        }
        let mut video = self.shared.video.lock().await;
        if video.tiles.round_out() {
            // The previous round is still being coded, and it *is* this call's answer:
            // its return re-arms the engine (`Self::round_returned`), and taking
            // anything now — even the restart — would act on tiles whose marks are away
            // with it.
            return Ok(());
        }
        if self.shared.restart_owed.swap(false, Ordering::Relaxed) {
            video.tiles.reset();
        }
        let now = tokio::time::Instant::now();
        // A picture starting over is never held back: it is for a repaint, a reattach
        // or a resize, and every one of those is a client sitting in front of nothing
        // until it arrives.
        if !video.tiles.restarting() && video.due_at.is_some_and(|due| now < due) {
            // Deliberately before the round is taken: the tiles stay owed and the next
            // round carries them. Something must come back for them, which is what
            // `due_at` tells the engines.
            return Ok(());
        }
        // The browser's painting paces the rounds: one is taken while what the browser
        // was sent is painted back under the paint window, and waits otherwise, the
        // damage meanwhile merging in the mirror. A painting that makes room wakes the
        // engine.
        let Some(room) = self.shared.ahead.room() else {
            return Ok(());
        };
        // A round the link would take too long to carry goes coarse, and settles.
        let level = self.shared.ahead.level(video.tiles.changed_bytes());
        let Some(round) = video.tiles.take_round(room, level) else {
            return Ok(());
        };
        video.due_at = Some(now + ROUND_INTERVAL);
        // Dropped before the spawn and the push: the whole point is that `damage` gets
        // the lock back while the worker codes.
        drop(video);

        let handle = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let coded = round.code();
            (round, coded, micros(started))
        });
        let round_bytes = usize::try_from(self.shared.round_bytes.load(Ordering::Relaxed)).unwrap_or(usize::MAX);
        let held = self.hold(round_bytes).await;
        self.push(Pending::Round(handle, held)).await
    }

    /// Completes when a round may be taken that could not be before — the engines'
    /// third wake-up source, beside their own frame boundaries and [`Self::due_at`]:
    /// a round came back with tiles still owed, the browser painted tiles back under
    /// the paint window, or tiles went still and are owed their exact picture.
    ///
    /// Needed because `due_at` says nothing while a round is out or the browser has
    /// not painted what it was sent: an engine that went idle then would park on it
    /// and never hear that it may take a round now. A permit is stored if nobody is
    /// waiting, so the signal cannot be lost to timing; a spurious wake-up costs one
    /// no-op [`Self::frame`].
    pub async fn round_returned(&self) {
        self.shared.round_returned.notified().await;
    }

    /// When the engine must call [`Self::frame`] again, whatever its own boundaries
    /// are doing — or `None` when there is nothing it could take now.
    ///
    /// **This is the correctness half of pacing, not a convenience.**
    /// [`crate::shadow::Shadow::accept`] records source pixels as delivered to the
    /// client the moment `damage` blits them into the mirror, and nothing re-sends
    /// them. A deferred round that is never followed by another is therefore
    /// permanently wrong pixels — and "the motion stopped right after one" is the
    /// ordinary case rather than a corner: a video paused, a pointer come to rest, a
    /// window settled. The deadline is what comes back for them.
    ///
    /// `None` while nothing is owed, so an idle desktop parks on
    /// [`Self::round_returned`] instead of waking an engine to take nothing; while a
    /// round is out, or the browser has not painted what it was sent, whose ending
    /// signals that; and while a passed stream is the picture, when [`Self::frame`]
    /// takes nothing.
    pub async fn due_at(&self) -> Option<tokio::time::Instant> {
        if self.passing() {
            return None;
        }
        let video = self.shared.video.lock().await;
        let restart = self.shared.restart_owed.load(Ordering::Relaxed) && !video.tiles.round_out();
        if !(video.tiles.owes() || restart) || self.shared.ahead.room().is_none() {
            return None;
        }
        // Owed tiles with no deadline yet arrived before any round went out; they
        // are owed one now rather than in an interval, and so is a restart.
        let now = tokio::time::Instant::now();
        if restart {
            return Some(now);
        }
        Some(video.due_at.unwrap_or(now))
    }

    /// Start the picture over for a client that has to be able to draw from here.
    ///
    /// For a resize, where the picture size has changed, and for a repaint, where a
    /// client may hold nothing of what was sent before. Its call sites are exactly the
    /// moments a client has to be given the whole picture, which is why every tile is
    /// owed again here, and a passed stream's keyframe.
    pub fn reset_render(&self) {
        self.shared.restart_owed.store(true, Ordering::Relaxed);
        self.restart_pass();
    }

    /// Start only the passed stream over, at its next keyframe: for a unit the engine
    /// dropped, which the ones after it predict from. The tiles coded here are not
    /// touched, since the browser's picture is not in question.
    pub fn restart_pass(&self) {
        self.shared.pass_restart.store(true, Ordering::Relaxed);
    }

    /// Queue a `w`×`h` frame wlshare encoded itself as the next access unit, untouched
    /// ([`crate::vp9::pass`]).
    ///
    /// None of the tiles' machinery applies. There is no mirror, round, interval or
    /// refresh: the remote paces, codes and sharpens its stream itself, and learns how
    /// the browser is keeping up from the fences the engine echoes once
    /// [`Self::fence_hold`] says so. What is shared is the queue: the frame takes its
    /// size out of [`QUEUE_BUDGET`] like a round, and goes out in order with the
    /// messages around it.
    ///
    /// A browser that must start over ([`Self::reset_render`]) is sent nothing until a
    /// keyframe, which the full update the engine asks for at the same moment brings,
    /// and the keyframe goes out behind a fresh announcement.
    pub async fn pass(&self, w: u16, h: u16, frame: Vec<u8>) -> anyhow::Result<()> {
        let passed = crate::vp9::pass(w, h, &frame, self.shared.chroma)?;
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
    /// the Mac's rectangles are never coded on a session with a media stream, so one
    /// that passes the stream codes no tile at all. The stream coming back after a gap
    /// starts over at an IDR, announced again.
    pub async fn pass_media(
        &self,
        w: u16,
        h: u16,
        frame: Vec<u8>,
        passed: crate::vp9::Passed,
    ) -> anyhow::Result<bool> {
        video::check_picture((w, h))?;
        self.forward(w, h, frame, passed).await
    }

    /// Queue a checked, passed unit: see [`Self::pass`]. `false` when it was dropped
    /// while the browser waits for a keyframe.
    async fn forward(&self, w: u16, h: u16, frame: Vec<u8>, passed: crate::vp9::Passed) -> anyhow::Result<bool> {
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
        self.shared.passed_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        if passed.keyframe {
            self.shared.keyframes.fetch_add(1, Ordering::Relaxed);
            self.shared.keyframe_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        }
        if let Some(decode) = announce {
            self.push(Pending::Msg(ServerMsg::VideoFormat { decode, strips, engine: 0 })).await?;
        }
        let unit = VideoUnit { w, h, strip: passed.strip, keyframe: passed.keyframe, data: frame, held };
        self.push(Pending::Msg(ServerMsg::Video(unit))).await?;
        self.uncover_behind().await?;
        Ok(true)
    }

    /// Bring the browser's notice that the screen is not available down behind the
    /// next picture sent, coded here or passed: a
    /// `ScreenUnavailable { active: false }` follows it on the channel, so the notice
    /// never lifts on a canvas with nothing new on it. For an engine whose picture is
    /// a stream that has just delivered its first picture of a display
    /// ([`crate::vnc::DesktopState::canvas_live`]): the engine cannot tell when that
    /// picture is queued, since [`Self::frame`] defers it while a round is out or the
    /// interval has not passed, and a passed unit may be dropped for a keyframe.
    pub fn uncover(&self) {
        self.shared.uncover_owed.store(true, Ordering::Relaxed);
    }

    /// The stream stopped before the notice [`Self::uncover`] owes came down: it
    /// stays up for the next stream's first unit.
    pub fn cover(&self) {
        self.shared.uncover_owed.store(false, Ordering::Relaxed);
    }

    /// The notice [`Self::uncover`] owes, behind a passed unit just queued. A round's
    /// follows its tiles, in the order task.
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
    /// A session that passes the pipeline codes nothing, before it or beside it:
    /// its RDP client decodes no bitmap update.
    pub async fn graphics_start(&self) -> anyhow::Result<()> {
        self.shared.passing.store(true, Ordering::Relaxed);
        self.push(Pending::Msg(ServerMsg::GraphicsStart { engine: 0 })).await
    }

    /// Queue a message of an RDP host's graphics pipeline as the next record,
    /// untouched: as the host sent it, in its bulk compression, for the browser to
    /// unwrap and compose ([`crate::rdp_client::Event::Graphics`]).
    ///
    /// As [`Self::pass`] in what the queue does — the run takes its size out of
    /// [`QUEUE_BUDGET`] and goes out in order with the messages around it — and
    /// unlike it in having no restart: nothing in a pipeline is a keyframe, and a
    /// run is never dropped for one. A browser that needs the picture from the
    /// start is given a session that starts.
    ///
    /// `frame` is the acknowledgement the host is owed for the last frame the
    /// message ends, if it ends one, said once the browser has painted it. That, and not the
    /// queue, is what paces the host: no frame can be dropped between it and the
    /// page, so a page that composes more slowly than the host draws has to be what
    /// the host hears from. The budget is what is left for a browser that says
    /// nothing at all.
    pub async fn pass_graphics(&self, wrapped: Vec<u8>, frame: Option<Painted>) -> anyhow::Result<()> {
        self.shared.passing.store(true, Ordering::Relaxed);
        let bytes = wrapped.len();
        let held = self.hold(bytes).await;
        self.shared.graphics.fetch_add(1, Ordering::Relaxed);
        self.shared.graphics_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.push(Pending::Msg(ServerMsg::Graphics(GraphicsUnit { data: wrapped, frame, held }))).await
    }

    /// A Mac's Adaptive picture starts here, from nothing, and is the picture from
    /// now on ([`Self::pass_standard`]): said ahead of the Mac's first rectangle.
    pub async fn standard_start(&self) -> anyhow::Result<()> {
        self.shared.passing.store(true, Ordering::Relaxed);
        self.push(Pending::Msg(ServerMsg::StandardStart { engine: 0 })).await
    }

    /// Queue one of a Mac's rectangles in its Adaptive picture as the next record,
    /// untouched: where it is and its body as the Mac sent it, for the page to
    /// decode.
    ///
    /// As [`Self::pass_graphics`] in what the queue does, and in having no restart:
    /// each rectangle is drawn on what the ones before it left, so none is ever
    /// dropped, and a browser that needs the picture from the start is given a
    /// session that starts.
    ///
    /// What paces the Mac is the browser's painting: a rectangle waits here while
    /// as much of the ones before it as the browser paints in
    /// twice its shortest round trip is passed and not yet painted ([`PaintWindow`]),
    /// and while it waits the engine reads nothing more of the Mac's connection,
    /// which backs up, so the Mac sends as few updates as the browser's link takes.
    pub async fn pass_standard(&self, x: u16, y: u16, w: u16, h: u16, data: Vec<u8>) -> anyhow::Result<()> {
        self.shared.passing.store(true, Ordering::Relaxed);
        let bytes = data.len();
        let started = Instant::now();
        let ticket = self.shared.ahead.acquire(bytes as u64).await;
        self.shared.stalled_micros.fetch_add(micros(started), Ordering::Relaxed);
        let ahead = Arc::clone(&self.shared.ahead);
        let painted = Painted::new(move || ahead.release(ticket));
        let held = self.hold(bytes).await;
        self.shared.feedback.handed(tokio::time::Instant::now());
        self.shared.units.fetch_add(1, Ordering::Relaxed);
        self.shared.passed_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        let unit = StandardUnit { x, y, w, h, data, held, painted };
        self.push(Pending::Msg(ServerMsg::Standard(unit))).await
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

    /// The browser's link's distance: its smallest recent ping round trip
    /// ([`crate::feedback::LinkFeedback::distance`]), or `None` before the first
    /// pong.
    pub fn distance(&self) -> Option<Duration> {
        self.shared.feedback.distance()
    }

    /// Take `bytes` of [`QUEUE_BUDGET`], waiting for the browser's socket to make
    /// room. Counted as the engine stalling, which it is.
    async fn hold(&self, bytes: usize) -> Held {
        let started = Instant::now();
        let held = Held::take(&self.shared.budget, bytes, QUEUE_BUDGET).await;
        self.shared.stalled_micros.fetch_add(micros(started), Ordering::Relaxed);
        held
    }

    /// Queue anything that is not pixels, keeping it behind the rounds it follows.
    ///
    /// A resize is also how the tiles learn how big the desktop is. That is one
    /// interception rather than a size threaded through every place the tiles have to
    /// be rebuilt, and it cannot be forgotten by a place added later: telling the
    /// client its framebuffer changed and telling the tiles are the same event, and
    /// the first already happens everywhere the second must.
    ///
    /// Bookkeeping only — nothing here can fail, deliberately. This method's error is
    /// read by every caller as "the browser has gone", answered by returning without
    /// a word (`rdp::run`, `vnc::run`), so a desktop the tiles cannot hold would end
    /// the session silently on the picker. Building the mirror waits for
    /// [`Self::damage`], which is on the engines' `?` path and ends the session with
    /// the message attached.
    ///
    /// A resize is also where the picture is held or resumed, for a source that holds
    /// it ([`Oversize::Hold`]): past the ceiling there is none, and otherwise tiles.
    /// Back from a hold, the remote repaints the resized desktop in full as after any
    /// resize, and the picture starts over as a browser that attached would see it.
    pub async fn msg(&self, msg: ServerMsg) -> anyhow::Result<()> {
        if let ServerMsg::Resize { w, h, .. } = &msg {
            let (w, h) = (*w, *h);
            let within = video::within_ceiling((u32::from(w), u32::from(h)));
            let cause = match self.shared.oversize {
                Oversize::Refuse | Oversize::Shown => None,
                Oversize::Hold if !within => Some(HoldCause::Size),
                Oversize::Hold => None,
            };
            let oversized = cause.is_some();
            if self.shared.oversized.swap(oversized, Ordering::Relaxed) != oversized {
                if oversized {
                    self.shared.passing.store(false, Ordering::Relaxed);
                    info!(
                        "{}: a {w}x{h} desktop is past the picture ceiling; the picture is \
                         held until the remote sends another",
                        self.engine
                    );
                } else {
                    info!("{}: a {w}x{h} desktop is within the ceiling, and its picture shown again", self.engine);
                    self.reset_render();
                }
            }
            self.shared.video.lock().await.tiles.want(w, h);
            if self.shared.oversize == Oversize::Hold {
                self.push(Pending::Msg(msg)).await?;
                return self.push(Pending::Msg(ServerMsg::Oversize { cause })).await;
            }
        }
        self.push(Pending::Msg(msg)).await
    }

    /// Tell the tiles the desktop is `w`×`h` without putting a `Resize` on the
    /// channel, for an engine test that reads the channel and never sent one.
    #[cfg(test)]
    pub(crate) fn presize(&self, w: u16, h: u16) {
        self.shared.video.try_lock().expect("a fresh sink").tiles.want(w, h);
    }

    /// Shut the picture down: deliver what is still in flight, then log what it cost.
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

    /// Log what this engine's picture cost. Private: [`Self::finish`] is the only
    /// caller, so it cannot be run before the flush that completes the numbers.
    ///
    /// Explicit rather than emitted by the order task on its way out, for the reason
    /// [`Shared`] gives. Silent when an engine sent nothing, since a line of zeroes
    /// says nothing.
    fn report(&self) {
        let totals = Totals::of(&self.shared);
        if totals.units > 0 || totals.graphics > 0 || totals.rounds > 0 {
            info!("{}: picture totals: {totals}", self.engine);
        }
    }

    /// The error a push reports once the order task has stopped.
    ///
    /// A coding failure lands in the order task, so it is recorded there and
    /// surfaces here on the next push — which is what stops the shadow from
    /// believing the client holds pixels that were never sent.
    fn closed(&self) -> anyhow::Error {
        match self.shared.failure.lock().unwrap().take() {
            Some(error) => error,
            None => anyhow::anyhow!("frame channel closed"),
        }
    }
}

/// One settle tick: tiles sent as JPEG that have gone still are owed their exact
/// picture, and the engine is woken to take the round that sends it.
async fn settle_tiles(shared: &Shared) {
    if shared.video.lock().await.tiles.tick() {
        shared.round_returned.notify_one();
    }
}

/// Collect finished rounds in push order and forward their tiles, and owe tiles that
/// went still their exact picture.
///
/// The settle timer belongs here rather than anywhere the rounds arrive, because the
/// case it exists for is a screen that has stopped producing them.
async fn order_loop(
    engine: &'static str,
    mut rx: mpsc::Receiver<Pending>,
    frame_tx: mpsc::Sender<ServerMsg>,
    shared: Arc<Shared>,
) {
    let mut settle = tokio::time::interval(SETTLE_TICK);
    // Delay rather than Burst: a tick missed while the queue was busy is a tick
    // whose refresh is still owed, and firing the backlog at once would only mark
    // stillness that a burst of ticks did not measure.
    settle.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let item = tokio::select! {
            item = rx.recv() => match item {
                Some(item) => item,
                None => break,
            },
            _ = settle.tick() => {
                settle_tiles(&shared).await;
                continue;
            }
        };
        let (handle, held) = match item {
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
        // already coded when its turn comes adds coding time and no waiting — the
        // read loop overlapped it.
        let started = Instant::now();
        let joined = handle.await;
        shared.waited_micros.fetch_add(micros(started), Ordering::Relaxed);
        let (round, coded, code_micros) = match joined {
            Ok(finished) => finished,
            // Only reachable by cancellation — `panic = "abort"` in release means a
            // panicking worker never gets this far.
            Err(e) => {
                give_up(engine, &shared, anyhow::Error::new(e).context("the tile coder stopped"));
                break;
            }
        };
        shared.code_micros.fetch_add(code_micros, Ordering::Relaxed);
        let coded = match coded {
            Ok(coded) => coded,
            // Not recoverable: the shadow already counts the tiles' pixels as
            // delivered, so a round that could not be coded is pixels the client
            // never gets.
            Err(e) => {
                give_up(engine, &shared, e.context("coding a round of tiles"));
                break;
            }
        };
        let owed = {
            let mut video = shared.video.lock().await;
            video.tiles.put_back(&round, &coded);
            video.tiles.owes() || shared.restart_owed.load(Ordering::Relaxed)
        };
        // Damage may have landed while the round was out, and the engine may be
        // parked on a `due_at` that said nothing while it was: this is what tells it
        // tiles are owed again.
        if owed {
            shared.round_returned.notify_one();
        }
        if !send_round(engine, &shared, &frame_tx, &round, coded, held).await {
            break;
        }
    }
}

/// Send what a round coded, in order: the picture's start where the round owes one,
/// then each coded rectangle as a tile with its share of the round's budget and its
/// place in the paint window, then the notice's coming down where it is owed. `false`
/// once the browser has gone.
async fn send_round(
    engine: &'static str,
    shared: &Arc<Shared>,
    frame_tx: &mpsc::Sender<ServerMsg>,
    round: &Round,
    coded: Vec<Coded>,
    mut held: Held,
) -> bool {
    if round.start && frame_tx.send(ServerMsg::TilesStart { engine: 0 }).await.is_err() {
        return false;
    }
    let bytes: usize = coded.iter().map(|part| part.data.len()).sum();
    shared.round_bytes.store(bytes as u64, Ordering::Relaxed);
    held.settle(bytes);
    shared.rounds.fetch_add(1, Ordering::Relaxed);
    if round.exact() {
        shared.refreshes.fetch_add(1, Ordering::Relaxed);
    }
    if round.coarse() {
        shared.coarse.fetch_add(1, Ordering::Relaxed);
    }
    debug!(
        "{engine}: {} {} tile(s) in {} rectangle(s), {bytes} bytes",
        if round.exact() {
            "refreshed".to_owned()
        } else if round.coarse() {
            format!("sent coarse at q{}", round.quality())
        } else {
            "sent".to_owned()
        },
        round.tiles(),
        coded.len()
    );
    for part in coded {
        let len = part.data.len();
        let (count, total) = match part.format {
            Format::Jpeg => (&shared.jpeg, &shared.jpeg_bytes),
            Format::Png => (&shared.png, &shared.png_bytes),
        };
        count.fetch_add(1, Ordering::Relaxed);
        total.fetch_add(len as u64, Ordering::Relaxed);
        let ticket = shared.ahead.take_over(len as u64);
        let (ahead, wake) = (Arc::clone(&shared.ahead), Arc::clone(&shared.round_returned));
        // A painting that brings the window back under its bound is what lets the
        // next round be taken, so it wakes the engine too.
        let painted = Painted::new(move || {
            ahead.release(ticket);
            wake.notify_one();
        });
        let unit = TileUnit { x: part.x, y: part.y, w: part.w, h: part.h, format: part.format, data: part.data, held: held.split(len), painted };
        if frame_tx.send(ServerMsg::Tile(unit)).await.is_err() {
            return false;
        }
    }
    // Here and not where the round was queued: the notice comes down on a canvas
    // with the round's tiles on it.
    !(shared.uncover_owed.swap(false, Ordering::Relaxed)
        && frame_tx.send(ServerMsg::ScreenUnavailable { active: false }).await.is_err())
}

/// Record why the queue stopped, for the engine's next push to report.
fn give_up(engine: &str, shared: &Shared, error: anyhow::Error) {
    warn!("{engine}: {error:#}");
    *shared.failure.lock().unwrap() = Some(error);
}

fn micros(since: Instant) -> u64 {
    since.elapsed().as_micros() as u64
}

/// A snapshot of what one engine's picture cost, for [`VideoSink::report`].
///
/// The repo has no benchmark harness, so — like `wire::Totals` for the browser
/// link — this line is the only measurement of the picture that exists in
/// production. Each number earns its place by answering something the others
/// cannot:
///
/// - `passed` and `keyframe` are read together for a passed stream: keyframe bytes
///   against all of them say how much of the stream the remote spent starting over.
/// - `rounds`, `refreshes`, `coarse`, `jpeg` and `png` are the tiles: how many rounds
///   went, how many of them sent still tiles exactly and how many changed tiles
///   coarse, and what the rectangles came to in each format.
/// - `coding` against `waiting` says whether the rounds overlapped the read loop:
///   `waiting` accrues only while the order task finds a round *unfinished*.
/// - `stalled` is what the read loop still pays, waiting on the queue or the budget.
/// - the bytes cross-check against the `ws: outbound totals` line.
struct Totals {
    units: u64,
    passed_bytes: u64,
    keyframes: u64,
    keyframe_bytes: u64,
    graphics: u64,
    graphics_bytes: u64,
    rounds: u64,
    refreshes: u64,
    coarse: u64,
    jpeg: u64,
    jpeg_bytes: u64,
    png: u64,
    png_bytes: u64,
    code_micros: u64,
    waited_micros: u64,
    stalled_micros: u64,
}

impl Totals {
    fn of(shared: &Shared) -> Self {
        // Relaxed throughout, and read while the order task may still be running:
        // this is a log line, not a decision, and the counters only ever grow.
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Self {
            units: load(&shared.units),
            passed_bytes: load(&shared.passed_bytes),
            keyframes: load(&shared.keyframes),
            keyframe_bytes: load(&shared.keyframe_bytes),
            graphics: load(&shared.graphics),
            graphics_bytes: load(&shared.graphics_bytes),
            rounds: load(&shared.rounds),
            refreshes: load(&shared.refreshes),
            coarse: load(&shared.coarse),
            jpeg: load(&shared.jpeg),
            jpeg_bytes: load(&shared.jpeg_bytes),
            png: load(&shared.png),
            png_bytes: load(&shared.png_bytes),
            code_micros: load(&shared.code_micros),
            waited_micros: load(&shared.waited_micros),
            stalled_micros: load(&shared.stalled_micros),
        }
    }
}

impl fmt::Display for Totals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} passed unit(s) / {} bytes, {} keyframe(s) / {} bytes, \
             {} graphics run(s) / {} bytes passed, \
             {} round(s) of tiles, {} of them refreshes and {} coarse: {} JPEG / {} bytes and {} PNG / {} bytes, \
             {}µs coding in {}µs of waiting, engine stalled {}µs",
            self.units,
            self.passed_bytes,
            self.keyframes,
            self.keyframe_bytes,
            self.graphics,
            self.graphics_bytes,
            self.rounds,
            self.refreshes,
            self.coarse,
            self.jpeg,
            self.jpeg_bytes,
            self.png,
            self.png_bytes,
            self.code_micros,
            self.waited_micros,
            self.stalled_micros
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::UNSCALED;

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

    /// `w`×`h` of noise, more colours than a palette holds, so its tiles go as JPEG.
    fn noise(w: u16, h: u16, salt: u32) -> Vec<u8> {
        let mut seed = salt;
        (0..usize::from(w) * usize::from(h) * 3)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 16) as u8
            })
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

    /// Take the next round's tiles, stepping over the starts among them, up to the
    /// next message that is neither: what one round sent.
    async fn drain_tiles(rx: &mut mpsc::Receiver<ServerMsg>) -> Vec<TileUnit> {
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(ServerMsg::TilesStart { .. })) => {}
                Ok(Some(ServerMsg::Tile(unit))) => out.push(unit),
                Ok(Some(other)) => panic!("expected tiles, got {other:?}"),
                Ok(None) => panic!("frame channel closed early"),
                Err(_) => panic!("no tiles came"),
            }
            if !out.is_empty() && rx.is_empty() {
                return out;
            }
        }
    }

    const VIDEO: RenderPlan = RenderPlan { quality: 60, adaptive: false, apple_media: false, apple_h264: false, apple_standard: None, apple_combined: false, rdp_graphics: false, rdp_h264: false, wlshare: false, chroma: crate::vp9::Chroma::Yuv420 };

    /// A `wlshare` target's plan for a browser that decodes 4:4:4, whose stream wlshare
    /// passes at that chroma.
    const PASSED: RenderPlan = RenderPlan { chroma: crate::vp9::Chroma::Yuv444, ..VIDEO };

    /// A sink that has been told how big the desktop is, which is the one thing it
    /// needs before it will accept any pixels.
    async fn video_sink(w: u16, h: u16) -> (VideoSink, mpsc::Receiver<ServerMsg>) {
        video_sink_at(w, h, VIDEO).await
    }

    /// [`video_sink`] at `plan`.
    async fn video_sink_at(w: u16, h: u16, plan: RenderPlan) -> (VideoSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = VideoSink::new("test", frame_tx, plan, feedback(), Oversize::Refuse);
        sink.msg(ServerMsg::Resize { w, h, scale: UNSCALED }).await.unwrap();
        sink.flush().await;
        // The resize itself, so a test can count what follows.
        assert!(matches!(frame_rx.recv().await, Some(ServerMsg::Resize { .. })));
        (sink, frame_rx)
    }

    /// Have `sink`'s browser seen to paint as fast as a link carries anything, so no
    /// round goes coarse: for a test of something else.
    fn on_a_fast_link(sink: &VideoSink) {
        sink.shared.ahead.window.lock().unwrap().last_carried = Some(f64::MAX);
    }

    /// A sink for a source that holds an oversize desktop, told the desktop is
    /// `w`×`h`.
    async fn holding_sink(w: u16, h: u16) -> (VideoSink, mpsc::Receiver<ServerMsg>) {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
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

    /// A round, from damage to what it sent, waiting out the interval and the paint
    /// window as an engine does: the tiles it sent.
    async fn round(sink: &VideoSink, frame_rx: &mut mpsc::Receiver<ServerMsg>, area: Rect, pixels: &[u8]) -> Vec<TileUnit> {
        sink.damage(area, pixels).await.unwrap();
        if let Some(due) = sink.due_at().await {
            tokio::time::sleep_until(due).await;
        }
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_tiles(frame_rx).await
    }

    /// A passed graphics pipeline is announced where it starts, goes out run for run in
    /// order with the messages around it, and stops the tiles coded here: nothing of
    /// the mirror is sent while it is the picture.
    #[tokio::test]
    async fn a_graphics_pipeline_is_passed_in_order_and_nothing_is_coded_beside_it() {
        let (sink, mut rx) = video_sink(64, 32).await;
        sink.graphics_start().await.unwrap();
        sink.pass_graphics(vec![1; 40], None).await.unwrap();
        sink.msg(ServerMsg::Resize { w: 32, h: 32, scale: UNSCALED }).await.unwrap();
        sink.pass_graphics(vec![2; 9], Some(Painted::default())).await.unwrap();
        assert!(sink.passing());
        assert!(sink.due_at().await.is_none(), "there is nothing to come back and code");
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut rx, 4).await;
        assert!(matches!(out[0], ServerMsg::GraphicsStart { .. }));
        assert!(matches!(&out[1], ServerMsg::Graphics(run) if run.data == vec![1; 40] && run.frame.is_none()));
        assert!(matches!(out[2], ServerMsg::Resize { w: 32, h: 32, .. }));
        assert!(matches!(&out[3], ServerMsg::Graphics(run) if run.data == vec![2; 9] && run.frame.is_some()));
        assert!(rx.try_recv().is_err(), "and no tile beside them");
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

    /// A Mac's Adaptive rectangles go out in order behind the message that starts
    /// them, and a rectangle waits while the ones passed before it, as many as
    /// the [`PaintWindow`] allows, are not yet painted: the engine is held, and with
    /// it the Mac, by the browser's painting.
    #[tokio::test]
    async fn standard_rectangles_wait_for_the_ones_before_them_to_be_painted() {
        let (sink, mut rx) = video_sink(64, 32).await;
        sink.standard_start().await.unwrap();
        sink.pass_standard(0, 0, 64, 16, vec![1; 16 * 1024]).await.unwrap();
        sink.pass_standard(0, 16, 64, 16, vec![2; 16 * 1024]).await.unwrap();
        assert!(sink.passing());
        assert!(sink.due_at().await.is_none(), "there is nothing to come back and code");
        sink.flush().await;
        let out = drain(&mut rx, 3).await;
        assert!(matches!(out[0], ServerMsg::StandardStart { .. }));
        assert!(matches!(&out[1], ServerMsg::Standard(unit) if unit.data[0] == 1 && (unit.x, unit.y) == (0, 0)));
        assert!(matches!(&out[2], ServerMsg::Standard(unit) if unit.data[0] == 2 && (unit.x, unit.y) == (0, 16)));

        let waiting = tokio::time::timeout(Duration::from_millis(50), sink.pass_standard(0, 0, 8, 8, vec![3; 8])).await;
        assert!(waiting.is_err(), "two unpainted rectangles fill the bound, so the next waits");
        let mut out = out.into_iter();
        drop(out.nth(1));
        tokio::time::timeout(Duration::from_secs(1), sink.pass_standard(0, 0, 8, 8, vec![3; 8]))
            .await
            .expect("painting the first made room")
            .unwrap();

        // One larger than the widest bound waits for everything before it, and
        // holds the next until it is painted itself.
        drop(out);
        sink.flush().await;
        let small = drain(&mut rx, 1).await;
        drop(small);
        sink.pass_standard(0, 0, 64, 32, vec![4; AHEAD_MOST as usize + 1]).await.unwrap();
        sink.flush().await;
        let large = drain(&mut rx, 1).await;
        let waiting = tokio::time::timeout(Duration::from_millis(50), sink.pass_standard(0, 0, 8, 8, vec![5; 8])).await;
        assert!(waiting.is_err());
        drop(large);
        tokio::time::timeout(Duration::from_secs(1), sink.pass_standard(0, 0, 8, 8, vec![5; 8]))
            .await
            .expect("painting the large one made room")
            .unwrap();
    }

    /// The bound follows how fast the browser paints: no wider than the floor for
    /// a slow one, wider each round for a fast one up to the ceiling, and back to
    /// the floor once nothing has been painted for a while.
    #[test]
    fn the_paint_window_follows_the_rate_the_browser_paints_at() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut window = PaintWindow::default();
        assert_eq!(window.bound(at(0)), AHEAD);

        // A large rectangle's trip is mostly its own time on the link, and says
        // nothing of how far away the page is: the floor holds.
        let ticket = window.take(64 * 1024, at(0)).unwrap();
        window.painted(ticket, at(20));
        assert_eq!(window.bound(at(20)), AHEAD);

        // A rectangle as large as the bound waits for the one before it.
        let first = window.take(4 * 1024, at(500)).unwrap();
        assert!(window.take(AHEAD, at(500)).is_none());
        window.painted(first, at(510));
        // A page 10 ms away that paints all it is given: each round carries the
        // whole bound, and the next is twice that, up to the ceiling.
        let mut bound = window.bound(at(510));
        for round in 0..20 {
            let now = at(510 + round * 10);
            let ticket = window.take(bound, now).expect("room for the whole bound");
            window.painted(ticket, now + Duration::from_millis(10));
            let wider = window.bound(now + Duration::from_millis(10));
            assert_eq!(wider, (bound * 2).min(AHEAD_MOST), "round {round}");
            bound = wider;
        }
        assert_eq!(bound, AHEAD_MOST);
        // A larger one than the ceiling takes all of it, alone.
        let huge = window.take(AHEAD_MOST * 2, at(800)).unwrap();
        assert!(window.take(1, at(800)).is_none());
        window.painted(huge, at(900));

        // A second with nothing painted, and the floor again.
        assert_eq!(window.bound(at(2000)), AHEAD);
    }

    /// A round of tiles is taken while there is room under the bound, and what it
    /// coded is sent whatever it came to, holding the next round until it is painted
    /// back under.
    #[test]
    fn a_round_of_tiles_is_taken_in_the_room_left_and_sent_whole() {
        let start = Instant::now();
        let mut window = PaintWindow::default();
        assert_eq!(window.room(start), Some(AHEAD));
        let over = window.take_over(3 * AHEAD, start);
        assert_eq!(window.room(start), None, "a round past the bound leaves no room");
        window.painted(over, start + Duration::from_millis(5));
        assert!(window.room(start + Duration::from_millis(5)).is_some());
    }

    /// What the link carries is read from paintings that span enough of it, the most
    /// of them lately or the latest before that; a round goes at the finest level the
    /// link carries within the span, and the coarsest where it carries none, and on a
    /// link not yet seen to carry anything at the first coarse level where it is
    /// larger than the floor.
    #[test]
    fn a_round_goes_as_coarse_as_the_link_needs_to_carry_it_in_time() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        // A round of `fine` bytes at the plan's quality, at the levels' guessed ratios.
        let round = |fine: u64| [fine, fine / 5, fine * 12 / 100, fine / 12];
        let mut window = PaintWindow::default();
        assert!(window.carries(at(0)).is_none());
        assert_eq!(window.level(round(AHEAD), at(0)), 0);
        assert_eq!(window.level(round(AHEAD + 1), at(0)), 1, "an unmeasured link is a slow one");
        assert_eq!(window.level(round(16 * 1024 * 1024), at(0)), 1, "an unmeasured link went past the baseline");

        // A tile of a few hundred bytes taking a whole animation frame says nothing.
        let ticket = window.take(300, at(0)).unwrap();
        window.painted(ticket, at(16));
        assert!(window.carries(at(16)).is_none());

        // 1 Mbit/s: 128 KiB in about a second.
        let ticket = window.take_over(128 * 1024, at(100));
        window.painted(ticket, at(1150));
        let slow = window.carries(at(1150)).expect("measured");
        assert!((120_000.0..130_000.0).contains(&slow), "{slow}");
        assert_eq!(window.level(round(24 * 1024), at(1150)), 0, "a few tiles of typing went coarse");
        assert_eq!(window.level(round(128 * 1024), at(1150)), 1, "a new screen on a slow link stayed fine");
        assert_eq!(window.level(round(256 * 1024), at(1150)), 2, "a large one stayed at the baseline");
        assert_eq!(window.level(round(2 * 1024 * 1024), at(1150)), 3, "a whole desktop stayed short of the coarsest");

        // A LAN: 256 KiB in a frame, which the slow sample does not outweigh.
        let ticket = window.take_over(256 * 1024, at(2000));
        window.painted(ticket, at(2016));
        assert_eq!(window.level(round(2 * 1024 * 1024), at(2016)), 0, "a whole screen went coarse on a LAN");

        // Long past both, the latest still stands.
        assert_eq!(window.carries(at(60_000)), window.last_carried);
        assert_eq!(window.level(round(2 * 1024 * 1024), at(60_000)), 0);
    }

    /// A slow link's window holds no more than its floor in flight, and a painting of
    /// that much, less the distance, still reads the link: a whole screen goes past
    /// the baseline on it, where an unmeasured link holds it there.
    #[test]
    fn a_slow_links_window_still_reads_what_it_carries() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let screen = [2_000_000, 400_000, 240_000, 166_000];
        let mut window = PaintWindow::default();
        let ticket = window.take(300, at(0)).unwrap();
        window.painted(ticket, at(60));
        assert_eq!(window.level(screen, at(60)), 1);
        // 300 kbit/s: 24 KiB in about 0.65 s, and the 60 ms of distance.
        let ticket = window.take(24 * 1024, at(100)).expect("room under the floor");
        window.painted(ticket, at(815));
        let slow = window.carries(at(815)).expect("measured");
        assert!((36_000.0..40_000.0).contains(&slow), "{slow}");
        assert_eq!(window.level(screen, at(815)), 3);
    }

    /// A browser that attaches starts unmeasured, whatever the last one's link was: a
    /// LAN's measures are forgotten, and a rectangle sent to the last one gives its
    /// room back when it is let go and measures nothing of the new one.
    #[test]
    fn a_new_attachment_forgets_the_last_ones_link() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let screen = [2_000_000, 400_000, 240_000, 166_000];
        let mut window = PaintWindow::default();
        let ticket = window.take(300, at(0)).unwrap();
        window.painted(ticket, at(5));
        let ticket = window.take_over(256 * 1024, at(10));
        window.painted(ticket, at(26));
        assert_eq!(window.level(screen, at(26)), 0, "a LAN");
        let gone = window.take_over(512 * 1024, at(30));

        window.attach(1);
        assert!(window.carries(at(40)).is_none() && window.shortest.is_none(), "the LAN's measures outlived it");
        assert_eq!(window.level(screen, at(40)), 1, "a reattached browser went at the last one's link");
        window.painted(gone, at(41));
        assert_eq!(window.ahead, 0, "a rectangle of the last attachment kept its room");
        assert!(window.carries(at(41)).is_none(), "a rectangle of the last attachment measured the new one");
        window.attach(1);
        assert_eq!(window.attachment, 1);

        // The new attachment's first rectangle waits behind the repaint the old one
        // was owed, 450 ms on its 1 Mbit/s link, and is its shortest trip: a sample of
        // as long reads the link at twice what it painted in it, not as a LAN.
        let first = window.take(300, at(100)).unwrap();
        let queued = window.take(24 * 1024, at(100)).unwrap();
        window.painted(first, at(550));
        window.painted(queued, at(550));
        let carried = window.carries(at(550)).expect("measured");
        assert!(carried < 2.0 * (24 * 1024 + 300) as f64 / 0.45 + 1.0, "{carried}");
        assert_eq!(window.level(screen, at(550)), 3, "a queued start read a slow link as a fast one");
    }

    /// A 300 kbit/s link kept full for a minute: its queue never empties, every
    /// trip after the first is the queue's length, and the bound stays at its
    /// floor throughout.
    #[test]
    fn a_slow_link_kept_full_holds_the_paint_window_at_its_floor() {
        let start = Instant::now();
        let at = |seconds: f64| start + Duration::from_secs_f64(seconds);
        let rate = 300_000.0 / 8.0;
        let mut window = PaintWindow::default();
        // The opening: a full update, then the tables, alone on the link.
        let mut clock = 0.0;
        for bytes in [100 * 1024, 129] {
            let ticket = window.take(bytes, at(clock)).unwrap();
            clock += bytes as f64 / rate;
            window.painted(ticket, at(clock));
        }
        let mut queue = std::collections::VecDeque::new();
        while clock < 60.0 {
            while let Some(ticket) = window.take(8 * 1024, at(clock)) {
                queue.push_back(ticket);
            }
            let ticket = queue.pop_front().expect("the bound holds one");
            clock += ticket.bytes as f64 / rate;
            window.painted(ticket, at(clock));
            assert_eq!(window.bound(at(clock)), AHEAD, "at {clock:.1} s");
        }
    }

    /// The opening byte of a profile 1 VP9 frame — the 4:4:4 [`PASSED`] asks for:
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
        let (sink, mut frame_rx) = video_sink_at(1280, 800, PASSED).await;
        assert!(!sink.passing());
        sink.pass(1280, 800, passed_frame(false, 40)).await.unwrap();
        sink.pass(1280, 800, passed_frame(true, 900)).await.unwrap();
        sink.pass(1280, 800, passed_frame(false, 50)).await.unwrap();
        sink.flush().await;
        assert!(sink.passing());

        let out = drain(&mut frame_rx, 3).await;
        assert!(
            matches!(&out[0], ServerMsg::VideoFormat { decode, .. } if decode == "vp09.01.40.08.03.06.06.06.00"),
            "{:?}",
            out[0]
        );
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe && unit.data == passed_frame(true, 900)));
        assert!(matches!(&out[2], ServerMsg::Video(unit) if !unit.keyframe && unit.data.len() == 50));
        assert!(frame_rx.try_recv().is_err(), "a frame before the first keyframe went out");
    }

    /// A reset — a reattach, a resize — restarts the passed stream: nothing until a
    /// keyframe, which is announced again for the browser that has never seen the
    /// announcement.
    #[tokio::test]
    async fn a_reset_passed_stream_waits_for_a_keyframe_and_announces_it_again() {
        let (sink, mut frame_rx) = video_sink_at(1280, 800, PASSED).await;
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

    /// The notice that the screen is not available comes down behind the next
    /// picture queued, not when the engine asks: nothing owed queues nothing, and the
    /// notice waits with it.
    #[tokio::test(start_paused = true)]
    async fn the_notice_comes_down_behind_the_next_picture() {
        let (sink, mut frame_rx) = video_sink(64, 48).await;
        let area = rect(0, 0, 64, 48);

        sink.uncover();
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "nothing to queue, nothing to follow");

        sink.damage(area, &[7; 64 * 48 * 3]).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 3).await;
        assert!(matches!(&out[0], ServerMsg::TilesStart { .. }), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Tile(unit) if (unit.w, unit.h) == (64, 48)), "{:?}", out[1]);
        assert!(matches!(&out[2], ServerMsg::ScreenUnavailable { active: false }), "{:?}", out[2]);

        // Owed once: the next round brings no second notice.
        drop(out);
        let tiles = round(&sink, &mut frame_rx, area, &[8; 64 * 48 * 3]).await;
        assert_eq!(tiles.len(), 1);
        assert!(frame_rx.try_recv().is_err());

        // A passed unit dropped for a keyframe brings it no sooner than the one sent.
        sink.uncover();
        let hevc = |keyframe| crate::vp9::Passed { decode: "hev1.4.10.L150.BE.8".to_owned(), keyframe, strip: None };
        sink.reset_render();
        assert!(!sink.pass_media(64, 48, vec![1; 30], hevc(false)).await.unwrap());
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "dropped, so nothing follows");
        assert!(sink.pass_media(64, 48, vec![2; 900], hevc(true)).await.unwrap());
        sink.flush().await;
        let out = drain(&mut frame_rx, 3).await;
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe), "{:?}", out[1]);
        assert!(matches!(&out[2], ServerMsg::ScreenUnavailable { active: false }), "{:?}", out[2]);

        // A stream that stopped before its unit went leaves the notice up.
        sink.uncover();
        sink.cover();
        assert!(sink.pass_media(64, 48, vec![3; 900], hevc(true)).await.unwrap());
        sink.flush().await;
        let out = drain(&mut frame_rx, 1).await;
        assert!(matches!(&out[0], ServerMsg::Video(_)), "{:?}", out[0]);
        assert!(frame_rx.try_recv().is_err());
    }

    /// The sink can switch from passed HEVC back to tiles for a source that uses
    /// both, the stream beginning at a keyframe behind its announcement and the tiles
    /// at the picture's start. A High Performance Mac does not use that capability:
    /// its rectangles never reach [`VideoSink::damage`]. A unit dropped for a keyframe
    /// says so to the engine.
    #[tokio::test]
    async fn a_source_can_switch_between_passed_hevc_and_tiles() {
        const HEVC: &str = "hev1.4.10.L150.BE.8";
        let (sink, mut frame_rx) = video_sink(64, 48).await;
        let area = rect(0, 0, 64, 48);
        let hevc = |keyframe| crate::vp9::Passed { decode: HEVC.to_owned(), keyframe, strip: None };
        let is_hevc = |msg: &ServerMsg| matches!(msg, ServerMsg::VideoFormat { decode, .. } if decode == HEVC);

        // Before the passed stream flows: tiles from the rectangles.
        sink.damage(area, &[7; 64 * 48 * 3]).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 2).await;
        assert!(matches!(out[0], ServerMsg::TilesStart { .. }), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Tile(_)));
        drop(out);

        // The passed stream takes over at its IDR, announced.
        assert!(!sink.pass_media(64, 48, vec![1; 30], hevc(false)).await.unwrap(), "dropped for a keyframe");
        assert!(sink.pass_media(64, 48, vec![2; 900], hevc(true)).await.unwrap());
        assert!(sink.pass_media(64, 48, vec![3; 40], hevc(false)).await.unwrap());
        // A repaint while it passes codes nothing here.
        sink.reset_render();
        sink.frame().await.unwrap();
        assert_eq!(sink.due_at().await, None);
        assert!(sink.pass_media(64, 48, vec![4; 800], hevc(true)).await.unwrap());
        sink.flush().await;
        let out = drain(&mut frame_rx, 5).await;
        assert!(is_hevc(&out[0]), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe && unit.data.len() == 900));
        assert!(matches!(&out[2], ServerMsg::Video(unit) if !unit.keyframe && unit.data.len() == 40));
        assert!(is_hevc(&out[3]), "a repaint announces again: {:?}", out[3]);
        assert!(matches!(&out[4], ServerMsg::Video(unit) if unit.keyframe && unit.data.len() == 800));
        assert!(frame_rx.try_recv().is_err(), "nothing coded here while the stream passed");

        // The source switches back to rectangles: tiles again, the whole picture from
        // its start.
        sink.damage(rect(0, 0, 8, 8), &[9; 8 * 8 * 3]).await.unwrap();
        assert!(!sink.passing());
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 2).await;
        assert!(matches!(out[0], ServerMsg::TilesStart { .. }), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Tile(unit) if (unit.w, unit.h) == (64, 48)), "not the whole picture: {:?}", out[1]);
        drop(out);

        // And the passed stream back, from an IDR, announced for the decoder the tiles
        // displaced.
        assert!(!sink.pass_media(64, 48, vec![5; 30], hevc(false)).await.unwrap());
        assert!(sink.pass_media(64, 48, vec![6; 700], hevc(true)).await.unwrap());
        sink.flush().await;
        let out = drain(&mut frame_rx, 2).await;
        assert!(is_hevc(&out[0]), "{:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Video(unit) if unit.keyframe && unit.data.len() == 700));
        assert!(frame_rx.try_recv().is_err());
    }

    /// Only a stream at the plan's chroma — the one the announcement describes — is
    /// passed: a 4:2:0 frame to a session that asked for 4:4:4, and a 4:4:4 one to a
    /// session that asked for 4:2:0, which its browser may not decode, are refused.
    #[tokio::test]
    async fn a_passed_frame_of_another_profile_is_refused() {
        let (sink, _frame_rx) = video_sink_at(1280, 800, PASSED).await;
        let mut frame = passed_frame(true, 10);
        frame[0] = 0x80; // profile 0
        let error = sink.pass(1280, 800, frame).await.unwrap_err();
        assert!(format!("{error:#}").contains("profile 0, not the profile 1 of the 4:4:4"), "{error:#}");
        let (sink, _frame_rx) = video_sink(1280, 800).await;
        let error = sink.pass(1280, 800, passed_frame(true, 10)).await.unwrap_err();
        assert!(format!("{error:#}").contains("profile 1, not the profile 0 of the 4:2:0"), "{error:#}");
    }

    /// Past the ceiling, a source that holds sends nothing of the picture: its
    /// rectangles are dropped, no mirror is built, and nothing waits to go out.
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
        assert!(sink.due_at().await.is_none(), "an oversize desktop is owed a round");
    }

    /// A source with no ceiling is tiles at any size: a 5K desktop is coded like
    /// any other, and the browser is told nothing of a hold.
    #[tokio::test]
    async fn a_source_with_no_ceiling_shows_a_desktop_past_it() {
        let (frame_tx, mut frame_rx) = mpsc::channel(256);
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), Oversize::Shown);
        sink.msg(ServerMsg::Resize { w: 5376, h: 2288, scale: UNSCALED }).await.unwrap();
        assert!(!sink.oversized());
        let area = rect(5312, 2240, 64, 48);
        sink.damage(area, &rgb(64, 48, 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 3).await;
        assert!(matches!(out[0], ServerMsg::Resize { w: 5376, .. }), "{:?}", out[0]);
        assert!(matches!(out[1], ServerMsg::TilesStart { .. }), "a hold was said, or the picture had no start");
        assert!(matches!(&out[2], ServerMsg::Tile(unit) if (unit.x, unit.y) == (5312, 2240)), "{:?}", out[2]);
    }

    /// A Standard Mac's own rectangles are passed whatever the desktop's size: they
    /// are no stream, and have no ceiling.
    #[tokio::test]
    async fn a_standard_picture_past_the_ceiling_is_passed() {
        let (frame_tx, mut rx) = mpsc::channel(256);
        let sink = VideoSink::new("test", frame_tx, VIDEO, feedback(), Oversize::Shown);
        sink.msg(ServerMsg::Resize { w: 5376, h: 2288, scale: UNSCALED }).await.unwrap();
        sink.standard_start().await.unwrap();
        sink.pass_standard(5000, 2000, 64, 16, vec![1; 16]).await.unwrap();
        sink.flush().await;
        let out = drain(&mut rx, 3).await;
        assert!(matches!(out[0], ServerMsg::Resize { .. }));
        assert!(matches!(out[1], ServerMsg::StandardStart { .. }), "{:?}", out[1]);
        assert!(matches!(&out[2], ServerMsg::Standard(unit) if unit.data[0] == 1 && (unit.x, unit.y) == (5000, 2000)));
    }

    /// Within the ceiling a source that holds is tiles like any other, and a source
    /// that refuses past the ceiling fails as it always has.
    #[tokio::test]
    async fn only_a_desktop_past_the_ceiling_is_held() {
        let (sink, mut frame_rx) = holding_sink(1280, 800).await;
        assert!(!sink.oversized());
        let area = rect(0, 0, 64, 64);
        sink.damage(area, &rgb(64, 64, 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_tiles(&mut frame_rx).await;

        let (sink, _frame_rx) = video_sink(5376, 2288).await;
        assert!(!sink.oversized());
        let error = sink.damage(area, &rgb(64, 64, 1)).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("will not show a 5376x2288 picture"),
            "unexpected refusal: {error:#}"
        );
    }

    /// A desktop that shrinks back under the ceiling has a picture again, and it
    /// starts where a client can draw it: its start and its tiles. What came while it
    /// was past the ceiling is never sent.
    #[tokio::test]
    async fn crossing_the_ceiling_holds_and_resumes_at_the_resize() {
        let (sink, mut frame_rx) = holding_sink(1280, 800).await;
        let area = rect(0, 0, 64, 64);
        sink.damage(area, &rgb(64, 64, 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drop(drain_tiles(&mut frame_rx).await);

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
        assert!(matches!(out[4], ServerMsg::TilesStart { .. }), "the picture came back without its start");
        assert!(matches!(&out[5], ServerMsg::Tile(unit) if (unit.x, unit.y) == (0, 0)));
    }

    /// A picture starts behind its `tilesStart`, which is not said again while the
    /// picture goes on, and a repaint says it again ahead of the whole picture —
    /// because a repaint is what a browser that just attached gets, and it holds none
    /// of what was sent before.
    #[tokio::test(start_paused = true)]
    async fn a_picture_starts_behind_its_start_and_a_repaint_starts_it_again() {
        let (sink, mut frame_rx) = video_sink(640, 480).await;
        on_a_fast_link(&sink);
        let area = rect(0, 0, 320, 64);

        sink.damage(area, &rgb(area.w(), area.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 2).await;
        assert!(matches!(out[0], ServerMsg::TilesStart { .. }), "the first thing a picture sends must be its start, got {:?}", out[0]);
        assert!(matches!(&out[1], ServerMsg::Tile(unit) if (unit.x, unit.y, unit.w, unit.h) == (0, 0, 320, 64)));
        drop(out);

        // A second round starts nothing.
        tokio::time::sleep(ROUND_INTERVAL).await;
        sink.damage(area, &rgb(area.w(), area.h(), 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 1).await;
        assert!(matches!(out[0], ServerMsg::Tile(_)), "the start was said again for a picture going on");
        drop(out);

        // A repaint does, inside the interval and with nothing damaged since: this is
        // the reattach, `reset_render` being what `ClientMsg::Refresh` reaches.
        sink.reset_render();
        assert!(sink.due_at().await.is_some(), "a repaint is owed at once");
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 9).await;
        assert!(matches!(out[0], ServerMsg::TilesStart { .. }), "a repaint must start the picture again, got {:?}", out[0]);
        let rows: Vec<_> = out[1..].iter().map(|msg| match msg {
            ServerMsg::Tile(unit) => (unit.y, unit.w),
            other => panic!("expected tiles, got {other:?}"),
        }).collect();
        assert_eq!(rows, (0..8).map(|row| (row * 64, 640)).collect::<Vec<_>>(), "not the whole picture, a row at a time");
    }

    /// A session's first picture on a link not yet measured comes coarse, every tile
    /// of many colours a JPEG, and settles to the exact picture once it is still;
    /// on a link measured fast it comes exact at once.
    #[tokio::test]
    async fn a_first_picture_on_an_unmeasured_link_comes_coarse_and_settles() {
        let (sink, mut frame_rx) = video_sink(256, 128).await;
        let whole = rect(0, 0, 256, 128);
        let pixels = noise(256, 128, 3);
        let first = round(&sink, &mut frame_rx, whole, &pixels).await;
        assert!(first.iter().all(|unit| unit.format == Format::Jpeg));
        assert_eq!(sink.shared.coarse.load(Ordering::Relaxed), 1);
        drop(first);
        // The settle ticks make it exact, nothing coarse among them: as an engine
        // does, every wake-up a frame.
        let mut exact = 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            while exact < 8 {
                sink.round_returned().await;
                sink.frame().await.unwrap();
                sink.flush().await;
                while let Ok(msg) = frame_rx.try_recv() {
                    let ServerMsg::Tile(unit) = msg else { panic!("expected tiles, got {msg:?}") };
                    assert_eq!(unit.format, Format::Png, "a refresh was not exact");
                    exact += usize::from(unit.w / 64);
                }
            }
        })
        .await
        .expect("the coarse picture never settled");
        assert_eq!(sink.shared.coarse.load(Ordering::Relaxed), 1, "a refresh went coarse");

        let (sink, mut frame_rx) = video_sink(256, 128).await;
        on_a_fast_link(&sink);
        let flat = vec![200; 256 * 128 * 3];
        let first = round(&sink, &mut frame_rx, whole, &flat).await;
        assert!(first.iter().all(|unit| unit.format == Format::Png));
        assert_eq!(sink.shared.coarse.load(Ordering::Relaxed), 0);
    }

    /// A reattach, which resets the session's link feedback, takes the sink's paint
    /// window back to unmeasured: a whole picture on a link measured fast comes exact,
    /// and after a reattach comes coarse again, until the new link has been seen.
    #[tokio::test]
    async fn a_reattached_browser_is_not_taken_at_the_last_ones_link() {
        let (sink, mut frame_rx) = video_sink(256, 128).await;
        on_a_fast_link(&sink);
        let whole = rect(0, 0, 256, 128);
        let first = round(&sink, &mut frame_rx, whole, &noise(256, 128, 5)).await;
        assert_eq!(sink.shared.coarse.load(Ordering::Relaxed), 0, "a fast link's picture went coarse");
        drop(first);

        sink.shared.feedback.reset();
        let again = round(&sink, &mut frame_rx, whole, &noise(256, 128, 6)).await;
        assert!(again.iter().all(|unit| unit.format == Format::Jpeg));
        assert_eq!(sink.shared.coarse.load(Ordering::Relaxed), 1, "a reattached browser went at the last one's link");
    }

    /// The wake-up contract both engines rely on now that a round is coded on a
    /// worker: while a round is away, `frame()` is a no-op that consumes nothing — not
    /// even a restart — and the order task signals [`VideoSink::round_returned`] when
    /// the round lands with tiles still owed, which is what re-arms an engine parked on
    /// a clean `due_at`.
    ///
    /// The round is taken and pushed by hand rather than through `frame()`, so it is
    /// *deterministically* in flight for the middle of the test. The timeout is a hang
    /// guard on a broken signal, not an assertion about speed.
    #[tokio::test]
    async fn a_round_in_flight_defers_frame_and_its_return_wakes_the_engine() {
        let (sink, mut frame_rx) = video_sink(64, 64).await;
        sink.damage(rect(0, 0, 64, 64), &rgb(64, 64, 1)).await.unwrap();
        let round = sink.shared.video.lock().await.tiles.take_round(u64::MAX, 0).expect("owed tiles");

        // frame() while the round is out: early return, with the restart left for a
        // call that has the tiles' marks home.
        sink.reset_render();
        sink.frame().await.unwrap();
        assert!(sink.shared.restart_owed.load(Ordering::Relaxed), "the restart was consumed while the round was away");

        // Damage lands while the round is out, and nothing is due for it yet.
        sink.damage(rect(0, 0, 64, 64), &rgb(64, 64, 2)).await.unwrap();
        assert!(sink.due_at().await.is_none(), "due while the round is out");

        // Hand the round to the real order task, the way frame() does.
        let handle = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let coded = round.code();
            (round, coded, micros(started))
        });
        sink.push(Pending::Round(handle, Held::default())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), sink.round_returned())
            .await
            .expect("the order task never signalled the returning round");
        assert!(sink.due_at().await.is_some(), "the wake-up promised tiles no round has carried");

        // The woken engine's next frame() carries them, and the preserved restart makes
        // its round the picture's start.
        sink.frame().await.unwrap();
        sink.flush().await;
        let out = drain(&mut frame_rx, 4).await;
        assert!(matches!(out[0], ServerMsg::TilesStart { .. }));
        assert!(matches!(out[1], ServerMsg::Tile(_)));
        assert!(matches!(out[2], ServerMsg::TilesStart { .. }), "the preserved restart never started the picture again");
        assert!(matches!(out[3], ServerMsg::Tile(_)));
        assert!(frame_rx.try_recv().is_err(), "two rounds sent more than two tiles");
    }

    /// The test for the whole design: `damage` is called once per damage
    /// *rectangle*, and a frame is what the engine says it is. Three rectangles
    /// between two frame boundaries are one round, each tile they touched once.
    #[tokio::test]
    async fn one_round_per_frame_not_per_damage() {
        let (sink, mut frame_rx) = video_sink(640, 480).await;
        for y in [0, 16, 32] {
            let area = rect(0, y, 320, 16);
            sink.damage(area, &rgb(area.w(), area.h(), 3)).await.unwrap();
        }
        sink.frame().await.unwrap();
        sink.flush().await;

        let tiles = drain_tiles(&mut frame_rx).await;
        assert_eq!(tiles.len(), 1, "three rectangles in one row of tiles were more than one rectangle");
        assert_eq!(sink.shared.rounds.load(Ordering::Relaxed), 1);
    }

    /// A tile at the edge of a desktop whose side is not a multiple of a tile is cut
    /// at the desktop's edge.
    #[tokio::test]
    async fn a_tile_at_the_edge_is_cut_there() {
        let (sink, mut frame_rx) = video_sink(1919, 1079).await;
        let area = rect(1900, 1060, 19, 19);
        sink.damage(area, &rgb(area.w(), area.h(), 5)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        let tiles = drain_tiles(&mut frame_rx).await;
        assert_eq!(tiles.iter().map(|unit| (unit.x, unit.y, unit.w, unit.h)).collect::<Vec<_>>(), [(1856, 1024, 63, 55)]);
    }

    /// RDP's loop turns once per PDU and most redraw nothing, so a frame boundary
    /// with nothing behind it has to cost nothing.
    #[tokio::test]
    async fn a_frame_boundary_with_no_damage_sends_nothing() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;

        sink.frame().await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "an untouched framebuffer was sent");

        let area = rect(0, 0, 320, 64);
        sink.damage(area, &rgb(area.w(), area.h(), 9)).await.unwrap();
        sink.frame().await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_tiles(&mut frame_rx).await;
        assert!(frame_rx.try_recv().is_err(), "the same pixels were sent twice");
    }

    /// The pacing, from the engine's side: a boundary is a proposal. Three boundaries
    /// inside one interval are one round, and the two that did not take one must have
    /// left their tiles owed rather than dropped them.
    #[tokio::test(start_paused = true)]
    async fn several_frame_boundaries_inside_one_interval_are_one_round() {
        let (sink, mut frame_rx) = video_sink(640, 480).await;

        for y in [0, 64, 128] {
            let area = rect(0, y, 320, 64);
            sink.damage(area, &rgb(area.w(), area.h(), 3)).await.unwrap();
            sink.frame().await.unwrap();
        }
        sink.flush().await;

        drain_tiles(&mut frame_rx).await;
        assert!(frame_rx.try_recv().is_err(), "the interval did not hold the later boundaries");
        assert!(sink.due_at().await.is_some(), "the deferred blits were dropped rather than held for the next round");
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
        drop(drain_tiles(&mut frame_rx).await);

        sink.damage(area, &rgb(area.w(), area.h(), 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "the second frame was not held");

        // What an engine's flush arm does, and the only thing that happens here.
        let due = sink.due_at().await.expect("pixels the shadow calls delivered are owed");
        tokio::time::sleep_until(due).await;
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_tiles(&mut frame_rx).await;
        assert!(sink.due_at().await.is_none(), "tiles are still owed that nothing will send again");
    }

    /// The browser's painting paces the rounds: one past the paint window holds the
    /// next until it is painted, the damage meanwhile merging in the mirror, and the
    /// painting wakes the engine to take it.
    #[tokio::test]
    async fn a_round_waits_for_the_tiles_before_it_to_be_painted() {
        let (sink, mut frame_rx) = video_sink(640, 192).await;
        on_a_fast_link(&sink);
        let whole = rect(0, 0, 640, 192);
        let first = round(&sink, &mut frame_rx, whole, &noise(640, 192, 1)).await;
        assert!(first.iter().map(|unit| unit.data.len() as u64).sum::<u64>() > AHEAD, "a round smaller than the window's floor");

        // Two changes while the browser has not painted the first round: nothing goes.
        tokio::time::sleep(ROUND_INTERVAL * 2).await;
        for salt in [2, 3] {
            sink.damage(whole, &noise(640, 192, salt)).await.unwrap();
            sink.frame().await.unwrap();
        }
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "a round went ahead of the browser's painting");
        assert!(sink.due_at().await.is_none(), "due while the browser has not painted");

        // Painted: the engine is woken, and takes one round of the latest pixels.
        drop(first);
        tokio::time::timeout(Duration::from_secs(5), sink.round_returned()).await.expect("the painting never woke the engine");
        sink.frame().await.unwrap();
        sink.flush().await;
        let second = drain_tiles(&mut frame_rx).await;
        assert_eq!(sink.shared.rounds.load(Ordering::Relaxed), 2, "the two changes were not one round");
        assert_eq!(second.iter().map(|unit| unit.h).sum::<u16>(), 192);
    }

    /// A tile sent as JPEG is sent again exactly once it is still, woken by the settle
    /// tick the way a returning round wakes the engine, and nothing after that: a still
    /// screen costs nothing once it is exact.
    #[tokio::test(start_paused = true)]
    async fn a_still_jpeg_tile_is_sent_again_exactly_and_then_nothing() {
        let (sink, mut frame_rx) = video_sink(128, 64).await;
        let photo = rect(0, 0, 64, 64);
        let lossy = round(&sink, &mut frame_rx, photo, &noise(64, 64, 1)).await;
        assert_eq!(lossy.iter().map(|unit| unit.format).collect::<Vec<_>>(), [Format::Jpeg]);
        drop(lossy);

        // As an engine does: every wake-up a frame, until one sends something. The
        // painting wakes it before the tick has found the tile still.
        tokio::time::timeout(SETTLE_TICK * 4, async {
            while frame_rx.is_empty() {
                sink.round_returned().await;
                sink.frame().await.unwrap();
                sink.flush().await;
            }
        })
        .await
        .expect("a still JPEG tile was never refreshed");
        let exact = drain_tiles(&mut frame_rx).await;
        assert_eq!(exact.iter().map(|unit| (unit.x, unit.y, unit.w, unit.h, unit.format)).collect::<Vec<_>>(), [(0, 0, 64, 64, Format::Png)]);
        assert_eq!(sink.shared.refreshes.load(Ordering::Relaxed), 1);
        drop(exact);

        tokio::time::sleep(SETTLE_TICK * 4).await;
        assert!(sink.due_at().await.is_none(), "an exact picture was owed again");
        sink.frame().await.unwrap();
        sink.flush().await;
        assert!(frame_rx.try_recv().is_err(), "an exact picture was sent again");
    }

    /// A tile of few colours is exact the first time, and owes nothing when it goes
    /// still.
    #[tokio::test(start_paused = true)]
    async fn an_exact_tile_owes_nothing_when_it_goes_still() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        let area = rect(0, 0, 320, 64);
        let tiles = round(&sink, &mut frame_rx, area, &vec![90; 320 * 64 * 3]).await;
        assert!(tiles.iter().all(|unit| unit.format == Format::Png));
        drop(tiles);
        tokio::time::sleep(SETTLE_TICK * 4).await;
        assert!(sink.due_at().await.is_none(), "an exact tile was owed a refresh");
    }

    /// What the engines park on. `None` has to mean "nothing could be taken now", or a
    /// still target's loop would wake to take nothing.
    #[tokio::test(start_paused = true)]
    async fn nothing_is_due_while_nothing_is_owed() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        assert!(sink.due_at().await.is_none(), "an untouched mirror owes nothing");

        let area = rect(0, 0, 320, 64);
        sink.damage(area, &rgb(area.w(), area.h(), 4)).await.unwrap();
        assert!(sink.due_at().await.is_some(), "blitted pixels are owed a round");

        sink.frame().await.unwrap();
        sink.flush().await;
        drain_tiles(&mut frame_rx).await;
        assert!(sink.due_at().await.is_none(), "the round settled the debt");
    }

    /// A resize is the client reallocating its canvas, so the picture has to start
    /// over: tiles of the new size, behind their start.
    #[tokio::test]
    async fn a_resize_starts_the_picture_again() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        let area = rect(0, 0, 320, 64);
        sink.damage(area, &rgb(area.w(), area.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;
        drain_tiles(&mut frame_rx).await;

        sink.reset_render();
        sink.msg(ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED }).await.unwrap();
        sink.damage(rect(0, 0, 640, 64), &rgb(640, 64, 2)).await.unwrap();
        sink.frame().await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 3).await;
        assert!(matches!(out[0], ServerMsg::Resize { w: 640, .. }), "the resize lost its place");
        assert!(matches!(out[1], ServerMsg::TilesStart { .. }));
        assert!(matches!(&out[2], ServerMsg::Tile(unit) if unit.w == 640), "the tiles kept the old picture size");
    }

    /// Where a desktop past the ceiling has to surface. Learning the size must not
    /// fail — `msg`'s error means "the browser has gone" to every caller, and
    /// answering it by returning would end the session silently on the picker. The
    /// failure belongs on the next call that is on the engine's `?` path.
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
    /// after a frame must not overtake the round that frame is still coding.
    #[tokio::test]
    async fn a_control_message_cannot_overtake_the_round_before_it() {
        let (sink, mut frame_rx) = video_sink(320, 240).await;
        let area = rect(0, 0, 320, 64);
        sink.damage(area, &rgb(area.w(), area.h(), 1)).await.unwrap();
        sink.frame().await.unwrap();
        sink.msg(ServerMsg::RemoteOs { macos: false }).await.unwrap();
        sink.flush().await;

        let out = drain(&mut frame_rx, 3).await;
        assert!(matches!(out[0], ServerMsg::TilesStart { .. }));
        assert!(matches!(out[1], ServerMsg::Tile(_)), "the round lost its place");
        assert!(matches!(out[2], ServerMsg::RemoteOs { .. }), "the message overtook the round");
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
        // Nothing failed to code, so this is the plain closed-channel message.
        assert_eq!(format!("{error}"), "frame channel closed");
    }
}
