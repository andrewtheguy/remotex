import Foundation

/// What is wrong with a config, in the gateway's own words.
///
/// A type of its own rather than a bare `String` in a `Result` — which does not
/// conform to `Error` — and the message is deliberately not rewritten on the way
/// through: `check-config` already names the file, the target and the key, and a
/// friendlier sentence in front of it would only hide the part that says what to fix.
struct ConfigProblem: Error, Equatable {
    let message: String

    init(_ message: String) {
        self.message = message
    }
}

/// The instance's `remotex.toml`: read it, check it, write it.
///
/// Checking is delegated to the gateway binary (`check-config --embedded`), which is
/// the whole design: what the editor accepts is by construction what the gateway
/// accepts, and there is no Swift-side idea of what a config means that could drift
/// from the Rust one. The two closures below are that call, injected so the panel's
/// behaviour — refuse and keep the text, or write and restart — can be tested
/// without a built binary.
@MainActor
final class GatewayConfigStore {
    /// Checks candidate text, answering with the gateway's own complaint.
    typealias Validator = @Sendable (String) async -> Result<Void, ConfigProblem>

    let instance: InstanceDirectory
    private let validate: Validator

    init(
        instance: InstanceDirectory,
        validate: @escaping Validator
    ) {
        self.instance = instance
        self.validate = validate
    }

    /// The store the app uses, over the gateway in this bundle.
    static func overBinary(instance: InstanceDirectory, binary: GatewayBinary) -> GatewayConfigStore {
        GatewayConfigStore(
            instance: instance,
            validate: { text in
                do {
                    let output = try await binary.run(["check-config", "--embedded"], input: text)
                    return output.succeeded ? .success(()) : .failure(ConfigProblem(output.failure))
                } catch {
                    return .failure(ConfigProblem(error.localizedDescription))
                }
            }
        )
    }

    /// The current text, or the template when there is no file yet.
    func read() -> String {
        (try? String(contentsOf: instance.configURL, encoding: .utf8)) ?? Self.template
    }

    /// Make sure there is a config to serve.
    ///
    /// A first launch has no targets, which is a valid config the picker reports as
    /// an empty list — so this only has to put the template on disk where none exists.
    func bootstrapIfNeeded() async throws {
        try instance.create()
        guard !FileManager.default.fileExists(atPath: instance.configURL.path) else {
            return
        }
        try write(Self.template)
    }

    /// Check `text` and write it only if the gateway would serve it.
    ///
    /// Nothing is written on a refusal — not a backup, not a partial file. The point
    /// of validating first is that the config on disk is always one the gateway
    /// starts on, so a rejected edit leaves the app exactly as usable as it was.
    func save(_ text: String) async -> Result<Void, ConfigProblem> {
        if case .failure(let problem) = await validate(text) {
            return .failure(problem)
        }
        do {
            try write(text)
            return .success(())
        } catch {
            return .failure(
                ConfigProblem("The configuration could not be saved: \(error.localizedDescription)")
            )
        }
    }

    /// Write atomically at `0600`.
    ///
    /// Atomically because this file holds every target's credentials and the gateway
    /// may be restarted the instant it changes: a half-written config is one the
    /// gateway refuses, and the way to never have one is to never have it visible.
    /// `0600` for the same reason the directory is `0700`.
    private func write(_ text: String) throws {
        try instance.create()
        let url = instance.configURL
        let temporary = url.appendingPathExtension("new")
        // Registered before the first thing that can throw: a `remotex.toml.new` left
        // behind by a failed save is litter in a directory documented as holding three
        // files, and the next save would be writing over somebody's unexplained
        // leftovers. Harmless once the move below has consumed it.
        defer { try? FileManager.default.removeItem(at: temporary) }
        try Data(text.utf8).write(to: temporary, options: .atomic)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: temporary.path)
        if FileManager.default.fileExists(atPath: url.path) {
            // `replaceItemAt` keeps the destination's identity, so an editor watching
            // the file follows the change rather than losing track of it.
            _ = try FileManager.default.replaceItemAt(url, withItemAt: temporary)
        } else {
            // First launch: there is no item to replace, which is not what
            // `replaceItemAt` is for. A move is the same atomic rename without the
            // pretence.
            try FileManager.default.moveItem(at: temporary, to: url)
        }
        try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
    }

    /// What a first launch gets: no targets, and a commented example of each
    /// protocol.
    ///
    /// Commented rather than filled in, because an example target that *looks*
    /// configured is worse than none — the picker would offer a machine that does not
    /// exist. With everything commented out the gateway serves an empty list and the
    /// picker says so in words.
    static let template = """
        # remotex.app's own configuration.
        #
        # This file belongs to the app and to nothing else: it is not the installed
        # server's /opt/remotex/etc/remotex.toml, and the two never read each other.
        #
        # There is no [server] block here. The app decides where its gateway listens
        # (an ephemeral port on 127.0.0.1), serves no web UI, and authenticates itself,
        # so a [server] block would only contradict it and is refused.
        #
        # Add one [[targets]] block per machine. Edit this from the app —
        # Remote > Configuration… — which checks it before saving.

        # What this instance calls itself: the heading above the target list, the
        # window title, and the launch screen. Useful when two instances run at once.
        # branding = "remotex"

        # A Windows machine over RDP:
        #
        # [[targets]]
        # name = "work"
        # protocol = "rdp"
        # host = "192.168.1.20"
        # username = "andrew"
        # password = "…"
        # domain = "CORP"        # optional
        # resize = true          # let the window drive the remote's size
        # clipboard = true
        # audio = true           # rdp only

        # A VNC server:
        #
        # [[targets]]
        # name = "pi"
        # protocol = "vnc"
        # host = "192.168.1.30"
        # vnc_password = "…"     # the VNC server's own password

        """
}
