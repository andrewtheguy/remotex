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
    /// One binary frame's worth of tiles, in wire order. Delivered whole so the
    /// renderer asks for one redraw per frame rather than one per tile.
    case tiles([DecodedTile])
    /// One wave buffer's worth of Opus packets, in wire order.
    ///
    /// Undecoded, deliberately: decoding needs the `audioFormat` that arrived as a
    /// control message, and putting the decoder here would give this actor an audio
    /// engine to own and the receive loop something to wait on. The packets go to the
    /// sink as they came off the socket, which also keeps them in order with the
    /// `audioFormat` that configures them.
    case audio([Data])
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
    /// Whether this connection has already said it cannot decode a video target.
    /// Latched, so one unusable target produces one message rather than one per
    /// batch, and so a reconnect to the same target does not shout again.
    private var refusedVideo = false
    /// The tiles the gateway has told this client to remember, by slot.
    ///
    /// Encoded payloads rather than decoded pixels: a decoded 320x64 tile is 80 KB
    /// where its PNG is a few hundred bytes, and re-decoding on a reference is
    /// cheaper than the transfer it replaced. Fixed length because the wire says
    /// how many slots there are, so a gateway cannot grow it; and this client never
    /// evicts — the gateway names the slot to overwrite, which keeps the two ends
    /// in step without either modelling the other's memory.
    private var tileCache = [TileFrame?](repeating: nil, count: Int(BatchFrame.slotCount))
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
                // The kind byte is the whole of how the two binary frames are told
                // apart, and both parsers check it again for themselves. Dispatching
                // here rather than trying one parser and falling through to the other
                // keeps a malformed batch from being reported as an unknown kind.
                switch data.first {
                case BatchFrame.frameKind:
                    await deliver(tile: data)
                case AudioFrame.frameKind:
                    await deliver(audio: data)
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

    private func deliver(audio data: Data) async {
        guard let packets = AudioFrame.decode(data) else {
            log.warning("malformed audio frame of \(data.count, privacy: .public) bytes")
            return
        }
        // An empty frame is well formed and means nothing to play. Not published, for
        // the same reason an empty batch is not: it would only ask the player to do
        // nothing.
        guard !packets.isEmpty else {
            return
        }
        await publish(.audio(packets))
    }

    private func deliver(tile data: Data) async {
        guard let records = BatchFrame.decode(data) else {
            log.warning("malformed batch frame of \(data.count, privacy: .public) bytes")
            return
        }
        if !refusedVideo, records.contains(where: \.isVideo) {
            // Said once, and then the target is left rather than watched. Dropping
            // these records one by one would be the honest thing to do with an
            // undecodable *tile*, but a video target sends nothing else — so the
            // desktop would simply never paint, which is the outcome worth spending
            // code to avoid.
            refusedVideo = true
            log.error("this target sends H.264 video, which this viewer cannot decode")
            await publish(.rejected(reason: """
                This target sends its desktop as one H.264 video stream, which this \
                viewer cannot decode yet. Open it in a browser, or give the target a \
                different render_type.
                """))
            send(.disconnect)
            return
        }
        guard !refusedVideo else {
            return
        }
        // At most one reset per batch: a hundred references into a cache this
        // client lost are one disagreement, not a hundred.
        var askedForReset = false

        // In order, and each awaited: a later tile has to overwrite an earlier one
        // that covers the same pixels.
        //
        // An undecodable record is dropped alone rather than taking its batch with
        // it — the rest decoded, and the pixels they cover would otherwise stay
        // stale until something repaints them.
        var decoded = [DecodedTile]()
        decoded.reserveCapacity(records.count)
        for record in records {
            guard let frame = resolve(record, askedForReset: &askedForReset) else {
                continue
            }
            guard let tile = await decoder.decode(frame) else {
                log.warning(
                    """
                    undecodable \(String(describing: frame.format), privacy: .public) tile \
                    \(frame.w, privacy: .public)x\(frame.h, privacy: .public)
                    """
                )
                // A tile that will not decode is one dropped tile — unless the
                // gateway is keeping it as a slot, in which case every later
                // reference to it would fail the same way.
                if frame.slot != BatchFrame.noSlot {
                    askForCacheReset(&askedForReset)
                }
                continue
            }
            decoded.append(tile)
        }
        // Emptied once, after the batch, rather than the moment a reference misses.
        // Clearing mid-pass would throw away slots this batch's own earlier records
        // filled, so a reference naming one of them — legal, and something the
        // gateway emits within a single batch — would be dropped for company. By
        // here nothing left reads the cache, and the next batch arrives holding
        // nothing, which is what the server's own reset will agree with.
        if askedForReset {
            tileCache = Array(repeating: nil, count: Int(BatchFrame.slotCount))
        }
        // An empty batch is well formed and means nothing to paint, so it must not
        // reach the renderer and ask for a redraw of nothing.
        guard !decoded.isEmpty else {
            return
        }
        await publish(.tiles(decoded))
    }

    /// The payload a record stands for: its own, or the one its slot holds.
    ///
    /// Storing happens here too, so the cache is written in wire order by the same
    /// pass that reads it — a reference may legitimately name a slot filled earlier
    /// in its own batch.
    private func resolve(
        _ record: BatchFrame.Record,
        askedForReset: inout Bool
    ) -> TileFrame? {
        switch record {
        case .tile(let frame):
            if frame.slot != BatchFrame.noSlot {
                tileCache[Int(frame.slot)] = frame
            }
            return frame
        case .reference(let slot, let x, let y):
            guard let held = tileCache[Int(slot)] else {
                // The gateway believes this client holds a tile it does not.
                // Nothing else will ever correct that.
                log.warning("reference to empty tile slot \(slot, privacy: .public)")
                askForCacheReset(&askedForReset)
                return nil
            }
            return TileFrame(
                format: held.format,
                slot: held.slot,
                x: x,
                y: y,
                w: held.w,
                h: held.h,
                payload: held.payload
            )
        }
    }

    private func askForCacheReset(_ askedForReset: inout Bool) {
        guard !askedForReset else {
            return
        }
        askedForReset = true
        outbound.enqueue(.cacheReset)
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
