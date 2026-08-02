import Foundation
import Network
import OSLog
import Synchronization

/// The loopback listener the canvas page is served from and streamed to.
///
/// **Why a server at all.** The page needs two things that are one decision.
/// It must be a *secure context*, or WebCodecs hands back no `AudioDecoder` and
/// remote sound disappears with no error worth reading; and it must receive
/// 256 KB tile batches without paying to base64 them through
/// `callAsyncJavaScript`, which is the only other way bytes reach a `WKWebView`.
/// Serving the document from `http://127.0.0.1:<port>/` answers both: loopback
/// is potentially trustworthy, so the origin is secure without
/// `loadSimulatedRequest` tricks or a custom scheme, and the same listener can
/// hold one response open and write raw bytes into it.
///
/// **Why it is safe to listen.** One address (`127.0.0.1`, so no firewall
/// prompt and nothing off this machine can reach it) and a random path prefix
/// minted per launch, checked before anything is served — the same shape as the
/// embedded gateway's bearer token (`src/auth.rs`), and for the same reason:
/// the port is not a secret and the token is.
final class CanvasServer: Sendable {
    /// Where the page is, once the kernel has named a port.
    struct Address: Equatable {
        let port: UInt16
        /// The random path prefix, without slashes.
        let token: String

        /// The document URL, with the stream's absolute URL as a query so the
        /// page works the same when Vite is serving it (`REMOTEX_VIEWER_DEV_URL`).
        func documentURL(base: URL? = nil) -> URL {
            var components = URLComponents()
            components.scheme = "http"
            components.host = "127.0.0.1"
            components.port = Int(port)
            components.path = "/\(token)/"
            let frames = components.url!.appendingPathComponent("frames")
            guard let base, var dev = URLComponents(url: base, resolvingAgainstBaseURL: false)
            else {
                // Unwrapped deliberately: these fields always compose, and a nil
                // here would mean Foundation cannot build `http://127.0.0.1:1/`.
                components.queryItems = [URLQueryItem(name: "frames", value: frames.absoluteString)]
                return components.url!
            }
            dev.queryItems = [URLQueryItem(name: "frames", value: frames.absoluteString)]
            return dev.url ?? base
        }
    }

    enum StartError: Error, Equatable {
        /// `Contents/Resources/canvas/viewer.html` is missing, which means the
        /// bundle was assembled without `bun run build:viewer`.
        case noPage
        case listener(String)
    }

    /// One envelope on the downstream stream: `[u32 be length][u8 kind][payload]`,
    /// where the length counts the kind byte. Big-endian because this framing is
    /// ours; the gateway's own frames travel through as opaque payload and keep
    /// their little-endian layout.
    enum Envelope {
        static let control: UInt8 = 0x00
        static let frame: UInt8 = 0x01
    }

    private struct State: @unchecked Sendable {
        var listener: NWListener?
        /// The one held-open response. A second attachment supersedes the first:
        /// there is one page, and a reload is the ordinary way this happens.
        var stream: NWConnection?
        var address: Address?
        var started: CheckedContinuation<Address, any Error>?
        /// Frame bytes handed to the connection and not yet written out. See
        /// `maxPendingFrameBytes`.
        var pendingFrameBytes = 0
        /// Whether the ceiling above has cost this stream any pixels, so the
        /// desktop can be asked for again once the backlog clears.
        var droppedFrames = false
    }

    private let state = Mutex(State())
    private let root: URL
    private let token: String
    private let queue = DispatchQueue(label: "dev.remotex.viewer.canvas")
    private let log = Logger(subsystem: "dev.remotex.viewer", category: "canvas")
    /// A stream became live. Every attachment starts from nothing, so this is
    /// what re-sends the current size and asks the gateway to repaint.
    private let onAttach: @Sendable () -> Void

    /// - Parameter root: the directory holding `viewer.html`. Defaults to the
    ///   bundle's, which is what a packaged app has; tests pass their own.
    init(root: URL? = nil, onAttach: @escaping @Sendable () -> Void) {
        self.root = root ?? CanvasServer.bundledPage
        self.token = CanvasServer.mintToken()
        self.onAttach = onAttach
    }

    /// `Contents/Resources/canvas`, where `build-viewer-app.sh` puts the built
    /// page. Absent from an unbundled build, which `start()` reports.
    static var bundledPage: URL {
        Bundle.main.resourceURL?.appendingPathComponent("canvas", isDirectory: true)
            ?? URL(fileURLWithPath: "/nonexistent")
    }

