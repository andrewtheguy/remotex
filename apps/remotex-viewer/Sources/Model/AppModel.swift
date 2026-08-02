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

    /// The remembered "follow this window's size" default, bound to the picker's
    /// "… if compatible" toggle. One value with two editors: the View menu's live
    /// "Auto Resize" writes it too (through `setAutoResize`), so a choice made
    /// mid-session is the one the next connection starts from. Applied to a session
    /// in `handle(_:)` only where the target can honour it.
    var autoResizeByDefault: Bool {
        didSet {
            guard autoResizeByDefault != oldValue else {
                return
            }
            preferences.autoResizeByDefault = autoResizeByDefault
        }
    }

    /// The remembered "play the remote's sound" default, bound to the picker's
    /// "… if compatible" toggle. Same one-value-two-places arrangement as
    /// `autoResizeByDefault`; the Remote menu's live "Enable Audio" writes it
    /// through `setAudioEnabled`.
    var audioByDefault: Bool {
        didSet {
            guard audioByDefault != oldValue else {
                return
            }
            preferences.audioByDefault = audioByDefault
        }
    }

    /// Bound to the home screen's address field. Not the choice itself — see
    /// `chosen` — because a field being typed into is not yet a decision.
    var gatewayAddress: String
    /// Which of the two options the home screen has selected.
    var prefersRemoteGateway: Bool

    private(set) var branding = "remotex"
    private(set) var session = ViewerSessionState()
    private(set) var targets: [TargetInfo] = []
    /// Which gateway this session is against, once one has been chosen. Nil on the
    /// home screen, which is the screen that answers it.
    ///
    /// Everything that differs between the two branches reads this: whether there is
    /// a process of ours to restart, whether the configuration panel describes
    /// anything the session can see, and what a 401 means.
    private(set) var chosen: GatewayChoice?
    /// A launch, a login or a relaunch is in flight, so the controls that would start
    /// another are locked.
    private(set) var isBusy = false
    /// Shown on the home screen: an address that will not parse, a gateway that
    /// cannot be reached, or one speaking a protocol this build does not.
    private(set) var homeError: String?
    /// Shown under the credentials on the login screen.
    private(set) var loginError: String?
    /// Why there is no gateway to talk to, if there is not: an incomplete bundle, a
    /// config it refused, a gateway that died. Shown on the launch screen with the
    /// gateway's own output beneath it, because that output is what names the fix.
    /// The embedded branch only — a remote gateway's failures land on the home
    /// screen, where the address that produced them is.
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
    let audio: AudioControl

    /// The gateway this app can run, and the config it would run on. Held rather
    /// than created per launch: `stop()` has to reach the same process `start()`
    /// made, and the configuration panel edits the same instance the gateway reads.
    ///
    /// Present whether or not it is in use — this is the bundle's gateway, not the
    /// session's. `usesEmbeddedGateway` is the question about the session.
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
    /// Deliberately outside Observation. Frames arrive dozens of times a second;
    /// routing them through `@Observable` would invalidate the view hierarchy on
    /// every batch.
    @ObservationIgnored
    private weak var canvas: (any RemoteCanvas)?
    /// The last input gate sent to the page, so the same answer is not resent
    /// after every session event. Cleared with the canvas.
    @ObservationIgnored
    private var lastInputEnabled: Bool?
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
        audio: AudioControl = AudioControl(),
        urlSession: URLSession = GatewayClient.defaultSession
    ) {
        self.preferences = preferences
        self.gateway = gateway
        self.config = config
        self.clipboard = clipboard
        self.audio = audio
        self.urlSession = urlSession
        macOSKeyboardOverridesEnabled = preferences.macOSKeyboardOverridesEnabled
        autoResizeByDefault = preferences.autoResizeByDefault
        audioByDefault = preferences.audioByDefault
        gatewayAddress = preferences.remoteGatewayAddress ?? ""
        // An app with no gateway to run has one option, and it is the other one. The
        // preference cannot say so on a first launch, so the bundle does.
        prefersRemoteGateway = preferences.prefersRemoteGateway || gateway == nil
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
        guard let target = session.connectedTarget else {
            return branding
        }
        // A speaker suffix while sound is playing — the one persistent surface that
        // can say so, since the toggle is a menu item nobody is looking at. In
        // session only: `audio.isEnabled` is cleared on the way to the picker, so
        // this never trails the branding on the picker screen.
        let base = "\(target) — remotex"
        return audio.isEnabled ? "\(base) 🔊" : base
    }

    /// Whether this session runs on the gateway in this bundle.
    ///
    /// False on the home screen too, where nothing has been chosen yet: everything
    /// that reads this is asking "may I act on the local gateway", and before a
    /// choice the answer is no.
    var usesEmbeddedGateway: Bool {
        chosen?.isEmbedded ?? false
    }

    /// Whether **Configuration…** means anything right now.
    ///
    /// It describes the gateway in this bundle: the file is the one
    /// `serve-embedded` reads. Against a remote gateway it is not part of the
    /// session — its targets are its own, edited wherever it runs — so offering it
    /// would be offering to edit a file nothing on screen is reading.
    var canEditConfiguration: Bool {
        config != nil && usesEmbeddedGateway
    }

    /// Whether there is a live session to leave, which is what decides between
    /// "Change Gateway…" being a way back and being the screen you are already on.
    var canChangeGateway: Bool {
        chosen != nil && !isBusy
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
    /// halves: the permission and the mode. A session with resize denied is not
    /// following the window whatever the mode says, so that display is holding as
    /// still as any other — and can be fitted to.
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

    // MARK: - Choosing a gateway

    /// Act on the home screen's choice.
    ///
    /// The two branches diverge here and nowhere else: the embedded one starts a
    /// process and takes the token it prints, the remote one validates an address and
    /// asks whether this client is signed in. Past this point they are the same
    /// session over the same routes — see `GatewayClient`.
    func chooseGateway() async {
        guard !isBusy, session.screen == .home else {
            return
        }
        homeError = nil
        loginError = nil
        guard prefersRemoteGateway else {
            preferences.prefersRemoteGateway = false
            await launch()
            return
        }

        let location: GatewayLocation
        do {
            location = try GatewayLocation.parse(gatewayAddress)
        } catch {
            homeError = error.localizedDescription
            return
        }
        // Normalized: a bare host gains a scheme, a path is dropped. Shown back
        // before anything is attempted, so what answered is what is on screen.
        gatewayAddress = location.url.absoluteString
        await connectToRemote(location)
    }

    /// Validate a remote gateway and go on to its login or its picker.
    private func connectToRemote(_ location: GatewayLocation) async {
        isBusy = true
        defer { isBusy = false }

        // The stored token is a hint, not an assumption: sessions live in the
        // gateway's memory with a sliding expiry, so one that restarted an hour ago
        // has forgotten this. `authState` is what decides.
        var client = GatewayClient(
            gateway: location,
            credential: preferences.remoteSessionToken.map { .session($0) } ?? .none,
            session: urlSession
        )

        let state: GatewayAuthState
        do {
            branding = try await client.configuration().branding
            state = try await client.authState()
        } catch {
            homeError = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
            return
        }
        if state == .noLoginOffered {
            // Almost always another copy of this app: `serve-embedded` binds
            // loopback and refuses `/api/auth/*`, and its token was handed to the
            // client that started it and to nothing else.
            homeError = """
                That gateway takes no login, so nothing here can sign in to it. \
                It is an embedded gateway, which only the app that started it can use.
                """
            return
        }
        // Persisted only once it answered, so a typo does not become the address the
        // next launch starts from.
        chosen = .remote(location)
        preferences.prefersRemoteGateway = true
        preferences.remoteGatewayAddress = location.url.absoluteString

        guard state == .authenticated else {
            // The stored token, if there was one, is not one any more.
            preferences.remoteSessionToken = nil
            client = client.with(credential: .none)
            self.client = client
            session = ViewerSessionState(screen: .login)
            return
        }
        self.client = client
        await beginSession(over: client)
    }

    /// Sign in to the remote gateway. The address was already validated by the home
    /// screen, so a failure here can only be about who you are.
    func logIn(username: String, password: String) async {
        guard !isBusy, let client, case .remote = chosen else {
            return
        }
        isBusy = true
        defer { isBusy = false }
        loginError = nil
        do {
            switch try await client.logIn(username: username, password: password) {
            case .ok(let token):
                let authenticated = client.with(credential: .session(token))
                self.client = authenticated
                preferences.remoteSessionToken = token
                await beginSession(over: authenticated)
            case .invalidCredentials:
                loginError = "Invalid credentials"
            case .missingSessionCookie:
                loginError = "The gateway accepted the login but set no session cookie."
            case .noLoginOffered:
                loginError = "That gateway takes no login."
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

    /// Back to the home screen, to choose again.
    ///
    /// The one way out of a chosen gateway, and it gives up everything that stood on
    /// it: the session, the login, and the gateway process if it was ours. A remote
    /// gateway is told, so its own session slot and login are released rather than
    /// left for the reattach grace period — which is what made a browser that came
    /// back inside the minute resume a desktop instead of seeing the picker (see
    /// `logout_handler` in src/server.rs).
    func changeGateway() async {
        guard !isBusy else {
            return
        }
        isBusy = true
        defer { isBusy = false }
        await teardown()
        if case .remote = chosen, let client {
            try? await client.logOut()
        }
        preferences.remoteSessionToken = nil
        await gateway?.stop()
        client = nil
        chosen = nil
        relaunchedForUnauthorized = false
        session = ViewerSessionState(screen: .home)
        targets = []
        remoteCursor = nil
        branding = "remotex"
        launchError = nil
        launchLog = ""
        loginError = nil
        homeError = nil
    }

    // MARK: - Launching the gateway

    /// Bring the embedded gateway up: make sure there is a config, start it, and
    /// attach.
    ///
    /// Nothing is asked here. The gateway is in this bundle, its address is whatever
    /// port it reports, and the token it prints is the whole of the authentication —
    /// so the screen between launching and the target picker exists only to say what
    /// went wrong when something does.
    private func launch() async {
        guard !isBusy else {
            return
        }
        isBusy = true
        defer { isBusy = false }
        launchError = nil
        launchLog = ""
        chosen = .embedded
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
            credential: .token(handshake.token),
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
    /// The embedded branch only, and the guard is why it is safe to leave the menu
    /// item enabled by a condition rather than by a screen: there is no process of
    /// ours behind a remote gateway to restart, and "restart" there would mean
    /// restarting somebody else's server.
    ///
    /// Asking for it clears the automatic-relaunch budget below: a person pressing
    /// Retry is entitled to the same one free recovery a fresh launch gets.
    func relaunchGateway() async {
        guard usesEmbeddedGateway else {
            return
        }
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

    /// Ask the window to open the configuration panel. Available wherever the
    /// embedded gateway is in use: on the launch screen it is the fix for a refused
    /// config, and from the Remote menu it is how a target gets added.
    func editConfiguration() {
        guard canEditConfiguration else {
            return
        }
        configurationRequests += 1
    }

    /// Ask the window to open the About panel — which version this is and which
    /// instance it runs.
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
        // Not while the launch screen is already up, and not at all once this app has
        // moved to a remote gateway: `changeGateway` stops the local one on the way
        // past, and an exit it asked for must not put a failure screen over the
        // session that replaced it.
        guard session.screen != .launching, usesEmbeddedGateway else {
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
        audio.stopPlayback = { [weak self] in
            self?.canvas?.send(.audioStop)
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
        audio.stopPlayback = nil
    }

    /// The gateway refused this client's credential — and what that means is the one
    /// place the two branches genuinely differ after a session has started.
    ///
    /// **A remote gateway** issued a session that has expired, been logged out
    /// elsewhere, or died with the process holding it. That is what a login is for,
    /// so it is offered: the stored token is dropped so the next launch does not
    /// present it again, and the login screen comes up.
    ///
    /// **The embedded gateway** has no login to offer. Its token was minted by the
    /// process this app started and is good for as long as that process lives, so a
    /// 401 means the gateway behind that port is not the one that issued it — it
    /// restarted, or died and something else answered there. The answer is a fresh
    /// gateway, not fresh credentials.
    ///
    /// **Once,** on that branch. A relaunch that comes straight back with another 401
    /// is not a situation more relaunches improve, and each one spawns a process — so
    /// the second refusal goes to the launch screen with the gateway's own output
    /// instead, where Retry is a decision somebody makes rather than a loop nobody
    /// can see.
    private func handleUnauthorized() async {
        guard usesEmbeddedGateway else {
            await teardown()
            preferences.remoteSessionToken = nil
            client = client?.with(credential: .none)
            session = ViewerSessionState(screen: .login)
            targets = []
            remoteCursor = nil
            loginError = "The gateway ended this session. Sign in again."
            return
        }
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
        // Every session transition arrives here, so this is the one place the
        // page's input gate can be kept in step with `canSendInput` without
        // three call sites having to remember to.
        defer { publishInputEnablement() }
        switch event {
        case .status(let status):
            session.connectionStatus = status
            updateClipboardEnablement()
            updateAudioAvailability()
        case .control(let message):
            handle(message)
        case .frame(let data):
            canvas?.send(frame: data)
        case .clearFramebuffer:
            // Dropping the size is what puts the "waiting for the remote
            // desktop" interstitial back up; the gateway always repaints in full.
            session.remoteSize = nil
            canvas?.send(.clear)
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
            // `AudioControl.update(available:)` handles — a reconnect keeps playing.
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
            // Whether this attach is one the user asked for — a pick or a takeover,
            // both of which set `pendingTarget` — as opposed to a silent reconnect,
            // which does not. Read before the line below clears it. Only a
            // user-initiated attach seeds the audio default: a reconnect keeps the
            // last attachment's intent (see the audio block below), and re-seeding
            // there would un-mute a session the user muted before it dropped.
            let userInitiated = session.pendingTarget != nil
            session.pendingTarget = nil
            setConnectError(nil)
            // The operator's permission, settled here for every protocol. A fresh
            // policy also puts the mode back to manual, which is what makes a
            // reattach, a target switch and a takeover all leave the remote's own
            // size alone.
            viewportPolicy = ViewportPolicy(resize: payload.resize)
            publishViewportPolicy()
            session.canClipboard = payload.clipboard
            updateClipboardEnablement()
            session.canAudio = payload.audio
            updateAudioAvailability()
            if userInitiated, audioByDefault, session.canAudio {
                // A pick or takeover, the remembered default wants sound, and the
                // target carries it: subscribe. `audio.reset()` on the `picker`
                // before this left intent off, so this is the first ask of the
                // attachment — where the target has no audio the default silently
                // does nothing, which is the "… if compatible" of the picker toggle.
                audio.setEnabled(true)
            } else {
                // A reconnect, or no default, or a target with no sound. The
                // gateway's subscription belongs to an *attachment*, so a reconnect
                // arrives with it off while the menu still says on; re-asserted here
                // for the same reason the viewport and host scale are below — a
                // freshly attached session knows nothing about this client. A no-op
                // when intent is off, so it also covers the other two cases.
                audio.reassert()
            }
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
            // Seed the remembered auto-resize default now that the dedupes are
            // clear — its first act is a viewport report, and reporting before the
            // reset above would have it swallowed. Permission is settled at connect
            // for every protocol, so the default takes or does not take effect here;
            // where the target allows no resize, this does not fire and the default
            // silently does nothing.
            if autoResizeByDefault, session.canResize {
                applyAutoResize(true)
            }
            // And this window's screen density, so a display the agent made comes
            // up matching the screen it is about to be shown on rather than at
            // whatever it was left at. Undeduped for the same reason as the
            // viewport: the previous target's value means nothing to this one.
            lastHostScale = nil
            sendHostScale()

        case .resize(let w, let h, let scale):
            let size = DisplayMode(w: w, h: h)
            session.remoteSize = size
            // Sanitized once and used for both. A gateway reporting a density of
            // zero would otherwise leave the page dividing by whatever it made of
            // the raw number while this side had already settled on 1 — and
            // `canvasAttached` re-sends the sanitized one, so a page that
            // reloaded would present the desktop at a different size than the
            // page that did not.
            let density = scale > 0 ? scale : 1
            session.remoteScale = CGFloat(density)
            // The canvas bitmap is the remote's pixels and its CSS box is those
            // pixels divided by this density, so unlike the Metal texture this
            // replaced, the page needs both numbers — see `desktopCanvas.ts`.
            canvas?.send(.resize(w: w, h: h, scale: density))

        case .displays(let active, let displays):
            let switched = session.activeDisplayID != active
            session.displays = displays
            session.activeDisplayID = active
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
            canvas?.send(.cursor(payload))

        case .audioFormat(let format):
            canvas?.send(.audioFormat(format))

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

    /// Which surface's canvas this is, when one named itself.
    ///
    /// SwiftUI may build the replacement `RemoteCanvasHost` *before* dismantling
    /// the one it replaces, so a coordinator's teardown can run after a newer one
    /// has already installed its own canvas here. Detaching by identity rather
    /// than unconditionally is what keeps that teardown from taking the live
    /// surface with it.
    @ObservationIgnored
    private var canvasOwner: ObjectIdentifier?

    func attach(canvas: (any RemoteCanvas)?, owner: ObjectIdentifier? = nil) {
        self.canvas = canvas
        canvasOwner = canvas == nil ? nil : owner
        canvasAttached()
    }

    /// Give up the canvas, if it is still this owner's — and say whether it was,
    /// because the callbacks installed beside it live on the caller and have to
    /// be released on exactly the same answer.
    @discardableResult
    func releaseCanvas(owner: ObjectIdentifier) -> Bool {
        guard canvasOwner == owner else {
            return false
        }
        attach(canvas: nil)
        return true
    }

    /// The page measured the room it has for the desktop, and what the desktop
    /// is laid out at.
    ///
    /// Traced rather than acted on, and that is the point of it. The fit is
    /// computed from the web view's bounds, which is the *window* chrome and
    /// nothing else; this pair is the only way to see what the page made of the
    /// result. Equal means the desktop sits flush with no scroll bars, and the
    /// two differing by a bar's width is what a latched scroll bar looks like
    /// from here — which is a bug that cost a long evening precisely because
    /// neither side could see it alone.
    func noteCanvasRoom(_ room: DisplayMode, content: DisplayMode) {
        trace("canvas room \(room.w)x\(room.h), content \(content.w)x\(content.h)")
    }

    /// A line for `REMOTEX_VIEWER_TRACE=1`, on stderr.
    ///
    /// The app is launched by Launch Services, whose `os_log` output is not
    /// something a terminal can read back; a QA run started from a shell can
    /// read stderr. Off unless asked for, so a normal launch is silent.
    ///
    /// The throwing write, and the discarded error with it: `write(_:)` raises an
    /// Objective-C exception that Swift cannot catch, so a shell that closed its
    /// end — quitting the terminal a QA launch was started from — would take the
    /// app down through its logging.
    func trace(_ message: @autoclosure () -> String) {
        guard ProcessInfo.processInfo.environment["REMOTEX_VIEWER_TRACE"] != nil else {
            return
        }
        try? FileHandle.standardError.write(contentsOf: Data("viewer: \(message())\n".utf8))
    }

    /// Tell the page whether to report input, when the answer has changed.
    ///
    /// Deduped because it is called after every session event and the answer
    /// moves twice a session; the memo is cleared with the canvas, so a fresh
    /// page is always told.
    private func publishInputEnablement() {
        let enabled = canSendInput
        guard lastInputEnabled != enabled else {
            return
        }
        lastInputEnabled = enabled
        canvas?.send(.input(enabled: enabled))
    }

    /// Bring a blank canvas up to date.
    ///
    /// Called when the view is built and again whenever the page's stream
    /// attaches — which is also what a reload looks like from here, since the app
    /// never saw the view go away. So everything it sends is unconditional and
    /// idempotent, and the input gate's dedupe is cleared first: what the page
    /// was last told went with the page.
    func canvasAttached() {
        lastInputEnabled = nil
        publishInputEnablement()
        guard let canvas else {
            return
        }
        // Sound belongs to the page that was playing it, and this one has never
        // heard an `audioFormat`: the gateway sends one when a subscription
        // starts and never again, so a reloaded page would sit there receiving
        // packets it has no decoder for, silent with nothing to say why.
        // Re-subscribing is what makes the gateway describe the stream again. A
        // no-op unless sound was actually on, so it costs a target with none
        // nothing — and it is above the size guard because a page that came back
        // before the first `resize` still needs its decoder.
        audio.reassert()
        guard let size = session.remoteSize else {
            return
        }
        canvas.send(.resize(w: size.w, h: size.h, scale: Double(session.remoteScale)))
        if let remoteCursor {
            canvas.send(.cursor(remoteCursor))
        }
        refresh()
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
    /// acting on it can be expensive at the other end — a full reactivation on an
    /// RDP host that allows resize, say — so re-sending a value it already matches
    /// costs a repaint for nothing. Unlike the viewport there is no debounce: a
    /// window changes screen once, discretely, where a drag-resize reports every
    /// frame.
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
    /// Nothing to do with `takeOver()` below, which claims this gateway's slot from
    /// another client of it.
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

    /// The page and the gateway disagree about the tile slot table.
    ///
    /// A plain `refresh` cannot repair this: it repaints without clearing the
    /// gateway's own table, so every reference into a slot this client lost would
    /// miss again. Raised by the page, which is the side that owns the table.
    func resetTileCache() {
        connection?.send(.cacheReset)
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
        // The View menu's toggle also writes the remembered default, so the value
        // set mid-session is the one the next connect starts from — the same single
        // value the picker's toggle edits.
        autoResizeByDefault = enabled
        applyAutoResize(enabled)
    }

    /// The session-only half of `setAutoResize`: set the mode this connection runs
    /// in without touching the remembered default. The connect-time seed uses this,
    /// so applying a remembered default never rewrites it.
    private func applyAutoResize(_ enabled: Bool) {
        guard session.canResize else {
            return
        }
        viewportPolicy.autoFollows = enabled
        publishViewportPolicy()
        if enabled {
            sendViewport(manual: true)
        }
    }

    /// The Remote menu's "Enable Audio" toggle. Like `setAutoResize` it also writes
    /// the remembered default, so muting or unmuting mid-session is remembered for
    /// the next connect. The live effect is `AudioControl`'s.
    func setAudioEnabled(_ enabled: Bool) {
        audioByDefault = enabled
        audio.setEnabled(enabled)
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

    func sendWheel(dx: Float, dy: Float, unit: WheelUnit) {
        guard canSendInput else {
            return
        }
        connection?.send(.wheel(dx: dx, dy: dy, unit: unit))
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

    func sendMouseButton(_ button: MouseButton, pressed isPressed: Bool, clicks: Int) {
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
        connection?.send(.mouseButton(button: button, pressed: isPressed, clicks: clicks))
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
