import CryptoKit
import Foundation

/// Everything this launch reads or writes, in one directory.
///
/// `remotex.app` carries its own gateway, so it also carries that gateway's whole
/// installation: the config somebody edits, the log it writes, and the handful of
/// preferences the client keeps. One directory rather than several homes — a
/// `UserDefaults` suite here, a cookie jar there, a config file somewhere else —
/// because a single unit of isolation is the only kind that can be swapped whole.
/// `--instance-dir` is exactly that swap, and it is what a QA run uses instead of
/// the settings-suite juggling this replaced.
///
/// **Nothing under `/opt/remotex` is ever consulted.** A Mac may run the server
/// install and this app at once, and neither can change what the other does: the
/// installed gateway finds its config from the location of its own executable,
/// while this one is told where to look.
struct InstanceDirectory: Equatable, Sendable {
    /// The directory itself. Not guaranteed to exist until [`create`] has run.
    let url: URL

    /// The flag that names a directory other than the default one.
    static let flag = "--instance-dir"

    /// `~/Library/Application Support/<this bundle's name>`.
    ///
    /// Derived from the bundle rather than hardcoded, and that is the whole of what
    /// makes a second *bundle* a second installation: LaunchServices hands a
    /// double-clicked app no arguments at all, so anything an instance depends on has
    /// to come from the bundle itself. A variant stamped out by
    /// `packaging/macos-viewer/make-instance-bundle.sh` therefore needs no launcher and
    /// no flag — it is its own app, with its own name, icon and instance.
    ///
    /// The shipped bundle is named `remotex`, so this is exactly where it always was.
    static var defaultURL: URL {
        URL.applicationSupportDirectory.appending(
            path: instanceName(ofBundleNamed: Bundle.main.object(forInfoDictionaryKey: "CFBundleName") as? String),
            directoryHint: .isDirectory
        )
    }

    /// Which WebKit data store this instance's page gets
    /// (`WKWebsiteDataStore(forIdentifier:)`).
    ///
    /// The client keeps its three remembered preferences in `localStorage` — the
    /// Command-translation override and the two "if compatible" defaults — so the
    /// store has to persist, or every launch forgets them. WebKit keeps it in this
    /// app's container under the identifier below rather than in this directory,
    /// which is why the identifier has to come *from* this directory: that is what
    /// makes `--instance-dir` isolate the preferences too, the same way it isolates
    /// the config and the log.
    ///
    /// Derived rather than stored: a UUID written into the instance would be a
    /// fourth file to keep, and one that a copied directory would silently share.
    /// The path is hashed because a `UUID` is 16 bytes and a path is not.
    var dataStoreIdentifier: UUID {
        // Standardized so `/tmp/x` and `/tmp/./x` are one instance, which is what
        // they are to everything else here.
        let path = url.standardizedFileURL.path
        let digest = SHA256.hash(data: Data(path.utf8))
        var bytes = [UInt8](digest.prefix(16))
        // Version 4 / variant 1, so this is a well-formed UUID rather than 16 bytes
        // that happen to print like one.
        bytes[6] = (bytes[6] & 0x0F) | 0x40
        bytes[8] = (bytes[8] & 0x3F) | 0x80
        return UUID(uuid: (
            bytes[0], bytes[1], bytes[2], bytes[3],
            bytes[4], bytes[5], bytes[6], bytes[7],
            bytes[8], bytes[9], bytes[10], bytes[11],
            bytes[12], bytes[13], bytes[14], bytes[15]
        ))
    }

    /// The directory name a bundle name maps to.
    ///
    /// Split out from [`defaultURL`] so it can be tested at all: these tests run
    /// unbundled, where there is no `CFBundleName` to read — which is also the case
    /// this has to answer for, since an unbundled build still has to look somewhere.
    static func instanceName(ofBundleNamed bundleName: String?) -> String {
        let fallback = "remotex"
        guard let raw = bundleName?.trimmingCharacters(in: .whitespacesAndNewlines),
              !raw.isEmpty
        else {
            return fallback
        }
        // Forced into one path component, whatever the bundle is called. A `/` would
        // silently put the instance in a subdirectory of somebody else's, and a
        // leading `.` would hide the directory somebody is meant to be able to open.
        let cleaned = raw
            .replacingOccurrences(of: "/", with: "-")
            .replacingOccurrences(of: ":", with: "-")
            .drop(while: { $0 == "." })
        return cleaned.isEmpty ? fallback : String(cleaned)
    }

    /// The directory this launch owns.
    static func resolved(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> InstanceDirectory {
        InstanceDirectory(url: named(in: arguments) ?? defaultURL)
    }

    /// Split out from `resolved` so the parsing can be tested: a test process
    /// cannot choose its own `ProcessInfo` arguments.
    static func named(in arguments: [String]) -> URL? {
        guard let index = arguments.firstIndex(of: flag),
              arguments.indices.contains(index + 1)
        else {
            return nil
        }
        let path = arguments[index + 1].trimmingCharacters(in: .whitespaces)
        guard !path.isEmpty else {
            // A trailing `--instance-dir` with nothing after it is a mistake, and
            // falling back to the default is the wrong direction for a flag whose
            // whole purpose is to keep a test run away from the real instance. Said
            // out loud, then refused.
            FileHandle.standardError.write(
                Data("remotex: \(flag) needs a path; using the default instance\n".utf8)
            )
            return nil
        }
        // Expanded and resolved here rather than at each use: a path from the
        // command line may be `~/x` or relative, and an app launched by `open`
        // inherits `/` as its working directory — so a relative path would land
        // somewhere nobody meant.
        return URL(fileURLWithPath: (path as NSString).expandingTildeInPath).standardizedFileURL
    }

    /// The one file a user edits.
    var configURL: URL {
        url.appending(path: "remotex.toml")
    }

    /// The gateway's stderr, appended across launches. The app's own diagnosis of a
    /// remote that would not connect lives here — without it that output would go
    /// to a pipe nobody reads.
    var logURL: URL {
        url.appending(path: "gateway.log")
    }

    /// The client's preferences (see [`ViewerPreferences`]).
    var preferencesURL: URL {
        url.appending(path: "viewer.json")
    }

    /// Create the directory if it is not there, owner-only.
    ///
    /// `0700` because `remotex.toml` holds the credentials of every machine this app
    /// can reach: the file is written `0600` as well, but a directory anyone can
    /// list is a directory anyone can watch for a replacement.
    func create() throws {
        try FileManager.default.createDirectory(
            at: url,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
    }
}
