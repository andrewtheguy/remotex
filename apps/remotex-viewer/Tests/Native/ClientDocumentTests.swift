import Foundation
import Testing

@testable import RemotexViewer

/// The two things that changed when the client moved into the bundle: which
/// navigations the window allows, and how the page is told where its gateway is.
@MainActor
struct ClientDocumentTests {
    let index = URL(fileURLWithPath: "/Apps/remotex.app/Contents/Resources/web/index.html")

    /// The client itself and its own assets, and nothing else on the disk.
    ///
    /// This is the assertion the old rule could not make. It compared scheme, host
    /// and port — and a `file://` URL has no host and no port, so every file URL
    /// matched every other one and any path on the machine could have replaced the
    /// page. A remote desktop is somebody else's pixels and somebody else's
    /// clipboard strings; neither may pick what this window shows.
    @Test
    func theClientMayLoadItselfAndItsOwnAssets() {
        for allowed in [
            "/Apps/remotex.app/Contents/Resources/web/index.html",
            "/Apps/remotex.app/Contents/Resources/web/assets/index-abc123.js",
            "/Apps/remotex.app/Contents/Resources/web/assets/index-abc123.css",
        ] {
            #expect(
                NativeBridge.permits(URL(fileURLWithPath: allowed), from: index),
                "\(allowed) is the client or part of it"
            )
        }
    }

    @Test
    func nothingElseOnTheDiskIsAllowed() {
        for refused in [
            "/etc/passwd",
            "/Apps/remotex.app/Contents/Resources/other/index.html",
            // The parent of the web root: a prefix match on the *string* would let
            // this through, which is why the check appends a separator.
            "/Apps/remotex.app/Contents/Resources/webhook/index.html",
            "/Users/andrew/.ssh/id_ed25519",
        ] {
            #expect(
                !NativeBridge.permits(URL(fileURLWithPath: refused), from: index),
                "\(refused) is not the client"
            )
        }
    }

    /// Traversal is resolved before the comparison, not compared as written.
    @Test
    func aPathThatClimbsOutIsRefused() {
        let climbing = URL(
            fileURLWithPath: "/Apps/remotex.app/Contents/Resources/web/../../../../../etc/passwd"
        )
        #expect(!NativeBridge.permits(climbing, from: index))
    }

    /// Another scheme is another thing entirely — including the `http://` the page
    /// talks to its gateway with. That traffic is `fetch` and `WebSocket`, never a
    /// navigation, so nothing legitimate is lost by refusing it here.
    @Test
    func anotherSchemeIsRefused() {
        for refused in ["http://127.0.0.1:49213/", "https://example.test/", "javascript:alert(1)"] {
            #expect(!NativeBridge.permits(URL(string: refused), from: index), "\(refused)")
        }
        #expect(!NativeBridge.permits(nil, from: index))
    }

    /// The page is handed an origin and nothing else — no token, no config, no
    /// session — and it is handed it as *data*: encoded, never interpolated, for
    /// the reason every command on this bridge is.
    @Test
    func theGatewayScriptEncodesTheOriginRatherThanInterpolatingIt() throws {
        let endpoint = GatewayEndpoint(port: 49_213, token: "unused-here")
        let script = RemoteWebHost.Coordinator.gatewayScript(for: endpoint)

        #expect(script.contains("window.__remotexGateway"))
        #expect(!script.contains("unused-here"), "the token is a cookie, not a global")

        // Decoded rather than string-matched, because the encoder is free to escape
        // — it writes `http:\/\/…`, which is valid JSON and the same string once
        // parsed. Asserting on the literal text would be asserting on the escaping.
        let literal = try #require(
            script
                .split(separator: "=", maxSplits: 1).last?
                .trimmingCharacters(in: .whitespaces)
                .replacingOccurrences(of: "[0];", with: "")
        )
        let decoded = try JSONSerialization.jsonObject(
            with: Data(literal.utf8)
        ) as? [String]
        #expect(
            decoded == ["http://127.0.0.1:49213"],
            "no trailing slash: the client appends paths to this. Got \(script)"
        )
    }
}
