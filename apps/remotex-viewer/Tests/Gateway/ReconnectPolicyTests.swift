import Foundation
import Testing
@testable import RemotexViewer

struct ReconnectPolicyTests {
    @Test
    func delaysDoubleUntilTheyReachTheCap() {
        let policy = ReconnectPolicy()
        #expect(policy.delay(forAttempt: 0) == .milliseconds(1_000))
        #expect(policy.delay(forAttempt: 1) == .milliseconds(2_000))
        #expect(policy.delay(forAttempt: 2) == .milliseconds(4_000))
        #expect(policy.delay(forAttempt: 3) == .milliseconds(8_000))
        // 16s would be next, so the cap lands here and stays.
        #expect(policy.delay(forAttempt: 4) == .milliseconds(15_000))
        #expect(policy.delay(forAttempt: 5) == .milliseconds(15_000))
    }

    /// A session left flapping overnight reaches attempt counts that would shift
    /// past the width of Int. The cap has been in effect since attempt 4, so
    /// there is nothing to compute — but it must not trap on the way to saying so.
    @Test
    func anAbsurdAttemptCountStaysAtTheCapRatherThanOverflowing() {
        let policy = ReconnectPolicy()
        for attempt in [62, 63, 64, 1_000, Int.max] {
            #expect(policy.delay(forAttempt: attempt) == .milliseconds(15_000))
        }
    }

    @Test
    func aCapBelowTheBaseStillBounds() {
        let policy = ReconnectPolicy(baseMilliseconds: 5_000, capMilliseconds: 1_000)
        #expect(policy.delay(forAttempt: 0) == .milliseconds(1_000))
        #expect(policy.delay(forAttempt: 3) == .milliseconds(1_000))
    }
}