    /// 32 random bytes, base64url. Not hashed and not stored: it is minted in
    /// memory, handed to one `WKWebView`, and gone when this process is.
    private static func mintToken() -> String {
        var generator = SystemRandomNumberGenerator()
        var bytes = Data(count: 32)
        for index in bytes.indices {
            bytes[index] = UInt8.random(in: .min ... .max, using: &generator)
        }
        return bytes.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    // MARK: - Lifecycle

    /// Bind, then report the address. The order is the contract: a URL handed
    /// out before the port is bound is one the web view cannot load.
    func start() async throws -> Address {
        guard FileManager.default.fileExists(atPath: root.appendingPathComponent("viewer.html").path)
        else {
            throw StartError.noPage
        }
        let parameters = NWParameters.tcp
        // One address, and it is the one that makes the origin trustworthy.
        // Without this the listener would take every interface its host name
        // resolves to, which is a different program than this one.
        parameters.requiredLocalEndpoint = .hostPort(host: .ipv4(.loopback), port: .any)
        parameters.allowLocalEndpointReuse = true

        let listener: NWListener
        do {
            listener = try NWListener(using: parameters)
        } catch {
            throw StartError.listener(error.localizedDescription)
        }
        listener.newConnectionHandler = { [weak self] connection in
            self?.accept(connection)
        }
        listener.stateUpdateHandler = { [weak self] update in
            guard let self else {
                return
            }
            switch update {
            case .ready:
                let port = listener.port?.rawValue ?? 0
                self.resume(.success(Address(port: port, token: self.token)))
            case .failed(let error):
                self.resume(.failure(StartError.listener(error.localizedDescription)))
            case .waiting(let error):
                // Not terminal: a listener waits for a resource it expects to
                // get, and reaches `.ready` when it does. Failing here reports a
                // dead canvas over something transient — the deadline below is
                // what answers a listener that never arrives.
                self.log.warning(
                    "canvas listener waiting: \(error.localizedDescription, privacy: .public)"
                )
            default:
                break
            }
        }
        return try await withCheckedThrowingContinuation { continuation in
            let ready = state.withLock { state -> Address? in
                state.listener = listener
                if let address = state.address {
                    return address
                }
                state.started = continuation
                return nil
            }
            if let ready {
                continuation.resume(returning: ready)
                return
            }
            // A bind of one loopback port either happens at once or is not going
            // to. `resume` only settles the first caller, so a deadline that
            // fires after `.ready` costs nothing.
            queue.asyncAfter(deadline: .now() + Self.startDeadline) { [weak self] in
                self?.resume(.failure(StartError.listener("the listener never became ready")))
            }
            listener.start(queue: queue)
        }
    }

    /// How long `start()` waits for `.ready` before giving up.
    private static let startDeadline: DispatchTimeInterval = .seconds(10)

    private func resume(_ result: Result<Address, any Error>) {
        let continuation = state.withLock { state -> CheckedContinuation<Address, any Error>? in
            if case .success(let address) = result {
                state.address = address
            }
            defer { state.started = nil }
            return state.started
        }
        continuation?.resume(with: result)
    }

    func stop() {
        let (listener, stream) = state.withLock { state -> (NWListener?, NWConnection?) in
            defer {
                state.listener = nil
                state.stream = nil
            }
            return (state.listener, state.stream)
        }
        listener?.cancel()
        guard let stream else {
            return
        }
        // The terminator, so a page that is still up reads a clean end of body
        // and reconnects rather than reporting a truncated response — and the
        // cancel waits for it. Cancelling straight after the send tears the
        // connection down with the write still queued, which is the truncated
        // response this exists to avoid.
        stream.send(
            content: Self.chunkTerminator,
            completion: .contentProcessed { _ in stream.cancel() }
        )
    }

    // MARK: - Sending

    /// Queue one control command for the page. Dropped when no stream is
    /// attached: the next attachment is re-primed from scratch, so a command
    /// held for it would be a duplicate at best.
    func send(_ command: CanvasCommand) {
        guard let json = command.jsonData() else {
            log.warning("unencodable canvas command dropped")
            return
        }
        send(kind: Envelope.control, payload: json)
    }

    /// Queue one gateway binary frame, verbatim — its own `0x02`/`0x03` kind
    /// byte included, because the page parses it with the same code the browser
    /// SPA uses.
    func send(frame: Data) {
        send(kind: Envelope.frame, payload: frame)
    }

    /// Most unwritten frame bytes to hold before dropping more.
    ///
    /// A wedged or very slow page stops draining the socket while the gateway
    /// keeps producing, and `NWConnection` will queue whatever it is given: at a
    /// 256 KB batch a frame that is nothing to spare a few of, this grows without
    /// limit. Eight of them is far more backlog than a live page ever has and far
    /// less than a leak.
    private static let maxPendingFrameBytes = 8 * 256 * 1024

    private func send(kind: UInt8, payload: Data) {
        let chunk = Self.chunk(kind: kind, payload: payload)
        // Control envelopes are never dropped. They are small, and each one is a
        // fact the page cannot be told twice — a `resize` or a `clear` skipped
        // here leaves it drawing into the wrong geometry with nothing to correct
        // it.
        let isFrame = kind == Envelope.frame

        enum Outcome {
            case send(NWConnection)
            case drop
            case none
        }
        let outcome = state.withLock { state -> Outcome in
            guard let stream = state.stream else {
                return .none
            }
            if isFrame, state.pendingFrameBytes > Self.maxPendingFrameBytes {
                // Dropped pixels are only recoverable because something asks for
                // them again; see the re-prime below.
                state.droppedFrames = true
                return .drop
            }
            if isFrame {
                state.pendingFrameBytes += chunk.count
            }
            return .send(stream)
        }
        switch outcome {
        case .none:
            return
        case .drop:
            log.warning("canvas stream backed up; dropped a frame")
            return
        case .send(let stream):
            // `NWConnection` sends in call order, which is what keeps a `resize`
            // from overtaking tiles drawn in the space it replaces.
            stream.send(
                content: chunk,
                completion: isFrame
                    ? .contentProcessed { [weak self] _ in
                        self?.frameWritten(chunk.count)
                    }
                    : .idempotent
            )
        }
    }

    /// One frame is off our hands. When the backlog has cleared and anything was
    /// dropped, ask for the desktop again: tiles carry no delta state and the
    /// gateway believes it has already sent what it sent, so nothing else would
    /// ever repaint what went missing.
    private func frameWritten(_ bytes: Int) {
        let reprime = state.withLock { state -> Bool in
            state.pendingFrameBytes = max(0, state.pendingFrameBytes - bytes)
            guard state.pendingFrameBytes == 0, state.droppedFrames else {
                return false
            }
            state.droppedFrames = false
            return true
        }
        if reprime {
            onAttach()
        }
    }

    /// One HTTP chunk carrying one envelope.
    static func chunk(kind: UInt8, payload: Data) -> Data {
        var envelope = Data(capacity: payload.count + 5)
        let length = UInt32(payload.count + 1)
        envelope.append(UInt8(truncatingIfNeeded: length >> 24))
        envelope.append(UInt8(truncatingIfNeeded: length >> 16))
        envelope.append(UInt8(truncatingIfNeeded: length >> 8))
        envelope.append(UInt8(truncatingIfNeeded: length))
        envelope.append(kind)
        envelope.append(payload)

        var chunk = Data(String(format: "%X\r\n", envelope.count).utf8)
        chunk.append(envelope)
        chunk.append(contentsOf: [0x0d, 0x0a])
        return chunk
    }

    private static let chunkTerminator = Data("0\r\n\r\n".utf8)

    // MARK: - Serving

    private func accept(_ connection: NWConnection) {
        connection.start(queue: queue)
        read(connection, head: Data())
    }

    /// Accumulate until the request head is complete. Only the request line is
    /// read from it: both ends of this conversation are ours, so there are no
    /// headers worth honouring and none worth tolerating.
    private func read(_ connection: NWConnection, head: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 8192) {
            [weak self] chunk, _, isComplete, error in
            guard let self else {
                connection.cancel()
                return
            }
            var head = head
            head.append(chunk ?? Data())
            if let end = head.range(of: Data("\r\n\r\n".utf8)) {
                self.route(connection, requestLine: head.prefix(upTo: end.lowerBound))
                return
            }
            // 16 KB of head with no blank line is not a request this serves.
            guard error == nil, !isComplete, head.count < 16 * 1024 else {
                connection.cancel()
                return
            }
            self.read(connection, head: head)
        }
    }

