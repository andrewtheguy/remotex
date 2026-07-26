import Foundation

enum ProductInfo {
    /// The wire protocol revision this build speaks, checked against the
    /// `protocolVersion` in `GET /api/config`. Mirrors `PROTOCOL_VERSION` in
    /// `src/protocol.rs`; `ProductInfoTests` pins the two together.
    static let protocolVersion = 1
    /// The fallback for an unbundled build, which has no `Info.plist` to read a
    /// version out of. `build-viewer-app.sh` refuses to package a mismatch, so
    /// `--version` stays honest.
    static let developmentVersion = "0.0.30"

    static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? developmentVersion
    }
}
