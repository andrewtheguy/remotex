import Foundation

/// The gateway process this app runs, and the promise that it stops when the app
/// does.
///
/// Two pipes, in opposite directions, doing two unrelated jobs — the same pair
/// described from the other side in `src/embedded.rs`:
///
/// - **its stdout → us**, once: the [`Handshake`] line, printed after the gateway has
///   bound its socket. It is how the port and the token are learned, and the reason
///   neither has to be guessed or passed in.
/// - **us → its stdin**, never: nothing is written on it in either direction. It is
///   open for as long as this process lives, so when this process ends — cleanly,
///   crashed, Force Quit, `kill -9` — the kernel closes our end and the gateway
///   reads end-of-file and exits. That is the layer the "no stray gateway" guarantee
///   rests on, because it needs no code of ours to run at the right moment.
///
/// [`terminate`] and [`stop`] are the polite paths on top of it, not instead of it.
@MainActor
final class EmbeddedGateway {
    /// What the gateway prints once, before serving.
    struct Handshake: Decodable, Equatable, Sendable {
        let port: UInt16
        let token: String
    }

    /// Why a launch did not produce a serving gateway.
    ///
    /// Each case exists because it needs a different sentence in front of somebody:
    /// a broken bundle is not a broken config, and neither is a gateway that started
    /// and then died.
    enum LaunchFailure: LocalizedError, Equatable {
        /// No `remotex-gateway` in this bundle — an unbundled or half-built app.
        case executableMissing
        /// The instance directory could not be created or read.
        case instanceUnavailable(String)
        /// The process could not be started at all (permissions, a bad architecture).
        case notStarted(String)
        /// It ran and exited without serving. The output is its own explanation,
        /// which for the common case is a config it refused.
        case refused(String)
        /// It is running but said nothing in time. Distinguished from `refused`
        /// because there is no message to show and the answer is different: this one
        /// is a bug or a wedged machine, not a file to fix.
        case silent
        /// It printed something that is not a handshake — a version mismatch between
        /// the app and the binary it carries.
        case malformedHandshake(String)

        var errorDescription: String? {
            switch self {
            case .executableMissing:
                "This copy of remotex is incomplete: it has no gateway to run."
            case .instanceUnavailable(let reason):
                "The remotex folder could not be opened: \(reason)"
            case .notStarted(let reason):
                "The local gateway could not be started: \(reason)"
            case .refused(let output):
                output.isEmpty ? "The local gateway stopped without saying why." : output
            case .silent:
                "The local gateway did not answer. Try again."
            case .malformedHandshake(let line):
                "The local gateway answered in a way this version does not understand: \(line)"
            }
        }
    }

    /// How long to wait for the handshake. Generous: it covers a cold start of a
    /// notarized binary whose signature Gatekeeper is checking for the first time,
    /// which is slow once and instant afterwards.
    static let handshakeTimeout: Duration = .seconds(20)

    /// How long a terminated gateway gets to go quietly before it is killed.
    static let terminationGrace: Duration = .milliseconds(1500)

    let instance: InstanceDirectory
    private let binary: GatewayBinary

    private var process: Process?
    private var standardInput: Pipe?
    private var standardOutput: Pipe?
    private var standardError: Pipe?
    private var errorReader: PipeReader?
    /// Resumed by the handshake line, by the pipe closing, or by the timeout —
    /// whichever happens first. Nil once resumed, which is what keeps "exactly once"
    /// a fact rather than an intention.
    private var waitingForHandshake: CheckedContinuation<Handshake?, Never>?

    /// Called when the gateway exits while the app is still using it, which is a
    /// failure the app has to show rather than discover on the next request.
    var onUnexpectedExit: ((LaunchFailure) -> Void)?

    init(instance: InstanceDirectory, binary: GatewayBinary) {
        self.instance = instance
        self.binary = binary
    }

    /// Whether a gateway is running right now.
    var isRunning: Bool {
        process?.isRunning ?? false
    }

    /// What the gateway has written to its stderr lately, for a failure to show.
    func log(lines: Int = PipeReader.tailLines) -> String {
        errorReader?.tail(lines: lines) ?? ""
    }

