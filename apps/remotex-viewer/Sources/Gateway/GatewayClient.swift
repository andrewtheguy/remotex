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

/// How a request proves it may be made — the whole of what differs between the two
/// kinds of gateway this app talks to (`GatewayChoice`).
///
/// Both are bearer credentials in the ordinary sense; they differ in which header
/// carries them, and that is decided by which gateway minted them. `require_auth` in
/// src/server.rs matches on the gateway's own `GatewayAuth` and on nothing else: a
/// login gateway reads the cookie and knows nothing about tokens, an embedded one
/// reads the header and refuses a cookie outright. So a credential of the wrong kind
/// is not a weaker credential, it is no credential at all — which is why this is one
/// enum rather than two optional fields that could both be set.
enum GatewayCredential: Sendable, Equatable {
    /// `Authorization: Bearer <token>` — an embedded gateway's, handed over on a
    /// pipe before the first request.
    case token(String)
    /// `Cookie: remotex_session=<token>` — a remote gateway's, minted by
    /// `/api/auth/login`.
    ///
    /// Held here and set by hand rather than left to `HTTPCookieStorage`, which is
    /// what the socket upgrade always had to do anyway: that storage matches a
    /// `Secure` cookie only against an `https` scheme, and behind a TLS-terminating
    /// proxy the gateway does set `Secure` while the socket's scheme is `wss`. Doing
    /// it the same way on every request also keeps this app's logins out of
    /// `HTTPCookieStorage.shared`, which matches by host and **ignores the port** —
    /// two instances against two gateways on one loopback host would otherwise share
    /// one cookie and log each other out.
    case session(String)
    /// A remote gateway that has not been signed in to yet. Requests still go out —
    /// `/api/config` and `/api/auth/status` are public — and the guarded ones 401,
    /// which is the answer the login screen is waiting for.
    case none

    /// The header this credential rides in, if any.
    var header: (field: String, value: String)? {
        switch self {
        case .token(let token):
            ("Authorization", "Bearer \(token)")
        case .session(let token):
            ("Cookie", "\(GatewayClient.cookieName)=\(token)")
        case .none:
            nil
        }
    }
}

/// What a gateway says about this client's standing, before anything is claimed.
enum GatewayAuthState: Sendable, Equatable {
    case authenticated
    case needsLogin
    /// This gateway takes a token and offers no login at all: `/api/auth/*` answers
    /// 403 (`no_login_handler`). Reachable by pointing the app at another copy of
    /// *itself* — an embedded gateway on a port somebody typed — and worth its own
    /// case, because "sign in" is not the fix and a login form would never succeed.
    case noLoginOffered
}

/// The answer to a login attempt.
enum LoginOutcome: Sendable, Equatable {
    /// Signed in, carrying the session token the gateway set as a cookie.
    case ok(session: String)
    case invalidCredentials
    /// A gateway with no login to attempt — see `GatewayAuthState.noLoginOffered`.
    case noLoginOffered
    case failed(status: Int)
    /// It accepted the credentials and set no cookie this client could find. Its own
    /// case because it is a working login that leaves nothing to authenticate with,
    /// which would otherwise present as an immediate 401 on the next request.
    case missingSessionCookie
}

enum GatewayClientError: LocalizedError, Equatable {
    /// URLSession could not complete the request: no such host, nothing listening,
    /// a certificate this Mac will not trust, App Transport Security declining a
    /// plain-HTTP address, a timeout.
    ///
    /// One case for all of them, and the system's own description is the message.
    /// `URLError.localizedDescription` already tells the two apart in the user's
    /// language — "A server with the specified hostname could not be found" versus
    /// "An SSL error has occurred and a secure connection to the server cannot be
    /// made" — so a taxonomy of `URLError.Code`s here would only be a second, worse
    /// copy of it, and one that goes stale as codes are added. The numeric code
    /// comes along because it is what a search engine answers questions about.
    case transport(code: Int, reason: String)
    /// Not the network at all: the request could never be made. Encoding the body,
    /// mainly — kept apart from [`transport`] because calling it a connection
    /// problem is what sends somebody to check DNS for a bug in the client.
    case requestFailed(String)
    /// The gateway address itself cannot be turned into a URL.
    case badAddress(String)
    case incompatible(gateway: Int, viewer: Int)
    case unauthorized
    case unexpectedStatus(Int)
    case malformedResponse

