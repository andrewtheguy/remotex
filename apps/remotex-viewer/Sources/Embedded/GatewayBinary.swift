import Foundation

/// The gateway executable this app carries, run for a one-shot answer.
///
/// `remotex-gateway` inside the bundle is the same binary the CLI installs, so the
/// app can ask it the questions it would otherwise have to answer for itself:
/// whether a config is valid, and what this instance's `rxa` public key is. Asking
/// the binary is the point — a TOML parser written in Swift would be a *second*
/// opinion about what a config means, and the one that mattered would be whichever
/// ran last.
///
/// Long-running use — the session gateway itself — is [`EmbeddedGateway`]'s.
struct GatewayBinary: Sendable {
    /// What the executable is called inside `Contents/MacOS`.
    ///
    /// Not `remotex`: the app's own executable is in that directory too, and two
    /// files cannot share a name. The `-gateway` suffix also makes it obvious in
    /// Activity Monitor and in `pgrep` which process is which.
    static let name = "remotex-gateway"

    /// The copy inside this bundle, or `nil` in an unbundled build (`swift test`,
    /// `swift run`), where there is none.
    static func inBundle() -> GatewayBinary? {
        Bundle.main.url(forAuxiliaryExecutable: name).map { GatewayBinary(executable: $0) }
    }

    struct Output: Sendable {
        let status: Int32
        let standardOutput: String
        let standardError: String

        var succeeded: Bool { status == 0 }

        /// What to show a human when this failed: the gateway's own message,
        /// falling back to the exit status when it said nothing at all.
        var failure: String {
            let message = standardError.trimmingCharacters(in: .whitespacesAndNewlines)
            return message.isEmpty ? "the gateway exited with status \(status)" : message
        }
    }

    /// How long a one-shot subcommand gets before it is killed and reported as a
    /// failure.
    ///
    /// These answer in milliseconds — parse a config, derive a key — so this is not a
    /// budget, it is the difference between a wedged child and a **Save** button stuck
    /// on "Checking…" with no way out. Generous enough that a cold start whose
    /// signature Gatekeeper is checking for the first time is not mistaken for a hang.
    static let defaultTimeout: Duration = .seconds(15)

    let executable: URL
    /// Overridable so a test can watch this happen without waiting fifteen seconds
    /// for it; nothing in the app passes anything but the default.
    var timeout: Duration = GatewayBinary.defaultTimeout

    /// Run the binary, optionally writing `input` to its stdin, and collect both
    /// streams.
    ///
    /// The order matters twice over. Both pipes are drained **before** anything is
    /// written to stdin and concurrently with each other: a child that fills one
    /// while this waits on the other — or while this is still feeding it — is a
    /// deadlock, and the fact that today's inputs and outputs are small is not a
    /// property worth depending on. The write itself goes to a queue rather than
    /// happening here, because a blocking `write` on a cooperative thread is one
    /// fewer thread for everything else in the process.
    func run(_ arguments: [String], input: String? = nil) async throws -> Output {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        let standardInput = Pipe()
        let standardOutput = Pipe()
        let standardError = Pipe()
        process.standardInput = standardInput
        process.standardOutput = standardOutput
        process.standardError = standardError
        try process.run()

        async let out = Self.readToEnd(standardOutput.fileHandleForReading)
        async let error = Self.readToEnd(standardError.fileHandleForReading)
        // Started before the write and never awaited before it either, so a child that
        // talks back while being fed cannot wedge the pair of us.
        Self.write(input, to: standardInput.fileHandleForWriting)

        // The watchdog is what turns a wedged child into an ordinary failure: killing
        // it closes both pipes, so the reads above finish and this returns through the
        // path a non-zero exit already takes.
        let watchdog = Task { [timeout] in
            try? await Task.sleep(for: timeout)
            guard !Task.isCancelled else {
                return
            }
            let box = ProcessBox(process: process)
            DispatchQueue.global(qos: .userInitiated).async {
                if box.process.isRunning {
                    box.process.terminate()
                }
            }
        }
        defer { watchdog.cancel() }

        let (text, errorText) = await (out, error)
        let status = await Self.exitStatus(of: process)
        return Output(status: status, standardOutput: text, standardError: errorText)
    }

    /// Feed the child its stdin and close it.
    ///
    /// Closed either way, input or none: a subcommand reading its config from stdin
    /// waits for end-of-file, so leaving this open is a hang rather than an empty
    /// input. Failures are ignored because they all mean the same thing — the child is
    /// not listening — which its exit status and stderr describe better than a write
    /// error could.
    private static func write(_ input: String?, to handle: FileHandle) {
        let box = HandleBox(handle: handle)
        DispatchQueue.global(qos: .userInitiated).async {
            if let input {
                try? box.handle.write(contentsOf: Data(input.utf8))
            }
            try? box.handle.close()
        }
    }

    private static func readToEnd(_ handle: FileHandle) async -> String {
        await withCheckedContinuation { continuation in
            let box = HandleBox(handle: handle)
            DispatchQueue.global(qos: .userInitiated).async {
                let data = (try? box.handle.readToEnd()) ?? Data()
                continuation.resume(returning: String(decoding: data, as: UTF8.self))
            }
        }
    }

    /// Wait for the child without parking a cooperative thread.
    ///
    /// `waitUntilExit` on a global queue rather than `terminationHandler`, because a
    /// handler installed after the process has already exited may never be called —
    /// and by the time both pipes above have reached end-of-file, that is the likely
    /// case.
    private static func exitStatus(of process: Process) async -> Int32 {
        await withCheckedContinuation { continuation in
            let box = ProcessBox(process: process)
            DispatchQueue.global(qos: .userInitiated).async {
                box.process.waitUntilExit()
                continuation.resume(returning: box.process.terminationStatus)
            }
        }
    }
}

/// `Process` and `FileHandle` are not `Sendable`, and these two boxes are the
/// smallest honest way to hand one to the queue that will wait on it.
///
/// Safe because of what is *not* done with them: each box is used by exactly one
/// closure, which touches only the blocking call it was made for
/// (`waitUntilExit`/`terminationStatus`, or `readToEnd`) and never mutates
/// configuration. Nothing else holds the box, so there is no second reader to race.
private struct ProcessBox: @unchecked Sendable {
    let process: Process
}

private struct HandleBox: @unchecked Sendable {
    let handle: FileHandle
}
