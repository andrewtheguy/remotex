import AppKit
import CoreGraphics
import Foundation
import ImageIO
import Synchronization
import Testing
import UniformTypeIdentifiers
@testable import RemotexViewer

/// A scripted session socket.
///
/// Frames can be queued up front, pushed later, or both, and the socket either
/// closes once the script drains or stays open — a socket that closes the moment
/// its script runs out cannot be sent to, which is most of what there is to test
/// on the outbound side.
final class FakeWebSocketTransport: WebSocketTransport {
    private struct State {
        var inbound: [WebSocketFrame]
        var closeAfterDraining: Bool
        var pendingClose: Int?
        var waiter: CheckedContinuation<WebSocketFrame, any Error>?
        var ended = false
        var sent: [String] = []
        var cancelled = false
    }

    private let state: Mutex<State>

    init(
        inbound: [WebSocketFrame] = [],
        closeCode: Int? = nil,
        closeAfterDraining: Bool = true
    ) {
        state = Mutex(
            State(
                inbound: inbound,
                closeAfterDraining: closeAfterDraining,
                pendingClose: closeCode
            )
        )
    }

    /// Only known once the socket has ended, as with the real one.
    var closeCode: Int? {
        state.withLock { $0.ended ? $0.pendingClose : nil }
    }

    var sentFrames: [String] {
        state.withLock { $0.sent }
    }

    var wasCancelled: Bool {
        state.withLock { $0.cancelled }
    }

    func send(_ text: String) async throws {
        try state.withLock { state in
            guard !state.ended, !state.cancelled else {
                throw FakeTransportError.closed
            }
            state.sent.append(text)
        }
    }

    func receive() async throws -> WebSocketFrame {
        try await withCheckedThrowingContinuation { continuation in
            // Resuming has to happen outside the lock, or a synchronous resume
            // could re-enter it.
            enum Next {
                case frame(WebSocketFrame)
                case closed
                case park
            }
            let next: Next = state.withLock { state in
                if !state.inbound.isEmpty {
                    return .frame(state.inbound.removeFirst())
                }
                guard state.closeAfterDraining || state.cancelled else {
                    // One consumer, as with the real socket: a second `receive`
                    // would overwrite the parked continuation and leak the first,
                    // which shows up as a test that hangs rather than fails.
                    precondition(
                        state.waiter == nil,
                        "overlapping receive() on FakeWebSocketTransport"
                    )
                    state.waiter = continuation
                    return .park
                }
                state.ended = true
                return .closed
            }
            switch next {
            case .frame(let frame):
                continuation.resume(returning: frame)
            case .closed:
                continuation.resume(throwing: FakeTransportError.closed)
            case .park:
                break
            }
        }
    }

    /// Deliver a frame to a socket that stayed open. Dropped once the socket has
    /// ended, or `cancel` clearing the queue would not be worth much: the next
    /// frame pushed would sit in it and be handed to a `receive` after the session
    /// let this socket go.
    func push(_ frame: WebSocketFrame) {
        let waiter = state.withLock { state -> CheckedContinuation<WebSocketFrame, any Error>? in
            guard !state.ended, !state.cancelled else {
                return nil
            }
            guard let waiter = state.waiter else {
                state.inbound.append(frame)
                return nil
            }
            state.waiter = nil
            return waiter
        }
        waiter?.resume(returning: frame)
    }

    /// End a socket that stayed open, as the gateway would.
    func close(code: Int?) {
        let waiter = state.withLock { state -> CheckedContinuation<WebSocketFrame, any Error>? in
            state.pendingClose = code
            state.closeAfterDraining = true
            guard let waiter = state.waiter else {
                return nil
            }
            state.waiter = nil
            state.ended = true
            return waiter
        }
        waiter?.resume(throwing: FakeTransportError.closed)
    }

    /// Over for good, as `URLSessionWebSocketTask.cancel()` is: whatever is left
    /// of the script goes with it, and the socket counts as ended whether or not
    /// anyone was parked on it. Keeping the buffer would let a `receive` that is
    /// already in flight hand a frame to a session that has dropped this socket —
    /// which is the reordering `closeTransport` exists to prevent, made invisible.
    func cancel() {
        let waiter = state.withLock { state -> CheckedContinuation<WebSocketFrame, any Error>? in
            state.cancelled = true
            state.ended = true
            state.inbound.removeAll()
            let waiter = state.waiter
            state.waiter = nil
            return waiter
        }
        waiter?.resume(throwing: FakeTransportError.closed)
    }
}

