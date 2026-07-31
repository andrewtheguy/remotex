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

    /// `~/Library/Application Support/remotex`.
    ///
    /// `remotex`, not `remotex-viewer`: the app is the product now, and the agent's
    /// own directory next to it is `remotex-agent`, so the three names stay apart.
    static var defaultURL: URL {
        URL.applicationSupportDirectory.appending(path: "remotex", directoryHint: .isDirectory)
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

    /// Icon file names this instance may carry, in the order they are preferred.
    ///
    /// `.icns` first because it is what macOS wants — multiple resolutions in one file —
    /// with `.png` accepted because it is what somebody actually has to hand.
    static let iconNames = ["icon.icns", "icon.png"]

    /// A Dock icon for *this* instance, if one has been dropped into its directory.
    ///
    /// A file rather than a config key, so it needs no schema and no validation: an
    /// instance either has an `icon.icns` beside its config or it does not. Two
    /// instances running at once are otherwise identical in the Dock and in ⌘-Tab —
    /// `branding` distinguishes their windows, and this distinguishes everything else.
    ///
    /// Nil is the ordinary case and means the app keeps its own icon.
    func iconURL() -> URL? {
        Self.iconNames
            .map { url.appending(path: $0) }
            .first { FileManager.default.fileExists(atPath: $0.path) }
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
