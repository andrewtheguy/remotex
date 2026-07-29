import Foundation
import Testing
@testable import RemotexViewer

/// The scheduling arithmetic, which is the only part of a player a test can reach
/// without an audio device.
///
/// The mirror of `frontend/src/audioSchedule.test.ts`, pinning the same properties on
/// the same constants — except for the ceiling case, where the two clients deliberately
/// differ (see `AudioSchedule.place`).
struct AudioScheduleTests {
    /// One wave buffer from the tested Windows host: 32 KiB of 44 100 Hz stereo PCM,
    /// which is ~186 ms of sound.
    private let buffer = 0.186

    @Test
    func aFreshStartGetsTheCushionRatherThanPlayingImmediately() {
        let placed = AudioSchedule.place(nextAt: 0, now: 12.5, duration: buffer)
        #expect(placed.startAt == 12.5 + AudioSchedule.startLead)
        #expect(placed.nextAt == 12.5 + AudioSchedule.startLead + buffer)
        #expect(!placed.flush)
    }

    /// The ordinary case, and the one that must not drift: a gap would be heard as a
    /// click and an overlap as a muddle, and 5 buffers a second compounds either.
    ///
    /// A link delivering content at exactly the rate it plays, which is what the tested
    /// host does over any long window — 55 340 ms of sound in 55 360 ms of clock.
    @Test
    func consecutiveBuffersAreBackToBack() {
        var nextAt = 0.0
        var now = 4.0
        var starts: [Double] = []
        for _ in 0 ..< 20 {
            let placed = AudioSchedule.place(nextAt: nextAt, now: now, duration: buffer)
            #expect(!placed.flush)
            starts.append(placed.startAt)
            nextAt = placed.nextAt
            now += buffer
        }
        for (earlier, later) in zip(starts, starts.dropFirst()) {
            #expect(abs((later - earlier) - buffer) < 1e-9, "\(earlier) then \(later)")
        }
    }

    /// A source pacing slower than real time — the host's *median* inter-arrival was
    /// 189 ms against a 186 ms buffer, so this is the measured case rather than a
    /// hypothetical.
    ///
    /// The cushion drains, and `max(nextAt, now + startLead)` then re-cushions: each
    /// buffer starts a few milliseconds after the previous one ended, so what is heard
    /// is a small silence rather than the schedule falling behind the clock and playing
    /// late for the rest of the session. Pinned because the alternative reading — that
    /// this case is back-to-back — is what the test above looked like it proved.
    @Test
    func aSourceSlowerThanRealTimeRecushionsRatherThanFallingBehind() {
        let shortfall = 0.003
        var nextAt = 0.0
        var now = 4.0
        var previousEnd: Double?
        for _ in 0 ..< 20 {
            let placed = AudioSchedule.place(nextAt: nextAt, now: now, duration: buffer)
            #expect(!placed.flush, "running dry is not a reason to flush")
            if let previousEnd {
                let gap = placed.startAt - previousEnd
                #expect(gap >= 0, "buffers overlapped by \(-gap)")
                #expect(gap <= shortfall + 1e-9, "gap of \(gap) is more than the shortfall")
            }
            // The lead never decays past the cushion, which is what keeps a
            // consistently slow source from becoming a consistently late one.
            #expect(placed.startAt - now >= AudioSchedule.startLead - 1e-9)
            previousEnd = placed.nextAt
            nextAt = placed.nextAt
            now += buffer + shortfall
        }
    }

    /// A remote that went quiet and came back. The schedule restarts at the cushion
    /// instead of trying to play the missing seconds, and the gap is silence rather
    /// than a stumble.
    @Test
    func aGapLongerThanTheLeadRestartsAtTheCushion() {
        let placed = AudioSchedule.place(nextAt: 3.0, now: 8.0, duration: buffer)
        #expect(placed.startAt == 8.0 + AudioSchedule.startLead)
        #expect(!placed.flush, "falling behind is not a reason to throw the queue away")
    }

    /// Past the ceiling the queue goes, and the timeline restarts from zero — because
    /// `AVAudioPlayerNode.stop()` rebases its own clock, which the offline probe
    /// measured.
    @Test
    func pastTheCeilingTheQueueIsFlushedAndTheTimelineRebases() {
        let placed = AudioSchedule.place(nextAt: 9.0, now: 1.0, duration: buffer)
        #expect(placed.flush)
        #expect(placed.startAt == AudioSchedule.startLead)
        #expect(placed.nextAt == AudioSchedule.startLead + buffer)
    }

    /// A burst arriving faster than real time. The lead must not settle above the
    /// ceiling, which is the entire purpose of the flush — the failure this catches is
    /// a delay that grows for the rest of the session.
    @Test
    func aBurstCannotBankMoreThanTheCeiling() {
        var nextAt = 0.0
        // The clock barely moves while 50 buffers arrive: a link delivering a minute of
        // audio in a second.
        var now = 0.0
        var flushes = 0
        for _ in 0 ..< 50 {
            let placed = AudioSchedule.place(nextAt: nextAt, now: now, duration: buffer)
            if placed.flush {
                flushes += 1
                // The caller stops the node, so the clock this arithmetic sees restarts.
                now = 0
            }
            nextAt = placed.nextAt
            #expect(
                nextAt - now <= AudioSchedule.maxLead + buffer + 1e-9,
                "lead reached \(nextAt - now)"
            )
            now += 0.002
        }
        #expect(flushes > 0, "a burst that never flushed did not test the ceiling")
    }

    /// Swept rather than sampled, because the interesting values are the boundaries and
    /// they move with the constants. Nothing may be scheduled before the clock — an
    /// `AVAudioTime` in the past plays immediately and silently loses its place in the
    /// order — and the timeline must never move backwards.
    @Test
    func nothingIsEverPlacedInThePastAndTheTimelineNeverGoesBackwards() {
        let now = 100.0
        for step in -5000 ... 60_000 {
            let lead = Double(step) / 1000
            let nextAt = now + lead
            let placed = AudioSchedule.place(nextAt: nextAt, now: now, duration: buffer)
            // After a flush the clock rebases to zero, so "the past" is measured from
            // there rather than from `now`.
            let clock = placed.flush ? 0.0 : now
            #expect(placed.startAt >= clock, "lead \(lead) placed at \(placed.startAt)")
            #expect(
                placed.startAt <= clock + AudioSchedule.maxLead + 1e-9,
                "lead \(lead) placed past the ceiling at \(placed.startAt)"
            )
            #expect(placed.nextAt == placed.startAt + buffer, "lead \(lead)")
            // A flush is the *only* thing that may lower `nextAt`, and only because the
            // clock it is measured against moved with it.
            if !placed.flush {
                #expect(placed.nextAt >= nextAt - 1e-9, "lead \(lead) moved the timeline back")
            }
        }
    }

    /// A zero-length buffer is not something the gateway sends (an empty frame is
    /// skipped before it reaches the wire, and an empty one is dropped by the
    /// connection), so the property worth pinning is only that it cannot produce a
    /// timeline that goes backwards.
    @Test
    func aZeroLengthBufferLeavesTheTimelineWhereItWas() {
        let placed = AudioSchedule.place(nextAt: 5.0, now: 4.95, duration: 0)
        #expect(placed.nextAt == placed.startAt)
        #expect(placed.startAt >= 4.95)
    }
}
