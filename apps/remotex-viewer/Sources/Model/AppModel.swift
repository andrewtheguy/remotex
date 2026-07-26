import AppKit
import Observation

/// The viewer's whole state, and the one place the gateway's control messages
/// become it.
///
/// `ViewerScreen` used to be *reported by the page* over the host bridge. It is
/// now derived here, which is the substance of the port: `picker` and `connected`
/// come from the gateway's session layer, and everything the Remote menu enables
/// or disables follows from them plus `resize`, `remoteOs`, and `displayModes`.
@MainActor
@Observable
final class AppModel: GatewaySessionSink {
    private static let gatewayDefaultsKey = "gatewayAddress"
    private static let keyboardOverridesDefaultsKey = "macOSKeyboardOverridesEnabled"
    static let fallbackAddress = "http://127.0.0.1:52380"

    /// Bound to the login screen's Server field. There is no separate Settings
    /// window: the address you connect to is chosen where the credentials are.
    var gatewayAddress: String

    var macOSKeyboardOverridesEnabled: Bool {
        didSet {
            guard macOSKeyboardOverridesEnabled != oldValue else {
                return
            }
            defaults.set(
                macOSKeyboardOverridesEnabled,
                forKey: Self.keyboardOverridesDefaultsKey
            )
            // The convention just changed under whatever is held down.
            releaseInput()
        }
    }

    private(set) var gateway: GatewayLocation
    private(set) var branding = "remotex"
    private(set) var session = ViewerSessionState()
    private(set) var targets: [TargetInfo] = []
    /// A probe or login is in flight, so the login screen's controls are locked.
    private(set) var isBusy = false
    /// Shown under the Server field: unreachable, or a protocol version this
    /// build cannot speak.
    private(set) var gatewayError: String?
    /// Shown under the credentials.
    private(set) var loginError: String?
    /// The alert.
    var actionError: String?

    let clipboard: ClipboardSynchronizer

    @ObservationIgnored
    private let defaults: UserDefaults
    @ObservationIgnored
    private var client: GatewayClient
    @ObservationIgnored
    private var connection: GatewayConnection?
    @ObservationIgnored
    private var pressed = PressedInput()
    /// The room available for the remote desktop, in device pixels, as the
    /// surface last measured it. Nil until a surface exists.
    @ObservationIgnored
    private var viewportSize: DisplayMode?

    /// `clipboard` is a parameter so tests can hand in one bound to a throwaway
    /// pasteboard instead of the user's own.
    init(
        defaults: UserDefaults = .standard,
        clipboard: ClipboardSynchronizer = ClipboardSynchronizer()
    ) {
        self.defaults = defaults
        self.clipboard = clipboard
        let stored = defaults.string(forKey: Self.gatewayDefaultsKey)
        let initial = Self.commandLineGateway() ?? stored ?? Self.fallbackAddress
        let parsed = (try? GatewayLocation.parse(initial))
            ?? (try! GatewayLocation.parse(Self.fallbackAddress))
        gateway = parsed
        gatewayAddress = parsed.url.absoluteString
        macOSKeyboardOverridesEnabled =
            defaults.object(forKey: Self.keyboardOverridesDefaultsKey) as? Bool ?? true
        client = GatewayClient(gateway: parsed)
    }

    // MARK: - Derived UI state

    var windowTitle: String {
        if let target = session.connectedTarget {
            "\(target) — remotex"
        } else {
            branding
        }
    }

    var canCaptureKeyboardNow: Bool {
        session.canCaptureKeyboard && session.remoteSize != nil
    }

    var macOSKeyboardOverridesActive: Bool {
        macOSKeyboardOverridesEnabled && !session.remoteIsMac
    }

    var macOSKeyboardOverridesLabel: String {
        session.remoteIsMac
            ? "macOS Keyboard Overrides (Not Applicable)"
            : "Enable macOS Keyboard Overrides"
    }

    /// "Resize to Window" needs both a target that takes one and a measured
    /// window to report.
    var canResizeNow: Bool {
        session.canResize && viewportSize != nil
    }

    /// The interstitial covers the connection lifecycle and the claim conflicts,
    /// and on the desktop it also covers the gap before the first frame. The
    /// picker owns the screen once connected.
    var showsStatusOverlay: Bool {
        session.connectionStatus != .connected
            || (session.screen == .desktop && session.remoteSize == nil)
    }

    // MARK: - Launch and login

