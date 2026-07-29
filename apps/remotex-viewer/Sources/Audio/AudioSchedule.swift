import Foundation

/// Pure remote-audio scheduling policy.
///
/// Times use the player node's clock. Buffers keep a small start cushion; excess
/// lead flushes the queue so playback stays near live.
enum AudioSchedule {
    /// Furthest ahead of the player clock the schedule may run.
    static let maxLead = 0.3

    /// Cushion after first buffer, underrun, or flush.
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

    /// Place one buffer back-to-back, after an underrun cushion, or after flushing
    /// excess lead. AVAudioPlayerNode can clear only the whole queued timeline.
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
