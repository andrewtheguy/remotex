//! What the attachment's paint window has learned about the link, published for
//! the encoders.
//!
//! The paint window (`src/ws.rs`) already collects the one latency figure this
//! gateway has — how long the oldest unacknowledged batch has been owed — but
//! until now it only *gated* the send window with it. This handle is the other
//! half: the same measurement, readable where quality decisions are made
//! (`src/encode.rs`), so an adaptive target can move quality *before* the window
//! parks instead of after.
//!
//! The figure published is **queueing lag**, not raw staleness. A batch's
//! end-to-end time includes the network round trip, and on a distant link that
//! is large while the link is perfectly healthy. So the tracker also publishes a
//! baseline — the link's distance, the smallest ping round trip seen over a
//! recent window — and [`LinkFeedback::lag`] answers with the age of the oldest
//! owed batch *minus* that baseline. A clean link reads as zero lag no matter how
//! far away it is; what counts is the time a batch spends beyond the distance:
//! being sent, queued behind others, and decoded by a client that cannot keep up.
//!
//! The baseline is a ping's round trip and not the smallest batch's, the move
//! RustDesk's QoS and Guacamole's `sync` handler make, because a batch's time
//! carries its own transmission. A stream whose frames are all one size — a
//! passed VP9 stream, one frame in flight — spends the same time sending every
//! one, and a baseline taken from them read that time as distance: at 5 Mbit/s
//! the browser painted 8 frames a second a quarter of a second behind and no
//! frame ever counted as late. A ping is a few bytes and carries nothing but
//! the distance.
//!
//! One writer, many cheap readers: the ws bridge stores two atomics on the
//! events it already handles (a batch sent, a batch acknowledged), the passing
//! sink one more on a frame handed over, and a reader pays a few loads and a
//! subtraction. Nothing here polls, allocates, or locks.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

/// The sentinel meaning "no baseline learned yet". Until one acknowledgment has
/// completed there is no way to tell queueing from distance, and the safe answer
/// is no lag at all — coarsening a picture on an unmeasured link would punish
/// every session's first second.
const BASELINE_UNKNOWN: u32 = u32::MAX;

/// The process-wide instant every stored timestamp counts from.
///
/// An epoch rather than storing `Instant`s because the stores are atomics: a
/// `u64` of microseconds crosses threads for free, and the process outliving
/// `u64::MAX` microseconds is not a case worth a branch.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn micros_since_epoch(at: Instant) -> u64 {
    // +1 so a batch sent in the same microsecond as the epoch cannot collide
    // with the "nothing pending" sentinel of 0.
    at.saturating_duration_since(epoch()).as_micros() as u64 + 1
}

/// The link as the paint window sees it, shared between the ws bridge (writer)
/// and the encoders (readers). One per session slot; it outlives attachments,
/// and [`Self::reset`] on every attachment change is what keeps a new browser
/// from inheriting the last one's verdict.
#[derive(Debug)]
pub struct LinkFeedback {
    /// Microseconds since [`epoch`] (offset by one) when the oldest currently
    /// unacknowledged batch was sent. `0` means nothing is owed.
    oldest_sent_us: AtomicU64,
    /// The smallest ping round trip (in ms) over the tracker's recent window —
    /// the link's distance. [`BASELINE_UNKNOWN`] until the first pong.
    baseline_ms: AtomicU32,
    /// Microseconds since [`epoch`] (offset by one) when the last passed frame
    /// was handed to the browser's queue. `0` before any was.
    handed_us: AtomicU64,
}

impl LinkFeedback {
    pub fn new() -> Self {
        Self {
            oldest_sent_us: AtomicU64::new(0),
            baseline_ms: AtomicU32::new(BASELINE_UNKNOWN),
            handed_us: AtomicU64::new(0),
        }
    }

    /// How long the oldest owed batch has waited *beyond* the link's own floor.
    ///
    /// Zero when nothing is owed, and zero until a baseline exists: both are
    /// states in which "the client is behind" cannot be asserted, and the
    /// callers of this only ever act on lag by giving quality up.
    pub fn lag(&self, now: Instant) -> Duration {
        let oldest = self.oldest_sent_us.load(Ordering::Relaxed);
        if oldest == 0 {
            return Duration::ZERO;
        }
        let baseline = self.baseline_ms.load(Ordering::Relaxed);
        if baseline == BASELINE_UNKNOWN {
            return Duration::ZERO;
        }
        let age = now
            .saturating_duration_since(epoch())
            .saturating_sub(Duration::from_micros(oldest - 1));
        age.saturating_sub(Duration::from_millis(u64::from(baseline)))
    }

    /// Record when the oldest unacknowledged batch was sent, or that none is.
    pub fn owed_since(&self, sent: Option<Instant>) {
        let value = sent.map_or(0, micros_since_epoch);
        self.oldest_sent_us.store(value, Ordering::Relaxed);
    }

    /// Record that a passed frame was handed to the browser's queue at `at` —
    /// what [`Self::hold`] tells the batches ahead of it from its own.
    pub fn handed(&self, at: Instant) {
        self.handed_us.store(micros_since_epoch(at), Ordering::Relaxed);
    }

    /// Record the link's distance: the smallest ping round trip over the paint
    /// tracker's recent window.
    pub fn baseline(&self, ms: u32) {
        self.baseline_ms.store(ms, Ordering::Relaxed);
    }