enum FakeTransportError: Error, Equatable {
    case closed
    case refused
}

/// A scripted gateway: claim answers and sockets in the order the test wants
/// them, plus a record of what was asked for.
final class FakeGateway: SessionGateway {
    private struct State {
        var claims: [ClaimOutcome]
        var sockets: [FakeWebSocketTransport]
        var audioSockets: [FakeWebSocketTransport] = []
        var claimCalls: [(force: Bool, sessionId: String?)] = []
        var socketTokens: [String] = []
        var audioSocketTokens: [String] = []
        /// Opens parked by `holdAudioOpens`, so a test can make two of them overlap
        /// on purpose rather than hoping the scheduler does it.
        var holdingAudio = false
        var audioGates: [CheckedContinuation<Void, Never>] = []
    }

    private let state: Mutex<State>

    init(
        claims: [ClaimOutcome],
        sockets: [FakeWebSocketTransport],
        audioSockets: [FakeWebSocketTransport] = []
    ) {
        state = Mutex(State(claims: claims, sockets: sockets, audioSockets: audioSockets))
    }

    var claimCalls: [(force: Bool, sessionId: String?)] {
        state.withLock { $0.claimCalls }
    }

    var socketTokens: [String] {
        state.withLock { $0.socketTokens }
    }

    /// Which claims the audio socket was opened with, separately from the session
    /// socket's — the two are the same token, and a test that could not tell them
    /// apart could not show that.
    var audioSocketTokens: [String] {
        state.withLock { $0.audioSocketTokens }
    }

    func claimSession(force: Bool, sessionId: String?) async throws -> ClaimOutcome {
        try state.withLock { state in
            state.claimCalls.append((force: force, sessionId: sessionId))
            guard !state.claims.isEmpty else {
                throw FakeTransportError.refused
            }
            return state.claims.removeFirst()
        }
    }

    func openSocket(sessionToken: String) async throws -> any WebSocketTransport {
        try state.withLock { state in
            state.socketTokens.append(sessionToken)
            guard !state.sockets.isEmpty else {
                throw FakeTransportError.refused
            }
            return state.sockets.removeFirst()
        }
    }

    func openAudioSocket(sessionToken: String) async throws -> any WebSocketTransport {
        // Suspend *before* handing a socket back, which is where the real one suspends
        // too: `URLSession`'s upgrade is the await, and an actor runs other calls
        // across it.
        if state.withLock({ $0.holdingAudio }) {
            await withCheckedContinuation { continuation in
                state.withLock { $0.audioGates.append(continuation) }
            }
        }
        return try state.withLock { state in
            state.audioSocketTokens.append(sessionToken)
            guard !state.audioSockets.isEmpty else {
                throw FakeTransportError.refused
            }
            return state.audioSockets.removeFirst()
        }
    }

    /// Park every audio-socket open from here on.
    func holdAudioOpens() {
        state.withLock { $0.holdingAudio = true }
    }

    /// Let the parked opens finish, oldest first, and stop parking.
    func releaseAudioOpens() {
        let gates = state.withLock { state -> [CheckedContinuation<Void, Never>] in
            state.holdingAudio = false
            defer { state.audioGates = [] }
            return state.audioGates
        }
        for gate in gates {
            gate.resume()
        }
    }

    /// How many opens are parked, so a test can wait for both to be in flight rather
    /// than sleeping and hoping.
    var parkedAudioOpens: Int {
        state.withLock { $0.audioGates.count }
    }
}

/// Records the whole event stream so tests can assert on its order.
@MainActor
final class RecordingSink: GatewaySessionSink {
    private(set) var events: [SessionEvent] = []

    func apply(_ event: SessionEvent) {
        events.append(event)
    }

