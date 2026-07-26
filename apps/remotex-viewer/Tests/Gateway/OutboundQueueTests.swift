import Foundation
import Testing
@testable import RemotexViewer

struct OutboundQueueTests {
    /// The property the whole type exists for: order is never traded away. An
    /// inverted press/release pair is a modifier stuck down on the remote.
    @Test
    func aLongKeySequenceDrainsInExactlyTheOrderItWasQueued() {
        let queue = OutboundQueue()
        var expected: [ClientMessage] = []
        for index in 0 ..< 1_000 {
            let message = ClientMessage.key(
                code: "Key\(index % 26)",
                pressed: index.isMultiple(of: 2),
                caps: false
            )
            expected.append(message)
            queue.enqueue(message)
        }
        #expect(queue.drain() == expected)
        #expect(queue.drain().isEmpty)
    }

    @Test
    func consecutivePointerMovesCollapseToTheLatest() {
        let queue = OutboundQueue()
        queue.enqueue(.mouseMove(x: 1, y: 1))
        queue.enqueue(.mouseMove(x: 2, y: 2))
        queue.enqueue(.mouseMove(x: 3, y: 3))
        #expect(queue.drain() == [.mouseMove(x: 3, y: 3)])
    }

    /// Collapsing looks only at the last element, and this is why. A press
    /// happened at the position reported before it; if a later move could reach
    /// back past the press, the click would land somewhere the user never clicked.
    @Test
    func aMoveAfterAButtonDoesNotReachBackPastIt() {
        let queue = OutboundQueue()
        queue.enqueue(.mouseMove(x: 10, y: 10))
        queue.enqueue(.mouseButton(button: .left, pressed: true))
        queue.enqueue(.mouseMove(x: 99, y: 99))
        #expect(
            queue.drain() == [
                .mouseMove(x: 10, y: 10),
                .mouseButton(button: .left, pressed: true),
                .mouseMove(x: 99, y: 99),
            ]
        )
    }

    @Test
    func anIdenticalViewportIsDroppedAndAChangedOneIsNot() {
        let queue = OutboundQueue()
        queue.enqueue(.viewport(w: 1280, h: 800))
        queue.enqueue(.viewport(w: 1280, h: 800))
        queue.enqueue(.viewport(w: 1440, h: 900))
        queue.enqueue(.viewport(w: 1440, h: 900))
        #expect(queue.drain() == [.viewport(w: 1280, h: 800), .viewport(w: 1440, h: 900)])
    }

    /// The gateway wants a fresh report per connection — the size it knows about
    /// went away with the last socket — so the dedupe cannot survive a reconnect.
    @Test
    func resettingTheMemoLetsTheSameViewportBeReportedAgain() {
        let queue = OutboundQueue()
        queue.enqueue(.viewport(w: 1280, h: 800))
        #expect(queue.drain() == [.viewport(w: 1280, h: 800)])

        queue.enqueue(.viewport(w: 1280, h: 800))
        #expect(queue.drain().isEmpty, "still deduped within the connection")

        queue.resetViewportMemo()
        queue.enqueue(.viewport(w: 1280, h: 800))
        #expect(queue.drain() == [.viewport(w: 1280, h: 800)])
    }

    /// Input queued while the socket is down is stale by the time one exists, and
    /// the gateway repaints and re-announces on reattach anyway.
    @Test
    func discardingDropsPendingWorkButKeepsTheQueueUsable() {
        let queue = OutboundQueue()
        queue.enqueue(.key(code: "KeyA", pressed: true, caps: false))
        queue.discardPending()
        #expect(queue.drain().isEmpty)

        queue.enqueue(.refresh)
        #expect(queue.drain() == [.refresh])
    }

    @Test
    func finishingRefusesFurtherMessages() {
        let queue = OutboundQueue()
        queue.enqueue(.refresh)
        queue.finish()
        queue.enqueue(.disconnect)
        #expect(queue.drain().isEmpty)
    }

    /// One wake-up per burst is enough, because the drain takes everything — but
    /// a burst must never leave the consumer asleep with work outstanding.
    @Test
    func aQueuedMessageAlwaysProducesAWakeup() async throws {
        let queue = OutboundQueue()
        var wakeups = queue.wakeups.makeAsyncIterator()

        queue.enqueue(.refresh)
        #expect(await wakeups.next() != nil)
        #expect(queue.drain() == [.refresh])

        queue.enqueue(.disconnect)
        #expect(await wakeups.next() != nil)
        #expect(queue.drain() == [.disconnect])
    }

    /// A dropped message must not produce a wake-up that finds nothing to do, or
    /// a still trackpad would keep the drain loop spinning.
    @Test
    func aDedupedViewportProducesNoWakeup() async throws {
        let queue = OutboundQueue()
        var wakeups = queue.wakeups.makeAsyncIterator()

        queue.enqueue(.viewport(w: 800, h: 600))
        #expect(await wakeups.next() != nil)
        _ = queue.drain()

        queue.enqueue(.viewport(w: 800, h: 600))
        queue.finish()
        // The stream ends without another element: the duplicate never woke it.
        #expect(await wakeups.next() == nil)
    }
}
