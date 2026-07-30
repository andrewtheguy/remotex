import Foundation
import Testing
@testable import RemotexViewer

/// Serialized because `StubURLProtocol` routes through static state, and using
/// an ephemeral session configuration throughout so nothing here touches the
/// real cookie jar.
///
/// Note what is *not* tested with a stub: the session socket. `URLProtocol` does
/// not intercept `URLSessionWebSocketTask`, so what is checked here is the
/// upgrade request the client builds — the socket itself is covered by
/// `GatewayConnectionTests` through the transport seam.
@Suite(.serialized)
struct GatewayClientTests {
    @Test
    func aMatchingProtocolVersionYieldsTheBranding() async throws {
        let client = client { _ in
            (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
        }
        let config = try await client.configuration()
        #expect(config.branding == "acme")
        #expect(config.protocolVersion == ProductInfo.protocolVersion)
    }

    /// The point of the field. A gateway on another revision is refused up front,
    /// where it can be explained, rather than producing an unreadable frame later.
    @Test
    func aDifferentProtocolVersionIsRefused() async throws {
        let client = client { _ in
            (200, #"{"branding":"acme","protocolVersion":9999}"#)
        }
        await #expect(
            throws: GatewayClientError.incompatible(
                gateway: 9999,
                viewer: ProductInfo.protocolVersion
            )
        ) {
            try await client.configuration()
        }
    }

    /// The token goes on **every** request, including the public one.
    ///
    /// `/api/config` needs no credential, which is exactly why this is worth pinning:
    /// a client that only authenticates the routes it believes are guarded is one
    /// route away from a 401 nobody expected, and there is no cost to sending it.
    @Test
    func everyRequestCarriesTheBearerToken() async throws {
        let recorded = Recorder()
        let client = client { request in
            recorded.store(request)
            return (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
        }
        _ = try await client.configuration()
        #expect(
            recorded.request?.value(forHTTPHeaderField: "Authorization") == "Bearer tok-abc"
        )
        #expect(
            recorded.request?.httpShouldHandleCookies == false,
            "there are no cookies in this arrangement"
        )
    }

