import Foundation
import Testing
@testable import RemotexViewer

struct ProductInfoTests {
    /// There is no version to pin any more — the bundle's is the workspace's, put
    /// there by `build-viewer-app.sh` — but a build that is *not* bundled has to
    /// say so rather than name a release it is not.
    @Test
    func anUnbundledBuildClaimsNoRelease() {
        // These tests run unbundled, so this is that path.
        #expect(ProductInfo.version == "0.0.0-unbundled")
    }

    /// The viewer ships separately from the gateway, so the only thing keeping
    /// them on the same wire protocol is this constant matching the Rust one it
    /// is compared against at runtime. Read out of the source rather than
    /// duplicated, so a bump on either side fails here instead of in the field.
    @Test
    func protocolVersionMatchesTheGateway() throws {
        let source = try String(
            contentsOf: repositoryRoot.appending(path: "src/protocol.rs"),
            encoding: .utf8
        )
        let version = try firstCapture(
            #"PROTOCOL_VERSION: u32 = ([0-9]+)"#,
            in: source
        )
        #expect(String(ProductInfo.protocolVersion) == version)
    }

    private var repositoryRoot: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
    }

    private func firstCapture(_ pattern: String, in value: String) throws -> String {
        let expression = try NSRegularExpression(pattern: pattern)
        let range = NSRange(value.startIndex..., in: value)
        guard let match = expression.firstMatch(in: value, range: range),
              let capture = Range(match.range(at: 1), in: value)
        else {
            throw ContractTestError.noMatch(pattern)
        }
        return String(value[capture])
    }
}

private enum ContractTestError: Error {
    case noMatch(String)
}
