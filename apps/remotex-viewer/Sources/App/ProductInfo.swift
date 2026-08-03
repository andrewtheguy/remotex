import Foundation

enum ProductInfo {
    /// The wire protocol revision this build speaks, checked against the
    /// `protocolVersion` in `GET /api/config`. Mirrors `PROTOCOL_VERSION` in
    /// `src/protocol.rs`; `ProductInfoTests` pins the two together.
    static let protocolVersion = 10

    /// The workspace version, and only ever out of the bundle:
    /// `build-viewer-app.sh` puts `workspace.package.version` from Cargo.toml into
    /// `CFBundleShortVersionString`, which makes this the same single source
    /// `env!("CARGO_PKG_VERSION")` gives the agent — with no second copy to keep in
    /// step with a bump.
    ///
    /// An unbundled build has no `Info.plist`, and so no version: it says as much
    /// rather than carrying that copy. Nothing is validated against an unbundled
    /// viewer anyway — CLAUDE.md rules out `swift run` and the executable under
    /// `.build` for this sort of reason — and what a gateway is checked against is
    /// `protocolVersion`, not this.
    static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? "0.0.0-unbundled"
    }
}
