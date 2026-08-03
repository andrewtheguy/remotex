import Foundation
import Testing
@testable import RemotexViewer

/// What the menu bar does, which is the app's whole job above the page.
///
/// Two rules are being checked, and they are the ones that make a menu honest:
/// an item is live only where the client says the thing is possible, and pressing
/// it sends a request rather than changing anything here. Nothing in this app may
/// decide it has done something the client has not reported doing.
@MainActor
struct MenuCommandTests {
    @Test
    func offTheDesktopEveryRemoteCommandIsDead() {
        let sink = RecordingSink()
        let model = AppModel.underTest(sink: sink)

        // The picker: a live client, but nothing to act on.
        var picker = NativeState()
        picker.mode = .picker
        picker.status = .connected
        model.apply(.state(picker))

        #expect(!model.isOnDesktop)
        #expect(!model.canClipboard)
        #expect(!model.canAudio)
        #expect(!model.canResizeNow)
        #expect(!model.canResizeToDisplay)
        #expect(!model.canCaptureKeyboardNow)
        #expect(model.displays.isEmpty)
        #expect(model.takeOverTitle == nil)
    }

    /// Capability flags come from the target, and each one gates exactly its own
    /// item. Checked together because the failure they guard against is one flag
    /// enabling the wrong item, which no single case can see.
    @Test
    func eachCapabilityGatesItsOwnItem() {
        let model = AppModel.underTest(sink: RecordingSink())

        model.apply(.state(AppModel.desktopState(canClipboard: true)))
        #expect(model.canClipboard)
        #expect(!model.canAudio)

        model.apply(.state(AppModel.desktopState(canAudio: true)))
        #expect(model.canAudio)
        #expect(!model.canClipboard)
    }

    /// The audio toggle reports what the client is doing, not what was pressed:
    /// a target with no sound leaves the item greyed, and the tick follows the
    /// client's own answer.
    @Test
    func theAudioItemAsksAndThenFollowsTheAnswer() {
        let sink = RecordingSink()
        let model = AppModel.underTest(sink: sink)
        model.apply(.state(AppModel.desktopState(canAudio: true)))

        #expect(model.canAudio)
        #expect(!model.audioEnabled)

        model.setAudioEnabled(true)
        #expect(sink.sent(.setAudio(true)))
        #expect(!model.audioEnabled, "asking is not the same as playing")

        model.apply(.state(AppModel.desktopState(canAudio: true, audioEnabled: true)))
        #expect(model.audioEnabled)
        #expect(model.windowTitle.hasSuffix("🔊"), "the one surface that can say so")
    }

    /// Take Over is absent rather than greyed when nobody else holds the session:
    /// an item that is disabled reads as an action that is unavailable, and this
    /// one has no meaning at all until somebody else is on the desktop.
    @Test
    func takeOverAppearsOnlyWhenSomebodyElseHasTheSession() {
        let sink = RecordingSink()
        let model = AppModel.underTest(sink: sink)

        model.apply(.state(AppModel.desktopState()))
        #expect(model.takeOverTitle == nil)

        model.apply(.state(AppModel.desktopState(status: .busy)))
        #expect(model.takeOverTitle == "Take Over Session")

        model.apply(.state(AppModel.desktopState(status: .takenOver)))
        #expect(model.takeOverTitle == "Take Session Back")

        model.takeOver()
        #expect(sink.sent(.takeOver))
    }

    /// The Mac-keyboard item says why it is off when the reason is the guest
    /// rather than the preference — an unticked box on a Mac remote would read as
    /// something somebody turned off.
    @Test
    func theKeyboardOverrideItemNamesTheCaseItCannotHelpWith() {
        let sink = RecordingSink()
        let model = AppModel.underTest(sink: sink)

        model.apply(.state(AppModel.desktopState()))
        #expect(model.macOSKeyboardOverridesLabel == "Enable macOS Keyboard Overrides")
        #expect(model.macOSKeyboardOverridesActive)

        model.apply(
            .state(AppModel.desktopState(macKeyOverridesActive: false, remoteIsMac: true))
        )
        #expect(
            model.macOSKeyboardOverridesLabel
                == "Enable macOS Keyboard Overrides (Not Applicable)"
        )
        #expect(!model.macOSKeyboardOverridesActive)

