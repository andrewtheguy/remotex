import Foundation

/// When each buffer of remote audio should play, and when to throw the queue away.
///
/// Split out from `AudioOutput` because it is the whole idea expressed as arithmetic,
/// and arithmetic can be tested without an audio device. The player around it does the
/// parts that cannot: an `AVAudioEngine`, a decoder, and a device that may vanish.
///
/// **The point: the client owns the schedule.** This mirrors
/// `frontend/src/audioSchedule.ts`, which mirrors Guacamole's `RawAudioPlayer.sync()`:
///
/// ```js
/// nextPacketTime = Math.min(nextPacketTime, now + maxLatency);   // 0.3
/// ```
///
/// A schedule the client holds can be pulled back toward live. One belonging to a media
/// element cannot — it resumes where it stopped and never skips forward — which is why
/// the browser's `<audio>` design could not shed a delay it had accumulated, and why
/// this viewer schedules every buffer explicitly instead of handing bytes to `AVPlayer`.
///
/// All times are seconds on the **player node's own** clock, not the engine's: an
/// `AVAudioPlayerNode` timeline restarts at zero every time it is stopped (measured),
/// so the caller reads `playerTime(forNodeTime:)` and this arithmetic never sees the
/// device clock.
enum AudioSchedule {
    /// The furthest ahead of the clock the schedule may run.
    ///
    /// Guacamole's `maxLatency`, and the same number: a third of a second is comfortably
    /// more than the ~186 ms wave buffer the tested Windows host sends, so a link
    /// delivering at real time never reaches it, and a burst cannot bank more than one
    /// buffer's worth of delay.
    static let maxLead = 0.3

    /// How far ahead a fresh start — or a recovery from an underrun, or a flush — is
    /// scheduled.
    ///
    /// Guacamole has no equivalent: it schedules at `max(now, nextPacketTime)`, so a
    /// stream that has just started or just run dry plays with **zero** cushion and the
    /// very next jitter is another gap. The host paces itself to real time but not to a
    /// clock — measured inter-arrival ran 169–200 ms around a 186 ms buffer — so a
    /// cushion in that range costs a tenth of a second of latency and buys out the
    /// ordinary case.
    static let startLead = 0.1

    /// Where to put one decoded buffer, and what to do with what is already queued.
    struct Placement: Equatable {
        /// When to start it, on the player's clock. After a flush this is relative to
        /// the timeline the flush restarts, which is why the caller must stop the node
        /// *before* converting this to an `AVAudioTime`.
        let startAt: Double
        /// Where the timeline stands once this buffer has played.
        let nextAt: Double
        /// Discard everything already queued and restart the timeline.
        ///
        /// The ceiling was exceeded, which means more audio has arrived than real time
        /// can absorb. Dropping the backlog is the point rather than a side effect: the
        /// alternative is playing further and further behind live with no way back.
        let flush: Bool
    }

    /// Place one decoded buffer on the timeline.
    ///
    /// `nextAt` is where the previous buffer left it (`0` before anything has played),
    /// `now` is the player's current time, and `duration` is the buffer's own length.
    ///
    /// Three cases, and the first two are Guacamole's:
    ///
    /// - **Back to back.** The ordinary one: this buffer follows the last exactly, with
    ///   no gap to hear and no overlap to muddy.
    /// - **Behind the clock.** The schedule has run dry — a first buffer, or a gap while
    ///   the remote was quiet — so it restarts at `now + startLead`. Anything earlier
    ///   than `now` cannot play at all, and anything without the cushion is one jitter
    ///   from another underrun. `AVAudioPlayerNode` keeps the silence in between rather
    ///   than closing the gap up, which is measured rather than assumed.
    /// - **Too far ahead.** `flush`, and start again at the cushion.
    ///
    /// **The third case is where this diverges from the browser, deliberately.** Web
    /// Audio can truncate audio it has already committed (`source.stop(when)`), so the
    /// SPA pulls the schedule back to the ceiling and trims the front of the arriving
    /// buffer to match. `AVAudioPlayerNode` has no per-buffer `stop(at:)` — its only
    /// eraser is `stop()`, which takes the whole queue — so the choice here is between
    /// dropping the backlog outright and carrying it. Dropping it is both what the API
    /// offers and what is wanted: the excess is latency, and the one audible skip buys
    /// back all of it. It is the same call the gateway's own queue makes when a consumer
    /// falls behind, and the tighter bound of the two — `startLead` immediately after the
    /// event, where the browser lands at `maxLead`.
    static func place(nextAt: Double, now: Double, duration: Double) -> Placement {
        let startAt = max(nextAt, now + startLead)
        if startAt <= now + maxLead {
            return Placement(startAt: startAt, nextAt: startAt + duration, flush: false)
        }
        // The flush rebases the player's clock to zero, so this start is measured from
        // the restart rather than from `now`.
        return Placement(startAt: startLead, nextAt: startLead + duration, flush: true)
    }
}
