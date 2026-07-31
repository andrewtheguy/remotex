import Foundation

enum WebSocketFrame: Sendable, Equatable {
    case text(String)
    case binary(Data)
}

/// One session socket. Abstracted so the connection driver can be tested against
/// a scripted transport — there is no other seam for it, because `URLProtocol`
/// stubs do not intercept `URLSessionWebSocketTask`.
protocol WebSocketTransport: Sendable {
    func send(_ text: String) async throws
    /// Awaits the next frame, throwing when the socket ends. Read `closeCode`
    /// afterwards to find out why.
    func receive() async throws -> WebSocketFrame
    func cancel()
    /// The close code, once known. 4000 means the claim token was invalid or
    /// superseded; 4001 means another client took the slot.
    var closeCode: Int? { get }
}

/// Everything the connection driver needs from the gateway: the one seam the
/// tests replace.
protocol SessionGateway: Sendable {
    func claimSession(force: Bool, sessionId: String?) async throws -> ClaimOutcome
    func openSocket(sessionToken: String) async throws -> any WebSocketTransport
}

/// `URLSessionWebSocketTask` behind the protocol.
///
/// A task is single-use — once `receive()` throws it cannot be re-armed — so a
/// fresh one is built per connection attempt.
final class URLSessionWebSocketTransport: WebSocketTransport {
    /// Headroom over `URLSessionWebSocketTask`'s **1 MiB default**, because going
    /// past that limit fails the whole socket rather than dropping the one frame —
    /// which would read as a random disconnect under load rather than as a size
    /// problem.
    ///
    /// Measured, not guessed: `--probe` against a 3204×1758 Mac desktop saw a
    /// largest strip of ~100 KB, so the default would have held there. But a
    /// strip is a full-width band of up to 64 rows, so the ceiling scales with
    /// the remote's width and how badly its content compresses, and there is no
    /// reason to sit near a cliff whose far side is a torn session. The gateway's
    /// own ceiling (axum's `max_message_size`) is 64 MiB.
    static let maximumMessageSize = 16 << 20

    private let task: URLSessionWebSocketTask

    init(task: URLSessionWebSocketTask) {
        task.maximumMessageSize = Self.maximumMessageSize
        self.task = task
        task.resume()
    }

    var closeCode: Int? {
        let code = task.closeCode
        return code == .invalid ? nil : code.rawValue
    }

    func send(_ text: String) async throws {
        try await task.send(.string(text))
    }

    func receive() async throws -> WebSocketFrame {
        switch try await task.receive() {
        case .string(let text):
            return .text(text)
        case .data(let data):
            return .binary(data)
        @unknown default:
            throw WebSocketTransportError.unknownFrameKind
        }
    }

    func cancel() {
        task.cancel(with: .goingAway, reason: nil)
    }
}

enum WebSocketTransportError: Error, Equatable {
    case unknownFrameKind
}
