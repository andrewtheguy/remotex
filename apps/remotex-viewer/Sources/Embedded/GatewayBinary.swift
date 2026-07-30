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

    let executable: URL

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

    /// Run the binary, optionally writing `input` to its stdin, and collect both
    /// streams.
    ///
    /// For subcommands that answer and exit. Both pipes are drained concurrently
    /// rather than one after the other: a child that fills one while this waits on
    /// the other is a deadlock, and the fact that today's outputs are small is not a
    /// property worth depending on.
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

        if let input {
            try? standardInput.fileHandleForWriting.write(contentsOf: Data(input.utf8))
        }
        // Closed either way: a subcommand reading its config from stdin waits for
        // end-of-file, so leaving this open is a hang rather than an empty input.
        try? standardInput.fileHandleForWriting.close()

        async let out = Self.readToEnd(standardOutput.fileHandleForReading)
        async let error = Self.readToEnd(standardError.fileHandleForReading)
        let (text, errorText) = await (out, error)
        let status = await Self.exitStatus(of: process)
        return Output(status: status, standardOutput: text, standardError: errorText)
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