        model.setMacKeyOverrides(false)
        #expect(sink.sent(.setMacKeyOverrides(false)))
    }

    /// The gateway's own name, which the window title, the About item and the
    /// launch screen all read.
    ///
    /// It comes off the page because the page is the only half that talks to the
    /// gateway. This is a regression test: the app used to fetch `/api/config`
    /// itself, and when the session moved to the client that fetch went with it —
    /// leaving a stored property nothing assigned, so a gateway branded "QA" still
    /// put "remotex" in the title bar and in "About remotex".
    @Test
    func thebrandingComesFromThePage() {
        let model = AppModel.underTest(sink: RecordingSink())
        #expect(model.branding == "remotex", "until a page has said otherwise")

        var state = AppModel.desktopState()
        state.branding = "QA"
        model.apply(.state(state))
        #expect(model.branding == "QA")
        #expect(model.windowTitle == "QA")

        // And with sound playing, where the title carries the speaker too.
        state.audioEnabled = true
        state.canAudio = true
        model.apply(.state(state))
        #expect(model.windowTitle == "QA 🔊")
    }

    /// The keyboard-override item is greyed by one rule, in one place. Off the
    /// desktop and against a Mac guest are different reasons for the same answer,
    /// and the label says which — so the two must not be able to disagree.
    @Test
    func theKeyboardOverrideRuleLivesInOnePlace() {
        let model = AppModel.underTest(sink: RecordingSink())
        #expect(!model.canOverrideMacKeys, "no desktop, nothing to translate for")

        model.apply(.state(AppModel.desktopState()))
        #expect(model.canOverrideMacKeys)

        model.apply(.state(AppModel.desktopState(remoteIsMac: true)))
        #expect(!model.canOverrideMacKeys)
        #expect(model.macOSKeyboardOverridesLabel.hasSuffix("(Not Applicable)"))
    }

    /// The display list is the client's, and the checkmark moves only when the
    /// remote confirms — so a display the Mac refused leaves the menu agreeing with
    /// what is on screen rather than with what was clicked.
    @Test
    func theDisplayMenuFollowsTheRemoteRatherThanTheClick() {
        let sink = RecordingSink()
        let model = AppModel.underTest(sink: sink)
        let displays = [
            DisplayChoice(id: 1, label: "Built-in", detail: "1512×982 at 2x"),
            DisplayChoice(id: 2, label: "Studio Display", detail: "2560×1440"),
        ]
        model.apply(.state(AppModel.desktopState(displays: displays, activeDisplayId: 1)))

        #expect(model.displays == displays)
        #expect(model.activeDisplayID == 1)

        model.selectDisplay(2)
        #expect(sink.sent(.selectDisplay(2)))
        #expect(model.activeDisplayID == 1, "the tick waits for the remote's answer")

        model.apply(.state(AppModel.desktopState(displays: displays, activeDisplayId: 2)))
        #expect(model.activeDisplayID == 2)
    }

    /// Keyboard capture belongs to a desktop with something painted on it. Before
    /// the first frame there is nothing to type at, and capturing would swallow
    /// ⌘Q on the way to a session that may never come up.
    @Test
    func keyboardCaptureWaitsForTheFirstFrame() {
        let model = AppModel.underTest(sink: RecordingSink())

        model.apply(.state(AppModel.desktopState(status: .connecting, size: nil)))
        #expect(!model.canCaptureKeyboardNow)

        model.apply(.state(AppModel.desktopState(size: nil)))
        #expect(!model.canCaptureKeyboardNow, "connected, but nothing painted yet")

        model.apply(.state(AppModel.desktopState()))
        #expect(model.canCaptureKeyboardNow)
    }

    /// The pasteboard follows the target's own permission, and stops following when
    /// the desktop goes away: polling somebody's clipboard on a screen that is not
    /// showing their desktop is not something to leave running.
    @Test
    func thePasteboardIsWatchedOnlyForATargetThatAsked() {
        let model = AppModel.underTest(sink: RecordingSink())

        model.apply(.state(AppModel.desktopState(canClipboard: false)))
        #expect(!model.clipboard.isEnabled)

        model.apply(.state(AppModel.desktopState(canClipboard: true)))
        #expect(model.clipboard.isEnabled)

        var picker = NativeState()
        picker.mode = .picker
        model.apply(.state(picker))
        #expect(!model.clipboard.isEnabled)
    }

    /// A gateway that will not take its own launch token cannot be argued with from
    /// the page: there is no login here to get it wrong. The app takes the screen
    /// back and offers the restart that mints a new one.
    @Test
    func arefusedTokenReturnsToTheAppsOwnScreen() {
        let model = AppModel.underTest(sink: RecordingSink())
        model.apply(.state(AppModel.desktopState()))

        model.apply(.unauthenticated)

        #expect(model.screen == .launching)
        #expect(model.launchError != nil)
    }

    /// Letting go of the page forgets what it said. A menu left describing the last
    /// session would offer Take Over on a window with nothing in it.
    @Test
    func releasingThePageClearsWhatItReported() {
        let sink = RecordingSink()
        let model = AppModel.underTest(sink: sink)
        model.apply(.state(AppModel.desktopState(canClipboard: true, canAudio: true)))
        #expect(model.isOnDesktop)

        model.release(bridge: sink)

        #expect(!model.isOnDesktop)
        #expect(!model.canAudio)
        #expect(!model.clipboard.isEnabled)
    }

    /// And only for the page that is going. SwiftUI can build the replacement
    /// surface before dismantling the old one, in which case the bridge on screen
    /// is already the new one — clearing on the old one's teardown would blank a
    /// live desktop's menus with nothing to put them back.
    @Test
    func releasingAnOlderPageLeavesTheCurrentOneAlone() {
        let old = RecordingSink()
        let model = AppModel.underTest(sink: old)
        let new = RecordingSink()
        model.attach(bridge: new)
        model.apply(.state(AppModel.desktopState()))

        model.release(bridge: old)

        #expect(model.isOnDesktop)
        model.refresh()
        #expect(new.sent(.refresh))
        #expect(old.commands.isEmpty)
    }
}
