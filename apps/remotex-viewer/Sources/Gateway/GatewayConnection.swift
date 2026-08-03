import Foundation
import OSLog

/// Everything the session produces, in arrival order.
///
/// One case rather than a method per concern so a test can record the whole
/// stream and assert on its *order* — which is the property that matters most
/// here and the one that is easiest to lose.
enum SessionEvent: Sendable {
    case status(ViewerConnectionStatus)
    case control(ServerMessage)
    /// One binary frame, exactly as it came off the socket — a tile batch
    /// (`0x02`) or a wave buffer of Opus packets (`0x03`), kind byte included.
    ///
    /// Undecoded, deliberately. The canvas page owns the slot table, the image
    /// decode, the H.264 stream and the audio decoder, so anything this actor
    /// parsed would be a second implementation of a format the page has to read
    /// anyway — which is why a video target needed nothing here beyond the
    /// version bump. What is preserved is the only thing this layer is owed:
    /// arrival order.
    ///
    /// The *kind* byte is still read, and only that: `receiveLoop` passes the two
    /// this build knows and logs anything else rather than putting bytes with no
    /// reader on the bridge. The tradeoff is accepted rather than overlooked — a
    /// third frame kind has to be added to that allowlist as well as to the page,
    /// and an unlisted one is silently invisible until it is.
    case frame(Data)
    case clearFramebuffer
    case releaseInput
    case failPendingClipboardFetch
    /// The login is gone (the gateway's auth sessions live in memory, so its
    /// restart does this). Back to the login screen; retrying cannot help.
    case unauthorized
    /// The session could not be opened, for a reason that will not change by
    /// waiting: a refused TLS connection, a gateway of another protocol version, an
    /// answer that could not be read. Shown to the user, because the alternative is
    /// what this replaced — "Reconnecting…" forever, with the reason in the log.
    case rejected(reason: String)
}

extension SessionStateMachine.Action {
    /// What this action delivers to the sink, or nil when it is work for the
    /// connection itself. The split is what lets a whole transition reach the sink
    /// in one hop; the machine stays free of it.
    var sinkEvent: SessionEvent? {
        switch self {
        case .clearFramebuffer: .clearFramebuffer
        case .releaseInput: .releaseInput
        case .failPendingClipboardFetch: .failPendingClipboardFetch
        case .toLogin: .unauthorized
        case .report(let reason): .rejected(reason: reason)
        case .claim, .openSocket, .scheduleRetry: nil
        }
    }
}

/// `Sendable` so the connection actor can hand events to it across isolation; a
/// `@MainActor` class conformer satisfies that on its own.
@MainActor
protocol GatewaySessionSink: AnyObject, Sendable {
    func apply(_ event: SessionEvent)
}

