import Testing
@testable import RemotexViewer

struct GatewayLocationTests {
    @Test
    func normalizesTheGatewayToItsRoot() throws {
        let location = try GatewayLocation.parse("https://example.test:8443/somewhere")
        #expect(location.url.absoluteString == "https://example.test:8443/")
    }

    @Test
    func addsHTTPSWhenTheSchemeIsOmitted() throws {
        let location = try GatewayLocation.parse("gateway.example.test")
        #expect(location.url.absoluteString == "https://gateway.example.test/")
    }

    @Test
    func rejectsCredentialsAndNonHTTPProtocols() {
        #expect(throws: GatewayLocationError.self) {
            try GatewayLocation.parse("https://user:secret@example.test")
        }
        #expect(throws: GatewayLocationError.self) {
            try GatewayLocation.parse("file:///tmp/index.html")
        }
    }

    /// Equality decides whether the gateway *changed*, which tears the session
    /// down and forgets the login cookie. Case alone must not read as a new host.
    @Test
    func aHostIsNormalizedToLowercase() throws {
        let mixed = try GatewayLocation.parse("http://Example.Test:8443")
        #expect(mixed.url.absoluteString == "http://example.test:8443/")
        #expect(mixed == (try GatewayLocation.parse("http://example.test:8443")))
    }

    @Test
    func originUsesDefaultPorts() throws {
        let https = try GatewayLocation.parse("https://example.test")
        let explicit = try GatewayLocation.parse("https://example.test:443")
        #expect(https.origin == explicit.origin)
    }
}
