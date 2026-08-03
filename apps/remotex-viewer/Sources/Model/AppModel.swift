import AppKit
import Observation

/// What screen the window is showing.
///
/// Two, and the second one is a web view. Everything that used to be a screen here
/// — login, the target picker, the desktop, the takeover interstitial — is the
/// client's, which is the same client a browser gets.
enum ViewerScreen: Equatable {
    /// Starting the gateway in this bundle, or explaining why it did not start.
    case launching
    /// The gateway is up and the page is showing.
    case ready(GatewayEndpoint)
}

/// The app around the client: the gateway process, the window, the menu bar, the
/// pasteboard, and the keyboard.
///
/// It holds no session. There is no claim here, no socket, no wire format and no
/// framebuffer — the page has all of that, and this reads what it reports
/// (`NativeState`) to decide what the menus say. The rule that keeps the two from
/// drifting is that nothing here is ever set optimistically: a menu tick moves when
/// the page says the thing changed, not when the item was pressed.
@MainActor
@Observable
final class AppModel {
    private(set) var screen = ViewerScreen.launching
    /// What the page last reported about itself. Every menu is derived from it.
    private(set) var state = NativeState()

    /// Why the gateway is not running, and what it said on the way out.
    private(set) var launchError: String?
    private(set) var launchLog = ""
    private(set) var isBusy = false

    /// A message for the alert: an action that failed where the user was looking.
    var actionError: String?

    /// Bumped to ask the window to open a sheet. The Remote menu's items are AppKit
    /// actions on this object, and nothing outside SwiftUI can present a sheet — so
    /// the view watches a counter instead of being called.
    private(set) var configurationRequests = 0
    private(set) var aboutRequests = 0

    let clipboard: ClipboardSynchronizer

    @ObservationIgnored
    let gateway: EmbeddedGateway?
    @ObservationIgnored
    let config: GatewayConfigStore?
    /// The page, while one is up.
    @ObservationIgnored
    private weak var bridge: (any CommandSink)?

    /// Sizes the window to the remote. Installed by `RemoteWebHost`, which is the
    /// only thing that holds a window; outside Observation because it is set from
    /// inside SwiftUI's own update pass.
    @ObservationIgnored
    var fitWindowToRemote: (() -> Void)?
    /// This window's backing scale, read on demand: the Display menu's readout has
    /// to name the screen the window is on now.
    @ObservationIgnored
    var hostScaleReader: (() -> CGFloat)?

    /// Everything is a parameter so a test can drive the model without the app
    /// around it: a throwaway pasteboard instead of the user's own, and **no gateway
    /// at all** — `nil` is the unbundled case (`swift test`), where there is no
    /// `remotex-gateway` to run.
    init(
        gateway: EmbeddedGateway? = nil,
        config: GatewayConfigStore? = nil,
        clipboard: ClipboardSynchronizer = ClipboardSynchronizer()
    ) {
        self.gateway = gateway
        self.config = config
        self.clipboard = clipboard
        clipboard.sendText = { [weak self] text in
            self?.send(.clipboardLocal(text))
        }
    }

    /// The app's own model: the instance directory this launch was given, the
    /// gateway in this bundle, the SPA beside it, and the config store over both.
    static func forApp(instance: InstanceDirectory = .resolved()) -> AppModel {
        let binary = GatewayBinary.inBundle()
        let webRoot = GatewayBinary.webRootInBundle()
        // Both or neither: an unbundled build has no gateway *and* no SPA, and half
        // of a bundle is not a case worth having a state for.
        var gateway: EmbeddedGateway?
        if let binary, let webRoot {
            gateway = EmbeddedGateway(instance: instance, binary: binary, webRoot: webRoot)
        }
        return AppModel(
            gateway: gateway,
            config: binary.map { GatewayConfigStore.overBinary(instance: instance, binary: $0) }
        )
    }

    // MARK: - Derived UI state

    /// This gateway's display name: the window title, the About item, the launch
    /// screen.
    ///
    /// Read off the page's report rather than held here, and that is the fix for a
    /// real fault: the app used to fetch `/api/config` itself, and when the session
    /// moved to the client the fetch went with it — leaving a stored property that
    /// nothing ever assigned, so every window said `remotex` however the config was
    /// branded. There is no second copy to go stale now.
    var branding: String {
        state.branding
    }

    var windowTitle: String {
        // A speaker suffix while sound is playing — the one persistent surface that
        // can say so, since the toggle is a menu item nobody is looking at.
        state.mode == .desktop && state.audioEnabled ? "\(branding) 🔊" : branding
    }

    /// Whether the page is showing a live desktop that may be typed at.
    var canCaptureKeyboardNow: Bool {
        if case .ready = screen {
            return state.capturesKeyboard
        }
        return false
    }

    var macOSKeyboardOverridesEnabled: Bool {
        state.macKeyOverridesEnabled
    }

