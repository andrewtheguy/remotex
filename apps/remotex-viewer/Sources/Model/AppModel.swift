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
    /// A gateway check or a login is in flight, so the current step's controls
    /// are locked.
    private(set) var isBusy = false
    /// Shown on the server step: a malformed address, an unreachable gateway, or
    /// a protocol version this build cannot speak.
    private(set) var gatewayError: String?
    /// Shown under the credentials.
    private(set) var loginError: String?
    /// The alert.
    var actionError: String?
    /// The remote's pointer shape, once an engine hands one over. Nil means the
    /// remote is drawing its own pointer into the framebuffer.
    private(set) var remoteCursor: ServerMessage.Cursor?

    let clipboard: ClipboardSynchronizer

    @ObservationIgnored
    private let defaults: UserDefaults
    @ObservationIgnored
    private let urlSession: URLSession
    @ObservationIgnored
    private var client: GatewayClient
    @ObservationIgnored
    private var connection: GatewayConnection?
    @ObservationIgnored
    private var pressed = PressedInput()
    /// The room available for the remote desktop, in the remote's own pixels, as
    /// the surface last measured it. Nil until a surface exists.
    ///
    /// Observed, unlike the rest of the session plumbing below, because
    /// `canResizeNow` reads it: the first measurement is what enables "Resize to
    /// Window", and a surface that appears after `connected` would otherwise leave
    /// the item disabled with nothing to invalidate it.
    ///
    /// Readable so a test can see that a window resize reached the model at all —
    /// the half of automatic resizing that lives in AppKit notifications rather
    /// than in `ViewportPolicy`.
    private(set) var viewportSize: DisplayMode?
    @ObservationIgnored
    private var viewportPolicy = ViewportPolicy()
    /// Debounces automatic reports. A window drag changes the visible area on
    /// every frame, and a VNC target acts on each one it is told about.
    @ObservationIgnored
    private var viewportDebounce: Task<Void, Never>?
    /// Deliberately outside Observation. Tiles arrive dozens of times a second;
    /// routing them through `@Observable` would invalidate the view hierarchy on
    /// every strip.
    @ObservationIgnored
    private weak var renderer: FramebufferRenderer?

    /// `clipboard` and `urlSession` are parameters so tests can hand in one bound
    /// to a throwaway pasteboard instead of the user's own, and a stubbed
    /// transport instead of the network.
    init(
        defaults: UserDefaults = .standard,
        clipboard: ClipboardSynchronizer = ClipboardSynchronizer(),
        urlSession: URLSession = GatewayClient.defaultSession
    ) {
        self.defaults = defaults
        self.clipboard = clipboard
        self.urlSession = urlSession
        let stored = defaults.string(forKey: Self.gatewayDefaultsKey)
        let initial = Self.commandLineGateway() ?? stored ?? Self.fallbackAddress
        let parsed = (try? GatewayLocation.parse(initial))
            ?? (try! GatewayLocation.parse(Self.fallbackAddress))
        gateway = parsed
        gatewayAddress = parsed.url.absoluteString
        macOSKeyboardOverridesEnabled =
            defaults.object(forKey: Self.keyboardOverridesDefaultsKey) as? Bool ?? true
        client = GatewayClient(gateway: parsed, session: urlSession)
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

    // MARK: - The server step

    /// Adopt whatever is in the Server field and confirm the gateway answers.
    ///
    /// The server step's only action, and the only place a gateway is validated.
    /// Nothing probes on launch: reaching an address is a thing the user asks for
    /// and gets an answer to, not something that happens to them while a spinner
    /// is up. It also means an unreachable gateway is reported next to the field
    /// that caused it, before any credentials have been typed.
    ///
    /// Where it lands depends on the cookie, which outlives the app: still signed
    /// in and this goes straight to the session, skipping the login step.
    func connectToGateway() async {
        guard !isBusy else {
            return
        }
        isBusy = true
        defer { isBusy = false }
        gatewayError = nil
        loginError = nil

        let next: GatewayLocation
        do {
            next = try GatewayLocation.parse(gatewayAddress)
        } catch {
            gatewayError = error.localizedDescription
            return
        }
        if next != gateway {
            await teardown()
            // A token for the previous host would be sent to this one and 401
            // with nothing to explain it.
            client.forgetSessionCookie()
            gateway = next
            client = GatewayClient(gateway: next, session: urlSession)
        }
        // Normalized: a bare host gains a scheme, a path is dropped.
        gatewayAddress = next.url.absoluteString

        let authenticated: Bool
        do {
            branding = try await client.configuration().branding
            authenticated = try await client.isAuthenticated()
        } catch {
            gatewayError = error.localizedDescription
            return
        }
        // Persisted only once it answered, so a typo does not become the address
        // the next launch starts from.
        defaults.set(gatewayAddress, forKey: Self.gatewayDefaultsKey)

        if authenticated {
            await beginSession()
        } else {
            session.screen = .login
        }
    }

    /// Back to the server step, to point somewhere else.
    func changeGateway() async {
        await teardown()
        session = ViewerSessionState(screen: .server)
        targets = []
        loginError = nil
        gatewayError = nil
    }

    // MARK: - Login

    /// The credentials only. The gateway was already validated by the server
    /// step, so a failure here can only be about who you are.
    func logIn(username: String, password: String) async {
        guard !isBusy else {
            return
        }
        isBusy = true
        defer { isBusy = false }
        loginError = nil
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

    /// Log out but stay on this gateway — it is the credentials being given up,
    /// not the address.
    func logOut() async {
        await teardown()
        try? await client.logOut()
        session = ViewerSessionState(screen: .login)
        targets = []
        loginError = nil
    }

    private func beginSession() async {
        await beginSession(over: client)
    }

    /// Split out from `beginSession` so a test can drive a whole session — claim,
    /// attach, control messages, and what gets sent back — over a scripted socket
    /// instead of the network.
    func beginSession(over gateway: any SessionGateway) async {
        let connection = GatewayConnection(gateway: gateway, sink: self)
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
        case .tile(let tile):
            renderer?.upload(tile)
        case .clearFramebuffer:
            // Dropping the size is what puts the "waiting for the remote
            // desktop" interstitial back up; the gateway always repaints in full.
            session.remoteSize = nil
            renderer?.clear()
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
            // Back to the default density, with the size: it is the next
            // target's first `resize` that says what its is, and until then a
            // Retina Mac's 2 would double the viewport reported for whatever
            // was picked next — including the report `connected` sends before
            // any resize has arrived.
            session.remoteScale = 1
            session.canResize = false
            session.manualResize = false
            session.canClipboard = false
            remoteCursor = nil
            viewportPolicy = ViewportPolicy()
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
            viewportPolicy = ViewportPolicy(
                protocolName: payload.protocolName,
                resize: payload.resize
            )
            session.manualResize = viewportPolicy.manualOnly
            session.canResize = payload.protocolName == "rdp" && payload.resize
            session.canClipboard = payload.clipboard
            updateClipboardEnablement()
            // A freshly started engine knows nothing about this window, and both
            // dedupes would swallow the first report for repeating a size already
            // sent — for the previous target, or for the picker. Both have to be
            // cleared, not just the policy's: the queue's memo survives a target
            // switch because the socket does.
            viewportPolicy.resetForNewConnection()
            connection?.resetViewportMemo()
            sendViewport(manual: false)

        case .resize(let w, let h, let scale):
            let size = DisplayMode(w: w, h: h)
            session.remoteSize = size
            // The texture is the remote's pixels; the density only decides how
            // large those pixels are drawn (`RemoteGeometry`), so the renderer
            // never hears about it.
            session.remoteScale = scale > 0 ? CGFloat(scale) : 1
            renderer?.resize(to: size)

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

        case .cursor(let payload):
            // Receiving one of these at all means the viewer owns pointer
            // rendering for the rest of the session.
            remoteCursor = payload

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

    // MARK: - The remote surface

    func attach(renderer: FramebufferRenderer?) {
        self.renderer = renderer
        // A surface appearing mid-session has an empty texture, so ask for the
        // pixels rather than waiting for the remote to change something.
        if renderer != nil, let size = session.remoteSize {
            renderer?.resize(to: size)
            refresh()
        }
    }

    /// The surface measured how much room it has, in the remote's pixels.
    ///
    /// Debounced rather than sent straight through: a window drag reports on every
    /// frame, and VNC acts on every report it receives.
    func reportViewport(_ size: DisplayMode) {
        viewportSize = size
        viewportDebounce?.cancel()
        viewportDebounce = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(250))
            guard !Task.isCancelled else {
                return
            }
            self?.sendViewport(manual: false)
        }
    }

    /// The surface exists for the picker as well as the desktop — it has to, so the
    /// framebuffer survives a trip to the picker and back — so a window resized
    /// while choosing a target measures and records, but has nothing to report to:
    /// there is no engine yet. Sending anyway also taught the queue's dedupe the
    /// size, which then swallowed the report that matters, the one from `connected`.
    private func sendViewport(manual: Bool) {
        guard session.screen == .desktop,
              let viewportSize,
              let message = viewportPolicy.report(viewportSize, manual: manual)
        else {
            return
        }
        connection?.send(message)
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

    /// "Resize to Window": the one report that gets past `manualOnly`.
    func resizeToWindow() {
        guard session.canResize else {
            return
        }
        sendViewport(manual: true)
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

    func sendPointer(x: Int32, y: Int32) {
        guard session.canCaptureKeyboard else {
            return
        }
        connection?.send(.mouseMove(x: x, y: y))
    }

    func sendWheel(dx: Float, dy: Float) {
        guard session.canCaptureKeyboard else {
            return
        }
        connection?.send(.wheel(dx: dx, dy: dy))
    }

    func sendKey(code: String, pressed isPressed: Bool, caps: Bool) {
        pressed.record(code: code, pressed: isPressed)
        connection?.send(.key(code: code, pressed: isPressed, caps: caps))
    }

    func sendMouseButton(_ button: MouseButton, pressed isPressed: Bool) {
        // A release is always forwarded, even off the desktop: a button recorded
        // as held has to be able to come back up.
        guard session.canCaptureKeyboard || !isPressed else {
            return
        }
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
