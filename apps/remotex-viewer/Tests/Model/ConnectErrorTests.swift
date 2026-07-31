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

}