    /// A compact rendering of the stream, so an ordering failure reads as a
    /// diff rather than as a wall of associated values.
    var trace: [String] {
        events.map { event in
            switch event {
            case .status(let status): "status:\(status.rawValue)"
            case .control(let message): "control:\(Self.label(message))"
            // Kind byte and length, which is the whole of what this layer knows
            // about a binary frame now that it forwards them whole.
            case .frame(let data): "frame:\(data.first ?? 0),\(data.count)"
            case .clearFramebuffer: "clear"
            case .releaseInput: "release"
            case .failPendingClipboardFetch: "failFetch"
            case .rejected(let reason): "rejected:\(reason)"
            case .unauthorized: "unauthorized"
            }
        }
    }

    /// Poll until `predicate` holds, or fail on the deadline. Polling rather than
    /// a fixed wait: the work is asynchronous and this must not encode a guess
    /// about how long it takes.
    func wait(
        for predicate: @escaping @MainActor ([SessionEvent]) -> Bool,
        timeout: Duration = .seconds(5),
        sourceLocation: SourceLocation = #_sourceLocation
    ) async {
        let deadline = ContinuousClock.now + timeout
        while ContinuousClock.now < deadline {
            if predicate(events) {
                return
            }
            try? await Task.sleep(for: .milliseconds(2))
        }
        // Once more after the deadline: the predicate may have come true during the
        // last sleep, and failing on that would be a flake, not a finding.
        if predicate(events) {
            return
        }
        Issue.record(
            "timed out waiting; saw \(trace)",
            sourceLocation: sourceLocation
        )
    }

    private static func label(_ message: ServerMessage) -> String {
        switch message {
        case .resize(let w, let h, let scale): "resize(\(w)x\(h)@\(scale)x)"
        case .cursor: "cursor"
        case .error(let message): "error(\(message))"
        case .picker: "picker"
        case .connected(let payload): "connected(\(payload.name))"
        case .remoteOs(let macos): "remoteOs(\(macos))"
        case .clipboard: "clipboard"
        case .displays(let active, let displays): "displays(\(displays.count), active \(active))"
        case .audioFormat(let format): "audioFormat(\(format.codec))"
        case .unsupported(let type): "unsupported(\(type))"
        }
    }
}

/// A real model attached to a scripted socket, waiting in the picker.
///
/// The setup for any test whose subject is what reaches the *wire*: the model, the
/// connection and the outbound queue are all the shipped ones, and only the socket
/// under them is scripted.
@MainActor
struct AttachedSession {
    let model: AppModel
    let socket: FakeWebSocketTransport
    /// The audio sockets this session may open, in order, and the gateway that hands
    /// them out. Sound has a socket of its own, so "did it subscribe" is a question
    /// about *sockets* now rather than about a message on the session socket.
    let audioSockets: [FakeWebSocketTransport]
    let gateway: FakeGateway
    /// The synchronizer's pasteboard, so a test can put something on it to be
    /// pushed. Its own, never the user's.
    let pasteboard: NSPasteboard
    /// What the model told the canvas page, in order.
    let canvas: FakeCanvas

    static func attached(
        suite: String,
        audio: AudioControl = AudioControl()
    ) async throws -> AttachedSession {
        let socket = FakeWebSocketTransport(closeAfterDraining: false)
        // More than any test uses, so running out is never what a failure means.
        let audioSockets = (0..<8).map { _ in
            FakeWebSocketTransport(closeAfterDraining: false)
        }
        let pasteboard = NSPasteboard.withUniqueName()
        // No gateway and no preferences file: the subject of every suite that uses
        // this is what reaches the wire, and the socket under it is scripted — so
        // there is nothing for a real gateway process to do here. `suite` survives as
        // a label for the failure message rather than as a defaults domain.
        _ = suite
        let model = AppModel(
            clipboard: ClipboardSynchronizer(
                pasteboard: pasteboard,
                startsPolling: false
            ),
            audio: audio
        )
        let gateway = FakeGateway(
            claims: [.claimed("tok")],
            sockets: [socket],
            audioSockets: audioSockets
        )
        await model.beginSession(over: gateway)
        // `start` returns once the claim is under way, not once the socket is up,
        // and opening one discards whatever was queued before it. Waiting here is
        // what keeps a test measuring its own subject rather than that race.
        for _ in 0..<200 where model.session.connectionStatus != .connected {
            try await Task.sleep(for: .milliseconds(5))
        }
        #expect(model.session.connectionStatus == .connected)
        // Attached after the session is up, as the real one is: the surface
        // belongs to a screen, and the screen exists once there is a session.
        let canvas = FakeCanvas()
        model.attach(canvas: canvas)
        return AttachedSession(
            model: model,
            socket: socket,
            audioSockets: audioSockets,
            gateway: gateway,
            pasteboard: pasteboard,
            canvas: canvas
        )
    }