    /// Probe the configured gateway and resume if the login is still good.
    ///
    /// `isBusy` is part of the guard, not just the screen: SwiftUI can run the
    /// owning `.task` more than once, and the screen is not updated until the
    /// first probe's awaits finish — so screen alone lets a second call through
    /// and the gateway sees two of every request.
    func start() async {
        guard session.screen == .checking, !isBusy else {
            return
        }
        isBusy = true
        defer { isBusy = false }
        do {
            branding = try await client.configuration().branding
            if try await client.isAuthenticated() {
                await beginSession()
                return
            }
        } catch {
            gatewayError = error.localizedDescription
        }
        session.screen = .login
    }

    func logIn(username: String, password: String) async {
        guard !isBusy else {
            return
        }
        isBusy = true
        defer { isBusy = false }
        loginError = nil
        guard await adoptGatewayAddress() else {
            return
        }
        do {
            switch try await client.logIn(username: username, password: password) {
            case .ok:
                await beginSession()
            case .invalidCredentials:
                loginError = "Invalid credentials"
            case .failed(let status):
                loginError = "Login failed (\(status))"
            }
        } catch {
            loginError = "Network error"
        }
    }

    func logOut() async {
        await teardown()
        try? await client.logOut()
        session = ViewerSessionState(screen: .login)
        targets = []
        loginError = nil
    }

    /// Adopt whatever is in the Server field and confirm the gateway answers.
    /// Persisted only on success, so a typo does not become the new default.
    private func adoptGatewayAddress() async -> Bool {
        gatewayError = nil
        let next: GatewayLocation
        do {
            next = try GatewayLocation.parse(gatewayAddress)
        } catch {
            gatewayError = error.localizedDescription
            return false
        }
        if next != gateway {
            await teardown()
            // A token for the previous host would be sent to this one and 401
            // with nothing to explain it.
            client.forgetSessionCookie()
            gateway = next
            client = GatewayClient(gateway: next)
        }
        gatewayAddress = next.url.absoluteString
        do {
            branding = try await client.configuration().branding
        } catch {
            gatewayError = error.localizedDescription
            return false
        }
        defaults.set(gatewayAddress, forKey: Self.gatewayDefaultsKey)
        return true
    }

    private func beginSession() async {
        let connection = GatewayConnection(gateway: client, sink: self)
        self.connection = connection
        clipboard.send = { [weak connection] message in
            connection?.send(message)
        }
        // Provisional: the gateway's `picker` or `connected` decides which of the
        // two post-login screens this really is, and the interstitial covers the
        // wait either way.
        session.screen = .picker
        session.connectionStatus = .connecting
        await connection.start()
    }

    private func teardown() async {
        releaseInput()
        if let connection {
            await connection.stop()
        }
        connection = nil
        clipboard.send = nil
        clipboard.update(enabled: false)
    }

    private func handleUnauthorized() async {
        await teardown()
        session = ViewerSessionState(screen: .login)
        targets = []
        loginError = "The gateway ended this session. Sign in again."
    }

    // MARK: - Session events

    func apply(_ event: SessionEvent) {
        switch event {
        case .status(let status):
            session.connectionStatus = status
            updateClipboardEnablement()
        case .control(let message):
            handle(message)
        case .tile:
            // The renderer takes these from M4 on.
            break
        case .clearFramebuffer:
            // Dropping the size is what puts the "waiting for the remote
            // desktop" interstitial back up; the gateway always repaints in full.
            session.remoteSize = nil
        case .releaseInput:
            releaseInput()
        case .failPendingClipboardFetch:
            clipboard.failPendingFetch()
        case .unauthorized:
            Task { await handleUnauthorized() }
        }
    }