    /// The link's distance, as [`Self::baseline`] last recorded it, or `None`
    /// before the first pong.
    pub fn distance(&self) -> Option<Duration> {
        let baseline = self.baseline_ms.load(Ordering::Relaxed);
        (baseline != BASELINE_UNKNOWN).then(|| Duration::from_millis(u64::from(baseline)))
    }

    /// How long the queue ahead of the last frame handed over is: the oldest owed
    /// batch's wait beyond the distance, when that batch went out before the frame
    /// was handed over, and nothing when it did not — then the only batch owed
    /// carries the frame itself, and its own transmission is the link working,
    /// not the link behind.
    ///
    /// Judged by when the batch was written rather than by how many are owed: the
    /// frame joins the tracker's count from the socket's task, some time after it
    /// was handed over, and a fence that followed it at once found the count one
    /// short as often as not, and read a batch ahead of the frame as nothing.
    ///
    /// What a passed stream's fence is held for ([`crate::vnc`]): the remote keeps
    /// one frame in flight, times the echo and reads it as queueing. Held for the
    /// frame's own delivery instead, a lock-step remote read every frame's
    /// transmission as lag and coded for a third of the link; held for nothing
    /// while its frame is alone, it sends the next, and from then on the echo
    /// carries what is queued ahead of each frame and it paces itself to that.
    pub fn hold(&self, now: Instant) -> Duration {
        let oldest = self.oldest_sent_us.load(Ordering::Relaxed);
        if oldest == 0 || oldest >= self.handed_us.load(Ordering::Relaxed) {
            return Duration::ZERO;
        }
        self.lag(now)
    }

    /// Forget everything. Called when the attachment changes, so a detached
    /// engine stops acting on the departed browser's lag and a fresh browser
    /// starts from "unmeasured" rather than from its predecessor's link.
    pub fn reset(&self) {
        self.oldest_sent_us.store(0, Ordering::Relaxed);
        self.baseline_ms.store(BASELINE_UNKNOWN, Ordering::Relaxed);
        self.handed_us.store(0, Ordering::Relaxed);
    }
}

impl Default for LinkFeedback {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The tests ask about *later* instants rather than back-dating the send:
    // the epoch is initialized at first use, so an instant earlier than every
    // other one in the process would saturate to it and measure nothing.

    #[test]
    fn an_unmeasured_link_reports_no_lag() {
        let feedback = LinkFeedback::new();
        let now = Instant::now();
        assert_eq!(feedback.lag(now), Duration::ZERO);

        // A batch owed but no baseline yet: still no lag — queueing cannot be
        // told from distance until an acknowledgment has measured the floor.
        feedback.owed_since(Some(now));
        assert_eq!(feedback.lag(now + Duration::from_secs(1)), Duration::ZERO);
    }

    #[test]
    fn lag_is_the_owed_age_beyond_the_baseline() {
        let feedback = LinkFeedback::new();
        // Sent a millisecond after the epoch, not "now": the epoch is set by
        // whichever test touches it first, and one on tokio's paused clock sets it
        // ahead of the real one, where a real `now` would saturate to it.
        let sent = epoch() + Duration::from_millis(1);
        feedback.baseline(40);
        feedback.owed_since(Some(sent));
        // 100 ms owed on a 40 ms link: 60 ms of queueing, to the microsecond the
        // store rounds.
        let lag = feedback.lag(sent + Duration::from_millis(100));
        assert!(
            (Duration::from_millis(59)..=Duration::from_millis(61)).contains(&lag),
            "expected ~60ms, got {lag:?}"
        );

        // A batch younger than the baseline is not lag at all.
        assert_eq!(feedback.lag(sent + Duration::from_millis(10)), Duration::ZERO);
    }

    #[test]
    fn settling_and_reset_both_read_as_clear() {
        let feedback = LinkFeedback::new();
        let sent = Instant::now();
        let later = sent + Duration::from_millis(500);
        feedback.baseline(10);
        feedback.owed_since(Some(sent));
        assert!(feedback.lag(later) > Duration::from_millis(400));

        // Everything acknowledged: nothing owed, no lag.
        feedback.owed_since(None);
        assert_eq!(feedback.lag(later), Duration::ZERO);

        // A new attachment starts unmeasured.
        feedback.owed_since(Some(sent));
        feedback.reset();
        assert_eq!(feedback.lag(later), Duration::ZERO);
    }

    /// The hold is the queue ahead of the frame handed over: the lag, once the
    /// oldest owed batch went out before the frame did, and nothing for a frame
    /// whose batch is the oldest owed — or is not counted yet.
    #[test]
    fn the_hold_is_the_lag_behind_a_batch_older_than_the_frame() {
        let feedback = LinkFeedback::new();
        let sent = epoch() + Duration::from_millis(1);
        let handed = sent + Duration::from_millis(50);
        let later = sent + Duration::from_millis(400);
        feedback.baseline(20);
        feedback.owed_since(Some(sent));
        assert!(feedback.lag(later) > Duration::from_millis(370));
        assert_eq!(feedback.hold(later), Duration::ZERO, "a frame not handed over yet was held");
        feedback.handed(handed);
        assert_eq!(feedback.hold(later), feedback.lag(later), "the batch ahead of the frame was not held for");
        // The frame's own batch, counted or not, is no queue ahead of it.
        feedback.owed_since(Some(handed));
        assert_eq!(feedback.hold(later), Duration::ZERO, "a frame owed alone was held for its own delivery");
        feedback.owed_since(None);
        assert_eq!(feedback.hold(later), Duration::ZERO);
        feedback.owed_since(Some(sent));
        feedback.reset();
        assert_eq!(feedback.hold(later), Duration::ZERO);
    }
}