/// Drives one session: claim the slot, attach the socket, pump frames, reconnect.
///
/// An actor, but a thin one — the lifecycle rules live in `SessionStateMachine`,
/// which this only feeds events to and executes actions for.
///
/// **Inbound ordering.** There is deliberately no queue between the socket and
/// the sink. One loop calls `receive()` and fully handles each frame before
/// asking for the next, and everything it emits travels to the canvas page on one
/// stream. That is what preserves the gateway's arrival order all the way to the
/// draw, which the SPA gets from chaining every message onto one promise. It
/// matters because tiles carry no delta state and overwrite their rectangles: a
/// `resize` that overtook the tiles queued ahead of it would paint stale pixels
/// into a freshly sized canvas, and two reordered tiles leave the older one on
/// screen.
actor GatewayConnection {
    private let gateway: any SessionGateway
    private let log = Logger(subsystem: "dev.remotex.viewer", category: "session")

    /// Nonisolated so `send` needs no `await`: see `OutboundQueue` for why that
    /// matters for ordering.
    nonisolated let outbound = OutboundQueue()

    private weak var sink: (any GatewaySessionSink)?
    private var machine = SessionStateMachine()
    /// This process's claim on the slot, replayed on reconnect so the same viewer
    /// reclaims its own session without prompting the user to take it over.
    /// Deliberately not persisted: a relaunch should not silently evict whoever
    /// is using the desktop now.
    private var claimToken: String?
    private var transport: (any WebSocketTransport)?
    private var receiveTask: Task<Void, Never>?
    private var drainTask: Task<Void, Never>?
    private var claimTask: Task<Void, Never>?
    private var retryTask: Task<Void, Never>?
    private var running = false
    /// The audio socket, and whether one is wanted.
    ///
    /// Two fields rather than one, because the socket cannot always be open when the
    /// intent is on: there is no claim to attach it to until this connection has one.
    /// The intent is what `openAudioSocket` consults each time a session socket comes
    /// up, which is how sound follows a reconnect without the menu being touched.
    private var audioTransport: (any WebSocketTransport)?
    private var audioTask: Task<Void, Never>?
    private var audioWanted = false
    /// Bumped every time the audio socket is closed or a fresh open begins.
    ///
    /// Opening one suspends at the upgrade, and an actor lets other calls run during
    /// that suspension — so two opens can be in flight at once, which is not
    /// hypothetical: `openSocket` starts one on a reconnect and `AudioControl.reassert`
    /// starts another from the `connected` that follows it. Whichever resumed *last*
    /// used to win by overwriting the fields, leaving the other's transport and task
    /// live with nothing holding them: not even `stop()` could reach them, and they
    /// ended only when the gateway got round to superseding one of them. Capturing
    /// this before the await and checking it after is what makes the loser cancel
    /// itself instead.
    private var audioGeneration = 0

    /// `policy` is a parameter so tests can collapse the backoff to nothing
    /// rather than waiting out a real one.
    init(
        gateway: any SessionGateway,
        sink: (any GatewaySessionSink)? = nil,
        policy: ReconnectPolicy = ReconnectPolicy()
    ) {
        self.gateway = gateway
        self.sink = sink
        machine.policy = policy
    }

    func attach(sink: any GatewaySessionSink) {
        self.sink = sink
    }

    var connectionStatus: ViewerConnectionStatus {
        machine.status
    }

    /// Queue a message for the remote. Dropped while no socket is attached, as
    /// the browser drops sends on a closed WebSocket.
    nonisolated func send(_ message: ClientMessage) {
        outbound.enqueue(message)
    }

    /// Subscribe to the remote's sound, or stop. Opening `/ws/audio` is the whole of
    /// the request and closing it is the whole of the cancellation — there is no
    /// message for either.
    ///
    /// Idempotent, and safe before a claim exists: the intent is remembered and acted
    /// on when a session socket next comes up.
    func setAudio(_ on: Bool) async {
        audioWanted = on
        if on {
            await openAudioSocket()
        } else {
            closeAudioSocket()
        }
    }

    private func openAudioSocket() async {
        // Also the generation bump that invalidates any open still in flight, so this
        // one supersedes it however the two resume.
        closeAudioSocket()
        guard running, audioWanted, let claimToken else {
            return
        }
        let generation = audioGeneration
        let opened: any WebSocketTransport
        do {
            opened = try await gateway.openAudioSocket(sessionToken: claimToken)
        } catch {
            // No retry and no alert. Sound is the one part of a session whose absence
            // is indistinguishable from a quiet remote, so a failure here is a log
            // line; the next `connected` tries again.
            log.warning("audio socket open failed: \(error.localizedDescription, privacy: .public)")
            return
        }
        // The claim is compared as well as the generation, so this guard states the
        // whole invariant rather than trusting every path that changes a claim to also
        // have closed the socket.
        guard generation == audioGeneration, running, audioWanted, self.claimToken == claimToken
        else {
            opened.cancel()
            return
        }
        audioTransport = opened
        audioTask = Task { [weak self] in
            await self?.audioLoop(opened)
        }
    }

    private func closeAudioSocket() {
        audioGeneration &+= 1
        audioTask?.cancel()
        audioTask = nil
        audioTransport?.cancel()
        audioTransport = nil
    }

    /// The audio socket's only consumer. It carries exactly two things — the format,
    /// as a control message, and packets, as binary frames — so anything else is a
    /// gateway that has changed under this build.
    ///
    /// Both go to the canvas page, which decodes them with the same
    /// `frontend/src/audioPlayer.ts` the browser SPA uses. Nothing is decoded here.
    private func audioLoop(_ transport: any WebSocketTransport) async {
        while !Task.isCancelled {
            let frame: WebSocketFrame
            do {
                frame = try await transport.receive()
            } catch {
                return
            }
            switch frame {
            case .text(let text):
                guard let message = try? ServerMessage.decode(text) else {
                    log.warning("undecodable audio control frame")
                    continue
                }
                await publish(.control(message))
            case .binary(let data):
                guard data.first == AudioFrame.frameKind else {
                    log.warning(
                        "audio socket sent a frame of kind \(data.first ?? 0, privacy: .public)"
                    )
                    continue
                }
                await publish(.frame(data))
            }
        }
    }

    /// Forget the queue's viewport dedupe. Needed on every `connected`, not only
    /// on a new socket: a freshly started engine knows nothing about this window,
    /// so the report that follows has to go out even when it repeats the size
    /// already sent for the previous target — or for the picker.
    nonisolated func resetViewportMemo() {
        outbound.resetViewportMemo()
    }

    /// Start, or restart after a `busy`/`takenOver` stall. `force` evicts whoever
    /// holds the slot — the "Take over" / "Take it back" action.
    func start(force: Bool = false) async {
        running = true
        if drainTask == nil {
            drainTask = Task { [weak self] in
                await self?.drainLoop()
            }
        }
        await handle(.start(force: force))
    }

    /// Tear everything down. The gateway keeps the engine alive for its reattach
    /// grace period, so this is not a disconnect from the remote.
    ///
    /// **One-way.** The outbound queue's wake-up stream is finished here and an
    /// `AsyncStream` cannot be reopened, so a `start` after this would attach a
    /// drain loop that ends immediately and silently swallow every send. Nothing
    /// restarts a stopped connection — `AppModel.teardown` drops it and the next
    /// session builds a new one — and a restart should build a new one too.
    func stop() {
        running = false
        claimTask?.cancel()
        retryTask?.cancel()
        receiveTask?.cancel()
        drainTask?.cancel()
        claimTask = nil
        retryTask = nil
        receiveTask = nil
        drainTask = nil
        transport?.cancel()
        transport = nil
        audioWanted = false
        closeAudioSocket()
        outbound.finish()
    }

    /// Forget the claim so the next start claims fresh rather than reattaching.
    func forgetClaim() {
        claimToken = nil
    }

    // MARK: - The state machine's two halves

    /// One hop to the sink per transition.
    ///
    /// The status change and every event the transition produces go in a single
    /// `MainActor.run`, so half a transition cannot be observed — a `takenOver`
    /// whose framebuffer has not been dropped yet, say, or a test that reads the
    /// status and finds none of the cleanup that belongs with it. The connection's
    /// own work follows, in the order the machine returned it, which reorders
    /// nothing because every list it returns puts its sink actions first — pinned
    /// by `everyTransitionPutsItsSinkActionsFirst`.
    private func handle(_ event: SessionStateMachine.Event) async {
        let before = machine.status
        let actions = machine.handle(event)
        var events: [SessionEvent] = machine.status == before ? [] : [.status(machine.status)]
        events.append(contentsOf: actions.compactMap(\.sinkEvent))
        if !events.isEmpty {
            await MainActor.run { [sink] in
                for event in events {
                    sink?.apply(event)
                }
            }
        }
        for action in actions where action.sinkEvent == nil {
            await perform(action)
        }
    }

    /// Which failure event a caught error is.
    ///
    /// No inspection of `URLError` codes: `GatewayClientError` has already sorted the
    /// one transport case from the several that are facts rather than weather (see
    /// its `isRetryable`), and anything that is not a `GatewayClientError` at all is
    /// given the benefit of the doubt and retried.
    private func event(for error: Error) -> SessionStateMachine.Event {
        let reason = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
        if let client = error as? GatewayClientError, !client.isRetryable {
            return .claimRejected(reason: reason)
        }
        return .claimFailed(reason: reason)
    }

    /// The connection's own half of a transition. The sink's half has already been
    /// delivered by `handle`.
    private func perform(_ action: SessionStateMachine.Action) async {
        switch action {
        case .claim(let force):
            claim(force: force)
        case .openSocket(let token):
            await openSocket(token: token)
        case .scheduleRetry(let delay):
            scheduleRetry(after: delay)
        case .clearFramebuffer, .releaseInput, .failPendingClipboardFetch, .toLogin,
            .report:
            // `sinkEvent` is what routes these, and `handle` filters them out
            // before calling this.
            break
        }
    }

    private func publish(_ event: SessionEvent) async {
        await MainActor.run { [sink] in
            sink?.apply(event)
        }
    }

    // MARK: - Claim

    private func claim(force: Bool) {
        closeTransport()
        claimTask?.cancel()
        claimTask = Task { [weak self] in
            guard let self else {
                return
            }
            await self.performClaim(force: force)
        }
    }

    private func performClaim(force: Bool) async {
        let outcome: ClaimOutcome
        do {
            outcome = try await gateway.claimSession(force: force, sessionId: claimToken)
        } catch GatewayClientError.unauthorized {
            await handle(.claimUnauthorized)
            return
        } catch {
            guard !Task.isCancelled else {
                return
            }
            log.warning("claim failed: \(error.localizedDescription, privacy: .public)")
            await handle(event(for: error))
            return
        }
        guard !Task.isCancelled else {
            return
        }
        switch outcome {
        case .claimed(let token):
            claimToken = token
            await handle(.claimed(token: token))
        case .busy:
            await handle(.claimBusy)
        case .unauthorized:
            await handle(.claimUnauthorized)
        }
    }

    private func scheduleRetry(after delay: Duration) {
        retryTask?.cancel()
        retryTask = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled, let self else {
                return
            }
            await self.handle(.retryElapsed)
        }
    }

    // MARK: - Socket

    private func openSocket(token: String) async {
        closeTransport()
        let opened: any WebSocketTransport
        do {
            opened = try await gateway.openSocket(sessionToken: token)
        } catch {
            log.warning("socket open failed: \(error.localizedDescription, privacy: .public)")
            await handle(event(for: error))
            return
        }
        guard running else {
            opened.cancel()
            return
        }
        transport = opened
        outbound.resetViewportMemo()
        await handle(.socketOpened)
        receiveTask = Task { [weak self] in
            await self?.receiveLoop(opened)
        }
        // Sound follows the session back. The gateway would hold the subscription
        // across a reattach on its own — it belongs to the claim now — but this end's
        // socket died with the network, so it is reopened here rather than waiting for
        // the menu to be touched.
        await openAudioSocket()
    }

    private func closeTransport() {
        receiveTask?.cancel()
        receiveTask = nil
        transport?.cancel()
        transport = nil
        // The audio socket goes with it: whatever took the session socket down took
        // this one too, and `audioWanted` is what brings it back.
        closeAudioSocket()
        outbound.discardPending()
    }

    /// The single consumer. See the note on the type for why every step is
    /// awaited here rather than handed to a queue.
    private func receiveLoop(_ transport: any WebSocketTransport) async {
        while !Task.isCancelled {
            let frame: WebSocketFrame
            do {
                frame = try await transport.receive()
            } catch {
                guard !Task.isCancelled else {
                    return
                }
                await handle(.socketClosed(code: transport.closeCode))
                return
            }
            switch frame {
            case .text(let text):
                await deliver(text: text)
            case .binary(let data):
                // The kind byte is checked only to keep a frame this build has no
                // reader for off the bridge; the page tells the two it knows apart
                // for itself, with the same code the browser SPA uses.
                switch data.first {
                case BatchFrame.frameKind, AudioFrame.frameKind:
                    await publish(.frame(data))
                default:
                    log.warning(
                        "binary frame of unknown kind \(data.first ?? 0, privacy: .public)"
                    )
                }
            }
        }
    }

    private func deliver(text: String) async {
        let message: ServerMessage
        do {
            message = try ServerMessage.decode(text)
        } catch {
            // One bad frame, dropped — the same call the gateway makes on a
            // client message it cannot read (src/ws.rs).
            log.warning("undecodable control frame: \(String(describing: error), privacy: .public)")
            return
        }
        // Before delivery: any control message, including an unknown one, proves
        // the socket really attached to the slot, which is what clears the backoff.
        await handle(.controlReceived)
        await publish(.control(message))
    }

    // MARK: - Outbound

    private func drainLoop() async {
        for await _ in outbound.wakeups {
            let messages = outbound.drain()
            guard let transport else {
                continue
            }
            for message in messages {
                guard let text = message.jsonText() else {
                    log.warning("unencodable \(message.tag, privacy: .public) dropped")
                    continue
                }
                do {
                    try await transport.send(text)
                } catch {
                    // The receive loop sees the same failure and drives the
                    // reconnect; there is nothing to add here.
                    break
                }
            }
        }
    }
}