    @Test
    func anUnreachableGatewayIsReportedAsSuch() async throws {
        let client = client { _ in nil }
        await #expect(throws: GatewayClientError.self) {
            try await client.configuration()
        }
    }

    @Test
    func targetsDecodeTheirProtocolNameAroundTheSwiftKeyword() async throws {
        let client = client { _ in
            (200, #"[{"name":"mac","protocol":"rxa","host":"fd00::1","port":52381}]"#)
        }
        let targets = try await client.targets()
        #expect(targets.count == 1)
        #expect(targets[0].name == "mac")
        #expect(targets[0].protocolName == "rxa")
        #expect(targets[0].port == 52381)
        #expect(targets[0].detail == "RXA · fd00::1:52381")
    }

    @Test
    func a401OnAGuardedRouteThrowsUnauthorized() async throws {
        let client = client { _ in (401, "unauthorized") }
        await #expect(throws: GatewayClientError.unauthorized) {
            _ = try await client.targets()
        }
    }

    /// 409 and 401 are states the UI has to show, not failures, so they come back
    /// as values. Anything else really is a failure.
    @Test
    func claimOutcomesMapFromStatus() async throws {
        #expect(
            try await client { _ in (200, #"{"sessionId":"tok-9"}"#) }
                .claimSession(force: false, sessionId: nil) == .claimed("tok-9")
        )
        #expect(
            try await client { _ in (409, "another browser holds the session") }
                .claimSession(force: false, sessionId: nil) == .busy
        )
        #expect(
            try await client { _ in (401, "unauthorized") }
                .claimSession(force: false, sessionId: nil) == .unauthorized
        )
        await #expect(throws: GatewayClientError.unexpectedStatus(503)) {
            _ = try await client { _ in (503, "nope") }
                .claimSession(force: false, sessionId: nil)
        }
    }

    @Test
    func aClaimSendsForceAndTheHeldToken() async throws {
        let recorded = Recorder()
        let client = client { request in
            recorded.store(request)
            return (200, #"{"sessionId":"tok-2"}"#)
        }
        _ = try await client.claimSession(force: true, sessionId: "tok-1")

        let body = try #require(recorded.body)
        let json = try #require(
            JSONSerialization.jsonObject(with: body) as? [String: Any]
        )
        #expect(json["force"] as? Bool == true)
        #expect(json["sessionId"] as? String == "tok-1")
        #expect(recorded.request?.httpMethod == "POST")
    }

    // MARK: - The upgrade request

    /// Two different tokens meet on this one request and neither may end up where the
    /// other belongs: the claim decides whose turn it is and rides in the query, the
    /// bearer decides whether this client may ask at all and rides in the header.
    @Test
    func theUpgradeRequestCarriesBothTokensInTheirOwnPlaces() throws {
        let client = GatewayClient(
            gateway: .loopback(port: 52675),
            token: "tok-abc",
            session: stubSession()
        )

        let request = try client.webSocketRequest(sessionToken: "claim-7")
        #expect(request.url?.absoluteString == "ws://127.0.0.1:52675/ws?session=claim-7")
        #expect(
            request.value(forHTTPHeaderField: "Authorization") == "Bearer tok-abc",
            "require_auth runs before the upgrade — see webSocketRequest"
        )
        #expect(request.value(forHTTPHeaderField: "Cookie") == nil, "no cookies anywhere")
        #expect(request.httpShouldHandleCookies == false)
    }

    /// The scheme still follows the gateway's, even though this build only ever talks
    /// to loopback: it is `GatewayLocation` that decides, and a `wss` gateway one day
    /// must not silently downgrade.
    @Test
    func anHTTPSGatewayUpgradesOverWSS() throws {
        let client = GatewayClient(
            gateway: try GatewayLocation.parse("https://remote.example.com"),
            token: "tok-abc",
            session: stubSession()
        )
        let request = try client.webSocketRequest(sessionToken: "a b")
        // The token is a UUID in practice, but it still has to be escaped rather
        // than pasted into the query.
        #expect(
            request.url?.absoluteString == "wss://remote.example.com/ws?session=a%20b"
        )
    }

    /// The app builds its address from a number the gateway printed, so the one thing
    /// worth pinning is that the number lands in the port and nowhere else.
    @Test
    func theLoopbackLocationIsBuiltFromThePortAlone() {
        #expect(GatewayLocation.loopback(port: 49213).url.absoluteString == "http://127.0.0.1:49213/")
        #expect(GatewayLocation.loopback(port: 1).url.port == 1)
    }

    // MARK: - Plumbing

    private func client(
        _ handler: @escaping @Sendable (URLRequest) -> (Int, String)?
    ) -> GatewayClient {
        StubURLProtocol.handler = handler
        return GatewayClient(
            gateway: .loopback(port: 52675),
            token: "tok-abc",
            session: stubSession()
        )
    }

    private func stubSession() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubURLProtocol.self]
        return URLSession(configuration: configuration)
    }
}

/// Captures the request a single stubbed call received.
private final class Recorder: @unchecked Sendable {
    private let lock = NSLock()
    private var captured: URLRequest?

    func store(_ request: URLRequest) {
        lock.withLock { captured = request }
    }

    var request: URLRequest? {
        lock.withLock { captured }
    }

    /// `URLProtocol` moves the body to a stream, so `httpBody` is nil by the time
    /// it arrives — read it back off the stream instead.
    var body: Data? {
        guard let request = self.request else {
            return nil
        }
        if let body = request.httpBody {
            return body
        }
        guard let stream = request.httpBodyStream else {
            return nil
        }
        stream.open()
        defer { stream.close() }
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4_096)
        while stream.hasBytesAvailable {
            let read = stream.read(&buffer, maxLength: buffer.count)
            guard read > 0 else {
                break
            }
            data.append(contentsOf: buffer[0 ..< read])
        }
        return data
    }
}

/// Answers requests from a per-test closure. `nil` stands for an unreachable
/// gateway.
final class StubURLProtocol: URLProtocol {
    nonisolated(unsafe) static var handler: (@Sendable (URLRequest) -> (Int, String)?)?

    override class func canInit(with request: URLRequest) -> Bool {
        true
    }

    override class func canonicalRequest(for request: URLRequest) -> URLRequest {
        request
    }

    override func startLoading() {
        guard let answer = Self.handler?(request) else {
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
        )!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(answer.1.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}
