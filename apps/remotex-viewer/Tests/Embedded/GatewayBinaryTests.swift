import Foundation
import Testing
@testable import RemotexViewer

/// One-shot runs of the gateway binary, against shell scripts standing in for it.
///
/// A script rather than the real binary for the same reason `EmbeddedGatewayTests`
/// uses one: what is being tested is this side of the arrangement — that both streams
/// come back, that stdin is fed and closed, and that a child which never finishes
/// becomes a failure instead of a wait with no end. What the real `check-config` says
/// about a config is the Rust suite's business.
@MainActor
struct GatewayBinaryTests {
    /// Both streams and the status, which is all `Output.failure` and the config
    /// store's verdicts are built from.
    @Test
    func bothStreamsAndTheStatusComeBack() async throws {
        let binary = try fake(
            """
            echo 'to stdout'
            echo 'to stderr' >&2
            exit 3
            """
        )

        let output = try await binary.run([])

        #expect(output.status == 3)
        #expect(!output.succeeded)
        #expect(output.standardOutput.contains("to stdout"))
        #expect(output.standardError.contains("to stderr"))
        // The failure message is the child's own words, not a status code, whenever it
        // said anything at all.
        #expect(output.failure == "to stderr")
    }

    /// A child that says nothing still has to produce a message, or a refused save
    /// would show an empty complaint.
    @Test
    func silenceFallsBackToTheExitStatus() async throws {
        let binary = try fake("exit 9")
        let output = try await binary.run([])
        #expect(output.failure.contains("status 9"), "got: \(output.failure)")
    }

    /// stdin is written *and closed*: a subcommand reading its config from stdin waits
    /// for end-of-file, so a pipe left open is a hang rather than an empty input.
    @Test
    func stdinIsDeliveredAndClosed() async throws {
        let binary = try fake("cat")
        let output = try await binary.run([], input: "branding = \"x\"\n")
        #expect(output.succeeded)
        #expect(output.standardOutput == "branding = \"x\"\n")
    }

    /// More than a pipe buffer in both directions at once, which is the deadlock the
    /// ordering in `run` exists to avoid: the drains start before the write, so a child
    /// that answers while being fed cannot wedge the pair of them.
    @Test
    func alargeInputAndAlargeAnswerDoNotDeadlock() async throws {
        // Well past the 64 KiB a pipe holds, so this fails by hanging if either half is
        // sequenced wrongly.
        let line = String(repeating: "x", count: 1_000) + "\n"
        let input = String(repeating: line, count: 200)
        let binary = try fake("cat; cat >&2")

        let output = try await binary.run([], input: input)

        #expect(output.succeeded)
        #expect(output.standardOutput.count == input.count)
    }

    /// A child that never finishes is a bounded failure, not a wait with no end — the
    /// difference between a refused save and a Save button stuck on "Checking…"
    /// forever.
    @Test
    func awedgedChildIsKilledAndReported() async throws {
        var binary = try fake("sleep 60")
        binary.timeout = .milliseconds(300)

        let started = ContinuousClock.now
        let output = try await binary.run([])
        let elapsed = ContinuousClock.now - started

        #expect(!output.succeeded, "a killed child cannot have succeeded")
        // Generous on the upper bound: the assertion is "it returned rather than
        // waiting for the child", not how promptly.
        #expect(elapsed < .seconds(10), "took \(elapsed)")
        #expect(!output.failure.isEmpty, "and it has something to show")
    }

    // MARK: - Helpers

    private func fake(_ script: String) throws -> GatewayBinary {
        let directory = try ScratchDirectory("binary")
        let executable = try directory.write("fake-gateway", "#!/bin/sh\n\(script)\n")
        try FileManager.default.setAttributes(
            [.posixPermissions: 0o755],
            ofItemAtPath: executable.path
        )
        // The directory removes itself when it goes; held for the length of the call by
        // the returned binary's URL alone, so it is kept alive here.
        keepAlive.append(directory)
        return GatewayBinary(executable: executable)
    }

    /// Scratch directories outlive `fake` and are removed when the test does.
    private let keepAlive = Retainer()
}

/// Holds the scratch directories a test made, so `deinit` does not remove one while
/// the child process is still being spawned from it.
@MainActor
private final class Retainer {
    private var directories: [ScratchDirectory] = []

    func append(_ directory: ScratchDirectory) {
        directories.append(directory)
    }
}
