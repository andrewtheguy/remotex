import Foundation
import Testing
@testable import RemotexViewer

struct SessionStateMachineTests {
    @Test
    func aStartClaimsAndClearsWhateverWasOnScreen() {
        var machine = SessionStateMachine()
        let actions = machine.handle(.start(force: false))
        #expect(actions == [.clearFramebuffer, .releaseInput, .claim(force: false)])
        #expect(machine.status == .connecting)
    }

    @Test
    func aSuccessfulClaimOpensTheSocket() {
        var machine = SessionStateMachine()
        _ = machine.handle(.start(force: false))
        #expect(machine.handle(.claimed(token: "tok-1")) == [.openSocket(token: "tok-1")])
        #expect(machine.handle(.socketOpened) == [])
        #expect(machine.status == .connected)
    }

    /// 409 means another client is on the desktop. Resolving it evicts them, so
    /// it waits for the user rather than retrying — and the framebuffer is
    /// cleared, because whatever is on screen belongs to a session we lost.
    @Test
    func aBusySlotStallsForTheUserInsteadOfRetrying() {
        var machine = SessionStateMachine()
        _ = machine.handle(.start(force: false))
        let actions = machine.handle(.claimBusy)
        #expect(actions == [.clearFramebuffer, .releaseInput])
        #expect(machine.status == .busy)

        // "Take over" is a forced start, and the counter begins again with it.
        let resumed = machine.handle(.start(force: true))
        #expect(resumed == [.clearFramebuffer, .releaseInput, .claim(force: true)])
        #expect(machine.status == .connecting)
    }

    /// The gateway's auth sessions live in memory, so its restart 401s a claim
    /// mid-session. Retrying cannot fix that, and a backoff loop would hide it.
    @Test
    func a401GoesToLoginRatherThanRetrying() {
        var machine = SessionStateMachine()
        _ = machine.handle(.start(force: false))
        #expect(machine.handle(.claimUnauthorized) == [.releaseInput, .toLogin])
    }

    @Test
    func aFailedClaimRetriesWithGrowingBackoff() {
        var machine = SessionStateMachine()
        _ = machine.handle(.start(force: false))
        #expect(machine.handle(.claimFailed) == [.scheduleRetry(after: .milliseconds(1_000))])
        #expect(machine.status == .reconnecting)
        #expect(machine.handle(.retryElapsed) == [.claim(force: false)])
        #expect(machine.handle(.claimFailed) == [.scheduleRetry(after: .milliseconds(2_000))])
        #expect(machine.handle(.retryElapsed) == [.claim(force: false)])
        #expect(machine.handle(.claimFailed) == [.scheduleRetry(after: .milliseconds(4_000))])
    }

    /// Close 4001 is an eviction: another client force-claimed the slot. Taking
    /// it back evicts *them*, so it needs the user to say so.
    @Test
    func anEvictionWaitsForTheUserAndDoesNotReconnect() {
        var machine = SessionStateMachine()
        _ = machine.handle(.start(force: false))
        _ = machine.handle(.claimed(token: "tok-1"))
        _ = machine.handle(.socketOpened)

        let actions = machine.handle(.socketClosed(code: 4001))
        #expect(actions == [.failPendingClipboardFetch, .clearFramebuffer, .releaseInput])
        #expect(machine.status == .takenOver)

        // A late close arriving after the eviction must not start a retry loop
        // behind the user's back.
        #expect(machine.handle(.socketClosed(code: nil)) == [])
        #expect(machine.status == .takenOver)
    }

    /// Code 4000 (token invalid or superseded) and a plain network drop take the
    /// same path, because the answer to both is to claim again.
    @Test
    func aStaleTokenAndADropBothReconnect() {
        for code in [4000, 1_006, nil] {
            var machine = SessionStateMachine()
            _ = machine.handle(.start(force: false))
            _ = machine.handle(.claimed(token: "tok-1"))
            _ = machine.handle(.socketOpened)

            let actions = machine.handle(.socketClosed(code: code))
            #expect(
                actions == [
                    .failPendingClipboardFetch,
                    .clearFramebuffer,
                    .releaseInput,
                    .scheduleRetry(after: .milliseconds(1_000)),
                ],
                "close \(String(describing: code))"
            )
            #expect(machine.status == .reconnecting)
        }
    }

    /// The subtle rule, and the reason `ReconnectPolicy` is separate from this:
    /// opening the socket is not proof of anything. A slot that accepts the
    /// upgrade and drops it immediately would otherwise retry at full speed for
    /// as long as it kept doing that. A control message is the proof.
    @Test
    func backoffResetsOnAControlMessageAndNotOnTheSocketOpening() {
        var machine = SessionStateMachine()
        _ = machine.handle(.start(force: false))
        _ = machine.handle(.claimFailed)
        _ = machine.handle(.retryElapsed)
        _ = machine.handle(.claimFailed)
        #expect(machine.attempts == 2)

        _ = machine.handle(.retryElapsed)
        _ = machine.handle(.claimed(token: "tok-1"))
        _ = machine.handle(.socketOpened)
        #expect(machine.attempts == 2, "opening the socket proves nothing yet")

        // A close now continues the schedule rather than restarting it.
        #expect(
            machine.handle(.socketClosed(code: nil)).contains(
                .scheduleRetry(after: .milliseconds(4_000))
            )
        )

        // Now do it properly: one control message and the schedule resets.
        _ = machine.handle(.retryElapsed)
        _ = machine.handle(.claimed(token: "tok-1"))
        _ = machine.handle(.socketOpened)
        _ = machine.handle(.controlReceived)
        #expect(machine.attempts == 0)
        #expect(
            machine.handle(.socketClosed(code: nil)).contains(
                .scheduleRetry(after: .milliseconds(1_000))
            )
        )
    }

    /// Every interruption releases input. Nothing on the remote may stay pressed
    /// because a socket dropped, and this is the only place that is decided.
    @Test
    func everyInterruptionReleasesInput() {
        let interruptions: [SessionStateMachine.Event] = [
            .claimBusy,
            .claimUnauthorized,
            .socketClosed(code: 4001),
            .socketClosed(code: nil),
        ]
        for event in interruptions {
            var machine = SessionStateMachine()
            _ = machine.handle(.start(force: false))
            _ = machine.handle(.claimed(token: "tok-1"))
            _ = machine.handle(.socketOpened)
            #expect(
                machine.handle(event).contains(.releaseInput),
                "\(event) should release input"
            )
        }
    }

    /// `GatewayConnection.handle` delivers a transition's sink events in one hop and
    /// does the connection's own work after it, which reorders nothing only as long
    /// as every list here is grouped that way. A list that put a `claim` ahead of a
    /// `clearFramebuffer` would come out with the framebuffer cleared *after* the
    /// claim it was written to precede.
    @Test
    func everyTransitionPutsItsSinkActionsFirst() {
        let events: [SessionStateMachine.Event] = [
            .start(force: false),
            .start(force: true),
            .claimed(token: "tok-1"),
            .claimBusy,
            .claimUnauthorized,
            .claimFailed,
            .socketOpened,
            .controlReceived,
            .socketClosed(code: 4001),
            .socketClosed(code: nil),
            .retryElapsed,
        ]
        for event in events {
            var machine = SessionStateMachine()
            let actions = machine.handle(event)
            let forSink = actions.map { $0.sinkEvent != nil }
            #expect(
                !forSink.drop(while: { $0 }).contains(true),
                "\(event) returned \(actions), which GatewayConnection.handle would reorder"
            )
        }
    }
}
