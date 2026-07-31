import Foundation
import Testing
@testable import RemotexViewer

/// Ephemeral session configurations throughout, so nothing here touches the real
/// cookie jar — and one stubbed gateway per client, so nothing here touches another
/// test's either (`StubURLProtocol`).
///
/// Note what is *not* tested with a stub: the session socket. `URLProtocol` does
/// not intercept `URLSessionWebSocketTask`, so what is checked here is the
/// upgrade request the client builds — the socket itself is covered by
/// `GatewayConnectionTests` through the transport seam.
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
            (200, #"[{"name":"mac","protocol":"vnc","host":"fd00::1","port":52381}]"#)
        }
        let targets = try await client.targets()
        #expect(targets.count == 1)
        #expect(targets[0].name == "mac")
        #expect(targets[0].protocolName == "vnc")
        #expect(targets[0].port == 52381)
        #expect(targets[0].detail == "VNC · fd00::1:52381")
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

    // MARK: - The two credentials

    /// The one thing that differs between an embedded gateway and a remote one, and
    /// it differs in exactly one place: which header the credential rides in.
    ///
    /// They are not interchangeable at the other end — `require_auth` reads the
    /// cookie on a login gateway and the bearer on a token one, and neither looks at
    /// the other — so a credential in the wrong header is not a weak credential, it
    /// is none at all.
    @Test
    func eachCredentialRidesInItsOwnHeaderAndNotTheOther() async throws {
        let cases: [(credential: GatewayCredential, field: String, value: String?)] = [
            (.token("tok-abc"), "Authorization", "Bearer tok-abc"),
            (.session("sess-1"), "Cookie", "remotex_session=sess-1"),
            (.none, "Authorization", nil),
        ]
        for expectation in cases {
            let recorded = Recorder()
            let client = client(credential: expectation.credential) { request in
                recorded.store(request)
                return (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
            }
            _ = try await client.configuration()
            let request = try #require(recorded.request)
            #expect(
                request.value(forHTTPHeaderField: expectation.field) == expectation.value,
                "\(expectation.credential)"
            )
            // The other header is absent, not empty: a cookie on a token gateway is
            // refused outright rather than ignored.
            let other = expectation.field == "Cookie" ? "Authorization" : "Cookie"
            #expect(request.value(forHTTPHeaderField: other) == nil, "\(expectation.credential)")
            #expect(
                request.httpShouldHandleCookies == false,
                "this client carries its own cookie — see GatewayCredential.session"
            )
        }
    }

    /// A remote gateway before its login still has public routes to ask, and asking
    /// them is what the home screen does. So no credential must not mean no request.
    @Test
    func aClientWithNoCredentialStillReadsThePublicRoutes() async throws {
        let client = client(credential: .none) { _ in
            (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
        }
        #expect(try await client.configuration().branding == "acme")
    }

    // MARK: - Authentication

    @Test
    func authStateMapsFromTheStatusRoute() async throws {
        #expect(try await client { _ in (200, #"{"authenticated":true}"#) }
            .authState() == .authenticated)
        #expect(try await client { _ in (200, #"{"authenticated":false}"#) }
            .authState() == .needsLogin)
        // `no_login_handler` — an embedded gateway, which somebody typed the address
        // of. Its own answer, because a login form would never succeed against it.
        #expect(try await client { _ in (403, "forbidden") }
            .authState() == .noLoginOffered)
        await #expect(throws: GatewayClientError.unexpectedStatus(503)) {
            _ = try await client { _ in (503, "nope") }.authState()
        }
    }

    /// The login's whole product is the cookie it sets, so a client that does not
    /// come away holding it has not signed in whatever the status said.
    @Test
    func aLoginKeepsTheSessionCookieItWasSet() async throws {
        let client = client(
            credential: .none,
            headers: { _ in
                ["Set-Cookie": "remotex_session=sess-9; HttpOnly; SameSite=Strict; Path=/"]
            }
        ) { _ in (200, #"{"ok":true}"#) }

        #expect(try await client.logIn(username: "admin", password: "hunter2")
            == .ok(session: "sess-9"))
    }

    /// A working login that leaves nothing to authenticate with is its own outcome.
    /// Read as success it would present as an immediate 401 on the next request, with
    /// nothing on screen to connect the two.
    @Test
    func aLoginThatSetsNoCookieIsNotSignedIn() async throws {
        let client = client(credential: .none) { _ in (200, #"{"ok":true}"#) }
        #expect(try await client.logIn(username: "admin", password: "hunter2")
            == .missingSessionCookie)
    }

    @Test
    func aRefusedLoginIsAnOutcomeRatherThanAnError() async throws {
        #expect(try await client(credential: .none) { _ in (401, "no") }
            .logIn(username: "admin", password: "wrong") == .invalidCredentials)
        #expect(try await client(credential: .none) { _ in (403, "no") }
            .logIn(username: "admin", password: "x") == .noLoginOffered)
        #expect(try await client(credential: .none) { _ in (503, "no") }
            .logIn(username: "admin", password: "x") == .failed(status: 503))
    }

    /// The parser, against the shapes the gateway actually produces — including the
    /// logout, which sets the same cookie to nothing and must not read as a
    /// credential.
    @Test
    func theSessionCookieIsFoundAmongItsAttributes() throws {
        let token = { (header: String?) -> String? in
            let fields = header.map { ["Set-Cookie": $0] } ?? [:]
            let response = HTTPURLResponse(
                url: URL(string: "http://127.0.0.1:1/api/auth/login")!,
                statusCode: 200,
                httpVersion: "HTTP/1.1",
                headerFields: fields
            )!
            return GatewayClient.sessionToken(fromSetCookie: response)
        }
        #expect(token("remotex_session=abc; HttpOnly; SameSite=Strict; Path=/") == "abc")
        #expect(token("remotex_session=abc; HttpOnly; SameSite=Strict; Path=/; Secure") == "abc")
        #expect(token("other=1; remotex_session=abc; Path=/") == "abc")
        // `logout_handler`: the same name, deliberately emptied.
        #expect(token("remotex_session=; HttpOnly; Path=/; Max-Age=0") == nil)
        #expect(token("other=1; Path=/") == nil)
        #expect(token(nil) == nil)
    }

    // MARK: - The upgrade request

    /// Two different tokens meet on this one request and neither may end up where the
    /// other belongs: the claim decides whose turn it is and rides in the query, the
    /// bearer decides whether this client may ask at all and rides in the header.
    @Test
    func theUpgradeRequestCarriesBothTokensInTheirOwnPlaces() throws {
        let client = GatewayClient(
            gateway: .loopback(port: 52675),
            credential: .token("tok-abc"),
            session: stubSession()
        )

        let request = try client.webSocketRequest(sessionToken: "claim-7")
        #expect(request.url?.absoluteString == "ws://127.0.0.1:52675/ws?session=claim-7")
        #expect(
            request.value(forHTTPHeaderField: "Authorization") == "Bearer tok-abc",
            "require_auth runs before the upgrade — see webSocketRequest"
        )
        #expect(
            request.value(forHTTPHeaderField: "Cookie") == nil,
            "a token gateway refuses a cookie outright"
        )
        #expect(request.httpShouldHandleCookies == false)
    }

    /// The same request for a remote gateway, whose credential is a cookie this
    /// client sets by hand.
    ///
    /// This is the request that could never be left to `HTTPCookieStorage`: behind a
    /// TLS-terminating proxy the gateway sets `Secure`, the scheme here is `wss`, and
    /// the cookie would be dropped — for a 401 before the upgrade, with nothing to
    /// explain it.
    @Test
    func theUpgradeRequestCarriesASessionCookieWhenThatIsTheCredential() throws {
        let client = GatewayClient(
            gateway: try GatewayLocation.parse("https://remote.example.com"),
            credential: .session("sess-9"),
            session: stubSession()
        )

        let request = try client.webSocketRequest(sessionToken: "claim-7")
        #expect(request.url?.absoluteString == "wss://remote.example.com/ws?session=claim-7")
        #expect(request.value(forHTTPHeaderField: "Cookie") == "remotex_session=sess-9")
        #expect(request.value(forHTTPHeaderField: "Authorization") == nil)
        #expect(
            request.httpShouldHandleCookies == false,
            "the cookie is set here precisely so URLSession cannot drop it"
        )
    }

    /// The scheme still follows the gateway's, even though this build only ever talks
    /// to loopback: it is `GatewayLocation` that decides, and a `wss` gateway one day
    /// must not silently downgrade.
    @Test
    func anHTTPSGatewayUpgradesOverWSS() throws {
        let client = GatewayClient(
            gateway: try GatewayLocation.parse("https://remote.example.com"),
            credential: .token("tok-abc"),
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

    /// A client against a gateway only this call is using — see
    /// `StubURLProtocol.uniqueHost`. Per call rather than per test, so the several
    /// clients a table-driven test builds cannot answer each other's requests either.
    private func client(
        credential: GatewayCredential = .token("tok-abc"),
        headers: StubURLProtocol.Headers? = nil,
        _ handler: @escaping StubURLProtocol.Handler
    ) -> GatewayClient {
        let host = StubURLProtocol.uniqueHost()
        StubURLProtocol.register(host: host, headers: headers, handler: handler)
        return GatewayClient(
            gateway: try! GatewayLocation.parse("http://\(host)"),
            credential: credential,
            session: stubSession()
        )
    }

    private func stubSession() -> URLSession {
        StubURLProtocol.session()
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
