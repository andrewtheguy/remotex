import Foundation
import Testing
@testable import RemotexViewer

/// The failure this catches: the gateway grows a message, nobody touches the
/// viewer, and the drift shows up at runtime as a `.unsupported` frame silently
/// stepped over — or as an input event the gateway logs and drops.
///
/// Both enums are read out of the Rust source instead of being listed twice,
/// following `ProductInfoTests`, which already pins constants that way.
struct WireContractTests {
    @Test
    func everyClientMsgVariantHasATagThisBuildSends() throws {
        let source = try protocolSource()
        #expect(try tags(ofEnum: "ClientMsg", in: source) == ClientMessage.allTags)
    }

    @Test
    func everyControlMsgVariantHasATagThisBuildUnderstands() throws {
        let source = try protocolSource()
        #expect(try tags(ofEnum: "ControlMsg", in: source) == ServerMessage.allTags)
    }

    private func protocolSource() throws -> String {
        let root = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        return try String(
            contentsOf: root.appending(path: "src/protocol.rs"),
            encoding: .utf8
        )
    }

    /// The serde tags of a `#[serde(tag = "type", rename_all = "camelCase")]`
    /// enum: its variant names with the first letter lowercased.
    ///
    /// Variants are the lines indented exactly four spaces that start with a
    /// capital, which skips doc comments, `#[serde(…)]` attributes, and the
    /// struct-variant fields indented eight.
    private func tags(ofEnum name: String, in source: String) throws -> Set<String> {
        guard let declaration = source.range(of: "enum \(name)"),
              let open = source.range(of: "{", range: declaration.upperBound ..< source.endIndex)
        else {
            throw ContractError.enumNotFound(name)
        }
        let body = source[open.upperBound...]
        var tags = Set<String>()
        for line in body.split(separator: "\n", omittingEmptySubsequences: false) {
            if line == "}" {
                return tags
            }
            guard line.hasPrefix("    "),
                  let first = line.dropFirst(4).first,
                  first.isUppercase
            else {
                continue
            }
            let variant = line.dropFirst(4).prefix { $0.isLetter || $0.isNumber }
            tags.insert(variant.prefix(1).lowercased() + variant.dropFirst())
        }
        throw ContractError.unterminated(name)
    }
}

private enum ContractError: Error {
    case enumNotFound(String)
    case unterminated(String)
}
