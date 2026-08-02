import Foundation
import Network
import Synchronization
import Testing
@testable import RemotexViewer

/// The loopback listener, over a real socket.
///
/// Driven over a socket rather than by calling into the type, because what these
/// assert is what a `WKWebView` would actually receive: the status, the bytes,
/// and — for the stream — that they arrive *before* the response ends.
@MainActor
struct CanvasServerTests {
    /// A page directory with one file in it, standing in for `Contents/Resources/canvas`.
    private static func page(_ scratch: ScratchDirectory) throws -> URL {
        let root = scratch.url.appendingPathComponent("canvas", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try Data("<!doctype html><title>c</title>".utf8)
            .write(to: root.appendingPathComponent("viewer.html"))
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("assets", isDirectory: true),
            withIntermediateDirectories: true
        )
        try Data("export const x = 1;\n".utf8)
            .write(to: root.appendingPathComponent("assets/viewer.js"))
        return root
    }

    private static func started(
        _ scratch: ScratchDirectory,
        onAttach: @escaping @Sendable () -> Void = {}
    ) async throws -> (CanvasServer, CanvasServer.Address) {
        let server = CanvasServer(root: try page(scratch), onAttach: onAttach)
        let address = try await server.start()
        return (server, address)
    }

    private static func url(_ address: CanvasServer.Address, _ path: String) -> URL {
        URL(string: "http://127.0.0.1:\(address.port)\(path)")!
    }

    private static let session = URLSession(configuration: .ephemeral)

    @Test
    func thePageIsServedUnderTheTokenAndNowhereElse() async throws {
        let scratch = try ScratchDirectory()
        let (server, address) = try await Self.started(scratch)
        defer { server.stop() }

        let (body, response) = try await Self.session.data(
            from: Self.url(address, "/\(address.token)/")
        )
        #expect((response as? HTTPURLResponse)?.statusCode == 200)
        #expect(String(decoding: body, as: UTF8.self).contains("<!doctype html>"))
        #expect(
            (response as? HTTPURLResponse)?.value(forHTTPHeaderField: "Content-Type")
                == "text/html; charset=utf-8"
        )

