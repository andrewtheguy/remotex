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

    /// The three resize behaviours. Only RDP gets a button; VNC is followed
    /// automatically; a Mac is never resized from here at all. (`rxa` with
    /// `resize` is included because an older gateway could still send it — the
    /// config layer rejects it now.)
    @Test
    func resizeCapabilitiesFollowTheProtocol() {
        let expectations: [(protocolName: String, resize: Bool, canResize: Bool)] = [
            ("rdp", true, true),
            ("rxa", true, false),
            ("vnc", true, false),
            ("rdp", false, false),
            ("vnc", false, false),
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

    /// The pair is mutually exclusive, and the menu shows both either way. A target
    /// that takes a size from here resizes the remote; every other one resizes this
    /// window, which is the only side that can move for it.
    @Test
    func exactlyOneOfTheTwoResizeDirectionsIsEverAvailable() {
        let expectations: [(protocolName: String, resize: Bool, toWindow: Bool)] = [
            ("rdp", true, true),
            ("rxa", false, false),
            ("vnc", true, false),
            ("vnc", false, false),
            ("rdp", false, false),
        ]
        for expectation in expectations {
            let model = makeModel()
            model.apply(.status(.connected))
            model.apply(
                .control(
                    .connected(
                        connected(
                            protocolName: expectation.protocolName,
                            resize: expectation.resize
                        )
                    )
                )
            )
            model.apply(.control(.resize(w: 1920, h: 1080, scale: 1)))
            model.reportViewport(DisplayMode(w: 1600, h: 900))

            let label: Comment = "\(expectation.protocolName) resize=\(expectation.resize)"
            #expect(model.canResizeNow == expectation.toWindow, label)
            #expect(model.canResizeToDisplay == !expectation.toWindow, label)
        }
    }

    /// Both are disabled off the desktop and before the first `resize`: there is no
    /// remote to fit the window to, and a window fitted to nothing is a window
    /// resized for no reason.
    @Test
    func resizingToTheDisplayNeedsARemoteToFitTo() {
        let model = makeModel()
        var fitted = 0
        model.fitWindowToRemote = { fitted += 1 }

        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rxa"))))
        #expect(!model.canResizeToDisplay, "no size has arrived yet")
        model.resizeToDisplay()
        #expect(fitted == 0)

        model.apply(.control(.resize(w: 3200, h: 2000, scale: 2)))
        #expect(model.canResizeToDisplay)
        model.resizeToDisplay()
        #expect(fitted == 1)

        model.apply(.control(.picker))
        #expect(!model.canResizeToDisplay)
        model.resizeToDisplay()
        #expect(fitted == 1, "the picker has no desktop to fit")
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
        model.apply(.control(.remoteOs(macos: true)))
        model.apply(.control(.displays(active: 7, displays: twoDisplays)))
        #expect(!model.session.displays.isEmpty, "there is something to reset")

        model.apply(.control(.picker))

        #expect(model.session.screen == .picker)
        #expect(model.session.connectedTarget == nil)
        #expect(model.session.protocolName == nil)
        #expect(model.session.remoteSize == nil)
        // The Retina Mac's density goes with it: kept, it would double the
        // viewport reported for the next target before its own resize lands.
        #expect(model.session.remoteScale == 1)
        #expect(!model.session.canClipboard)
        #expect(!model.session.canResize)
        #expect(!model.session.remoteIsMac)
        #expect(model.session.displays.isEmpty)
        #expect(model.session.activeDisplayID == nil)
        #expect(model.windowTitle == "remotex")
    }

    /// The Display menu is the whole of what the viewer knows about the remote's
    /// screens: it holds no list of its own and derives the checkmark from
    /// `activeDisplayID`, so both have to survive the message intact.
    @Test
    func displaysArriveWithTheActiveOneMarked() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rxa"))))
        #expect(model.session.displays.isEmpty, "nothing to pick until the remote says so")

        model.apply(.control(.displays(active: 9, displays: twoDisplays)))

        #expect(model.session.displays == twoDisplays)
        #expect(model.session.activeDisplayID == 9)
    }

    /// The checkmark follows the remote, never the click. A selection that the
    /// remote refused, or has not answered yet, must leave the menu agreeing with
    /// what is actually on screen.
    @Test
    func selectingADisplayDoesNotMoveTheCheckmarkOnItsOwn() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rxa"))))
        model.apply(.control(.displays(active: 7, displays: twoDisplays)))

        model.selectDisplay(9)
        #expect(
            model.session.activeDisplayID == 7,
            "still on display 7 until the remote reports the switch"
        )

        model.apply(.control(.displays(active: 9, displays: twoDisplays)))
        #expect(model.session.activeDisplayID == 9)
    }

    /// The gateway address is read from the defaults this model was handed, which
    /// is what `--settings` relies on: a QA run must not read — or later write —
    /// the address a real one left in `UserDefaults.standard`.
    @Test
    func theGatewayAddressComesFromTheDefaultsThisModelWasGiven() {
        let defaults = UserDefaults(suiteName: "AppModelTests.\(UUID().uuidString)")!
        // Stored as `GatewayLocation` normalizes it, trailing slash and all —
        // this is a round trip through the same parse a real launch does.
        defaults.set("http://10.0.0.9:52380/", forKey: "gatewayAddress")
        let model = AppModel(
            defaults: defaults,
            clipboard: ClipboardSynchronizer(
                pasteboard: NSPasteboard.withUniqueName(),
                startsPolling: false
            )
        )
        #expect(model.gatewayAddress == "http://10.0.0.9:52380/")
    }

    /// `--settings <name>` is the flag that keeps a QA run's gateway address and
    /// login off the real ones. Parsed from an argument list rather than from
    /// `ProcessInfo`, which a test process cannot choose.
    @Test
    func theSettingsFlagNamesASuiteOnlyWhenItHasOne() {
        #expect(ViewerDefaults.settingsName(in: ["remotex-viewer"]) == nil)
        #expect(
            ViewerDefaults.settingsName(
                in: ["remotex-viewer", "--settings", "qa", "--gateway", "http://x"]
            ) == "qa"
        )
        // A trailing flag, or one with only spaces after it, is a mistake — and
        // must not become a suite named "".
        #expect(ViewerDefaults.settingsName(in: ["remotex-viewer", "--settings"]) == nil)
        #expect(ViewerDefaults.settingsName(in: ["remotex-viewer", "--settings", "  "]) == nil)
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

    /// A Mac with a screen somebody is at and an extra one the agent made.
    private let twoDisplays: [ServerMessage.DisplayInfo] = [
        .init(
            id: 7,
            label: "Display 1",
            detail: "1920×1080 at 1x",
            main: true,
            isVirtual: false
        ),
        .init(
            id: 9,
            label: "Virtual display",
            detail: "3200×2000 at 2x",
            main: false,
            isVirtual: true
        ),
    ]

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
