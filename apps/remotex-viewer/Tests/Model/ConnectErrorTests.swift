import AppKit
import Foundation
import Testing

@testable import RemotexViewer

/// Which "the session did not open" message goes away by itself, and which stays.
///
/// Both are `session.connectError` and both are read in the same two places, so
/// nothing about them is distinguishable on screen — which is why the rule they
/// differ by is pinned here rather than left to whoever next touches `picker`.
@MainActor
struct ConnectErrorTests {
    private func freshModel() -> AppModel {
        AppModel(
            defaults: UserDefaults(suiteName: "ConnectErrorTests.\(UUID().uuidString)")!,
            clipboard: ClipboardSynchronizer(
                pasteboard: NSPasteboard.withUniqueName(),
                startsPolling: false
            )
        )
    }

    /// A reason the session could not be opened is history the moment one opens: any
    /// control message is proof of that, so leaving it up blames a working session for
    /// a failure it recovered from.
    @Test
    func aRejectionsReasonGoesAwayOnceTheSessionAttaches() {
        let model = freshModel()
        model.apply(.rejected(reason: "an SSL error has occurred (-1200)"))
        #expect(model.session.connectError == "an SSL error has occurred (-1200)")

        // `picker` in particular, because it is the message that used to leave this
        // on screen: it is where a *gateway* error is read, so it clears nothing.
        model.apply(.control(.picker))
        #expect(model.session.connectError == nil)
    }

    /// The mirror case, and the reason the two cannot simply be cleared together: the
    /// gateway sends `error` and then puts the session back on the picker, so a
    /// picker that cleared everything would erase the message it exists to show.
    @Test
    func aGatewayErrorSurvivesThePickerThatFollowsIt() {
        let model = freshModel()
        model.apply(.control(.error(message: "the target refused the connection")))
        model.apply(.control(.picker))
        #expect(
            model.session.connectError == "the target refused the connection",
            "the picker is where a gateway error is read"
        )
    }

    /// A busy remote follows the same path — reported, then the picker — and has to
    /// survive it for the same reason. What makes it its own state is that the picker
    /// puts a button on it, so it also has to name the target the takeover applies to.
    @Test
    func aBusyRemoteSurvivesThePickerAndNamesItsTarget() {
        let model = freshModel()
        // Through the front door: `picker` is what puts this screen up, and
        // `connect` is what makes a pick pending.
        model.apply(.control(.picker))
        model.connect(to: "mac")
        #expect(model.session.pendingTarget == "mac")

        model.apply(.control(.remoteBusy(holder: "192.168.1.5", heldSecs: 754, takenOver: false)))
        model.apply(.control(.picker))

        #expect(model.session.remoteBusy?.target == "mac")
        #expect(model.session.remoteBusy?.holder == "192.168.1.5")
        #expect(model.session.remoteBusy?.heldSecs == 754)
        #expect(
            model.session.connectError == nil,
            "a busy remote is an answer, not an error — showing both says it twice"
        )
        // The pick is over either way, or every row stays locked with nothing in
        // flight and the takeover button is disabled along with them.
        #expect(model.session.pendingTarget == nil)
    }

    /// Somebody took the remote from us *mid-session*, during a reconnect. There is no
    /// pending pick to name then, so the offer has to fall back to the target the
    /// session was already on — otherwise the button has nothing to act on.
    @Test
    func aRemoteTakenMidSessionStillNamesTheTargetItWasOn() {
        let model = freshModel()
        model.apply(.control(.connected(connected(to: "mac"))))
        #expect(model.session.pendingTarget == nil, "a live session has no pick pending")

        model.apply(.control(.remoteBusy(holder: "10.0.0.9", heldSecs: 3, takenOver: true)))
        #expect(model.session.remoteBusy?.target == "mac")
        // And it must arrive marked as a loss rather than a refusal: the picker says
        // "your session was taken over", not "that target is in use", to somebody
        // who asked for nothing.
        #expect(model.session.remoteBusy?.takenOver == true)
    }

    private func connected(to name: String) -> ServerMessage.Connected {
        ServerMessage.Connected(
            name: name,
            protocolName: "rxa",
            resize: true,
            clipboard: true,
            audio: false
        )
    }

    /// And the offer must not outlive the situation: connecting somewhere is what
    /// answers it, whether or not that connect was the takeover.
    @Test
    func aSessionThatOpensClearsTheOfferToTakeItOver() {
        let model = freshModel()
        model.apply(.control(.remoteBusy(holder: "192.168.1.5", heldSecs: 754, takenOver: false)))
        model.apply(.control(.connected(connected(to: "mac"))))
        #expect(model.session.remoteBusy == nil)
    }

    /// "12m" rather than "754s". The precision matters because the number is only
    /// ever read by a person deciding whether to interrupt somebody.
    @Test
    func theHeldDurationReadsAtThePrecisionAGlanceWants() {
        #expect(TargetPickerView.heldFor(0) == "0s")
        #expect(TargetPickerView.heldFor(59) == "59s")
        #expect(TargetPickerView.heldFor(60) == "1m")
        #expect(TargetPickerView.heldFor(754) == "12m")
        #expect(TargetPickerView.heldFor(3599) == "59m")
        #expect(TargetPickerView.heldFor(3600) == "1h 0m")
        #expect(TargetPickerView.heldFor(3600 * 5 + 60 * 7) == "5h 7m")
    }
}
