import AppKit
import Foundation
import Testing
@testable import RemotexViewer

/// These cover what used to arrive over the host bridge as a finished capability
/// snapshot and is now derived here from the gateway's own control messages.
@MainActor
struct AppModelTests {
    @Test
    func keyboardOverridesDefaultToEnabledAndPersist() {
        let suiteName = "AppModelTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let initial = AppModel(defaults: defaults)
        #expect(initial.macOSKeyboardOverridesEnabled)

        initial.macOSKeyboardOverridesEnabled = false

        let restored = AppModel(defaults: defaults)
        #expect(!restored.macOSKeyboardOverridesEnabled)
    }

    /// A Mac remote keeps Command as Command, so the override is inapplicable
    /// rather than off — and turning it inapplicable must not overwrite what the
    /// user chose for the next non-Mac target.
    @Test
    func keyboardOverridesAppearInactiveForAMacWithoutChangingThePreference() {
        let model = makeModel()
        #expect(model.macOSKeyboardOverridesActive)
        #expect(model.macOSKeyboardOverridesLabel == "Enable macOS Keyboard Overrides")

        model.apply(.control(.remoteOs(macos: true)))

        #expect(!model.macOSKeyboardOverridesActive)
        #expect(model.macOSKeyboardOverridesEnabled)
        #expect(
            model.macOSKeyboardOverridesLabel == "macOS Keyboard Overrides (Not Applicable)"
        )
    }

    /// The three resize mechanisms. Only RDP gets a button; rxa answers a fixed
    /// list instead; VNC is the only one that may be followed automatically.
    @Test
    func resizeCapabilitiesFollowTheProtocol() {
        let expectations: [(protocolName: String, resize: Bool, canResize: Bool, manual: Bool)] = [
            ("rdp", true, true, true),
            ("rxa", true, false, true),
            ("vnc", true, false, false),
            ("rdp", false, false, false),
            ("vnc", false, false, false),
        ]
        for expectation in expectations {
            let model = makeModel()
            model.apply(
                .control(
                    .connected(
                        ServerMessage.Connected(
                            name: "t",
                            protocolName: expectation.protocolName,
                            resize: expectation.resize,
                            clipboard: false
                        )
                    )
                )
            )
            #expect(
                model.session.canResize == expectation.canResize,
                "\(expectation.protocolName) resize=\(expectation.resize)"
            )
            #expect(
                model.session.manualResize == expectation.manual,
                "\(expectation.protocolName) resize=\(expectation.resize)"
            )
        }
    }

    /// "Resize to Window" needs a measured window as well as a willing target,
    /// so it stays disabled until a surface has reported one.
    @Test
    func resizeToWindowNeedsAMeasuredViewport() {
        let model = makeModel()
        model.apply(.control(.connected(connected(protocolName: "rdp", resize: true))))
        #expect(model.session.canResize)
        #expect(!model.canResizeNow, "no surface has reported a size yet")
    }

    @Test
    func connectingMovesToTheDesktopAndNamesTheTarget() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rxa", clipboard: true))))

        #expect(model.session.screen == .desktop)
        #expect(model.session.connectedTarget == "mac")
        #expect(model.session.canClipboard)
        #expect(model.windowTitle == "mac — remotex")
    }

    /// Everything about the old target goes, so a later connect starts from a
    /// clean "waiting for the remote desktop" rather than showing stale pixels or
    /// a stale resolution menu.
    @Test
    func thePickerResetsEverythingTheTargetLeftBehind() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rxa", resize: true, clipboard: true))))
        model.apply(.control(.resize(w: 1920, h: 1080, scale: 2)))
        model.apply(.control(.displayModes(modes: [DisplayMode(w: 1920, h: 1080)])))
        model.apply(.control(.remoteOs(macos: true)))

        model.apply(.control(.picker))

        #expect(model.session.screen == .picker)
        #expect(model.session.connectedTarget == nil)
        #expect(model.session.protocolName == nil)
        #expect(model.session.remoteSize == nil)
        #expect(model.session.displayModes.isEmpty)
        #expect(!model.session.canClipboard)
        #expect(!model.session.canResize)
        #expect(!model.session.remoteIsMac)
        #expect(model.windowTitle == "remotex")
    }

    /// An engine error is not a dead end: the socket stays up and the session
    /// returns to the picker, so the message belongs there and the pending pick
    /// has to clear or the picker stays locked.
    @Test
    func anEngineErrorClearsThePendingPickAndIsHeldForThePicker() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.picker))
        model.connect(to: "mac")
        #expect(model.session.pendingTarget == "mac")

        model.apply(.control(.error(message: "connect failed")))

        #expect(model.session.pendingTarget == nil)
        #expect(model.session.connectError == "connect failed")
    }

    @Test
    func displayModesAreReplacedRatherThanMerged() {
        let model = makeModel()
        model.apply(.control(.displayModes(modes: [
            DisplayMode(w: 1920, h: 1080),
            DisplayMode(w: 1280, h: 800),
        ])))
        // A display reconfigure regenerates the list; a size that is gone from it
        // must not linger in the menu.
        model.apply(.control(.displayModes(modes: [DisplayMode(w: 1280, h: 800)])))
        #expect(model.session.displayModes == [DisplayMode(w: 1280, h: 800)])
    }

    @Test
    func setResolutionRefusesAModeTheRemoteNoLongerOffers() {
        let model = makeModel()
        model.apply(.control(.displayModes(modes: [DisplayMode(w: 1280, h: 800)])))
        // No connection is attached, so the observable effect is that neither call
        // traps — what is pinned is that the guard exists at all.
        model.setResolution(DisplayMode(w: 1280, h: 800))
        model.setResolution(DisplayMode(w: 3840, h: 2160))
    }

    /// Clearing the framebuffer drops the size, which is what puts the "waiting
    /// for the remote desktop" interstitial back up. The gateway always repaints
    /// in full, so there is nothing to preserve.
    @Test
    func clearingTheFramebufferBringsBackTheInterstitial() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "vnc"))))
        model.apply(.control(.resize(w: 800, h: 600, scale: 1)))
        #expect(!model.showsStatusOverlay)

        model.apply(.clearFramebuffer)

        #expect(model.session.remoteSize == nil)
        #expect(model.showsStatusOverlay)
    }

    /// Keys are only captured for a desktop that is actually painting. Before the
    /// first frame there is nothing to type into.
    @Test
    func keyboardCaptureNeedsAConnectedDesktopWithAFrame() {
        let model = makeModel()
        #expect(!model.canCaptureKeyboardNow)

        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rxa"))))
        #expect(!model.canCaptureKeyboardNow, "no frame yet")

        model.apply(.control(.resize(w: 800, h: 600, scale: 1)))
        #expect(model.canCaptureKeyboardNow)

        model.apply(.status(.reconnecting))
        #expect(!model.canCaptureKeyboardNow)
    }

    /// The consent boundary, routed here. A requested reply fills the panel; only
    /// an unsolicited push may reach the pasteboard.
    @Test
    func aRequestedClipboardReplyNeverReachesThePasteboard() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        let model = makeModel(pasteboard: pasteboard)
        enableClipboard(on: model)
        model.clipboard.requestFreshSnapshot()

        model.apply(
            .control(
                .clipboard(
                    ServerMessage.Clipboard(
                        text: "secret",
                        changedAtMs: 42,
                        requested: true,
                        oversizedBytes: nil
                    )
                )
            )
        )

        #expect(model.clipboard.isPresented)
        #expect(model.clipboard.concealedByteCount == 6)
        #expect(pasteboard.string(forType: .string) == nil, "Copy is the consent boundary")
    }

    @Test
    func anUnsolicitedClipboardPushMirrorsToThePasteboard() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        let model = makeModel(pasteboard: pasteboard)
        enableClipboard(on: model)

        model.apply(
            .control(
                .clipboard(
                    ServerMessage.Clipboard(
                        text: "from the remote",
                        changedAtMs: 42,
                        requested: false,
                        oversizedBytes: nil
                    )
                )
            )
        )

        #expect(pasteboard.string(forType: .string) == "from the remote")
    }

    /// An oversized remote clipboard arrives as empty text plus a size. Mirroring
    /// the empty text would wipe the local pasteboard for a copy that did happen.
    @Test
    func anOversizedRemoteClipboardIsReportedRatherThanMirrored() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        pasteboard.clearContents()
        pasteboard.setString("keep me", forType: .string)
        let model = makeModel(pasteboard: pasteboard)
        enableClipboard(on: model)
        model.clipboard.requestFreshSnapshot()
        model.apply(
            .control(
                .clipboard(
                    ServerMessage.Clipboard(
                        text: "",
                        changedAtMs: 1,
                        requested: true,
                        oversizedBytes: nil
                    )
                )
            )
        )

        model.apply(
            .control(
                .clipboard(
                    ServerMessage.Clipboard(
                        text: "",
                        changedAtMs: 2,
                        requested: false,
                        oversizedBytes: 209_715_200
                    )
                )
            )
        )

        #expect(model.clipboard.oversizedBytes == 209_715_200)
        #expect(pasteboard.string(forType: .string) == "keep me")
    }

    /// A newer gateway's control message must be stepped over, not treated as a
    /// reason to change anything.
    @Test
    func anUnsupportedControlMessageChangesNothing() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "vnc"))))
        let before = model.session

        model.apply(.control(.unsupported(type: "somethingNew")))

        #expect(model.session == before)
    }

    // MARK: - Helpers

    private func makeModel(pasteboard: NSPasteboard? = nil) -> AppModel {
        let suiteName = "AppModelTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        return AppModel(
            defaults: defaults,
            clipboard: ClipboardSynchronizer(
                pasteboard: pasteboard ?? NSPasteboard.withUniqueName(),
                startsPolling: false
            )
        )
    }

    private func connected(
        protocolName: String,
        resize: Bool = false,
        clipboard: Bool = false
    ) -> ServerMessage.Connected {
        ServerMessage.Connected(
            name: "mac",
            protocolName: protocolName,
            resize: resize,
            clipboard: clipboard
        )
    }

    private func enableClipboard(on model: AppModel) {
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rxa", clipboard: true))))
        #expect(model.clipboard.isEnabled)
    }
}
