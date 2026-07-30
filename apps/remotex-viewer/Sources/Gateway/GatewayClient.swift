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
/// Authentication is one bearer token, minted by the gateway this app started and
/// handed over on a pipe before the first request (see `EmbeddedGateway`). So there
/// is no login to perform, no cookie to keep, and nothing about this client that
/// outlives the process it talks to: the token is a fact about *that* gateway, and a
/// new gateway means a new client.
struct GatewayClient: Sendable, SessionGateway {
    /// The session the app runs on. Built from a configuration of its own rather
    /// than being `URLSession.shared` so a timeout, a delegate, or a proxy stays
    /// something that can be configured here, which the shared session does not
    /// allow.
    static let defaultSession = URLSession(configuration: .ephemeral)

    let gateway: GatewayLocation
    /// The gateway's token, sent on every request and on the socket upgrade.
    private let token: String
    private let session: URLSession

    init(
        gateway: GatewayLocation,
        token: String,
        session: URLSession = GatewayClient.defaultSession
    ) {
        self.gateway = gateway
        self.token = token
        self.session = session
    }

    /// `Bearer <token>`, the one credential this client has.
    private var authorization: String {
        "Bearer \(token)"
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
    /// Two different tokens meet here and they are not interchangeable: the query's
    /// is the *session claim*, which decides whose turn it is, and the
    /// `Authorization` header's is this client's credential, which decides whether it
    /// may ask at all. `require_auth` runs before the upgrade, so a missing header is
    /// a bare 401 rather than a socket that closes with a reason.
    ///
    /// `httpShouldHandleCookies` stays off: there are no cookies in this arrangement,
    /// and leaving URLSession's cookie machinery out of a request that carries its own
    /// credential is one less thing that can decide to add or drop a header.
    func webSocketRequest(sessionToken: String) throws -> URLRequest {
        var components = try socketComponents()
        components.queryItems = [URLQueryItem(name: "session", value: sessionToken)]
        guard let url = components.url else {
            throw GatewayClientError.badAddress("could not build the session URL")
        }
        var request = URLRequest(url: url)
        request.timeoutInterval = 15
        request.httpShouldHandleCookies = false
        request.setValue(authorization, forHTTPHeaderField: "Authorization")
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
        let (data, status) = try await perform(request(path))
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
        var request = self.request(path)
        request.httpMethod = "POST"
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(body)
        }
        return try await perform(request)
    }

    /// A request to `path` carrying this client's credential.
    ///
    /// Every request goes through here, which is the point: the token is not optional
    /// on any route — even `/api/config`, which is public — so there is no path where
    /// forgetting it is possible.
    private func request(_ path: String) -> URLRequest {
        var request = URLRequest(url: url(path))
        request.httpShouldHandleCookies = false
        request.setValue(authorization, forHTTPHeaderField: "Authorization")
        return request
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