    /// How many times this session has opened the audio socket. One per subscription,
    /// because closing and reopening is the only way to resubscribe.
    var audioSocketsOpened: Int {
        gateway.audioSocketTokens.count
    }

    /// Whether the most recently opened audio socket is still open — the observable
    /// form of "is this session subscribed to sound right now".
    var isSubscribedToAudio: Bool {
        guard audioSocketsOpened > 0 else {
            return false
        }
        return !audioSockets[audioSocketsOpened - 1].wasCancelled
    }

    /// How many of the sockets handed out are still live. More than one means a
    /// transport was dropped without being cancelled — a leak nothing can reach.
    var liveAudioSockets: Int {
        audioSockets.prefix(audioSocketsOpened).count { !$0.wasCancelled }
    }

    func connect(
        protocolName: String,
        resize: Bool = true,
        // Defaulted to what the gateway would actually send for this protocol —
        // plain `vnc` may be driven by the window and RDP may not — so a harness
        // session is never in a state the real one cannot reach.
        autoResize: Bool? = nil,
        clipboard: Bool = false,
        audio: Bool = false
    ) {
        model.apply(
            .control(
                .connected(
                    ServerMessage.Connected(
                        name: "t",
                        protocolName: protocolName,
                        resize: resize,
                        autoResize: resize && (autoResize ?? (protocolName == "vnc")),
                        clipboard: clipboard,
                        audio: audio
                    )
                )
            )
        )
    }

    /// The frames the socket has been sent, decoded, in order.
    ///
    /// A frame that will not decode is recorded as a failure rather than dropped.
    /// What these suites mostly assert is that something was *not* sent, and a
    /// silent gap here is the one thing that could make such an assertion pass for
    /// the wrong reason: the frame went out, and only this harness lost it.
    var sent: [[String: Any]] {
        socket.sentFrames.compactMap { frame in
            guard let data = frame.data(using: .utf8),
                  let json = try? JSONSerialization.jsonObject(with: data)
                      as? [String: Any]
            else {
                Issue.record("outbound frame is not a JSON object: \(frame)")
                return nil
            }
            return json
        }
    }

    func sent(ofType type: String) -> [[String: Any]] {
        sent.filter { $0["type"] as? String == type }
    }

    /// Long enough for the viewport debounce to have fired and the queue to have
    /// drained, for the assertions that something was *not* sent.
    func settle() async throws {
        try await Task.sleep(for: .milliseconds(400))
    }
}

/// A canvas that records instead of drawing.
///
/// The real one is a loopback HTTP listener with a `WKWebView` reading it, and
/// none of that is what a session test is about: what these suites can assert is
/// that the model told the page the right things in the right order, which is
/// the whole of its part in rendering.
final class FakeCanvas: RemoteCanvas, @unchecked Sendable {
    /// One thing the model told the page.
    ///
    /// Commands and frames share a list because the order *between* them is the
    /// property worth asserting: a `resize` has to precede the tiles drawn in the
    /// space it names, and two separate arrays record each side faithfully while
    /// losing the only relationship that could be wrong.
    enum Event: Equatable {
        case command(CanvasCommand)
        case frame(Data)
    }

    private let recorded = Mutex<[Event]>([])

    /// Everything, in the order it was sent.
    var events: [Event] {
        recorded.withLock { $0 }
    }

    var commands: [CanvasCommand] {
        events.compactMap { if case .command(let command) = $0 { command } else { nil } }
    }

    /// The gateway binary frames forwarded, verbatim and in order.
    var frames: [Data] {
        events.compactMap { if case .frame(let data) = $0 { data } else { nil } }
    }

    func send(_ command: CanvasCommand) {
        recorded.withLock { $0.append(.command(command)) }
    }

    func send(frame: Data) {
        recorded.withLock { $0.append(.frame(frame)) }
    }
}

