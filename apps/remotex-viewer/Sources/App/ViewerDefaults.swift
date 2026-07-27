import Foundation

/// Where this launch keeps its settings, and whose login it uses.
///
/// The viewer normally has exactly one of each: `UserDefaults.standard` for the
/// gateway address, and `HTTPCookieStorage.shared` for the login. Both are the
/// right defaults and both are the problem when the app is being *tested*
/// against a throwaway gateway — the QA run's address is written over the real
/// one (`AppModel.connectToGateway` persists it once it answers), and the QA
/// login lands in the same cookie jar.
///
/// `--settings <name>` gives that run its own of each:
///
/// ```sh
/// open -n dist/remotex-viewer.app --args --settings qa --gateway http://127.0.0.1:52675
/// ```
///
/// The cookie half is not an extra: `HTTPCookieStorage` matches by host and
/// **ignores the port**, so a QA gateway on `127.0.0.1:52675` and a real one on
/// `127.0.0.1:52380` otherwise share one `remotex_session` — logging out of the
/// first logs out of the second. An ephemeral session is what keeps them apart,
/// at the price of a QA launch always starting at the login screen instead of
/// resuming. That is the right trade for a run whose point is to watch the app
/// come up.
/// Main-actor isolated because `UserDefaults` is not `Sendable`. No loss: the
/// only reader is the `AppModel` the app builds, which is main-actor itself.
@MainActor
enum ViewerDefaults {
    /// The flag, matching the shape of `AppModel`'s own `--gateway`.
    nonisolated private static let flag = "--settings"

    /// Suite names are prefixed so a stray `--settings standard` cannot name
    /// something else's domain.
    private static let prefix = "remotex-viewer"

    /// The name given on the command line, if there was one.
    static let name: String? = settingsName(in: ProcessInfo.processInfo.arguments)

    /// Split out from `name` so the parsing can be tested: a test process cannot
    /// choose its own `ProcessInfo` arguments.
    nonisolated static func settingsName(in arguments: [String]) -> String? {
        guard let index = arguments.firstIndex(of: flag),
              arguments.indices.contains(index + 1)
        else {
            return nil
        }
        let name = arguments[index + 1].trimmingCharacters(in: .whitespaces)
        // A trailing `--settings` with nothing after it, or with only spaces, is
        // a mistake rather than a request for a suite called "". Falling back to
        // the standard defaults is the wrong direction for isolation, so this is
        // the one case worth being loud about — but the flag is a developer's,
        // and the app still has to come up, so it stays a fallback.
        return name.isEmpty ? nil : name
    }

    /// The defaults this launch reads and writes.
    ///
    /// Falls back to `.standard` if the suite cannot be opened, which is the
    /// safe direction for the app but the wrong one for the isolation this was
    /// asked for — so it says so on the way past rather than silently writing
    /// where it was told not to.
    static let resolved: UserDefaults = {
        guard let name else {
            return .standard
        }
        guard let suite = UserDefaults(suiteName: "\(prefix).\(name)") else {
            FileHandle.standardError.write(
                Data("remotex-viewer: cannot open the \(name) settings suite; using the standard one\n".utf8)
            )
            return .standard
        }
        return suite
    }()

    /// The URL session this launch's gateway client runs on: ephemeral, and so
    /// with a cookie jar of its own, whenever settings are being kept apart.
    static let urlSession: URLSession = {
        name == nil
            ? GatewayClient.defaultSession
            : URLSession(configuration: .ephemeral)
    }()
}
