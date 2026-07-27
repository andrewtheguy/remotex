import Foundation

/// The claim/attach/reconnect lifecycle, as a pure value.
///
/// Transcribed from `frontend/src/useRemoteDesktop.ts` (the diagram at its top,
/// plus its `onclose` handling):
///
/// ```text
///   connecting ──► connected ──(drop)──► reconnecting ──► connected …
///        │              │                     │
///     (409) busy    (4001) takenOver      (409) busy
///        │              │
///     start(force:)  start(force:)
/// ```
///
/// Reconnects are automatic; `busy` and `takenOver` wait for the user, because
/// both mean somebody else is using the one session slot and resolving it evicts
/// them. A fatal engine error is deliberately *not* a state here — the socket
/// stays up and the session returns to the picker with the error shown there.
///
/// Kept separate from the actor that drives it so the whole table is testable
/// synchronously, with no socket and no clock.
struct SessionStateMachine {
    /// Close code sent when another client force-claimed the slot (`CLOSE_EVICTED`
    /// in src/ws.rs).
    static let evictedCloseCode = 4001

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
        case claimFailed
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
        /// Show no stale pixels across an interruption. Cheap to obey: the
        /// gateway always repaints in full on (re)attach.
        case clearFramebuffer
        /// Release every held key and button. The single path for it, so that
        /// nothing on the remote can stay pressed after an interruption.
        case releaseInput
        /// Nothing is left to answer a clipboard fetch, so fail it now instead of
        /// leaving the button reading "Fetching…" until its own deadline.
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

        case .claimFailed:
            status = .reconnecting
            return [.scheduleRetry(after: takeRetryDelay())]

        case .socketOpened:
            status = .connected
            // Deliberately no `attempts = 0`. A slot that accepts the socket and
            // drops it immediately would otherwise retry at full speed forever;
            // proof of a *working* attachment is a control message, below.
            return []

        case .controlReceived:
            attempts = 0
            return []

        case .socketClosed(let code):
            // After an eviction or a 409 the socket is already gone and the user
            // has to act, so a late close is not a reason to start retrying.
            guard status != .takenOver, status != .busy else {
                return []
            }
            if code == Self.evictedCloseCode {
                status = .takenOver
                return [.failPendingClipboardFetch, .clearFramebuffer, .releaseInput]
            }
            // Code 4000 (token invalid or superseded) takes this same path: the
            // answer to both a stale token and a dropped connection is to claim
            // again, which is unconditional anyway.
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
