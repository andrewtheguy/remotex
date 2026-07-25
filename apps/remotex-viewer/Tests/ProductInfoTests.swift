import Foundation
import Testing
@testable import RemotexViewer

struct ProductInfoTests {
    @Test
    func developmentVersionMatchesTheWorkspace() throws {
        let cargo = try String(
            contentsOf: repositoryRoot.appending(path: "Cargo.toml"),
            encoding: .utf8
        )
        let version = try firstCapture(
            #"(?m)^version = "([^"]+)"$"#,
            in: cargo
        )
        #expect(ProductInfo.developmentVersion == version)
    }

    @Test
    func bridgeVersionMatchesTheFrontend() throws {
        let source = try String(
            contentsOf: repositoryRoot.appending(path: "frontend/src/nativeHost.ts"),
            encoding: .utf8
        )
        let version = try firstCapture(
            #"NATIVE_HOST_BRIDGE_VERSION = ([0-9]+)"#,
            in: source
        )
        #expect(String(ProductInfo.bridgeVersion) == version)
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