    private func handle(_ message: ServerMessage) {
        switch message {
        case .picker:
            session.screen = .picker
            session.connectedTarget = nil
            session.pendingTarget = nil
            session.protocolName = nil
            session.remoteIsMac = false
            session.displayModes = []
            session.remoteSize = nil
            session.canResize = false
            session.manualResize = false
            session.canClipboard = false
            updateClipboardEnablement()
            clipboard.failPendingFetch()
            Task { await loadTargets() }

        case .connected(let payload):
            session.screen = .desktop
            session.connectedTarget = payload.name
            session.protocolName = payload.protocolName
            session.pendingTarget = nil
            session.connectError = nil
            // The three resize mechanisms, as `useRemoteDesktop.ts` picks between
            // them. RDP resizes only on request because a resize forces a heavy
            // Deactivation-Reactivation; rxa ignores viewport reports and offers a
            // fixed list instead; VNC is the only one that follows the window.
            session.manualResize =
                (payload.protocolName == "rdp" || payload.protocolName == "rxa")
                    && payload.resize
            session.canResize = payload.protocolName == "rdp" && payload.resize
            session.canClipboard = payload.clipboard
            updateClipboardEnablement()

        case .resize(let w, let h):
            session.remoteSize = DisplayMode(w: w, h: h)

        case .remoteOs(let macos):
            // Which Mac a Command chord belongs to just changed, so nothing may
            // stay held under the old convention.
            if session.remoteIsMac != macos {
                releaseInput()
            }
            session.remoteIsMac = macos

        case .displayModes(let modes):
            // Replaced wholesale: the Mac regenerates this on every display
            // reconfigure, so merging keeps sizes that no longer exist.
            session.displayModes = modes

        case .error(let message):
            // Not fatal — the session returns to the picker, which is where this
            // is shown.
            session.connectError = message
            session.pendingTarget = nil

        case .clipboard(let payload):
            receive(clipboard: payload)

        case .cursor:
            // The pointer arrives with the renderer, from M7.
            break

        case .unsupported(let type):
            // A newer gateway. Deliberately nothing: the frame was already
            // counted as proof of attachment.
            _ = type
        }
    }

    private func receive(clipboard payload: ServerMessage.Clipboard) {
        if payload.requested {
            // The answer to a Fetch. Must not reach NSPasteboard — Copy is the
            // consent boundary, and this is the one place that is decided.
            clipboard.receiveFetchReply(
                text: payload.text,
                changedAtMs: payload.changedAtMs,
                oversizedBytes: payload.oversizedBytes
            )
        } else if let bytes = payload.oversizedBytes {
            clipboard.noteRemoteOversized(bytes: bytes)
        } else {
            clipboard.receiveRemotePush(payload.text)
        }
    }

    private func updateClipboardEnablement() {
        clipboard.update(
            enabled: session.screen == .desktop
                && session.connectionStatus == .connected
                && session.canClipboard
        )
    }

    private func loadTargets() async {
        do {
            targets = try await client.targets()
        } catch GatewayClientError.unauthorized {
            await handleUnauthorized()
        } catch {
            session.connectError = error.localizedDescription
        }
    }

    // MARK: - Actions

    func connect(to target: String) {
        guard session.screen == .picker, session.pendingTarget == nil else {
            return
        }
        session.pendingTarget = target
        session.connectError = nil
        connection?.send(.connect(target: target))
    }

    func switchTarget() {
        guard session.screen == .desktop else {
            return
        }
        releaseInput()
        connection?.send(.disconnect)
    }

    func takeOver() {
        guard session.connectionStatus == .busy || session.connectionStatus == .takenOver else {
            return
        }
        Task { [connection] in
            await connection?.start(force: true)
        }
    }

    /// Re-announce the size and repaint everything — the escape hatch for a
    /// framebuffer that has gone wrong.
    func refresh() {
        connection?.send(.refresh)
    }

    func resizeToWindow() {
        guard session.canResize, let viewportSize else {
            return
        }
        connection?.send(.viewport(w: viewportSize.w, h: viewportSize.h))
    }

    /// Apply one of the resolutions the remote offered. Unlike `resizeToWindow`
    /// this is a pick off a fixed list — a Mac's virtual display takes nothing
    /// else — so an entry the gateway no longer offers is dropped here rather
    /// than sent and refused.
    func setResolution(_ mode: DisplayMode) {
        guard session.displayModes.contains(mode) else {
            return
        }
        connection?.send(.setResolution(w: mode.w, h: mode.h))
    }

    func sendKey(code: String, pressed isPressed: Bool, caps: Bool) {
        pressed.record(code: code, pressed: isPressed)
        connection?.send(.key(code: code, pressed: isPressed, caps: caps))
    }

    func sendMouseButton(_ button: MouseButton, pressed isPressed: Bool) {
        pressed.record(button: button, pressed: isPressed)
        connection?.send(.mouseButton(button: button, pressed: isPressed))
    }

    /// Let go of everything held on the remote. The single path for it — see
    /// `PressedInput`.
    func releaseInput() {
        for message in pressed.takeReleaseMessages() {
            connection?.send(message)
        }
    }

    func showError(_ message: String) {
        actionError = message
    }

    func clearError() {
        actionError = nil
    }

    private static func commandLineGateway() -> String? {
        let arguments = ProcessInfo.processInfo.arguments
        guard let flag = arguments.firstIndex(of: "--gateway"),
              arguments.indices.contains(flag + 1)
        else {
            return nil
        }
        return arguments[flag + 1]
    }
}