    /// Start a gateway and wait for its handshake.
    ///
    /// Any previous one is stopped first: two gateways on one instance directory
    /// would both work and would contend for the Mac agent's session slot, which
    /// looks exactly like somebody else taking the desktop away.
    func start() async throws -> Handshake {
        await stop()
        do {
            try instance.create()
        } catch {
            throw LaunchFailure.instanceUnavailable(error.localizedDescription)
        }

        let process = Process()
        process.executableURL = binary.executable
        process.arguments = ["serve-embedded", "--instance-dir", instance.url.path]
        let standardInput = Pipe()
        let standardOutput = Pipe()
        let standardError = Pipe()
        process.standardInput = standardInput
        process.standardOutput = standardOutput
        process.standardError = standardError

        // Both readers are attached before the process starts, so a gateway that
        // fails instantly cannot do it in the gap.
        let errorReader = PipeReader(logURL: instance.logURL)
        errorReader.attach(to: standardError.fileHandleForReading)
        let outputReader = PipeReader(
            firstLine: { [weak self] line in
                Task { @MainActor in self?.deliver(line: line) }
            },
            endOfFile: { [weak self] in
                Task { @MainActor in self?.deliver(line: nil) }
            }
        )
        outputReader.attach(to: standardOutput.fileHandleForReading)

        self.process = process
        self.standardInput = standardInput
        self.standardOutput = standardOutput
        self.standardError = standardError
        self.errorReader = errorReader

        do {
            try process.run()
        } catch {
            await stop()
            throw LaunchFailure.notStarted(error.localizedDescription)
        }

        // The timeout is a plain timer because nothing is blocked on a read — see
        // `PipeReader`. It resumes the same continuation the line would have.
        let deadline = Task { [weak self] in
            try? await Task.sleep(for: Self.handshakeTimeout)
            guard !Task.isCancelled else {
                return
            }
            await MainActor.run { self?.deliver(line: nil, timedOut: true) }
        }
        defer { deadline.cancel() }

        malformedHandshakeLine = nil
        let handshake = await withCheckedContinuation { continuation in
            waitingForHandshake = continuation
        }
        guard let handshake else {
            // Whatever went wrong, the gateway's own output is the best account of
            // it: a refused config is on stderr, and an empty tail means it never
            // got far enough to complain.
            let output = log().trimmingCharacters(in: .whitespacesAndNewlines)
            let running = process.isRunning
            let malformed = malformedHandshakeLine
            await stop()
            if let malformed {
                throw LaunchFailure.malformedHandshake(malformed)
            }
            throw output.isEmpty && running ? LaunchFailure.silent : LaunchFailure.refused(output)
        }
        return handshake
    }

    /// Stop the gateway: close the liveness pipe, ask, then insist.
    ///
    /// The order matters and it is the same order a quit takes. Closing stdin is what
    /// the gateway is watching for, so it is enough on its own; `SIGTERM` makes it
    /// immediate, and the kill after the grace period is for a gateway wedged
    /// somewhere neither reaches.
    func stop() async {
        guard let process else {
            clear()
            return
        }
        // Nothing may report this as a surprise: it is being asked for.
        onUnexpectedExitSuppressed = true
        defer { onUnexpectedExitSuppressed = false }

        closeLivenessPipe()
        if process.isRunning {
            process.terminate()
        }
        let deadline = ContinuousClock.now + Self.terminationGrace
        while process.isRunning, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(25))
        }
        if process.isRunning {
            kill(process.processIdentifier, SIGKILL)
        }
        deliver(line: nil)
        clear()
    }

    /// The quit path, and the one that cannot be `async`.
    ///
    /// `applicationWillTerminate` is synchronous and the process may be gone the
    /// moment it returns, so this does the two things that need no waiting and skips
    /// the grace period. It is also *not* the guarantee — that is the pipe, which the
    /// kernel closes for us however this app ends. This only makes the ordinary case
    /// immediate.
    nonisolated func terminateNow() {
        MainActor.assumeIsolated {
            closeLivenessPipe()
            if let process, process.isRunning {
                process.terminate()
            }
        }
    }

    /// Set while a stop is deliberate, so the exit it causes is not reported as one.
    private var onUnexpectedExitSuppressed = false

    /// Close the liveness pipe and nothing else.
    ///
    /// Exists so a test can assert the pipe carries the shutdown on its own, against a
    /// child that ignores `SIGTERM`: with `stop` there would be no way to tell which
    /// of the three layers did the work.
    func closeLivenessPipeForTesting() {
        closeLivenessPipe()
    }

    private func closeLivenessPipe() {
        try? standardInput?.fileHandleForWriting.close()
        standardInput = nil
    }

    /// The handshake line, the pipe closing, or the deadline — first one wins.
    private func deliver(line: String?, timedOut: Bool = false) {
        guard let continuation = waitingForHandshake else {
            // Nothing is waiting, so this is the pipe closing on a gateway that was
            // already serving: an exit the app did not ask for.
            if line == nil, !timedOut, !onUnexpectedExitSuppressed, process != nil {
                let output = log().trimmingCharacters(in: .whitespacesAndNewlines)
                onUnexpectedExit?(.refused(output))
            }
            return
        }
        waitingForHandshake = nil
        guard let line else {
            continuation.resume(returning: nil)
            return
        }
        guard let handshake = Self.decode(line: line) else {
            // Resumed with the line kept, so the caller can say what arrived rather
            // than only that something did.
            malformedHandshakeLine = line
            continuation.resume(returning: nil)
            return
        }
        continuation.resume(returning: handshake)
    }

    /// Set when a line arrived that was not a handshake, so `start` can name it.
    private var malformedHandshakeLine: String?

    /// Parse the handshake line. Split out from the pipe handling so a test can check
    /// the shape without a process.
    static func decode(line: String) -> Handshake? {
        guard let data = line.trimmingCharacters(in: .whitespacesAndNewlines).data(using: .utf8),
              let handshake = try? JSONDecoder().decode(Handshake.self, from: data),
              handshake.port != 0,
              !handshake.token.isEmpty
        else {
            return nil
        }
        return handshake
    }

    private func clear() {
        if let standardOutput {
            standardOutput.fileHandleForReading.readabilityHandler = nil
        }
        if let standardError {
            standardError.fileHandleForReading.readabilityHandler = nil
        }
        process = nil
        standardInput = nil
        standardOutput = nil
        standardError = nil
        // Kept: a failure panel is shown *after* the gateway is gone, and the tail is
        // the only account of why.
    }
}