    /// Named for what it is doing, not for what it is set to: against a Mac guest
    /// there is nothing to translate, and a plain unticked box would read as a
    /// preference somebody turned off.
    var macOSKeyboardOverridesLabel: String {
        state.remoteIsMac
            ? "Enable macOS Keyboard Overrides (Not Applicable)"
            : "Enable macOS Keyboard Overrides"
    }

    /// Whether the override is a choice at all. Beside the label above, because the
    /// two answer the same question and a menu that greys on one rule while
    /// labelling itself by another is how they come to disagree.
    var canOverrideMacKeys: Bool {
        isOnDesktop && !state.remoteIsMac
    }

    var macOSKeyboardOverridesActive: Bool {
        state.macKeyOverridesActive
    }

    /// Whether the window may drive the remote's size — the gateway's second
    /// permission, `autoResize`, and plain `vnc` alone has it.
    var canAutoResize: Bool {
        isOnDesktop && state.canResize && state.canAutoResize
    }

    /// Greying alone would read as "this session cannot resize", which the item
    /// below it disproves; so where the mode is refused but resizing is not, the
    /// item says which.
    var autoResizeLabel: String {
        isOnDesktop && state.canResize && !state.canAutoResize
            ? "Auto Resize (Not Applicable)"
            : "Auto Resize"
    }

    var autoResizes: Bool {
        state.autoResize
    }

    /// One resize now. Disabled while the window is driving the size continuously:
    /// that is what the mode does every frame.
    var canResizeNow: Bool {
        isOnDesktop && state.canResize && !state.autoResize
    }

    /// Fit the *window* to the desktop. Nothing is sent for it, so it needs no
    /// permission — only a desktop with a known size, and a size that is not about
    /// to follow the window back.
    var canResizeToDisplay: Bool {
        isOnDesktop && state.size != nil && !(state.canResize && state.autoResize)
    }

    var canClipboard: Bool {
        isOnDesktop && state.canClipboard
    }

    var canAudio: Bool {
        isOnDesktop && state.canAudio
    }

    var audioEnabled: Bool {
        state.audioEnabled
    }

    var displays: [DisplayChoice] {
        isOnDesktop ? state.displays : []
    }

    var activeDisplayID: UInt32? {
        state.activeDisplayId
    }

    /// The Display menu's read-only line.
    var displaySummaryLine: String {
        displaySummary(
            remote: state.remoteSize,
            remoteScale: state.remoteScale,
            hostScale: hostScaleReader?() ?? 1
        )
    }

    /// Whether somebody else holds the session, and which way round.
    ///
    /// `nil` hides the item rather than greying it: "Take Over Session" on a session
    /// nobody else has is not an action that is unavailable, it is one that has no
    /// meaning.
    var takeOverTitle: String? {
        switch state.status {
        case .busy: "Take Over Session"
        case .takenOver: "Take Session Back"
        default: nil
        }
    }

    /// Whether a session command has anything to act on.
    var isOnDesktop: Bool {
        if case .ready = screen {
            return state.mode == .desktop && state.status == .connected
        }
        return false
    }

    var canEditConfiguration: Bool {
        config != nil
    }

    var canRestartGateway: Bool {
        gateway != nil && !isBusy
    }

    // MARK: - The gateway

    /// Bring the embedded gateway up: make sure there is a config, start it, and
    /// show the page it serves.
    ///
    /// Nothing is asked here. The gateway is in this bundle, its address is whatever
    /// port it reports, and the token it prints goes into the web view's cookie
    /// store — so the screen between launching and the target picker exists only to
    /// say what went wrong when something does.
    func launch() async {
        guard !isBusy else {
            return
        }
        isBusy = true
        defer { isBusy = false }
        launchError = nil
        launchLog = ""
        screen = .launching

        guard let gateway else {
            // No `remotex-gateway` beside us. Only reachable in an unbundled build,
            // and worth saying plainly rather than looking like a network failure.
            fail(with: EmbeddedGateway.LaunchFailure.executableMissing)
            return
        }
        // Reported rather than propagated: a gateway that dies while the desktop is
        // up is the same situation as one that would not start, and the same screen
        // says so.
        gateway.onUnexpectedExit = { [weak self] failure in
            self?.gatewayDied(failure)
        }

        do {
            try await config?.bootstrapIfNeeded()
        } catch {
            fail(with: EmbeddedGateway.LaunchFailure.instanceUnavailable(error.localizedDescription))
            return
        }

        do {
            let handshake = try await gateway.start()
            screen = .ready(
                GatewayEndpoint(port: handshake.port, token: handshake.token)
            )
        } catch {
            fail(with: error)
        }
    }

    /// Start over: stop the gateway, start it again, load the page again.
    ///
    /// What a saved config change and the launch screen's **Try Again** both do. The
    /// session goes with it — the gateway is the session's ground, and a new process
    /// has no memory of the old one's slot, nor of the token that opened it.
    func relaunchGateway() async {
        state = NativeState()
        screen = .launching
        await gateway?.stop()
        await launch()
    }