        let (_, asset) = try await Self.session.data(
            from: Self.url(address, "/\(address.token)/assets/viewer.js")
        )
        #expect(
            (asset as? HTTPURLResponse)?.value(forHTTPHeaderField: "Content-Type")
                == "text/javascript; charset=utf-8"
        )
    }

    /// The port is not a secret and the token is — the same split as the embedded
    /// gateway's bearer token. Without this, anything else on this machine could
    /// read the frames of whatever desktop is open.
    @Test
    func aRequestWithoutTheTokenIsRefused() async throws {
        let scratch = try ScratchDirectory()
        let (server, address) = try await Self.started(scratch)
        defer { server.stop() }

        for path in ["/", "/viewer.html", "/frames", "/wrong-token/", "/wrong-token/frames"] {
            let (_, response) = try await Self.session.data(from: Self.url(address, path))
            #expect(
                (response as? HTTPURLResponse)?.statusCode == 404,
                "\(path) should not be served"
            )
        }
    }

    /// The page directory is the boundary. Nothing outside it is this server's to
    /// hand out, whatever a path says.
    @Test
    func aPathCannotClimbOutOfThePageDirectory() throws {
        let token = "tok"
        #expect(CanvasServer.resourcePath("/tok/viewer.html", token: token) == "viewer.html")
        #expect(CanvasServer.resourcePath("/tok/", token: token) == "")
        #expect(CanvasServer.resourcePath("/tok/?frames=x", token: token) == "")
        #expect(CanvasServer.resourcePath("/tok/assets/a.js", token: token) == "assets/a.js")
        #expect(CanvasServer.resourcePath("/tok/../../etc/passwd", token: token) == nil)
        #expect(CanvasServer.resourcePath("/tok/a/../../b", token: token) == nil)
        #expect(CanvasServer.resourcePath("/other/viewer.html", token: token) == nil)
        #expect(CanvasServer.resourcePath("/", token: token) == nil)
        #expect(CanvasServer.resourcePath("viewer.html", token: token) == nil)
    }

    /// A bundle assembled without `bun run build:viewer` has no page to serve, and
    /// saying so beats a web view that loads a 404 and reports nothing.
    @Test
    func aMissingPageIsRefusedAtStart() async throws {
        let scratch = try ScratchDirectory()
        let server = CanvasServer(root: scratch.url, onAttach: {})
        await #expect(throws: CanvasServer.StartError.noPage) {
            try await server.start()
        }
    }

    /// The framing the page's reassembler reads: `[u32 be length][u8 kind][payload]`,
    /// with the length counting the kind byte.
    @Test
    func anEnvelopeIsLengthPrefixedAndKindTagged() {
        let chunk = CanvasServer.chunk(kind: 0x01, payload: Data([0xAA, 0xBB, 0xCC]))
        // One HTTP chunk: hex length, CRLF, the envelope, CRLF. Eight bytes of
        // envelope — four of length, the kind, three of payload — and the length
        // itself reads 4, because it counts the kind byte with the payload.
        #expect(String(decoding: chunk.prefix(3), as: UTF8.self) == "8\r\n")
        #expect(Array(chunk.dropFirst(3).dropLast(2)) == [0, 0, 0, 4, 0x01, 0xAA, 0xBB, 0xCC])
        #expect(Array(chunk.suffix(2)) == [0x0d, 0x0a])
    }

    /// The point of the whole listener: envelopes reach the reader while the
    /// response is still open. A body that only arrived at close would make the
    /// desktop appear when the session ended.
    ///
    /// Read over a raw socket rather than through `URLSession`, which does not
    /// hand back a response until some of its body has arrived — so a test using
    /// it could not send *after* connecting without deadlocking itself, and could
    /// not tell a stream from a buffer either way. The bytes below are exactly
    /// what `frontend/src/viewer/stream.ts` reads.
    @Test
    func theStreamDeliversBeforeItEnds() async throws {
        let scratch = try ScratchDirectory()
        let attached = Attached()
        let (server, address) = try await Self.started(scratch) { attached.signal() }
        defer { server.stop() }

        let client = RawClient(port: address.port)
        try await client.get("/\(address.token)/frames")
        #expect(await attached.wait(), "the server never accepted the stream")

        server.send(.clear)
        server.send(frame: Data([0x02, 0x00, 0x00, 0x00]))

        let head = try await client.readHead()
        #expect(head.hasPrefix("HTTP/1.1 200 OK"))
        #expect(head.contains("Transfer-Encoding: chunked"))

        let first = try await client.readChunk()
        #expect(first.first == 0x00, "a control envelope")
        #expect(String(decoding: first.dropFirst(), as: UTF8.self).contains("\"clear\""))

        let second = try await client.readChunk()
        #expect(second == Data([0x01, 0x02, 0x00, 0x00, 0x00]), "a frame, verbatim")
        client.close()
    }

    /// A reload is the ordinary way a second stream appears, and there is one page:
    /// the newer attachment is the live one, and the app re-primes it from scratch.
    @Test
    func aSecondStreamSupersedesTheFirst() async throws {
        let scratch = try ScratchDirectory()
        let attached = Attached()
        let (server, address) = try await Self.started(scratch) { attached.signal() }
        defer { server.stop() }

        let first = RawClient(port: address.port)
        try await first.get("/\(address.token)/frames")
        #expect(await attached.wait(), "the server never accepted the first stream")
        _ = try await first.readHead()

        let second = RawClient(port: address.port)
        try await second.get("/\(address.token)/frames")
        #expect(await attached.wait(), "the server never accepted the second stream")
        _ = try await second.readHead()

        server.send(.clear)

        let delivered = try await second.readChunk()
        #expect(delivered.first == 0x00, "the live stream is the newer one")
        // And the superseded one was closed rather than left holding a socket
        // nothing will ever write to.
        #expect(try await first.readsNothingMore())
        first.close()
        second.close()
    }
}

/// A one-shot signal, resettable, for "the server accepted a stream".
///
/// The attach callback fires on the listener's queue while the test is awaiting
/// elsewhere, so the test needs something to wait on rather than a sleep that
/// would encode a guess about the machine.
private final class Attached: @unchecked Sendable {
    private let semaphore = DispatchSemaphore(value: 0)

    func signal() {
        semaphore.signal()
    }

    /// Whether the signal arrived before the deadline. Returned rather than
    /// swallowed: a test that carried on from a timeout would fail later, on
    /// some other assertion, describing the wrong thing.
    func wait() async -> Bool {
        await withCheckedContinuation { (continuation: CheckedContinuation<Bool, Never>) in
            DispatchQueue.global().async {
                continuation.resume(returning: self.semaphore.wait(timeout: .now() + 5) == .success)
            }
        }
    }
}

