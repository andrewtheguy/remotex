import Foundation
import Synchronization

/// Answers HTTP requests from a per-test closure instead of the network.
///
/// Registered **per host**, not globally, and that is the whole design. A single
/// static handler is the obvious shape and it is wrong here: swift-testing runs
/// suites concurrently, so one suite's handler replaces another's mid-test and the
/// failure that follows is a route answering with somebody else's body — which reads
/// as a bug in the code under test. Keying on the authority means two tests only
/// collide if they chose the same address, and `uniqueHost` exists so they do not
/// have to think about it.
///
/// An unregistered host fails as unreachable rather than falling through to the real
/// network: a test that forgot to register must not quietly resolve a name.
///
/// Note what is *not* stubbed: the session socket. `URLProtocol` does not intercept
/// `URLSessionWebSocketTask`, so anything past the upgrade belongs to the tests that
/// drive a scripted transport.
final class StubURLProtocol: URLProtocol {
    /// A status and a body. `nil` stands for an unreachable gateway.
    typealias Handler = @Sendable (URLRequest) -> (Int, String)?
    /// Response headers beyond `Content-Type`, for the one route whose header is the
    /// subject: `/api/auth/login`, which returns its session as a `Set-Cookie`.
    typealias Headers = @Sendable (URLRequest) -> [String: String]

    private struct Stub {
        var handler: Handler
        var headers: Headers?
    }

    private static let stubs = Mutex<[String: Stub]>([:])

    /// A host no other test is using, for a session that needs its own answers.
    static func uniqueHost() -> String {
        "\(UUID().uuidString.lowercased()).test"
    }

    /// Answer requests to `host` with `handler`. Replaces any previous registration
    /// for that host, so a test may be run twice without leaking the first run's
    /// answers into the second.
    static func register(
        host: String,
        headers: Headers? = nil,
        handler: @escaping Handler
    ) {
        stubs.withLock { $0[host] = Stub(handler: handler, headers: headers) }
    }

    private static func stub(for request: URLRequest) -> Stub? {
        guard let host = request.url?.host() else {
            return nil
        }
        return stubs.withLock { $0[host] }
    }

    override class func canInit(with request: URLRequest) -> Bool {
        true
    }

    override class func canonicalRequest(for request: URLRequest) -> URLRequest {
        request
    }

    override func startLoading() {
        guard let stub = Self.stub(for: request), let answer = stub.handler(request) else {
            client?.urlProtocol(
                self,
                didFailWithError: URLError(.cannotConnectToHost)
            )
            return
        }
        let response = HTTPURLResponse(
            url: request.url!,
            statusCode: answer.0,
            httpVersion: "HTTP/1.1",
            headerFields: ["Content-Type": "application/json"]
                .merging(stub.headers?(request) ?? [:]) { _, added in added }
        )!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(answer.1.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}

    /// A session that reaches nothing but this stub.
    static func session() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubURLProtocol.self]
        return URLSession(configuration: configuration)
    }
}