    private func fail(with error: any Error) {
        launchError = error.localizedDescription
        launchLog = gateway?.log() ?? ""
        screen = .launching
    }

    /// The bundle has a gateway but no client to show in the window.
    ///
    /// Reported from the web host rather than from `launch()`, because that is where
    /// it is discovered: the gateway starts fine — it is serving the same directory
    /// — and the missing piece only surfaces when there is a page to load.
    func reportClientMissing() {
        fail(with: EmbeddedGateway.LaunchFailure.clientMissing)
    }

    private func gatewayDied(_ failure: EmbeddedGateway.LaunchFailure) {
        state = NativeState()
        fail(with: failure)
    }

    /// The page could not be loaded at all, which on loopback means the gateway
    /// stopped answering between the handshake and the request.
    func pageFailedToLoad(_ reason: String) {
        launchError = "The gateway stopped answering: \(reason)"
        launchLog = gateway?.log() ?? ""
        screen = .launching
    }

    /// Put the model on the screen a running gateway produces, without one.
    ///
    /// For tests only, and named so: everything below the menu bar is the page's,
    /// so a suite about the menus needs a page and not a gateway. The app itself
    /// reaches this state through `launch()`.
    func showReadyForTesting(_ endpoint: GatewayEndpoint) {
        screen = .ready(endpoint)
    }

    func editConfiguration() {
        guard canEditConfiguration else {
            return
        }
        configurationRequests += 1
    }

    func showAbout() {
        aboutRequests += 1
    }

    func showError(_ message: String) {
        actionError = message
    }

    func clearError() {
        actionError = nil
    }

    // MARK: - The page

    func attach(bridge: any CommandSink) {
        self.bridge = bridge
    }

    /// Let go of the page, if it is still this one. SwiftUI can build the
    /// replacement surface before dismantling the old one, in which case the bridge
    /// has already been replaced by one that is on screen.
    func release(bridge: any CommandSink) {
        guard self.bridge === bridge else {
            return
        }
        self.bridge = nil
        state = NativeState()
        clipboard.update(enabled: false)
    }

    /// One event from the page.
    func apply(_ event: NativeEvent) {
        switch event {
        case .state(let state):
            self.state = state
            clipboard.update(enabled: state.mode == .desktop && state.canClipboard)
        case .clipboardFromRemote(let text):
            clipboard.receiveRemotePush(text)
        case .unauthenticated:
            // The gateway did not take the token this app minted for it. Nothing on
            // the page can fix that — there is no login here — so the app takes the
            // screen back and offers the restart that mints a new one.
            launchError =
                "The local gateway did not accept this session. Restart it to try again."
            launchLog = gateway?.log() ?? ""
            screen = .launching
        }
    }

    private func send(_ command: NativeCommand) {
        bridge?.send(command)
    }

    // MARK: - Input

    func sendKey(_ event: NativeKeyEvent) {
        send(.key(event))
    }

    func releaseInput() {
        send(.releaseInput)
    }

    // MARK: - Menu commands

    func setMacKeyOverrides(_ enabled: Bool) {
        send(.setMacKeyOverrides(enabled))
    }

    func openClipboardPanel() {
        send(.openClipboard)
    }

    /// Whether there is a page for the inspector to open on.
    ///
    /// Every other item in that menu greys where it cannot act, and this one could
    /// not: pressed on the launch screen it reached a bridge that is not there and
    /// did nothing, silently. The `screen` test is also what makes this observable —
    /// `bridge` is deliberately outside Observation, so a menu derived from it alone
    /// would not be rebuilt when the page comes and goes.
    var canShowDevTools: Bool {
        if case .ready = screen {
            return bridge != nil
        }
        return false
    }

    /// Chromium's inspector, on this page. The one menu item that does not go
    /// through `send`, because it asks the engine rather than the client.
    func showDevTools() {
        bridge?.showDevTools()
    }

    func setAudioEnabled(_ enabled: Bool) {
        send(.setAudio(enabled))
    }

    func refresh() {
        send(.refresh)
    }

    func switchTarget() {
        send(.switchTarget)
    }

    func takeOver() {
        send(.takeOver)
    }

    func selectDisplay(_ id: UInt32) {
        send(.selectDisplay(id))
    }

    func sendKeyCombo(_ codes: [String]) {
        send(.sendKeyCombo(codes))
    }

    func setAutoResize(_ enabled: Bool) {
        // Refused rather than merely hidden: the item is greyed where the mode is
        // not allowed, and this makes a menu that somehow got pressed anyway unable
        // to turn it on behind the item's back.
        guard !enabled || canAutoResize else {
            return
        }
        send(.setAutoResize(enabled))
    }

    func resizeToWindow() {
        guard canResizeNow else {
            return
        }
        send(.resizeToWindow)
    }

    /// Fit this window to the remote. Local only — nothing is sent.
    func resizeToDisplay() {
        guard canResizeToDisplay else {
            return
        }
        fitWindowToRemote?()
    }
}
