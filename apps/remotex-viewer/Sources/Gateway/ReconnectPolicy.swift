import Foundation

/// Capped exponential backoff between connection attempts: `min(1000·2^n, 15000)`
/// milliseconds, matching `MAX_RETRY_DELAY_MS` and the retry schedule in
/// `frontend/src/useRemoteDesktop.ts`.
///
/// Its own type only so the schedule and — more importantly — *when the counter
/// resets* can be tested without a socket. The reset rule is the subtle half:
/// see `SessionStateMachine`.
struct ReconnectPolicy: Sendable, Equatable {
    var baseMilliseconds = 1_000
    var capMilliseconds = 15_000

    /// The wait before attempt `attempt`, counting the first retry as 0.
    func delay(forAttempt attempt: Int) -> Duration {
        guard attempt > 0 else {
            return .milliseconds(min(baseMilliseconds, capMilliseconds))
        }
        // A session that flaps for a day would otherwise shift past the width of
        // Int and trap. The cap is reached by attempt 4 regardless.
        guard attempt < Int.bitWidth - 1 else {
            return .milliseconds(capMilliseconds)
        }
        let scaled = baseMilliseconds.multipliedReportingOverflow(by: 1 << attempt)
        guard !scaled.overflow else {
            return .milliseconds(capMilliseconds)
        }
        return .milliseconds(min(scaled.partialValue, capMilliseconds))
    }
}
