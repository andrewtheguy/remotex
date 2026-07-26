import Foundation

/// Public, pre-login gateway facts from `GET /api/config`.
struct GatewayConfig: Sendable, Equatable, Decodable {
    let branding: String
    /// `PROTOCOL_VERSION` in src/protocol.rs. Checked against
    /// `ProductInfo.protocolVersion` before a session is opened, because the
    /// viewer ships separately from the gateway and can therefore be older.
    let protocolVersion: Int
}

/// One `[[targets]]` profile as the picker sees it. Credentials never leave the
/// gateway, so this is the whole of it.
struct TargetInfo: Sendable, Equatable, Decodable, Identifiable {
    let name: String
    let protocolName: String
    let host: String
    let port: UInt16

    var id: String { name }
    /// The picker's second line, matching `frontend/src/TargetPicker.tsx`.
    var detail: String { "\(protocolName.uppercased()) · \(host):\(port)" }

    private enum CodingKeys: String, CodingKey {
        case name
        case protocolName = "protocol"
        case host
        case port
    }
}

/// The answer to a claim on the single session slot. 409 and 401 are UI states,
/// not failures, so they are values rather than thrown errors.
enum ClaimOutcome: Sendable, Equatable {
    case claimed(String)
    case busy
    case unauthorized
}

enum LoginOutcome: Sendable, Equatable {
    case ok
    case invalidCredentials
    case failed(status: Int)
}

enum GatewayClientError: LocalizedError, Equatable {
    case unreachable(String)
    case incompatible(gateway: Int, viewer: Int)
    case unauthorized
    case unexpectedStatus(Int)
    case malformedResponse

    var errorDescription: String? {
        switch self {
        case .unreachable(let reason):
            "Could not reach the gateway: \(reason)"
        case .incompatible(let gateway, let viewer):
            """
            This gateway speaks protocol \(gateway) and this viewer speaks \
            \(viewer). Install the matching viewer.
            """
        case .unauthorized:
            "The gateway rejected the session. Sign in again."
        case .unexpectedStatus(let status):
            "The gateway answered unexpectedly (\(status))."
        case .malformedResponse:
            "The gateway's answer could not be read."
        }
    }
}

/// The gateway's HTTP surface, plus the session socket it hands out.
///
/// One `URLSession` for the whole client so the login cookie is shared across
/// every call. Built from `.default`, whose cookie storage is
/// `HTTPCookieStorage.shared` and therefore persists across launches — which is
/// what `websiteDataStore = .default()` bought while this was a WKWebView, and
/// why quitting the app still lands you on the picker.
struct GatewayClient: Sendable, SessionGateway {
    /// `COOKIE_NAME` in src/auth.rs.
    static let cookieName = "remotex_session"

    let gateway: GatewayLocation
    private let session: URLSession

    init(gateway: GatewayLocation, session: URLSession = .shared) {
        self.gateway = gateway
        self.session = session
    }

    // MARK: - HTTP

    /// Branding and the protocol check. Refuses a gateway this build cannot
    /// speak to, rather than failing later in a way that looks like a bad
    /// address.
    func configuration() async throws -> GatewayConfig {
        let config: GatewayConfig = try await get("api/config")
        guard config.protocolVersion == ProductInfo.protocolVersion else {
            throw GatewayClientError.incompatible(
                gateway: config.protocolVersion,
                viewer: ProductInfo.protocolVersion
            )
        }
        return config
    }

    func isAuthenticated() async throws -> Bool {
        struct Status: Decodable {
            let authenticated: Bool
        }
        let status: Status = try await get("api/auth/status")
        return status.authenticated
    }

    func logIn(username: String, password: String) async throws -> LoginOutcome {
        struct Credentials: Encodable {
            let username: String
            let password: String
        }
        let (_, status) = try await send(
            "api/auth/login",
            body: Credentials(username: username, password: password)
        )
        switch status {
        case 200:
            return .ok
        // The gateway deliberately does not say which field was wrong.
        case 401:
            return .invalidCredentials
        default:
            return .failed(status: status)
        }
    }

    func logOut() async throws {
        _ = try await send("api/auth/logout", body: Optional<Never>.none)
    }

    func targets() async throws -> [TargetInfo] {
        try await get("api/targets")
    }

