import Foundation

enum ProductInfo {
    /// The workspace version, and only ever out of the bundle:
    /// `build-viewer-app.sh` puts `workspace.package.version` from Cargo.toml into
    /// `CFBundleShortVersionString`, which makes this the same single source
    /// `env!("CARGO_PKG_VERSION")` gives the agent — with no second copy to keep in
    /// step with a bump.
    ///
    /// An unbundled build has no `Info.plist`, and so no version: it says as much
    /// rather than carrying that copy. Nothing is validated against an unbundled
    /// viewer anyway — CLAUDE.md rules out `swift run` and the executable under
    /// `.build` for this sort of reason.
    ///
    /// There is no wire-protocol version here any more. This app speaks no wire
    /// protocol: the client it shows and the gateway it runs are built together and
    /// shipped in one bundle, so there is no pair of versions that could disagree.
    static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? "0.0.0-unbundled"
    }
}