    var errorDescription: String? {
        switch self {
        case .transport(let code, let reason):
            "\(reason) (\(code))"
        case .requestFailed(let reason):
            "The request could not be made: \(reason)"
        case .badAddress(let reason):
            "The gateway address could not be used: \(reason)"
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

    /// Whether waiting and trying again could plausibly change the answer.
    ///
    /// A switch over *this* enum rather than over `URLError.Code`: five cases the
    /// compiler checks, and every one of them is a fact that does not change while
    /// the viewer waits — the address, the gateway's build, the answer it gave. Only
    /// the transport case can pass, and a transport failure that *does* not pass is
    /// caught by the attempt budget instead of by a list of codes (see
    /// [`SessionStateMachine`]).
    var isRetryable: Bool {
        switch self {
        case .transport:
            true
        case .requestFailed, .badAddress, .incompatible, .unauthorized,
            .unexpectedStatus, .malformedResponse:
            false
        }
    }
}

/// The gateway's HTTP surface, plus the session socket it hands out.
///
/// One client for both kinds of gateway, because there is only one route table: the
/// embedded gateway and a remote one answer the same `/api/config`, `/api/targets`,
/// `/api/session` and `/ws` with the same shapes. The single difference is which
/// credential goes on the request, which is [`GatewayCredential`]'s whole job.
///
/// Immutable, and a credential is part of it: signing in produces a *new* client
/// rather than mutating this one, so there is no window in which a request could be
/// made against a half-applied login.
struct GatewayClient: Sendable, SessionGateway {
    /// `COOKIE_NAME` in src/auth.rs.
    static let cookieName = "remotex_session"

    /// The session the app runs on. Built from a configuration of its own rather
    /// than being `URLSession.shared` so a timeout, a delegate, or a proxy stays
    /// something that can be configured here, which the shared session does not
    /// allow.
    ///
    /// Ephemeral, and that is deliberate rather than incidental: this client carries
    /// its own credential on every request (see `GatewayCredential.session`), so
    /// there is nothing for a cookie jar to hold and nothing to share with another
    /// instance of this app.
    static let defaultSession = URLSession(configuration: .ephemeral)

    let gateway: GatewayLocation
    /// What this client authenticates with. See [`GatewayCredential`].
    let credential: GatewayCredential
    private let session: URLSession

    init(
        gateway: GatewayLocation,
        credential: GatewayCredential,
        session: URLSession = GatewayClient.defaultSession
    ) {
        self.gateway = gateway
        self.credential = credential
        self.session = session
    }

    /// The same gateway, spoken to with a different credential — what a login and a
    /// logout each produce.
    func with(credential: GatewayCredential) -> GatewayClient {
        GatewayClient(gateway: gateway, credential: credential, session: session)
    }

    // MARK: - Authentication

    /// Whether this client is signed in, and whether signing in is even offered.
    ///
    /// Public on a login gateway — it is what the login screen asks before showing
    /// itself — so it is safe to call with no credential at all, which is exactly the
    /// state it is called in.
    func authState() async throws -> GatewayAuthState {
        struct Status: Decodable {
            let authenticated: Bool
        }
        let (data, response) = try await perform(request("api/auth/status"))
        switch response.statusCode {
        case 200:
            guard let status = try? JSONDecoder().decode(Status.self, from: data) else {
                throw GatewayClientError.malformedResponse
            }
            return status.authenticated ? .authenticated : .needsLogin
        case 403:
            return .noLoginOffered
        case 401:
            // A login gateway leaves this route public, so this is not a shape the
            // gateway produces. Read as "not signed in" rather than thrown: it is
            // the same situation from the user's side and the login screen is the
            // same answer.
            return .needsLogin
        default:
            throw GatewayClientError.unexpectedStatus(response.statusCode)
        }
    }

    /// Sign in, and keep the session cookie the gateway sets.
    ///
    /// The cookie is read off the response rather than left to `URLSession` — see
    /// `GatewayCredential.session` for why this client holds its own.
    func logIn(username: String, password: String) async throws -> LoginOutcome {
        struct Credentials: Encodable {
            let username: String
            let password: String
        }
        let (_, response) = try await send(
            "api/auth/login",
            body: Credentials(username: username, password: password)
        )
        switch response.statusCode {
        case 200:
            guard let token = Self.sessionToken(fromSetCookie: response) else {
                return .missingSessionCookie
            }
            return .ok(session: token)
        // The gateway deliberately does not say which field was wrong.
        case 401:
            return .invalidCredentials
        case 403:
            return .noLoginOffered
        default:
            return .failed(status: response.statusCode)
        }
    }

    /// Give the login up. Best effort by construction: the caller has already
    /// decided to stop using this credential, so a gateway that cannot be reached to
    /// be told changes nothing on this side.
    ///
    /// Which is exactly why it gets its own short timeout. `changeGateway` awaits
    /// this on the way back to the home screen, and the common reason to be leaving a
    /// gateway is that it stopped answering — so the default timeout would make the
    /// menu item appear to hang for a minute, on the one path whose result is already
    /// decided.
    func logOut() async throws {
        _ = try await send("api/auth/logout", body: Optional<Never>.none, timeout: 5)
    }

    /// Pull `remotex_session` out of a `Set-Cookie` header.
    ///
    /// Written out rather than handed to `HTTPCookie.cookies(withResponseHeaderFields:)`,
    /// which needs a URL and applies domain and `Secure` rules this client does not
    /// want applied: the value is being carried by hand precisely so those rules
    /// cannot drop it. Multiple `Set-Cookie` headers are joined with `", "` by
    /// `HTTPURLResponse`, so the split below is over both separators — the gateway
    /// sets exactly one, and a build that sets more must not silently miss it.
    static func sessionToken(fromSetCookie response: HTTPURLResponse) -> String? {
        guard let header = response.value(forHTTPHeaderField: "Set-Cookie") else {
            return nil
        }
        for pair in header.split(whereSeparator: { $0 == ";" || $0 == "," }) {
            let fields = pair.trimmingCharacters(in: .whitespaces)
                .split(separator: "=", maxSplits: 1)
            guard fields.count == 2, fields[0] == cookieName else {
                continue
            }
            let value = String(fields[1])
            // A logout sets the same cookie to nothing, which is not a credential —
            // see `logout_handler` in src/server.rs.
            guard !value.isEmpty else {
                continue
            }
            return value
        }
        return nil
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
        let (data, response) = try await send(
            "api/session",
            body: Claim(force: force, sessionId: sessionId)
        )
        switch response.statusCode {
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
            throw GatewayClientError.unexpectedStatus(response.statusCode)
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
    /// Two different tokens meet here and they are not interchangeable: the query's
    /// is the *session claim*, which decides whose turn it is, and the credential
    /// header's is this client's, which decides whether it may ask at all.
    /// `require_auth` runs before the upgrade, so a missing header is a bare 401
    /// rather than a socket that closes with a reason.
    ///
    /// `httpShouldHandleCookies` stays off whichever credential this is. The session
    /// cookie is set by hand a few lines down for the reason spelled out on
    /// `GatewayCredential.session` — and behind a TLS proxy this URL's scheme is
    /// `wss`, which is where URLSession's own machinery would drop a `Secure` cookie
    /// and leave nothing to explain the 401.
    func webSocketRequest(sessionToken: String) throws -> URLRequest {
        var components = try socketComponents()
        components.queryItems = [URLQueryItem(name: "session", value: sessionToken)]
        guard let url = components.url else {
            throw GatewayClientError.badAddress("could not build the session URL")
        }
        var request = URLRequest(url: url)
        request.timeoutInterval = 15
        request.httpShouldHandleCookies = false
        if let header = credential.header {
            request.setValue(header.value, forHTTPHeaderField: header.field)
        }
        return request
    }

    private func socketComponents() throws -> URLComponents {
        guard var components = URLComponents(url: gateway.url, resolvingAgainstBaseURL: false) else {
            throw GatewayClientError.badAddress("could not read the gateway address")
        }
        // ATS treats ws/wss exactly as it treats http/https, so a plain-HTTP
        // gateway needs NSAllowsArbitraryLoads either way.
        components.scheme = components.scheme == "https" ? "wss" : "ws"
        components.path = "/ws"
        return components
    }

    // MARK: - Plumbing

    private func get<Response: Decodable>(_ path: String) async throws -> Response {
        let (data, response) = try await perform(request(path))
        switch response.statusCode {
        case 200:
            guard let decoded = try? JSONDecoder().decode(Response.self, from: data) else {
                throw GatewayClientError.malformedResponse
            }
            return decoded
        case 401:
            throw GatewayClientError.unauthorized
        default:
            throw GatewayClientError.unexpectedStatus(response.statusCode)
        }
    }

    private func send(
        _ path: String,
        body: (some Encodable)?,
        timeout: TimeInterval? = nil
    ) async throws -> (Data, HTTPURLResponse) {
        var request = self.request(path)
        request.httpMethod = "POST"
        if let timeout {
            request.timeoutInterval = timeout
        }
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(body)
        }
        return try await perform(request)
    }

    /// A request to `path` carrying this client's credential.
    ///
    /// Every request goes through here, which is the point: the credential is not
    /// optional on any route — even `/api/config`, which is public — so there is no
    /// path where forgetting it is possible. The one credential that adds no header
    /// is `.none`, which is a remote gateway before its login and is the state the
    /// public routes are asked in.
    private func request(_ path: String) -> URLRequest {
        var request = URLRequest(url: url(path))
        request.httpShouldHandleCookies = false
        if let header = credential.header {
            request.setValue(header.value, forHTTPHeaderField: header.field)
        }
        return request
    }

    private func perform(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        do {
            let (data, response) = try await session.data(for: request)
            guard let http = response as? HTTPURLResponse else {
                throw GatewayClientError.malformedResponse
            }
            return (data, http)
        } catch let error as GatewayClientError {
            throw error
        } catch let error as URLError {
            // Only URLSession's own failures are about the connection, and its own
            // description of one is better than anything this could write.
            throw GatewayClientError.transport(
                code: error.errorCode,
                reason: error.localizedDescription
            )
        } catch {
            // Encoding the body, or anything else that never reached the network.
            // Reported as itself: calling this unreachable is what sent people to
            // check DNS for a bug in the request.
            throw GatewayClientError.requestFailed(error.localizedDescription)
        }
    }

    private func url(_ path: String) -> URL {
        gateway.url.appending(path: path)
    }
}