    private func route(_ connection: NWConnection, requestLine head: Data) {
        let line = String(decoding: head.prefix(while: { $0 != 0x0d }), as: UTF8.self)
        let fields = line.split(separator: " ")
        guard fields.count >= 2, fields[0] == "GET",
              let path = Self.resourcePath(fields[1], token: token)
        else {
            respond(connection, status: "404 Not Found", body: Data(), type: "text/plain")
            return
        }
        if path == "frames" {
            attachStream(connection)
            return
        }
        serveFile(connection, path: path.isEmpty ? "viewer.html" : path)
    }

    /// The path under the token prefix, or nil when the request is not for this
    /// server. Refuses anything that could climb out of the page directory.
    static func resourcePath(_ target: some StringProtocol, token: String) -> String? {
        // Query strings belong to the page (`?frames=`), not to the file it asks
        // for.
        let withoutQuery = target.prefix(while: { $0 != "?" })
        let parts = withoutQuery.split(separator: "/", omittingEmptySubsequences: false)
        // A leading "/" makes the first component empty.
        guard parts.count >= 2, parts[0].isEmpty, parts[1] == token else {
            return nil
        }
        let rest = parts.dropFirst(2)
        guard rest.allSatisfy({ $0 != ".." && !$0.contains("\\") }) else {
            return nil
        }
        return rest.joined(separator: "/")
    }