    /// Claim the single session slot. `sessionId` is this process's previous
    /// token: presenting it reclaims the same slot after a drop without the
    /// takeover prompt. `force` evicts whoever holds it.
    func claimSession(force: Bool, sessionId: String?) async throws -> ClaimOutcome {
        struct Claim: Encodable {
            let force: Bool
            let sessionId: String?
        }
        struct Claimed: Decodable {
            let sessionId: String
        }
        let (data, status) = try await send(
            "api/session",
            body: Claim(force: force, sessionId: sessionId)
        )
        switch status {
        case 200:
            guard let claimed = try? JSONDecoder().decode(Claimed.self, from: data) else {
                throw GatewayClientError.malformedResponse
            }
            return .claimed(claimed.sessionId)
        case 409:
            return .busy
        case 401:
            return .unauthorized
        default:
            throw GatewayClientError.unexpectedStatus(status)
        }
    }

    // MARK: - WebSocket

    func openSocket(sessionToken: String) async throws -> any WebSocketTransport {
        URLSessionWebSocketTransport(
            task: session.webSocketTask(with: try webSocketRequest(sessionToken: sessionToken))
        )
    }

    /// The upgrade request for `/ws?session=<token>`.
    ///
    /// The `Cookie` header is set by hand, with `httpShouldHandleCookies` off, for
    /// two reasons. `HTTPCookieStorage` matches a `Secure` cookie only against an
    /// `https` scheme — and behind a TLS-terminating proxy the gateway does set
    /// `Secure` (`cookie_flags` in src/server.rs) while this URL's scheme is
    /// `wss`, so the cookie would be dropped. And `require_auth` runs *before* the
    /// upgrade, so the result of getting it wrong is a bare 401 with nothing to
    /// go on. This is what `tests/common/mod.rs` does too.
    func webSocketRequest(sessionToken: String) throws -> URLRequest {
        var components = try socketComponents()
        components.queryItems = [URLQueryItem(name: "session", value: sessionToken)]
        guard let url = components.url else {
            throw GatewayClientError.unreachable("could not build the session URL")
        }
        var request = URLRequest(url: url)
        request.timeoutInterval = 15
        request.httpShouldHandleCookies = false
        if let cookie = sessionCookieHeader() {
            request.setValue(cookie, forHTTPHeaderField: "Cookie")
        }
        return request
    }

    /// `name=value` for the login cookie, read against the *HTTP* origin the
    /// cookie was set on.
    func sessionCookieHeader() -> String? {
        guard let storage = session.configuration.httpCookieStorage,
              let cookies = storage.cookies(for: gateway.url)
        else {
            return nil
        }
        guard let cookie = cookies.first(where: { $0.name == Self.cookieName }) else {
            return nil
        }
        return "\(cookie.name)=\(cookie.value)"
    }

    /// Forget the login for this gateway. Called when the address changes: a
    /// leftover token for another host is a 401 with no obvious cause.
    func forgetSessionCookie() {
        guard let storage = session.configuration.httpCookieStorage,
              let cookies = storage.cookies(for: gateway.url)
        else {
            return
        }
        for cookie in cookies where cookie.name == Self.cookieName {
            storage.deleteCookie(cookie)
        }
    }

    private func socketComponents() throws -> URLComponents {
        guard var components = URLComponents(url: gateway.url, resolvingAgainstBaseURL: false) else {
            throw GatewayClientError.unreachable("could not read the gateway address")
        }
        // ATS treats ws/wss exactly as it treats http/https, so a plain-HTTP
        // gateway needs NSAllowsArbitraryLoads either way.
        components.scheme = components.scheme == "https" ? "wss" : "ws"
        components.path = "/ws"
        return components
    }

    // MARK: - Plumbing

    private func get<Response: Decodable>(_ path: String) async throws -> Response {
        let (data, status) = try await perform(URLRequest(url: url(path)))
        switch status {
        case 200:
            guard let decoded = try? JSONDecoder().decode(Response.self, from: data) else {
                throw GatewayClientError.malformedResponse
            }
            return decoded
        case 401:
            throw GatewayClientError.unauthorized
        default:
            throw GatewayClientError.unexpectedStatus(status)
        }
    }

    private func send(_ path: String, body: (some Encodable)?) async throws -> (Data, Int) {
        var request = URLRequest(url: url(path))
        request.httpMethod = "POST"
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(body)
        }
        return try await perform(request)
    }

    private func perform(_ request: URLRequest) async throws -> (Data, Int) {
        do {
            let (data, response) = try await session.data(for: request)
            guard let http = response as? HTTPURLResponse else {
                throw GatewayClientError.malformedResponse
            }
            return (data, http.statusCode)
        } catch let error as GatewayClientError {
            throw error
        } catch {
            throw GatewayClientError.unreachable(error.localizedDescription)
        }
    }

    private func url(_ path: String) -> URL {
        gateway.url.appending(path: path)
    }
}
