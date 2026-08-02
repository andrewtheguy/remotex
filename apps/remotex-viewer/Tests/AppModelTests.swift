import AppKit
import Foundation
import Testing
@testable import RemotexViewer

/// These cover what used to arrive over the host bridge as a finished capability
/// snapshot and is now derived here from the gateway's own control messages.
@MainActor
struct AppModelTests {
    /// The one preference this client keeps, in the instance directory with
    /// everything else — see `ViewerPreferences` for why that is not a defaults
    /// suite any more.
    @Test
    func keyboardOverridesDefaultToEnabledAndPersist() throws {
        let directory = try ScratchDirectory()
        let url = directory.url.appending(path: "viewer.json")

        let initial = AppModel(preferences: ViewerPreferences(url: url))
        #expect(initial.macOSKeyboardOverridesEnabled)

        initial.macOSKeyboardOverridesEnabled = false

        let restored = AppModel(preferences: ViewerPreferences(url: url))
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

    /// What a `connected` alone settles, which is permission and not behaviour —
    /// and there are two of them. `resize` is whether this session may resize the
    /// remote when the user asks; `autoResize` is whether the window may drive it,
    /// which the gateway grants to plain VNC alone. The client takes both verbatim:
    /// it decides neither, and the row where they differ is the RDP one.
    @Test
    func bothResizePermissionsFollowTheTarget() {
        let expectations:
            [(protocolName: String, resize: Bool, autoResize: Bool)] = [
                ("vnc", true, true),
                ("rdp", true, false),
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
                            autoResize: expectation.autoResize,
                            clipboard: false,
                            audio: false
                        )
                    )
                )
            )
            #expect(
                model.session.canResize == expectation.resize,
                "\(expectation.protocolName) resize=\(expectation.resize)"
            )
            #expect(
                model.session.canAutoResize == expectation.autoResize,
                "\(expectation.protocolName) autoResize=\(expectation.autoResize)"
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

    /// The two one-shots are independent, and the menu shows both either way.
    /// Pushing the window's size to the remote needs a target that takes one;
    /// pulling the remote's size into the window needs only a remote that will hold
    /// still — so a target that allows the first allows both, and a target that
    /// allows neither can still be fitted to.
    @Test
    func aTargetThatWillNotResizeCanStillBeFittedTo() {
        let expectations: [(protocolName: String, resize: Bool, toWindow: Bool, toDisplay: Bool)] = [
            ("rdp", true, true, true),
            ("vnc", true, true, true),
            // The gateway drops this one's reports, so its desktop holds still and
            // can be fitted to like any other.
            ("vnc", false, false, true),
            ("rdp", false, false, true),
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
            #expect(model.canResizeToDisplay == expectation.toDisplay, label)
        }
    }

    /// And auto resize is what greys them now: one of them is what auto does
    /// continuously, and the other cannot fit a window to a desktop that is already
    /// fitting itself to the window. Switching back restores both, so the mode is
    /// never a trap.
    @Test
    func autoResizeGreysBothOneShots() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "vnc", resize: true))))
        model.apply(.control(.resize(w: 1920, h: 1080, scale: 1)))
        model.reportViewport(DisplayMode(w: 1600, h: 900))
        #expect(model.canAutoResize)
        #expect(model.canResizeNow)
        #expect(model.canResizeToDisplay)

        model.setAutoResize(true)
        #expect(model.autoResizes)
        #expect(!model.canResizeNow)
        #expect(!model.canResizeToDisplay)

        model.setAutoResize(false)
        #expect(model.canResizeNow)
        #expect(model.canResizeToDisplay)
    }

    /// The mode is offered only where a resize is allowed at all, and asking for it
    /// anyway changes nothing — a menu item cannot be clicked while greyed, and this
    /// is the model saying so on its own.
    @Test
    func autoResizeIsRefusedWhereResizingIsNotAllowed() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "vnc", resize: false))))
        #expect(!model.canAutoResize)

        model.setAutoResize(true)
        #expect(!model.autoResizes)
    }

    /// Turning auto-resize on is now a lasting choice, not a per-session one: it
    /// writes the remembered default, so the next compatible target inherits it.
    /// The picker screen in between shows manual — it has no live session to follow.
    @Test
    func autoResizeIsRememberedAndInheritedByTheNextTarget() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "vnc", resize: true))))
        model.setAutoResize(true)
        #expect(model.autoResizes)

        model.apply(.control(.picker))
        #expect(!model.autoResizes, "the picker has no live session to follow")

        model.apply(.control(.connected(connected(protocolName: "vnc", resize: true))))
        #expect(model.autoResizes, "a compatible target inherits the remembered default")
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
        model.apply(.control(.connected(connected(protocolName: "vnc"))))
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
        model.apply(.control(.connected(connected(protocolName: "vnc", clipboard: true))))

        #expect(model.session.screen == .desktop)
        #expect(model.session.connectedTarget == "mac")
        #expect(model.session.canClipboard)
        #expect(model.windowTitle == "mac — remotex")
    }

    /// A speaker joins the window title while sound is playing, and only then — the
    /// one persistent surface that can show it, since the toggle is a menu item.
    @Test
    func theWindowTitleGainsASpeakerWhileAudioPlays() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rdp", audio: true))))
        #expect(model.windowTitle == "mac — remotex", "silent unless asked for")

        model.setAudioEnabled(true)
        #expect(model.windowTitle == "mac — remotex 🔊")

        model.setAudioEnabled(false)
        #expect(model.windowTitle == "mac — remotex")
    }

    /// Everything about the old target goes, so a later connect starts from a
    /// clean "waiting for the remote desktop" rather than showing stale pixels or
    /// a stale resolution menu.
    @Test
    func thePickerResetsEverythingTheTargetLeftBehind() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "vnc", resize: true, clipboard: true))))
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
        model.apply(.control(.connected(connected(protocolName: "vnc"))))
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
        model.apply(.control(.connected(connected(protocolName: "vnc"))))
        model.apply(.control(.displays(active: 7, displays: twoDisplays)))

        model.selectDisplay(9)
        #expect(
            model.session.activeDisplayID == 7,
            "still on display 7 until the remote reports the switch"
        )

        model.apply(.control(.displays(active: 9, displays: twoDisplays)))
        #expect(model.session.activeDisplayID == 9)
    }

    /// `--instance-dir <path>` is the whole of this app's configurability, and the one
    /// thing a QA run needs: everything the launch reads or writes is under the
    /// directory it names. Parsed from an argument list rather than from
    /// `ProcessInfo`, which a test process cannot choose.
    @Test
    func theInstanceFlagNamesADirectoryOnlyWhenItHasOne() {
        #expect(InstanceDirectory.named(in: ["remotex-viewer"]) == nil)
        #expect(
            InstanceDirectory.named(in: ["remotex-viewer", "--instance-dir", "/tmp/qa"])?.path
                == "/tmp/qa"
        )
        // A trailing flag, or one with only spaces after it, is a mistake — and must
        // fall back to the real instance rather than to a directory called "".
        #expect(InstanceDirectory.named(in: ["remotex-viewer", "--instance-dir"]) == nil)
        #expect(InstanceDirectory.named(in: ["remotex-viewer", "--instance-dir", "  "]) == nil)
        // A relative path is resolved, because an app launched by `open` inherits `/`
        // as its working directory and would otherwise write somewhere nobody meant.
        let relative = InstanceDirectory.named(in: ["x", "--instance-dir", "qa/here"])
        #expect(relative?.path.hasSuffix("qa/here") == true)
        #expect(relative?.path.hasPrefix("/") == true)
    }

    /// The app asks before it does anything. Nothing is contacted, no gateway is
    /// started, and no input reaches a remote from here — the whole point of the
    /// screen is that which gateway to use is a question only the user can answer.
    @Test
    func theAppStartsOnTheHomeScreenHavingContactedNothing() {
        let model = makeModel()
        #expect(model.session.screen == .home)
        #expect(model.chosen == nil, "nothing chosen, so nothing local to act on")
        #expect(!model.usesEmbeddedGateway)
        #expect(!model.canSendInput)
        #expect(model.homeError == nil, "not a failure yet — nothing has been tried")
        #expect(model.launchError == nil)
        // Unbundled, so there is no gateway binary to run and no config store over it.
        #expect(model.gateway == nil)
        #expect(model.config == nil)
    }

    /// A build with no gateway beside it has one option, so the home screen comes up
    /// on it rather than on the one that cannot work.
    @Test
    func aBuildWithNoBundledGatewayOffersTheRemoteOptionFirst() {
        #expect(makeModel().prefersRemoteGateway)
    }

    /// Choosing the local gateway without one in the bundle says so rather than
    /// looking like a network problem, and leaves the app where it can be retried.
    ///
    /// Unreachable from the home screen, which disables the option — this is the
    /// model's own guard, and it has to hold because `chooseGateway` is what a
    /// keyboard default action reaches.
    @Test
    func choosingAMissingLocalGatewaySaysSo() async {
        let model = makeModel()
        model.prefersRemoteGateway = false

        await model.chooseGateway()

        #expect(model.session.screen == .launching)
        let error = model.launchError ?? ""
        #expect(error.contains("incomplete"), "got: \(error)")
        #expect(!model.isBusy, "and the retry button is usable")
    }

    /// An address that is not one never leaves the home screen: it is answered where
    /// it was typed, before a process is started or a request is made.
    @Test
    func anUnusableAddressIsRefusedOnTheHomeScreen() async {
        let model = makeModel()
        model.prefersRemoteGateway = true

        for address in ["", "   ", "ftp://remotex.example.com", "http://"] {
            model.gatewayAddress = address
            await model.chooseGateway()
            #expect(model.session.screen == .home, "for \(address.debugDescription)")
            #expect(model.homeError != nil, "for \(address.debugDescription)")
            #expect(model.chosen == nil, "for \(address.debugDescription)")
        }
    }

    /// A remote gateway that answers is chosen, and its login is what comes next.
    ///
    /// Also what is *remembered*, and when: the address only after the gateway
    /// answered, so a typo never becomes the address the next launch starts from.
    @Test
    func aRemoteGatewayThatAnswersLandsOnItsLoginScreen() async throws {
        let directory = try ScratchDirectory()
        let preferences = ViewerPreferences(url: directory.url.appending(path: "viewer.json"))
        let (model, address) = makeRemoteModel(preferences: preferences) { request in
            request.url?.path.hasSuffix("/auth/status") == true
                ? (200, #"{"authenticated":false}"#)
                : (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
        }
        // Typed the way somebody would: no scheme, and the case they used.
        model.gatewayAddress = model.gatewayAddress
            .replacingOccurrences(of: "https://", with: "")
            .uppercased()

        await model.chooseGateway()

        #expect(model.session.screen == .login)
        #expect(model.chosen == .remote(try GatewayLocation.parse(address)))
        #expect(!model.usesEmbeddedGateway)
        #expect(model.branding == "acme")
        #expect(model.homeError == nil)
        // Normalized on the way through, so what is shown and stored is what answered.
        #expect(model.gatewayAddress == address)
        #expect(preferences.remoteGatewayAddress == address)
        #expect(preferences.prefersRemoteGateway)
    }

    /// The local gateway's config file describes the gateway in *this* bundle.
    /// Against a remote one it is not part of the session — its targets are its own —
    /// so editing it is not offered.
    @Test
    func aRemoteGatewayOffersNoLocalConfiguration() async throws {
        let (model, _) = makeRemoteModel { request in
            request.url?.path.hasSuffix("/auth/status") == true
                ? (200, #"{"authenticated":false}"#)
                : (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
        }
        await model.chooseGateway()

        #expect(!model.usesEmbeddedGateway)
        #expect(!model.canEditConfiguration)
        model.editConfiguration()
        #expect(model.configurationRequests == 0, "refused at the model, not only greyed")

        // The same guard on the other local-only action: there is no process of ours
        // behind a remote gateway to restart.
        await model.relaunchGateway()
        #expect(model.session.screen == .login, "still where it was")
    }

    /// A remote gateway on another wire protocol is refused on the home screen,
    /// before a login is even offered — the viewer ships separately from a remote
    /// gateway, so this is the check that keeps a mismatch from becoming an
    /// unreadable frame later. `configuration()` runs ahead of `authState()`, so the
    /// incompatibility is what the user sees rather than a login they cannot pass.
    @Test
    func anIncompatibleRemoteGatewayIsRefusedBeforeItsLogin() async throws {
        let (model, _) = makeRemoteModel { request in
            request.url?.path.hasSuffix("/auth/status") == true
                ? (200, #"{"authenticated":false}"#)
                : (200, #"{"branding":"acme","protocolVersion":9999}"#)
        }

        await model.chooseGateway()

        #expect(model.session.screen == .home, "not the login screen")
        #expect(model.chosen == nil)
        #expect(model.homeError?.contains("protocol") == true, "got: \(model.homeError ?? "")")
    }

    /// Pointing the app at another copy of *itself* — an embedded gateway, which
    /// refuses `/api/auth/*` — is answered with the reason rather than with a login
    /// form that could never succeed.
    @Test
    func aGatewayWithNoLoginIsRefusedWithTheReason() async throws {
        let (model, _) = makeRemoteModel { request in
            request.url?.path.hasSuffix("/auth/status") == true
                ? (403, "forbidden")
                : (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
        }

        await model.chooseGateway()

        #expect(model.session.screen == .home)
        #expect(model.chosen == nil)
        #expect(model.homeError?.contains("embedded gateway") == true, "got: \(model.homeError ?? "")")
    }

    /// A remembered login is a hint and never an assumption — the gateway's sessions
    /// live in its memory — but when it still holds, the login screen is skipped.
    ///
    /// The claim answers 409, which is a state the user acts on rather than a failure:
    /// it parks the session without opening a socket, so what this asserts is the
    /// screen it got to and not a race with the network.
    @Test
    func arememberedLoginSkipsTheLoginScreen() async throws {
        let directory = try ScratchDirectory()
        let preferences = ViewerPreferences(url: directory.url.appending(path: "viewer.json"))
        preferences.remoteSessionToken = "sess-9"
        let (model, _) = makeRemoteModel(preferences: preferences) { request in
            let path = request.url?.path ?? ""
            if path.hasSuffix("/auth/status") {
                return (200, #"{"authenticated":true}"#)
            }
            if path.hasSuffix("/session") {
                return (409, "another client holds the session")
            }
            return (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
        }

        await model.chooseGateway()

        #expect(model.session.screen == .picker, "past the login, into the session")
        #expect(preferences.remoteSessionToken == "sess-9", "and it is still remembered")
    }

    /// A stored token the gateway no longer knows costs one request and the login
    /// screen — and is forgotten, so the next launch does not present it again.
    @Test
    func aStaleStoredLoginIsDroppedRatherThanRetried() async throws {
        let directory = try ScratchDirectory()
        let preferences = ViewerPreferences(url: directory.url.appending(path: "viewer.json"))
        preferences.remoteSessionToken = "sess-expired"
        let (model, _) = makeRemoteModel(preferences: preferences) { request in
            request.url?.path.hasSuffix("/auth/status") == true
                ? (200, #"{"authenticated":false}"#)
                : (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
        }

        await model.chooseGateway()

        #expect(model.session.screen == .login)
        #expect(preferences.remoteSessionToken == nil)
    }

    /// Changing gateway gives up everything that stood on the one being left: the
    /// session, the stored login, and the screen.
    @Test
    func changingGatewayGivesUpTheLoginAndReturnsHome() async throws {
        let directory = try ScratchDirectory()
        let preferences = ViewerPreferences(url: directory.url.appending(path: "viewer.json"))
        let (model, address) = makeRemoteModel(preferences: preferences) { request in
            request.url?.path.hasSuffix("/auth/status") == true
                ? (200, #"{"authenticated":false}"#)
                : (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
        }
        await model.chooseGateway()
        #expect(model.canChangeGateway)

        await model.changeGateway()

        #expect(model.session.screen == .home)
        #expect(model.chosen == nil)
        #expect(preferences.remoteSessionToken == nil)
        #expect(model.targets.isEmpty)
        #expect(!model.canChangeGateway, "nothing left to change away from")
        // The address survives, because it is what the field comes up filled with.
        #expect(preferences.remoteGatewayAddress == address)
    }

    /// The menu item and the picker's button both go through the model, because a
    /// SwiftUI command cannot present a sheet. Two presses have to be two events, or
    /// opening the panel after cancelling it would do nothing.
    @Test
    func askingForTheConfigurationPanelIsAnEventPerRequest() {
        let model = makeModel()
        // Without a config store there is nothing to edit, so the request is refused
        // rather than opening an empty sheet.
        #expect(model.configurationRequests == 0)
        model.editConfiguration()
        #expect(model.configurationRequests == 0, "no store, no panel")
    }

    /// About is asked for the same way and, unlike the configuration panel, is never
    /// refused: the version is the reason it exists, and a build with no gateway
    /// beside it still has one to show.
    @Test
    func askingForAboutIsAnEventPerRequestAndNeedsNoGateway() {
        let model = makeModel()
        #expect(model.aboutRequests == 0)

        model.showAbout()
        model.showAbout()

        #expect(model.aboutRequests == 2, "two presses, two events")
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
        model.apply(.control(.connected(connected(protocolName: "vnc"))))
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
        AppModel(
            clipboard: ClipboardSynchronizer(
                pasteboard: pasteboard ?? NSPasteboard.withUniqueName(),
                startsPolling: false
            )
        )
    }

    /// A model whose HTTP goes to a stub, for the remote branch.
    ///
    /// Only the HTTP: `URLProtocol` does not intercept `URLSessionWebSocketTask`, so
    /// every test using this stops at a point no socket has been opened from — the
    /// login screen, or a claim the stub answered 409. That is a real limit and not a
    /// convenience: what a session does once it is up belongs to the tests that drive
    /// a scripted transport (`AttachedSession`), and one that let a real socket out
    /// would be testing this machine's network.
    private func makeRemoteModel(
        preferences: ViewerPreferences = ViewerPreferences(url: nil),
        _ handler: @escaping StubURLProtocol.Handler
    ) -> (model: AppModel, address: String) {
        // Its own host, so a suite running beside this one cannot answer its
        // requests — see `StubURLProtocol`.
        let host = StubURLProtocol.uniqueHost()
        StubURLProtocol.register(host: host, handler: handler)
        let model = AppModel(
            preferences: preferences,
            clipboard: ClipboardSynchronizer(
                pasteboard: NSPasteboard.withUniqueName(),
                startsPolling: false
            ),
            urlSession: StubURLProtocol.session()
        )
        model.prefersRemoteGateway = true
        model.gatewayAddress = "https://\(host)"
        return (model, "https://\(host)/")
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

    // MARK: - Remembered resize and audio defaults

    /// Both default off and survive a relaunch, like the keyboard preference — they
    /// are the same kind of lasting choice, kept in the same instance file.
    @Test
    func resizeAndAudioDefaultsAreOffAndPersist() throws {
        let directory = try ScratchDirectory()
        let url = directory.url.appending(path: "viewer.json")

        let initial = AppModel(preferences: ViewerPreferences(url: url))
        #expect(!initial.autoResizeByDefault)
        #expect(!initial.audioByDefault)

        initial.autoResizeByDefault = true
        initial.audioByDefault = true

        let restored = AppModel(preferences: ViewerPreferences(url: url))
        #expect(restored.autoResizeByDefault)
        #expect(restored.audioByDefault)
    }

    /// The remembered auto-resize default takes effect on a target that settles its
    /// permission at `connected`, and does nothing on one that allows no resize —
    /// the "… if compatible" the picker's caption promises, decided by the
    /// `connected` capability rather than by anything the picker knew.
    @Test
    func theAutoResizeDefaultFollowsACompatibleTarget() {
        let willing = makeModel()
        willing.autoResizeByDefault = true
        willing.apply(.status(.connected))
        willing.apply(.control(.connected(connected(protocolName: "vnc", resize: true))))
        #expect(willing.autoResizes, "a resizable target adopts the remembered default")

        let unwilling = makeModel()
        unwilling.autoResizeByDefault = true
        unwilling.apply(.status(.connected))
        unwilling.apply(.control(.connected(connected(protocolName: "vnc", resize: false))))
        #expect(!unwilling.autoResizes, "no permission, so the default does nothing")

        // And the row that is the point of the second permission: a target that
        // resizes when asked but must not be handed the window. The default is
        // silently not applied — "if compatible" covers this too — and the session
        // stays manual, where "Resize to Window" still works.
        let manualOnly = makeModel()
        manualOnly.autoResizeByDefault = true
        manualOnly.apply(.status(.connected))
        manualOnly.apply(
            .control(
                .connected(connected(protocolName: "rdp", resize: true, autoResize: false))
            )
        )
        #expect(manualOnly.session.canResize)
        #expect(!manualOnly.canAutoResize)
        #expect(!manualOnly.autoResizes, "the mode is not on offer, so the default does nothing")
        manualOnly.setAutoResize(true)
        #expect(!manualOnly.autoResizes, "and asking for it anyway changes nothing")
    }

    /// The remembered audio default is adopted on a pick when the target carries
    /// sound, and silently not when it does not or when the default is off.
    @Test
    func theAudioDefaultFollowsACompatibleTargetOnAPick() {
        func pickAndConnect(pref: Bool, audio: Bool) -> AppModel {
            let model = makeModel()
            model.audioByDefault = pref
            model.apply(.status(.connected))
            model.apply(.control(.picker))
            model.connect(to: "mac")
            model.apply(.control(.connected(connected(protocolName: "rdp", audio: audio))))
            return model
        }
        #expect(pickAndConnect(pref: true, audio: true).audio.isEnabled)
        #expect(!pickAndConnect(pref: true, audio: false).audio.isEnabled, "no sound to play")
        #expect(!pickAndConnect(pref: false, audio: true).audio.isEnabled, "the default is off")
    }

    /// A silent reattach — one the user did not ask for — must not turn sound on
    /// from the default: a reconnect keeps the last attachment's intent, and
    /// re-seeding would un-mute a session the user had quietened before it dropped.
    @Test
    func theAudioDefaultIsNotSeededOnASilentReattach() {
        let model = makeModel()
        model.audioByDefault = true
        model.apply(.status(.connected))
        // No pick precedes this `connected`, so `pendingTarget` is nil — a reattach.
        model.apply(.control(.connected(connected(protocolName: "rdp", audio: true))))
        #expect(!model.audio.isEnabled)
    }

    /// The live menu controls write the same remembered value the picker's toggles
    /// do — one setting, two editors — so a mid-session choice is the one the next
    /// connect starts from, and it persists.
    @Test
    func theLiveControlsWriteTheRememberedDefaults() throws {
        let directory = try ScratchDirectory()
        let url = directory.url.appending(path: "viewer.json")
        let model = AppModel(preferences: ViewerPreferences(url: url))
        model.apply(.status(.connected))
        model.apply(
            .control(.connected(connected(protocolName: "vnc", resize: true, audio: true)))
        )

        model.setAutoResize(true)
        model.setAudioEnabled(true)
        #expect(model.autoResizeByDefault)
        #expect(model.audioByDefault)

        let restored = AppModel(preferences: ViewerPreferences(url: url))
        #expect(restored.autoResizeByDefault, "the View menu's choice is remembered")
        #expect(restored.audioByDefault, "the Remote menu's choice is remembered")
    }

    /// `autoResize` defaults to `resize`, which is what the gateway sends for the
    /// plain `vnc` these cases connect to; the ones about the second permission
    /// name it.
    private func connected(
        protocolName: String,
        resize: Bool = false,
        autoResize: Bool? = nil,
        clipboard: Bool = false,
        audio: Bool = false
    ) -> ServerMessage.Connected {
        ServerMessage.Connected(
            name: "mac",
            protocolName: protocolName,
            resize: resize,
            autoResize: autoResize ?? resize,
            clipboard: clipboard,
            audio: audio
        )
    }

    private func enableClipboard(on model: AppModel) {
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "vnc", clipboard: true))))
        #expect(model.clipboard.isEnabled)
    }
}
