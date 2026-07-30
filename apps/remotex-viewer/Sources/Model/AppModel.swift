import AppKit
import Observation

/// The viewer's whole state, and the one place the gateway's control messages
/// become it.
///
/// `ViewerScreen` is derived here rather than reported to the viewer: `picker` and
/// `connected` come from the gateway's session layer, and everything the Remote
/// menu enables or disables follows from them plus `resize` and `remoteOs`.
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

    /// Per-target view-only intent. It blocks keyboard, pointer, wheel, and
    /// clipboard traffic while leaving rendering and inbound audio active.
    var isViewOnly = false {
        didSet {
            guard isViewOnly != oldValue else {
                return
            }
            // Whatever is held was pressed while input was still going through, and
            // the paths that would have released it are the ones just closed.
            releaseInput()
            updateClipboardEnablement()
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
    let audio = AudioOutput()

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
    /// Whether `session.connectError` is a *rejected connection's* reason rather than
    /// an error the gateway sent about an attached one.
    ///
    /// The two are cleared at different moments, which is the whole reason this is
    /// tracked. A gateway error has to survive the `picker` that follows it — the
    /// picker is where it is read — while a reason the session could not be opened is
    /// stale the moment one is: any control message means the link works, so leaving
    /// it up blames a working session for a failure it recovered from.
    @ObservationIgnored
    private var connectErrorIsFromRejection = false
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
    /// Reads current host backing scale on demand; cached screen-change
    /// notifications can be stale after AppKit restores a saved window frame.
    @ObservationIgnored
    var hostScaleReader: (() -> CGFloat)?
    /// The last value reported to the remote. `nil` means nothing has been
    /// reported for this connection or this display yet, so the next report goes
    /// even if it repeats the number.
    @ObservationIgnored
    private var lastHostScale: UInt16?
    /// This screen's density in hundredths, for the Display menu to show beside
    /// the remote's. Nothing about how the desktop is presented reads it — that
    /// comes from `session.remoteScale` alone, see `RemoteGeometry`. Observed
    /// where `lastHostScale` is not, because a menu has to redraw when it moves.
    private(set) var hostScale: UInt16 = 100
    /// Deliberately outside Observation. Tiles arrive dozens of times a second;
    /// routing them through `@Observable` would invalidate the view hierarchy on
    /// every strip.
    @ObservationIgnored
    private weak var renderer: FramebufferRenderer?
    /// The AppKit half of "Resize to Display", installed by the surface's
    /// coordinator for as long as it is on screen. Sizing a window needs the window,
    /// the scroll view and the room it gives the document, none of which this model
    /// has any business holding.
    ///
    /// Outside Observation deliberately: it is installed from inside SwiftUI's own
    /// update pass, where changing observed state is undefined, and it says nothing
    /// the menu's enabled state is derived from — the surface exists for the picker
    /// as well as the desktop, so on the screen where the item is enabled this is
    /// always set.
    @ObservationIgnored
    var fitWindowToRemote: (() -> Void)?

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

    /// Whether anything the user does may reach the remote. The single gate every
    /// outbound input path is written against, and the whole of what view only
    /// means.
    var canSendInput: Bool {
        session.canCaptureKeyboard && !isViewOnly
    }

    /// Whether `KeyboardCapture` takes key events for the remote at all.
    ///
    /// False in view only, and that is the point of the mode rather than a
    /// consequence of it: capture is a local event monitor that swallows every
    /// Command chord the system hands this app, so while it is up this Mac's keyboard
    /// belongs to the guest. Suspending it is the only way to get the chords back.
    var canCaptureKeyboardNow: Bool {
        canSendInput && session.remoteSize != nil
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

    /// Whether the local window may fit the remote. Disabled for a VNC desktop
    /// that continuously follows the window.
    var canResizeToDisplay: Bool {
        session.screen == .desktop && session.remoteSize != nil && !session.followsWindow
    }

    /// The interstitial covers the connection lifecycle and the claim conflicts,
    /// and on the desktop it also covers the gap before the first frame. The
    /// picker owns the screen once connected.
    var showsStatusOverlay: Bool {
        session.connectionStatus != .connected
            || (session.screen == .desktop && session.remoteSize == nil)
    }

    // MARK: - The server step

    /// Validate the entered gateway, persist it only after success, and continue
    /// to login or session according to the existing cookie.
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

    /// Back to the server step, to point somewhere else. The login screen's
    /// **Change** link is the only caller.
    ///
    /// The credentials step is also the only screen it is allowed from, and the
    /// guard says so rather than trusting that: the gateway is the ground
    /// everything after it stands on — the login cookie is scoped to that host,
    /// the claim token was minted by it, the socket is attached to it — so moving
    /// it out from under a live session is a log out that does not say so. From
    /// the picker or the desktop, Log Out is the step to take first, and it lands
    /// here. On the server step there is nothing to change *to*: it is already the
    /// step that asks.
    func changeGateway() async {
        guard session.screen == .login, !isBusy else {
            return
        }
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
            // The reason, not the word "network": half of what reaches here is not
            // the network at all, and the half that is says which half far better
            // than this could (see `GatewayClientError`).
            loginError = (error as? LocalizedError)?.errorDescription
                ?? error.localizedDescription
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
        audio.send = { [weak connection] message in
            connection?.send(message)
        }
        // The local audio device refusing is worth an alert: the user pressed Enable
        // Audio and nothing happened, and this is the only failure on that path a
        // client can actually distinguish. A remote that is merely quiet says nothing,
        // here or in the SPA.
        audio.report = { [weak self] message in
            self?.showError(message)
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
        audio.send = nil
        audio.report = nil
        audio.reset()
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
            updateAudioAvailability()
        case .control(let message):
            handle(message)
        case .tiles(let tiles):
            renderer?.upload(tiles)
        case .audio(let packets):
            audio.play(packets: packets)
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
        case .rejected(let reason):
            // Wherever the user is: the picker shows `connectError` under the
            // branding, and `StatusOverlayView` shows it under whatever the
            // connection status is. Neither replaces the status — a session still
            // reconnecting is still reconnecting — it stops being the *only* thing
            // said about a failure.
            setConnectError(reason, fromRejection: true)
            session.pendingTarget = nil
        }
    }

    /// Set or clear what the picker and the interstitial say about a session that did
    /// not open. `fromRejection` decides whether it goes away by itself — see
    /// `connectErrorIsFromRejection`.
    private func setConnectError(_ message: String?, fromRejection: Bool = false) {
        session.connectError = message
        connectErrorIsFromRejection = message != nil && fromRejection
    }

    private func handle(_ message: ServerMessage) {
        // Any control message is proof the session attached, so a reason it could not
        // is now history. Cleared before the switch so `error` below can set its own
        // in the same pass — that one is about the attached session and stays until
        // something replaces it.
        if connectErrorIsFromRejection {
            setConnectError(nil)
        }
        switch message {
        case .picker:
            session.screen = .picker
            session.connectedTarget = nil
            session.pendingTarget = nil
            session.protocolName = nil
            session.remoteIsMac = false
            session.remoteSize = nil
            // Back to the default density, with the size: it is the next
            // target's first `resize` that says what its is, and until then a
            // Retina Mac's 2 would double the viewport reported for whatever
            // was picked next — including the report `connected` sends before
            // any resize has arrived.
            session.remoteScale = 1
            session.canResize = false
            session.followsWindow = false
            session.canClipboard = false
            session.canAudio = false
            // The previous target's screens are not the next one's. Left in
            // place they would fill the Display menu for a target that has none,
            // and picking one would send a `selectDisplay` naming a display on
            // another machine.
            session.displays = []
            session.activeDisplayID = nil
            remoteCursor = nil
            // Cleared with the rest of what belonged to that target: view only is an
            // answer about the session being left, not a setting the next pick
            // inherits, and the picker's checkbox says as much by starting clear.
            isViewOnly = false
            // Cleared for the same reason as view only: the answer was about the target
            // being left. Not cleared on a mere disconnection, which is what
            // `AudioOutput.update(available:)` handles — a reconnect keeps playing.
            audio.reset()
            viewportPolicy = ViewportPolicy()
            updateClipboardEnablement()
            updateAudioAvailability()
            clipboard.failPendingFetch()
            Task { await loadTargets() }

        case .connected(let payload):
            session.screen = .desktop
            session.connectedTarget = payload.name
            session.protocolName = payload.protocolName
            session.pendingTarget = nil
            setConnectError(nil)
            // RXA remains non-resizable until a display list identifies an owned
            // display; VNC and RDP are settled here.
            viewportPolicy = ViewportPolicy(
                protocolName: payload.protocolName,
                resize: payload.resize
            )
            publishViewportPolicy()
            session.canClipboard = payload.clipboard
            updateClipboardEnablement()
            session.canAudio = payload.audio
            updateAudioAvailability()
            // The gateway's audio subscription belongs to an *attachment*, so a
            // reconnect arrives with it off while the menu still says on. Re-asserted
            // here for the same reason the viewport and the host scale are below: a
            // freshly attached session knows nothing about this client.
            audio.reassert()
            // A freshly started engine knows nothing about this window, and both
            // dedupes would swallow the first report for repeating a size already
            // sent — for the previous target, or for the picker. Both have to be
            // cleared, not just the policy's: the queue's memo survives a target
            // switch because the socket does.
            viewportPolicy.resetForNewConnection()
            connection?.resetViewportMemo()
            sendViewport(manual: false)
            // And this window's screen density, so a display the agent made comes
            // up matching the screen it is about to be shown on rather than at
            // whatever it was left at. Undeduped for the same reason as the
            // viewport: the previous target's value means nothing to this one.
            lastHostScale = nil
            sendHostScale()

        case .resize(let w, let h, let scale):
            let size = DisplayMode(w: w, h: h)
            session.remoteSize = size
            // The texture is the remote's pixels; the density only decides how
            // large those pixels are drawn (`RemoteGeometry`), so the renderer
            // never hears about it.
            session.remoteScale = scale > 0 ? CGFloat(scale) : 1
            renderer?.resize(to: size)

        case .displays(let active, let displays):
            let switched = session.activeDisplayID != active
            session.displays = displays
            session.activeDisplayID = active
            // Whether "Resize to Window" is offered is a question about *this*
            // display, not only about the target: a Mac's own panel is never
            // resized from here, and the user can switch onto and off an
            // agent-made one from the Display menu at any point. An `active` the
            // list does not contain — a screen unplugged between the two, which
            // this message allows — reads as not virtual, leaving the item greyed
            // rather than offering to resize a display nobody here can name.
            viewportPolicy.sharing(
                virtualDisplay: displays.first { $0.id == active }?.isVirtual ?? false
            )
            // Harmless for RDP and VNC, whose policy the call above cannot touch:
            // it writes back the values that are already there.
            publishViewportPolicy()
            // A different display is being shared, and the density was only ever
            // reported *at* the previous one — the agent acts on it for the display
            // it is currently sharing, so a switch onto one the agent made would
            // otherwise leave it at whatever density macOS remembered. The dedupe
            // has to be cleared for the same reason: the number is usually
            // unchanged, and it is the display underneath it that moved.
            if switched {
                lastHostScale = nil
                sendHostScale()
            }

        case .remoteOs(let macos):
            // Which Mac a Command chord belongs to just changed, so nothing may
            // stay held under the old convention.
            if session.remoteIsMac != macos {
                releaseInput()
            }
            session.remoteIsMac = macos

        case .error(let message):
            // Not fatal — the session returns to the picker, which is where this
            // is shown, and it must survive that `picker` arriving.
            setConnectError(message)
            session.pendingTarget = nil

        case .clipboard(let payload):
            receive(clipboard: payload)

        case .cursor(let payload):
            // Receiving one of these at all means the viewer owns pointer
            // rendering for the rest of the session.
            remoteCursor = payload

        case .audioFormat(let format):
            audio.start(format: format)

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
                && !isViewOnly
        )
    }

    /// Whether the Remote menu's audio toggle can be used.
    ///
    /// Unlike the clipboard's, view only is *not* one of the conditions: that mode is
    /// about nothing this Mac does reaching the remote, and sound travels the other way.
    /// Watching a desktop without touching it is exactly when audio is wanted.
    private func updateAudioAvailability() {
        audio.update(
            available: session.screen == .desktop
                && session.connectionStatus == .connected
                && session.canAudio
        )
    }

    private func loadTargets() async {
        do {
            targets = try await client.targets()
        } catch GatewayClientError.unauthorized {
            await handleUnauthorized()
        } catch {
            setConnectError(error.localizedDescription)
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

    /// Publish ignored policy state so Observation invalidates menu enablement.
    private func publishViewportPolicy() {
        session.canResize = viewportPolicy.manualOnly
        session.followsWindow = viewportPolicy.followsWindow
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

    /// The density of the screen this window is on, reported to the remote.
    ///
    /// Called on connect and whenever the window changes screen. Deduped because
    /// acting on it is expensive at the other end — a WindowServer reconfigure on
    /// an `rxa` Mac, a full reactivation on an RDP host that allows resize — so
    /// re-sending a value it already matches costs a relayed desktop or a repaint
    /// for nothing. Unlike the viewport there is no debounce: a window changes
    /// screen once, discretely, where a drag-resize reports every frame.
    func reportHostScale() {
        sendHostScale()
    }

    private func sendHostScale() {
        // Hundredths, because the wire carries an integer. A screen whose scale
        // is not a positive finite number is not one AppKit describes, so 1x is
        // the answer that asks the remote for the least.
        let read = hostScaleReader?() ?? 1
        let usable = read.isFinite && read > 0 ? read : 1
        let scale = UInt16(clamping: Int((usable * 100).rounded()))
        // Recorded before either guard below, and whether or not it is sent: the
        // Display menu shows this number, and a density this Mac's screen has that
        // the remote does not is exactly what someone reading it is looking for.
        // Observed, unlike `lastHostScale`, so the menu redraws when it moves.
        hostScale = scale
        guard session.screen == .desktop, scale != lastHostScale else {
            return
        }
        lastHostScale = scale
        connection?.send(.hostScale(scale: scale))
    }

    // MARK: - Actions

    func connect(to target: String) {
        guard session.screen == .picker, session.pendingTarget == nil else {
            return
        }
        session.pendingTarget = target
        setConnectError(nil)
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

    /// Share a different one of the remote's displays (the Display menu).
    ///
    /// Fire and forget, and deliberately not optimistic: the answer is the
    /// remote's next `displays`, which is what moves the checkmark. Selecting
    /// the display already active is dropped here rather than costing the remote
    /// a capture restart and this viewer a full repaint.
    func selectDisplay(_ id: UInt32) {
        guard id != session.activeDisplayID else {
            return
        }
        connection?.send(.selectDisplay(id: id))
    }

    /// "Resize to Window": the one report that gets past `manualOnly`.
    func resizeToWindow() {
        guard session.canResize else {
            return
        }
        sendViewport(manual: true)
    }

    /// "Resize to Display": the window takes the remote's size.
    ///
    /// Nothing goes on the wire — this is entirely local, which is why it is
    /// available alongside the item above rather than instead of it.
    func resizeToDisplay() {
        guard canResizeToDisplay else {
            return
        }
        fitWindowToRemote?()
    }

    func sendPointer(x: Int32, y: Int32) {
        guard canSendInput else {
            return
        }
        connection?.send(.mouseMove(x: x, y: y))
    }

    func sendWheel(dx: Float, dy: Float) {
        guard canSendInput else {
            return
        }
        connection?.send(.wheel(dx: dx, dy: dy))
    }

    /// Gated here as well as in `KeyboardCapture`, which is the only caller: this
    /// is where "nothing reaches the remote" is a property of the model rather than
    /// of one event monitor. Unlike a mouse button there is no exception for a
    /// release, because there is nothing left to release — switching view only on
    /// let go of everything held.
    func sendKey(code: String, pressed isPressed: Bool, caps: Bool) {
        guard canSendInput else {
            return
        }
        pressed.record(code: code, pressed: isPressed)
        connection?.send(.key(code: code, pressed: isPressed, caps: caps))
    }

    func sendMouseButton(_ button: MouseButton, pressed isPressed: Bool) {
        // A release gets past the gate, but only for a button this client recorded
        // as held — which is the whole case the exception is for: a press that did
        // go through, whose mouseUp lands after the screen changed under it.
        //
        // Held is the part that has to be checked rather than assumed. Switching
        // view only on releases everything first, so the physical mouseUp that
        // follows is for a button the remote has already been told about; forwarding
        // it would be the one input event to get past the toggle, on a path whose
        // own tests say nothing does.
        guard canSendInput || (!isPressed && pressed.isHeld(button)) else {
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
