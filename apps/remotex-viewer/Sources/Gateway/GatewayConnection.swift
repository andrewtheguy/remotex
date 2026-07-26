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
    case tile(DecodedTile)
    case clearFramebuffer
    case releaseInput
    case failPendingClipboardFetch
    /// The login is gone (the gateway's auth sessions live in memory, so its
    /// restart does this). Back to the login screen; retrying cannot help.
    case unauthorized
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
/// the sink. One loop calls `receive()` and fully handles each frame — including
/// awaiting the tile decode — before asking for the next. That is what preserves
/// the gateway's arrival order across an async decode, which the SPA gets from
/// chaining every message onto one promise. It matters because tiles carry no
/// delta state and overwrite their rectangles: a `resize` that overtook the tiles
/// queued ahead of it would blit stale pixels into a freshly allocated texture,
/// and two reordered tiles leave the older one on screen. Not calling `receive()`
/// while decoding is not a throughput problem either — it is backpressure, which
/// the gateway's bounded frame channel already expects.
actor GatewayConnection {
    private let gateway: any SessionGateway
    private let decoder = TileDecoder()
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
        case .clearFramebuffer, .releaseInput, .failPendingClipboardFetch, .toLogin:
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
            await handle(.claimFailed)
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
            await handle(.claimFailed)
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
    }

    private func closeTransport() {
        receiveTask?.cancel()
        receiveTask = nil
        transport?.cancel()
        transport = nil
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
                await deliver(tile: data)
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

    private func deliver(tile data: Data) async {
        guard let frame = TileFrame.decode(data) else {
            log.warning("malformed tile frame of \(data.count, privacy: .public) bytes")
            return
        }
        guard let decoded = await decoder.decode(frame) else {
            log.warning(
                """
                undecodable \(String(describing: frame.format), privacy: .public) tile \
                \(frame.w, privacy: .public)x\(frame.h, privacy: .public)
                """
            )
            return
        }
        await publish(.tile(decoded))
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
