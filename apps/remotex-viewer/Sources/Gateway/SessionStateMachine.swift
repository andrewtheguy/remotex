import Foundation

/// The claim/attach/reconnect lifecycle, as a pure value.
///
/// Matches the browser lifecycle in `frontend/src/useRemoteDesktop.ts`:
///
/// ```text
///   connecting ──► connected ──(drop)──► reconnecting ──► connected …
///        │              │                     │
///     (409) busy    (4001) takenOver      (409) busy
///        │              │
///     start(force:)  start(force:)
/// ```
///
/// Reconnects are automatic. `busy` and `takenOver` require user action because
/// recovery evicts the slot holder. Engine errors stay on the open socket and
/// return to the picker, so they are not states here. This value is synchronous
/// and independently testable.
struct SessionStateMachine {
    /// Close code sent when another client force-claimed the slot (`CLOSE_EVICTED`
    /// in src/ws.rs).
    static let evictedCloseCode = 4001

    /// Failures before the reason is shown. The fourth attempt reaches the
    /// 15-second backoff cap after roughly 30 seconds; a control message or user
    /// start resets the count.
    static let attemptsBeforeReporting = 4

    private(set) var status: ViewerConnectionStatus = .connecting
    /// Consecutive failed attempts, driving the backoff. See `noteAttached` for
    /// why a successful *open* does not reset this.
    private(set) var attempts = 0

    var policy = ReconnectPolicy()

    enum Event: Sendable, Equatable {
        /// A user-driven (re)start: first connect, takeover, or take-back.
        case start(force: Bool)
        case claimed(token: String)
        /// `POST /api/session` answered 409 — another client holds the slot.
        case claimBusy
        /// Any endpoint answered 401. The gateway's auth sessions are in memory,
        /// so a restart produces this mid-session.
        case claimUnauthorized
        /// A retryable transport failure.
        case claimFailed(reason: String)
        /// A non-retryable address, compatibility, or response failure.
        case claimRejected(reason: String)
        case socketOpened
        /// Any control message, including one whose type this build does not know.
        case controlReceived
        case socketClosed(code: Int?)
        case retryElapsed
    }

    enum Action: Sendable, Equatable {
        case claim(force: Bool)
        case openSocket(token: String)
        case scheduleRetry(after: Duration)
        /// Report a terminal failure in the UI.
        case report(reason: String)
        /// Remove stale pixels across an interruption; reattach repaints in full.
        case clearFramebuffer
        /// Release all held keys and buttons after an interruption.
        case releaseInput
        /// Fail a clipboard fetch that can no longer receive a reply.
        case failPendingClipboardFetch
        case toLogin
    }

    mutating func handle(_ event: Event) -> [Action] {
        switch event {
        case .start(let force):
            attempts = 0
            status = .connecting
            return [.clearFramebuffer, .releaseInput, .claim(force: force)]

        case .claimed(let token):
            return [.openSocket(token: token)]

        case .claimBusy:
            status = .busy
            return [.clearFramebuffer, .releaseInput]

        case .claimUnauthorized:
            return [.releaseInput, .toLogin]

        case .claimFailed(let reason):
            status = .reconnecting
            // Keep retrying indefinitely, but expose the reason after the budget.
            var actions: [Action] = attempts < Self.attemptsBeforeReporting
                ? []
                : [.report(reason: reason)]
            actions.append(.scheduleRetry(after: takeRetryDelay()))
            return actions

        case .claimRejected(let reason):
            return [.report(reason: reason)]

        case .socketOpened:
            status = .connected
            // Opening alone is not proof of a working attachment. Resetting here
            // would let a socket that immediately drops retry at full speed forever.
            return []

        case .controlReceived:
            attempts = 0
            return []

        case .socketClosed(let code):
            // Ignore a late close after entering a user-action state.
            guard status != .takenOver, status != .busy else {
                return []
            }
            if code == Self.evictedCloseCode {
                status = .takenOver
                return [.failPendingClipboardFetch, .clearFramebuffer, .releaseInput]
            }
            // Close code 4000 (stale/superseded token) and an ordinary drop both
            // require a fresh claim.
            status = .reconnecting
            return [
                .failPendingClipboardFetch,
                .clearFramebuffer,
                .releaseInput,
                .scheduleRetry(after: takeRetryDelay()),
            ]

        case .retryElapsed:
            return [.claim(force: false)]
        }
    }

    private mutating func takeRetryDelay() -> Duration {
        let delay = policy.delay(forAttempt: attempts)
        attempts += 1
        return delay
    }
}