    private func serveFile(_ connection: NWConnection, path: String) {
        let file = root.appendingPathComponent(path)
        guard file.standardizedFileURL.path.hasPrefix(root.standardizedFileURL.path),
              let body = try? Data(contentsOf: file)
        else {
            respond(connection, status: "404 Not Found", body: Data(), type: "text/plain")
            return
        }
        respond(connection, status: "200 OK", body: body, type: Self.contentType(of: path))
    }

    static func contentType(of path: String) -> String {
        switch (path as NSString).pathExtension.lowercased() {
        case "html": "text/html; charset=utf-8"
        case "js": "text/javascript; charset=utf-8"
        case "css": "text/css; charset=utf-8"
        case "json": "application/json"
        case "svg": "image/svg+xml"
        case "png": "image/png"
        case "woff2": "font/woff2"
        default: "application/octet-stream"
        }
    }

    private func respond(
        _ connection: NWConnection,
        status: String,
        body: Data,
        type: String
    ) {
        var response = Data(
            """
            HTTP/1.1 \(status)\r
            Content-Type: \(type)\r
            Content-Length: \(body.count)\r
            Cache-Control: no-store\r
            Connection: close\r
            \r\n
            """.utf8
        )
        response.append(body)
        connection.send(
            content: response,
            completion: .contentProcessed { _ in connection.cancel() }
        )
    }

    /// The one origin allowed to read the stream cross-origin, or nil.
    ///
    /// Nil is the shipped case and the point of it: the page is served by this
    /// listener, so it reads the stream same-origin and needs no header at all.
    /// `Access-Control-Allow-Origin: *` was letting anything else on this machine
    /// read the desktop if it learned the token — a second lock left open beside
    /// the one that matters. Only `REMOTEX_VIEWER_DEV_URL`, where Vite serves the
    /// page from another origin, needs one, and then it names that origin exactly.
    private static var devOrigin: String? {
        guard let raw = ProcessInfo.processInfo.environment["REMOTEX_VIEWER_DEV_URL"],
              let url = URL(string: raw),
              let scheme = url.scheme,
              let host = url.host
        else {
            return nil
        }
        guard let port = url.port else {
            return "\(scheme)://\(host)"
        }
        return "\(scheme)://\(host):\(port)"
    }

    /// Hold this response open and make it the stream.
    private func attachStream(_ connection: NWConnection) {
        let allowOrigin = Self.devOrigin.map { "Access-Control-Allow-Origin: \($0)\r\n" } ?? ""
        let head = Data(
            """
            HTTP/1.1 200 OK\r
            Content-Type: application/octet-stream\r
            Transfer-Encoding: chunked\r
            Cache-Control: no-store\r
            \(allowOrigin)Connection: close\r
            \r\n
            """.utf8
        )
        let previous = state.withLock { state -> NWConnection? in
            defer { state.stream = connection }
            return state.stream
        }
        previous?.cancel()
        connection.send(content: head, completion: .idempotent)
        // Chunked framing does not need a length up front, which is the whole
        // reason for it here: the app writes envelopes for as long as the page
        // is up.
        connection.stateUpdateHandler = { [weak self] update in
            switch update {
            case .failed, .cancelled:
                self?.state.withLock { state in
                    if state.stream === connection {
                        state.stream = nil
                    }
                }
            default:
                break
            }
        }
        onAttach()
    }
}
