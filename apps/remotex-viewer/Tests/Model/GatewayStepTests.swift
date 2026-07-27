import AppKit
import Foundation
import Testing
@testable import RemotexViewer

/// The server step: what Continue does, and what it refuses to do.
///
/// Serialized because `StubURLProtocol` routes through static state.
@MainActor
@Suite(.serialized)
struct GatewayStepTests {
    @Test
    func aViewerStartsOnTheServerStepWithoutContactingAnything() {
        let asked = RequestCounter()
        let model = makeModel { request in
            if request.url?.host() == "127.0.0.1" {
                asked.record()
            }
            return (200, "{}")
        }
        #expect(model.session.screen == .server)
        #expect(asked.count == 0, "nothing is probed until Continue is pressed")
    }

    @Test
    func aReachableGatewayAdvancesToTheLoginStep() async {
        let model = makeModel { request in
            switch request.url?.path {
            case "/api/config":
                (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
            case "/api/auth/status":
                (200, #"{"authenticated":false}"#)
            default:
                (404, "{}")
            }
        }

        await model.connectToGateway()

        #expect(model.session.screen == .login)
        #expect(model.branding == "acme")
        #expect(model.gatewayError == nil)
    }

    /// The cookie outlives the app, so a still-valid login skips the credentials
    /// entirely rather than asking for them again.
    @Test
    func anAlreadyAuthenticatedGatewaySkipsTheLoginStep() async {
        let model = makeModel { request in
            switch request.url?.path {
            case "/api/config":
                (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
            case "/api/auth/status":
                (200, #"{"authenticated":true}"#)
            case "/api/session":
                (200, #"{"sessionId":"tok-1"}"#)
            default:
                (404, "{}")
            }
        }

        await model.connectToGateway()

        #expect(model.session.screen == .picker)
        #expect(model.session.connectionStatus != nil)
        // The session's reconnect loop would otherwise outlive this test and land
        // on the next one's stub, since the handler is static. Log out rather than
        // change the gateway: from the picker that is the only way out now.
        await model.logOut()
    }

    @Test
    func aMalformedAddressIsRefusedWithoutAnyRequest() async {
        let asked = RequestCounter()
        let model = makeModel { request in
            // Counted by host, not in total: another test's session may still be
            // retrying against the same static handler.
            if request.url?.host() == "nope" {
                asked.record()
            }
            return (200, "{}")
        }
        model.gatewayAddress = "ftp://nope"

        await model.connectToGateway()

        #expect(model.session.screen == .server)
        #expect(model.gatewayError != nil)
        #expect(asked.count == 0, "a bad address is refused before the network")
    }

    @Test
    func anUnreachableGatewayStaysOnTheServerStep() async {
        let model = makeModel { _ in nil }

        await model.connectToGateway()

        #expect(model.session.screen == .server)
        #expect(model.gatewayError != nil)
    }

    /// The whole reason the field is served: a gateway on another wire revision
    /// is refused where it can be explained, not at the first unreadable frame.
    @Test
    func aProtocolVersionMismatchIsRefusedBeforeAnyCredentials() async {
        let model = makeModel { request in
            request.url?.path == "/api/config"
                ? (200, #"{"branding":"acme","protocolVersion":9999}"#)
                : (200, #"{"authenticated":true}"#)
        }

        await model.connectToGateway()

        #expect(model.session.screen == .server)
        #expect(model.gatewayError?.contains("9999") == true)
    }

    /// A typo must not become the address the next launch starts from.
    @Test
    func anAddressIsRememberedOnlyOnceItAnswers() async {
        let suiteName = "GatewayStepTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let failing = makeModel(defaults: defaults) { _ in nil }
        failing.gatewayAddress = "http://typo.invalid:1"
        await failing.connectToGateway()
        #expect(defaults.string(forKey: "gatewayAddress") == nil)

        let working = makeModel(defaults: defaults) { request in
            request.url?.path == "/api/config"
                ? (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
                : (200, #"{"authenticated":false}"#)
        }
        working.gatewayAddress = "http://127.0.0.1:52675"
        await working.connectToGateway()
        #expect(defaults.string(forKey: "gatewayAddress") == "http://127.0.0.1:52675/")
    }

    /// A bare host is a reasonable thing to type, and the normalized form is what
    /// gets shown and remembered.
    @Test
    func aBareHostIsNormalized() async {
        let model = makeModel { request in
            request.url?.path == "/api/config"
                ? (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
                : (200, #"{"authenticated":false}"#)
        }
        model.gatewayAddress = "remotex.example.com"

        await model.connectToGateway()

        #expect(model.gatewayAddress == "https://remotex.example.com/")
    }

    /// Signing in no longer validates anything about the gateway, so a wrong
    /// password is reported as a wrong password and the address is left alone.
    @Test
    func signingInReportsOnlyCredentialProblems() async {
        let model = makeModel { request in
            switch request.url?.path {
            case "/api/config":
                (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
            case "/api/auth/status":
                (200, #"{"authenticated":false}"#)
            case "/api/auth/login":
                (401, "unauthorized")
            default:
                (404, "{}")
            }
        }
        await model.connectToGateway()

        await model.logIn(username: "admin", password: "wrong")

        #expect(model.session.screen == .login, "still on the credentials step")
        #expect(model.loginError == "Invalid credentials")
        #expect(model.gatewayError == nil, "the gateway was never in question")
    }

    @Test
    func changingTheGatewayReturnsToTheServerStep() async {
        let model = makeModel { request in
            request.url?.path == "/api/config"
                ? (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
                : (200, #"{"authenticated":false}"#)
        }
        await model.connectToGateway()
        #expect(model.session.screen == .login)

        await model.changeGateway()

        #expect(model.session.screen == .server)
        #expect(model.loginError == nil)
    }

    /// Once signed in the address is not a thing to change: the cookie, the claim
    /// and the socket all belong to that gateway, so moving it is a log out
    /// wearing another name. The link that calls this is on the credentials step
    /// only, and the model refuses from anywhere else anyway.
    @Test
    func theGatewayCannotBeChangedWhileSignedIn() async {
        let model = makeModel { request in
            switch request.url?.path {
            case "/api/config":
                (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
            case "/api/auth/status":
                (200, #"{"authenticated":true}"#)
            case "/api/session":
                (200, #"{"sessionId":"tok-1"}"#)
            default:
                (404, "{}")
            }
        }
        await model.connectToGateway()
        let address = model.gatewayAddress
        #expect(model.session.screen == .picker)

        await model.changeGateway()
        #expect(model.session.screen == .picker, "the picker kept its gateway")

        // And on a live desktop, which is the same rule one screen further in.
        model.apply(
            .control(
                .connected(
                    ServerMessage.Connected(
                        name: "t",
                        protocolName: "vnc",
                        resize: false,
                        clipboard: false
                    )
                )
            )
        )
        await model.changeGateway()
        #expect(model.session.screen == .desktop, "the desktop kept its gateway")
        #expect(model.gatewayAddress == address)

        // Signing out is the way back to where it can be changed.
        await model.logOut()
        #expect(model.session.screen == .login)
        await model.changeGateway()
        #expect(model.session.screen == .server)
    }

    /// Logging out gives up the credentials, not the address, so it stops at the
    /// login step rather than making the user re-enter the server.
    @Test
    func loggingOutStaysOnTheValidatedGateway() async {
        let model = makeModel { request in
            switch request.url?.path {
            case "/api/config":
                (200, #"{"branding":"acme","protocolVersion":\#(ProductInfo.protocolVersion)}"#)
            case "/api/auth/status":
                (200, #"{"authenticated":false}"#)
            default:
                (200, #"{"ok":true}"#)
            }
        }
        await model.connectToGateway()

        await model.logOut()

        #expect(model.session.screen == .login)
    }

    /// Counts stub requests from whatever thread URLSession calls on.
    private final class RequestCounter: @unchecked Sendable {
        private let lock = NSLock()
        private var seen = 0

        func record() {
            lock.withLock { seen += 1 }
        }

        var count: Int {
            lock.withLock { seen }
        }
    }

    private func makeModel(
        defaults: UserDefaults? = nil,
        _ handler: @escaping @Sendable (URLRequest) -> (Int, String)?
    ) -> AppModel {
        StubURLProtocol.handler = handler
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubURLProtocol.self]
        let suiteName = "GatewayStepTests.\(UUID().uuidString)"
        return AppModel(
            defaults: defaults ?? UserDefaults(suiteName: suiteName)!,
            clipboard: ClipboardSynchronizer(
                pasteboard: NSPasteboard.withUniqueName(),
                startsPolling: false
            ),
            urlSession: URLSession(configuration: configuration)
        )
    }
}
