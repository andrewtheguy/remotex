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

    @Test
    func originUsesDefaultPorts() throws {
        let https = try GatewayLocation.parse("https://example.test")
        let explicit = try GatewayLocation.parse("https://example.test:443")
        #expect(https.origin == explicit.origin)
    }
}
