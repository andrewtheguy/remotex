import Foundation
import Synchronization

/// The ordered outbound side of the session socket.
///
/// Not an actor, and that is the whole point. `await someActor.send(_:)` called
/// from several tasks gives no FIFO guarantee *between* those tasks, and three
/// unrelated main-actor sources feed this one socket: pointer events from a
/// tracking area, key events from an `NSEvent` monitor, and clipboard pushes from
/// a timer. An inverted `key` press/release pair is a modifier stuck down on the
/// remote, which is the worst failure this client has. So enqueueing is a plain
/// synchronous append under a lock, callable from any isolation, and exactly one
/// task drains it.
final class OutboundQueue: Sendable {
    private struct State {
        var pending: [ClientMessage] = []
        /// The last viewport reported on this connection, for the dedupe below.
        var lastViewport: DisplayMode?
        var finished = false
    }

    private let state = Mutex(State())
    private let stream: AsyncStream<Void>
    private let wake: AsyncStream<Void>.Continuation

    init() {
        // bufferingNewest(1): a wake-up is a "there is work" nudge, not a count.
        // The drain takes everything queued, so extra nudges have nothing to do.
        let (stream, wake) = AsyncStream<Void>.makeStream(bufferingPolicy: .bufferingNewest(1))
        self.stream = stream
        self.wake = wake
    }

    /// Nudges to drain. One consumer only, as with any `AsyncStream`.
    var wakeups: AsyncStream<Void> {
        stream
    }

    /// Queue a message. Synchronous and ordered; never waits on the socket.
    ///
    /// Two coalescing rules, both from `useRemoteDesktop.ts`, which keep a
    /// trackpad flick or a window drag from growing the queue without bound:
    ///
    /// - a `mouseMove` replaces the **last** element if that is also a
    ///   `mouseMove`. Only the last: searching further back would move a click
    ///   past its own position.
    /// - a `viewport` identical to the last one queued is dropped (the SPA's
    ///   `lastViewport`). Reset the memo on every `connected`, since the gateway
    ///   expects an undeduped report per connection.
    func enqueue(_ message: ClientMessage) {
        let queued = state.withLock { state -> Bool in
            guard !state.finished else {
                return false
            }
            switch message {
            case .mouseMove:
                if case .mouseMove = state.pending.last {
                    state.pending[state.pending.count - 1] = message
                } else {
                    state.pending.append(message)
                }
            case .viewport(let w, let h):
                let reported = DisplayMode(w: w, h: h)
                guard state.lastViewport != reported else {
                    return false
                }
                state.lastViewport = reported
                state.pending.append(message)
            default:
                state.pending.append(message)
            }
            return true
        }
        if queued {
            wake.yield()
        }
    }

    /// Take everything queued, in order.
    func drain() -> [ClientMessage] {
        state.withLock { state in
            defer { state.pending.removeAll(keepingCapacity: true) }
            return state.pending
        }
    }

    /// Forget the viewport dedupe, so the first report on a new connection is
    /// always sent.
    func resetViewportMemo() {
        state.withLock { $0.lastViewport = nil }
    }

    /// Drop anything queued. Used when the socket goes away: the gateway will
    /// repaint and re-announce on reattach, and stale input is worse than none.
    func discardPending() {
        state.withLock { $0.pending.removeAll(keepingCapacity: true) }
    }

    /// Refuse further messages and end the wake-up stream.
    func finish() {
        state.withLock { state in
            state.finished = true
            state.pending.removeAll()
        }
        wake.finish()
    }
}