private enum RawClientError: Error {
    /// The peer stayed silent past the deadline.
    case timedOut
    /// The peer closed.
    case ended
}

/// A socket that speaks just enough HTTP to read this server's stream.
private final class RawClient: @unchecked Sendable {
    private let connection: NWConnection
    private var buffer = Data()

    init(port: UInt16) {
        connection = NWConnection(
            host: .ipv4(.loopback),
            port: NWEndpoint.Port(rawValue: port)!,
            using: .tcp
        )
        connection.start(queue: .global())
    }

    func get(_ path: String) async throws {
        let request = Data("GET \(path) HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".utf8)
        try await withCheckedThrowingContinuation {
            (continuation: CheckedContinuation<Void, any Error>) in
            connection.send(
                content: request,
                completion: .contentProcessed { error in
                    if let error {
                        continuation.resume(throwing: error)
                    } else {
                        continuation.resume()
                    }
                }
            )
        }
    }

    /// How long one whole read waits, however many socket reads it takes.
    ///
    /// The budget belongs to the operation rather than to each `fill`: a peer
    /// that dribbles empty reads would otherwise re-arm the timeout every time
    /// and spin here forever, which is the one failure a bounded read exists to
    /// rule out.
    private static let readBudget: TimeInterval = 5

    /// Everything up to the blank line, as text.
    func readHead() async throws -> String {
        let deadline = Date().addingTimeInterval(Self.readBudget)
        while true {
            if let end = buffer.range(of: Data("\r\n\r\n".utf8)) {
                let head = String(decoding: buffer[..<end.lowerBound], as: UTF8.self)
                buffer = Data(buffer[end.upperBound...])
                return head
            }
            try await fill(by: deadline)
        }
    }

    /// One HTTP chunk's payload — which is one envelope, minus its length prefix.
    func readChunk() async throws -> Data {
        let deadline = Date().addingTimeInterval(Self.readBudget)
        while true {
            if let end = buffer.range(of: Data("\r\n".utf8)),
               let size = Int(String(decoding: buffer[..<end.lowerBound], as: UTF8.self), radix: 16),
               buffer.count >= end.upperBound - buffer.startIndex + size + 2
            {
                let start = end.upperBound
                let chunk = Data(buffer[start..<(start + size)])
                buffer = Data(buffer[(start + size + 2)...])
                // The envelope's own four-byte length, which the page's
                // reassembler consumes; the rest is `[kind][payload]`.
                #expect(chunk.count >= 4)
                let declared = chunk.prefix(4).reduce(0) { Int($0) << 8 | Int($1) }
                #expect(declared == chunk.count - 4, "the length names the rest of the envelope")
                return Data(chunk.dropFirst(4))
            }
            try await fill(by: deadline)
        }
    }

    /// True when the peer closed without sending anything more. A timeout is
    /// *not* that: a socket nobody closed and nobody wrote to is a third
    /// outcome, and reporting it as a clean close would pass this test for the
    /// wrong reason.
    func readsNothingMore() async throws -> Bool {
        do {
            try await fill(by: Date().addingTimeInterval(Self.readBudget))
            return false
        } catch RawClientError.ended {
            return true
        }
    }

    /// One read, bounded by the caller's deadline. `NWConnection.receive` waits
    /// for as long as the peer stays silent, so an unbounded one turns "the
    /// server did not write what it should have" into a test that never finishes
    /// and says nothing.
    private func fill(by deadline: Date) async throws {
        let remaining = deadline.timeIntervalSinceNow
        guard remaining > 0 else {
            throw RawClientError.timedOut
        }
        let more: Data = try await withCheckedThrowingContinuation { continuation in
            let settled = Mutex(false)
            let finish: @Sendable (Result<Data, any Error>) -> Void = { result in
                let first = settled.withLock { done -> Bool in
                    defer { done = true }
                    return !done
                }
                if first {
                    continuation.resume(with: result)
                }
            }
            DispatchQueue.global().asyncAfter(deadline: .now() + remaining) {
                finish(.failure(RawClientError.timedOut))
            }
            connection.receive(minimumIncompleteLength: 1, maximumLength: 65536) {
                chunk, _, isComplete, error in
                if let error {
                    finish(.failure(error))
                } else if let chunk, !chunk.isEmpty {
                    finish(.success(chunk))
                } else if isComplete {
                    finish(.failure(RawClientError.ended))
                } else {
                    finish(.success(Data()))
                }
            }
        }
        buffer.append(more)
    }

    func close() {
        connection.cancel()
    }
}