/// A batch frame carrying one tile, which is what most of these tests want.
func tileFrame(
    x: UInt16,
    y: UInt16,
    size: UInt16 = 2,
    red: UInt8 = 0xFF
) throws -> Data {
    batchFrame([try tileRecord(x: x, y: y, size: size, red: red)])
}

/// A batch frame wrapping `records`, whose count the header reports honestly.
func batchFrame(_ records: [Data]) -> Data {
    var frame = Data([BatchFrame.frameKind, 0])
    let count = UInt16(records.count)
    frame.append(UInt8(count & 0xFF))
    frame.append(UInt8(count >> 8))
    for record in records {
        frame.append(record)
    }
    return frame
}

/// An audio frame wrapping `packets`, whose count the header reports honestly.
///
/// Built here rather than by `AudioFrame` so a test using it is not depending on the
/// parser it is testing; `AudioFrameTests` writes the layout out a third time for the
/// same reason.
func audioFrame(_ packets: [Data]) -> Data {
    var frame = Data([AudioFrame.frameKind, 0])
    appendLittleEndian(UInt16(packets.count), to: &frame)
    for packet in packets {
        appendLittleEndian(UInt16(packet.count), to: &frame)
        frame.append(packet)
    }
    return frame
}

private func appendLittleEndian(_ value: UInt16, to data: inout Data) {
    data.append(UInt8(value & 0xFF))
    data.append(UInt8(value >> 8))
}

/// A `TILE_REF` record: seven bytes naming a slot and where to redraw it.
func referenceRecord(slot: UInt16, x: UInt16, y: UInt16) -> Data {
    var record = Data([BatchFrame.opTileRef])
    for value in [slot, x, y] {
        record.append(UInt8(value & 0xFF))
        record.append(UInt8(value >> 8))
    }
    return record
}

/// One real single-colour PNG `TILE` record, header and all. Encoded rather than
/// hand-rolled so the decode under test is the same one production runs.
///
/// `red` is drawn into the pixels rather than selecting a fixture, so a test can
/// ask for "a tile distinguishable from that other tile" at any colour; the other
/// two channels are fixed so the decoded bytes name what was asked for.
///
/// `slot` defaults to "do not remember this", so a test only names one when the
/// cache is what it is about.
func tileRecord(
    x: UInt16,
    y: UInt16,
    size: UInt16 = 2,
    red: UInt8 = 0xFF,
    slot: UInt16 = BatchFrame.noSlot
) throws -> Data {
    let side = Int(size)
    var pixels = [UInt8]()
    for _ in 0 ..< side * side {
        pixels.append(contentsOf: [red, 0x20, 0x40, 0xFF])
    }
    let provider = try #require(CGDataProvider(data: Data(pixels) as CFData))
    let image = try #require(
        CGImage(
            width: side,
            height: side,
            bitsPerComponent: 8,
            bitsPerPixel: 32,
            bytesPerRow: side * 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
            provider: provider,
            decode: nil,
            shouldInterpolate: false,
            intent: .defaultIntent
        )
    )
    let encoded = NSMutableData()
    let destination = try #require(
        CGImageDestinationCreateWithData(encoded, UTType.png.identifier as CFString, 1, nil)
    )
    CGImageDestinationAddImage(destination, image, nil)
    #expect(CGImageDestinationFinalize(destination))
    return tileRecord(x: x, y: y, w: size, h: size, slot: slot, payload: encoded as Data)
}

/// A `TILE` record around an arbitrary payload, for the cases that care about the
/// header disagreeing with it.
func tileRecord(
    x: UInt16,
    y: UInt16,
    w: UInt16,
    h: UInt16,
    slot: UInt16 = BatchFrame.noSlot,
    format: TileFormat = .png,
    payload: Data
) -> Data {
    var record = Data([BatchFrame.opTile, format.rawValue])
    for value in [slot, x, y, w, h] {
        record.append(UInt8(value & 0xFF))
        record.append(UInt8(value >> 8))
    }
    let length = UInt32(payload.count)
    for shift in [0, 8, 16, 24] {
        record.append(UInt8((length >> shift) & 0xFF))
    }
    record.append(payload)
    return record
}
