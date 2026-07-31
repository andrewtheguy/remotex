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
    var macOSKeyboardOverridesEnabled: Bool {
        didSet {
            guard macOSKeyboardOverridesEnabled != oldValue else {
                return
            }
            preferences.macOSKeyboardOverridesEnabled = macOSKeyboardOverridesEnabled
            // The convention just changed under whatever is held down.
            releaseInput()
        }
    }

    private(set) var branding = "remotex"
    private(set) var session = ViewerSessionState()
    private(set) var targets: [TargetInfo] = []
    /// A launch or a relaunch is in flight, so the controls that would start
    /// another are locked.
    private(set) var isBusy = false
    /// Why there is no gateway to talk to, if there is not: an incomplete bundle, a
    /// config it refused, a gateway that died. Shown on the launch screen with the
    /// gateway's own output beneath it, because that output is what names the fix.
    private(set) var launchError: String?
    /// The gateway's recent stderr, shown under `launchError`.
    private(set) var launchLog = ""
    /// The alert.
    var actionError: String?
    /// Bumped by [`editConfiguration`], which is how a menu item — a SwiftUI command
    /// and an AppKit action, neither of which can present a sheet — asks the window to
    /// open the configuration panel. A counter rather than a flag: two requests in a
    /// row have to be two events, or the second one after a cancel does nothing.
    private(set) var configurationRequests = 0
    /// The same arrangement for the About panel, and a counter for the same reason.
    ///
    /// Unlike the configuration request this is never refused: what About is mainly
    /// for is the version, which an unbundled build with no gateway and no instance
    /// still has.
    private(set) var aboutRequests = 0
    /// The remote's pointer shape, once an engine hands one over. Nil means the
    /// remote is drawing its own pointer into the framebuffer.
    private(set) var remoteCursor: ServerMessage.Cursor?

    let clipboard: ClipboardSynchronizer
    let audio = AudioOutput()

    /// The gateway this app runs, and the config it runs on. Held rather than
    /// created per launch: `stop()` has to reach the same process `start()` made,
    /// and the configuration panel edits the same instance the gateway reads.
    @ObservationIgnored
    let gateway: EmbeddedGateway?
    @ObservationIgnored
    let config: GatewayConfigStore?

    @ObservationIgnored
    private let preferences: ViewerPreferences
    @ObservationIgnored
    private let urlSession: URLSession
    /// Nil until a gateway has handed over its port and token. Everything that
    /// talks to it is behind `beginSession`, which only runs once this exists.
    @ObservationIgnored
    private var client: GatewayClient?
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
    /// Whether a 401 has already bought this launch its one automatic relaunch.
    ///
    /// Cleared when a control message proves a session attached — the same proof
    /// `connectErrorIsFromRejection` is cleared by — so it bounds *consecutive*
    /// refusals rather than counting them for the life of the app. The runaway it
    /// exists to stop is the one with no progress in it: launch, 401, launch, 401,
    /// a process each time.
    @ObservationIgnored
    private var relaunchedForUnauthorized = false
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
    /// every frame, and a remote following the window acts on each one it is told
    /// about.
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

    /// Everything here is a parameter so a test can drive the model without the app
    /// around it: a throwaway pasteboard instead of the user's own, preferences in a
    /// temporary directory, a stubbed transport instead of the network, and **no
    /// gateway at all** — `nil` is the unbundled case (`swift test`, `swift run`),
    /// where there is no `remotex-gateway` to run and every test that matters drives
    /// a session over a scripted socket instead.
    init(
        preferences: ViewerPreferences = ViewerPreferences(url: nil),
        gateway: EmbeddedGateway? = nil,
        config: GatewayConfigStore? = nil,
        clipboard: ClipboardSynchronizer = ClipboardSynchronizer(),
        urlSession: URLSession = GatewayClient.defaultSession
    ) {
        self.preferences = preferences
        self.gateway = gateway
        self.config = config
        self.clipboard = clipboard
        self.urlSession = urlSession
        macOSKeyboardOverridesEnabled = preferences.macOSKeyboardOverridesEnabled
    }

    /// The app's own model: the instance directory this launch was given, the
    /// gateway in this bundle, and the config store over both.
    static func forApp(instance: InstanceDirectory = .resolved()) -> AppModel {
        let binary = GatewayBinary.inBundle()
        return AppModel(
            preferences: ViewerPreferences(url: instance.preferencesURL),
            gateway: binary.map { EmbeddedGateway(instance: instance, binary: $0) },
            config: binary.map { GatewayConfigStore.overBinary(instance: instance, binary: $0) },
            // Ephemeral, and not for isolation any more: this client authenticates
            // with a token on every request and is never issued a cookie, so there
            // is nothing to persist between launches and nothing to share with
            // another instance.
            urlSession: URLSession(configuration: .ephemeral)
        )
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
    /// outbound input path is written against.
    ///
    /// A live desktop and nothing else. Whether a *target* takes input at all is the
    /// target's own question, answered where it is configured — this client has no
    /// say in it and offers no way to hold input back.
    var canSendInput: Bool {
        session.canCaptureKeyboard
    }

    /// Whether `KeyboardCapture` takes key events for the remote at all.
    ///
    /// Capture is a local event monitor that swallows every Command chord the system
    /// hands this app, so while it is up this Mac's keyboard belongs to the guest.
    /// Moving focus off the desktop is what hands the chords back.
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

    /// The three resize items are one decision in three parts: the mode, and the
    /// two one-shots that only mean something in manual mode.
    ///
    /// "Auto Resize" needs nothing but the permission — it is the item that says
    /// which way this session works.
    var canAutoResize: Bool {
        session.canResize
    }

    /// Whether it is ticked.
    var autoResizes: Bool {
        session.autoResize
    }

    /// "Resize to Window" needs the permission and a measured window to report,
    /// and means nothing while the remote is already following this one.
    var canResizeNow: Bool {
        session.canResize && viewportSize != nil && !session.autoResize
    }

    /// Whether the local window may fit the remote. Disabled while the remote is
    /// following the window: a desktop that fits itself to this window cannot also
    /// be fitted to.
    var canResizeToDisplay: Bool {
        session.screen == .desktop && session.remoteSize != nil && !followsWindow
    }

    /// Whether the remote is *actually* following this window, which takes both
    /// halves. An rxa session that switched onto one of the Mac's own screens with
    /// the mode left on has the tick and no permission, and that display is holding
    /// as still as any other — so it can be fitted to.
    private var followsWindow: Bool {
        session.canResize && session.autoResize
    }

    /// The interstitial covers the connection lifecycle and the claim conflicts,
    /// and on the desktop it also covers the gap before the first frame. The
    /// picker owns the screen once connected.
    var showsStatusOverlay: Bool {
        session.connectionStatus != .connected
            || (session.screen == .desktop && session.remoteSize == nil)
    }

    // MARK: - Launching the gateway

    /// Bring the app up: make sure there is a config, start the gateway, and attach
    /// to it.
    ///
    /// There is nothing to ask first. The gateway is in this bundle, its address is
    /// whatever port it reports, and the token it prints is the whole of the
    /// authentication — so the screen between launching and the target picker exists
    /// only to say what went wrong when something does.
    func launch() async {
        guard !isBusy else {
            return
        }
        isBusy = true
        defer { isBusy = false }
        launchError = nil
        launchLog = ""
        session.screen = .launching

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

        let handshake: EmbeddedGateway.Handshake
        do {
            handshake = try await gateway.start()
        } catch {
            fail(with: error)
            return
        }

        let client = GatewayClient(
            gateway: .loopback(port: handshake.port),
            token: handshake.token,
            session: urlSession
        )
        self.client = client
        do {
            // Still checked, and now for a different reason: the two halves ship in
            // one bundle, so a mismatch is a broken build rather than an old server —
            // which is exactly the kind of thing that must not present as a hang.
            branding = try await client.configuration().branding
        } catch {
            fail(with: error)
            await gateway.stop()
            return
        }
        await beginSession(over: client)
    }

    /// Start over: stop the gateway, start it again, attach again.
    ///
    /// What a saved config change and the launch screen's **Retry** both do. The
    /// session goes with it — the gateway is the session's ground, and a new process
    /// has no memory of the old one's slot.
    ///
    /// Asking for it clears the automatic-relaunch budget below: a person pressing
    /// Retry is entitled to the same one free recovery a fresh launch gets.
    func relaunchGateway() async {
        relaunchedForUnauthorized = false
        await restartGateway()
    }

    private func restartGateway() async {
        await teardown()
        session = ViewerSessionState(screen: .launching)
        targets = []
        remoteCursor = nil
        await gateway?.stop()
        await launch()
    }

    /// Ask the window to open the configuration panel. Available wherever the app is:
    /// on the launch screen it is the fix for a refused config, and from the Remote
    /// menu it is how a target gets added.
    func editConfiguration() {
        guard config != nil else {
            return
        }
        configurationRequests += 1
    }

    /// Ask the window to open the About panel — which version this is, which instance
    /// it runs, and the gateway key a Mac has to authorize.
    func showAbout() {
        aboutRequests += 1
    }

    /// Put the launch screen up with a reason and whatever the gateway said.
    private func fail(with error: any Error) {
        launchError = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
        launchLog = gateway?.log() ?? ""
        session = ViewerSessionState(screen: .launching)
        targets = []
        // The client described a gateway process that is gone, so it is not a client
        // any more. Dropped rather than left to fail on its next use: `loadTargets`
        // treats a nil client as "nothing to ask" and a live one as a working gateway,
        // and a stale one would put a connection error on the launch screen — over the
        // failure that actually explains this.
        client = nil
    }

    /// The gateway exited on its own while the app was using it.
    ///
    /// Not restarted automatically: a gateway that died once on a config it accepted
    /// will die again, and a silent retry loop would hide the output that explains
    /// why. Retry is a button.
    private func gatewayDied(_ failure: EmbeddedGateway.LaunchFailure) {
        guard session.screen != .launching else {
            return
        }
        Task {
            await teardown()
            fail(with: failure)
        }
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

    /// The gateway refused this client's token.
    ///
    /// There is no "sign in again" any more: the token was minted by the process this
    /// app started and is good for as long as that process lives, so a 401 means the
    /// gateway behind it is not the one that issued it — it restarted, or died and
    /// something else answered on its port. Either way the answer is a fresh gateway,
    /// not fresh credentials.
    ///
    /// **Once.** A relaunch that comes straight back with another 401 is not a
    /// situation more relaunches improve, and each one spawns a process — so the second
    /// refusal goes to the launch screen with the gateway's own output instead, where
    /// Retry is a decision somebody makes rather than a loop nobody can see.
    private func handleUnauthorized() async {
        guard !relaunchedForUnauthorized else {
            await teardown()
            fail(with: EmbeddedGateway.LaunchFailure.refused(gateway?.log() ?? ""))
            return
        }
        // A relaunch already under way will attach or fail on its own; a second one
        // started on top of it would fight the first for the same process.
        guard !isBusy else {
            return
        }
        relaunchedForUnauthorized = true
        await restartGateway()
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
        // The same proof, spent on the same thing: this gateway accepted the token, so
        // a 401 arriving later is a new situation and gets its own automatic relaunch.
        relaunchedForUnauthorized = false
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
            // The mode goes with the session that was left, like the audio answer
            // below: the next target is asked about separately.
            session.autoResize = false
            session.canClipboard = false
            session.canAudio = false
            // The previous target's screens are not the next one's. Left in
            // place they would fill the Display menu for a target that has none,
            // and picking one would send a `selectDisplay` naming a display on
            // another machine.
            session.displays = []
            session.activeDisplayID = nil
            remoteCursor = nil
            // Cleared with the rest of what belonged to that target: the answer was
            // about the target being left, not a setting the next pick inherits. Not
            // cleared on a mere disconnection, which is what
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
            // The remote is ours after all, so an offer to take it over has nothing
            // left to refer to.
            session.remoteBusy = nil
            // The operator's permission. RXA remains denied until a display list
            // identifies an owned display; VNC and RDP are settled here. A fresh
            // policy also puts the mode back to manual, which is what makes a
            // reattach, a target switch and a takeover all leave the remote's own
            // size alone.
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
            // Both dedupes would otherwise swallow the first report of this session
            // for repeating a size already sent — for the previous target, or for
            // the picker. Both have to be cleared, not just the policy's: the
            // queue's memo survives a target switch because the socket does.
            //
            // Nothing is reported here. A freshly started engine knows nothing
            // about this window, and in manual mode that is the correct state to
            // leave it in: the remote keeps the size it has until somebody asks for
            // another. What used to be sent from here is now the first thing
            // `setAutoResize(true)` does.
            viewportPolicy.resetForNewConnection()
            connection?.resetViewportMemo()
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

        case .remoteBusy(let holder, let heldSecs, let takenOver):
            // Not an error: the remote answered correctly and said who has it. Like
            // `error` it must survive the `picker` that follows, and unlike `error`
            // it carries an action — the picker offers to take the remote over. The
            // target is the one this session was for, which is the pick still
            // pending on the ordinary refusal and the connected one when somebody
            // took the remote from us mid-session.
            setConnectError(nil)
            session.remoteBusy = .init(
                target: session.pendingTarget ?? session.connectedTarget ?? "",
                holder: holder,
                heldSecs: heldSecs,
                takenOver: takenOver
            )
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
        )
    }

    /// Whether the Remote menu's audio toggle can be used.
    private func updateAudioAvailability() {
        audio.update(
            available: session.screen == .desktop
                && session.connectionStatus == .connected
                && session.canAudio
        )
    }

    private func loadTargets() async {
        guard let client else {
            return
        }
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
    /// frame, and a remote following the window acts on every report it receives.
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
        session.canResize = viewportPolicy.allowed
        session.autoResize = viewportPolicy.autoFollows
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

    /// Pick a target from the picker.
    ///
    /// `force` takes the *remote's* session from whoever holds it, and is how the
    /// picker's Take over answers a `remoteBusy`. Nothing to do with `takeOver()`
    /// below, which claims this gateway's slot from another client of it.
    func connect(to target: String, force: Bool = false) {
        guard session.screen == .picker, session.pendingTarget == nil else {
            return
        }
        session.pendingTarget = target
        setConnectError(nil)
        session.remoteBusy = nil
        connection?.send(.connect(target: target, force: force))
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

    /// "Auto Resize": hand the remote's size to this window, or take it back.
    ///
    /// Switching on reports at once rather than waiting for the next window
    /// resize — "the remote follows this window" that starts by not matching it
    /// would read as an item that did nothing. It goes through the requested path
    /// deliberately: turning this on *is* a resize the user asked for, and the
    /// dedupe makes it free when the size already matches.
    func setAutoResize(_ enabled: Bool) {
        guard session.canResize else {
            return
        }
        viewportPolicy.autoFollows = enabled
        publishViewportPolicy()
        if enabled {
            sendViewport(manual: true)
        }
    }

    /// "Resize to Window": one report, now, whatever the mode.
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
    /// release, because every path that closes the gate calls `releaseInput` first,
    /// so there is nothing left to release.
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
        // Held is the part that has to be checked rather than assumed. Every path
        // that closes the gate releases what was held first, so the physical mouseUp
        // that follows one is for a button the remote has already been told about;
        // forwarding it would be the one input event to get past a closed gate.
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

}
