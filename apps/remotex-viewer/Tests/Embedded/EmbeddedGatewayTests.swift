import Foundation
import Testing
@testable import RemotexViewer

/// The supervisor, against a fake gateway that is a shell script.
///
/// A script rather than the real binary, because what is being tested here is *this*
/// side of the arrangement: the handshake is parsed, a gateway that says nothing or
/// says the wrong thing is reported as such, and the child is gone when the app is
/// finished with it. Whether the real gateway prints the right line is the Rust
/// suite's business (`tests/embedded_gateway_e2e.rs`), and a test that needed a built
/// binary would silently skip in a plain `swift test`.
@MainActor
struct EmbeddedGatewayTests {
    /// The port and token are what the whole launch depends on, so the parse is
    /// pinned on its own — including the two shapes that would sail through
    /// `JSONDecoder` and fail later as a connection refused.
    @Test
    func aHandshakeIsParsedAndNonsenseIsNot() {
        let handshake = EmbeddedGateway.decode(
            line: #"{"host":"remotex-abc.localhost","port":49213,"token":"abc"}"#
        )
        #expect(
            handshake
                == EmbeddedGateway.Handshake(
                    host: "remotex-abc.localhost",
                    port: 49213,
                    token: "abc"
                )
        )
        // Trailing newline and whitespace, which is how it actually arrives.
        #expect(
            EmbeddedGateway.decode(line: "{\"host\":\"h.localhost\",\"port\":1,\"token\":\"t\"}\n")?
                .port == 1
        )

        for rubbish in [
            "",
            "not json",
            #"{"host":"h.localhost","port":49213}"#,
            #"{"host":"h.localhost","token":"abc"}"#,
            // The origin is half the launch: without a name there is nothing to
            // load, and the port alone would put the page back on a moving origin.
            #"{"port":49213,"token":"abc"}"#,
            #"{"host":"","port":49213,"token":"abc"}"#,
            // Both of these decode and neither is usable.
            #"{"host":"h.localhost","port":0,"token":"abc"}"#,
            #"{"host":"h.localhost","port":49213,"token":""}"#,
        ] {
            #expect(EmbeddedGateway.decode(line: rubbish) == nil, "accepted \(rubbish)")
        }
    }

    /// The happy path, end to end on this side: a child that prints a handshake and
    /// keeps running is a gateway, and its port and token come back.
    @Test
    func aGatewayThatPrintsAHandshakeIsUsable() async throws {
        let directory = try ScratchDirectory()
        let gateway = try fakeGateway(
            in: directory,
            script: """
                echo '{"host":"remotex-fake.localhost","port":49213,"token":"tok-abc"}'
                # Stay alive, and stop when stdin closes — the real gateway's contract.
                cat > /dev/null
                """
        )

        let handshake = try await gateway.start()

        #expect(
            handshake
                == EmbeddedGateway.Handshake(
                    host: "remotex-fake.localhost",
                    port: 49213,
                    token: "tok-abc"
                )
        )
        #expect(gateway.isRunning)
        await gateway.stop()
        #expect(!gateway.isRunning)
    }

    /// The failure this will actually hit: a config the gateway refuses. Its
    /// complaint is on stderr, so the app has to keep that and show it — a launch
    /// that failed with an empty explanation is one nobody can act on.
    @Test
    func aRefusedStartCarriesTheGatewaysOwnComplaint() async throws {
        let directory = try ScratchDirectory()
        let gateway = try fakeGateway(
            in: directory,
            script: """
                echo 'in config file /x/remotex.toml: unknown field `hostname`' >&2
                exit 1
                """
        )

        await #expect(throws: EmbeddedGateway.LaunchFailure.self) {
            _ = try await gateway.start()
        }
        #expect(gateway.log().contains("unknown field"), "got: \(gateway.log())")
        #expect(!gateway.isRunning)

        // And it was written down, not only remembered: the log outlives the launch,
        // which is the point of having one.
        let log = try #require(directory.contents(of: "gateway.log"))
        #expect(log.contains("unknown field"), "got: \(log)")
    }

    /// A line that is not a handshake means the app and the binary beside it disagree
    /// about the contract — a broken build, reported as itself rather than as a
    /// timeout twenty seconds later.
    @Test
    func aGatewaySpeakingSomeOtherLanguageIsReportedAsSuch() async throws {
        let directory = try ScratchDirectory()
        let gateway = try fakeGateway(
            in: directory,
            script: """
                echo 'listening on 127.0.0.1:49213'
                cat > /dev/null
                """
        )

        let failure = await failure(of: gateway)
        guard case .malformedHandshake(let line) = failure else {
            Issue.record("expected a malformed handshake, got \(String(describing: failure))")
            return
        }
        #expect(line.contains("listening on"))
    }

    /// An executable that is not there is the unbundled case, and it must not read as
    /// a gateway that would not start.
    @Test
    func aMissingExecutableIsItsOwnFailure() async throws {
        let directory = try ScratchDirectory()
        let gateway = EmbeddedGateway(
            instance: directory.instance,
            binary: GatewayBinary(executable: directory.url.appending(path: "not-here")),
            webRoot: directory.url.appending(path: "web")
        )

        let failure = await failure(of: gateway)
        guard case .notStarted = failure else {
            Issue.record("expected notStarted, got \(String(describing: failure))")
            return
        }
    }

    /// A gateway that dies while the app is using it has to be reported, not
    /// discovered on the next request — and not restarted behind the user's back,
    /// since a gateway that died once on this config will die again.
    @Test
    func anExitAfterServingIsReported() async throws {
        let directory = try ScratchDirectory()
        // Serves, then goes away on its own a moment later.
        let gateway = try fakeGateway(
            in: directory,
            script: """
                echo '{"host":"remotex-fake.localhost","port":49213,"token":"tok-abc"}'
                echo 'the engine crashed' >&2
                exit 3
                """
        )
        let reported = Reported()
        gateway.onUnexpectedExit = { failure in reported.record(failure) }

        _ = try await gateway.start()
        // Poll rather than sleep a fixed time: the exit is the child's to schedule.
        for _ in 0 ..< 200 where reported.failure == nil {
            try await Task.sleep(for: .milliseconds(10))
        }

        let failure = try #require(reported.failure)
        guard case .refused(let output) = failure else {
            Issue.record("expected refused, got \(failure)")
            return
        }
        #expect(output.contains("the engine crashed"), "got: \(output)")
    }

    /// Stopping is not an unexpected exit. Asking for it and then being told about it
    /// would put the failure screen up over a relaunch that is already under way.
    @Test
    func stoppingDeliberatelyIsNotReportedAsAnExit() async throws {
        let directory = try ScratchDirectory()
        let gateway = try fakeGateway(
            in: directory,
            script: """
                echo '{"host":"remotex-fake.localhost","port":49213,"token":"tok-abc"}'
                cat > /dev/null
                """
        )
        let reported = Reported()
        gateway.onUnexpectedExit = { failure in reported.record(failure) }

        _ = try await gateway.start()
        await gateway.stop()
        try await Task.sleep(for: .milliseconds(100))

        #expect(reported.failure == nil)
    }

    /// Closing the liveness pipe is enough on its own — no signal, no cooperation.
    ///
    /// This is the layer the "no stray gateway" guarantee rests on, so it is asserted
    /// against a child that ignores `SIGTERM` outright: if the pipe were not doing the
    /// work, this test would hang until the kill and then fail on the elapsed time.
    @Test
    func aGatewayIgnoringSignalsStillDiesWithThePipe() async throws {
        let directory = try ScratchDirectory()
        let gateway = try fakeGateway(
            in: directory,
            script: """
                trap '' TERM
                echo '{"host":"remotex-fake.localhost","port":49213,"token":"tok-abc"}'
                cat > /dev/null
                echo 'stdin closed' >&2
                """
        )
        _ = try await gateway.start()
        #expect(gateway.isRunning)

        // Only the pipe: not `stop`, which would also terminate and then kill.
        gateway.closeLivenessPipeForTesting()

        for _ in 0 ..< 300 where gateway.isRunning {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(!gateway.isRunning, "the pipe alone must end it")
        #expect(gateway.log().contains("stdin closed"), "and it saw the EOF: \(gateway.log())")
    }

    // MARK: - Helpers

    /// A gateway whose "binary" is `script`, run by `/bin/sh`.
    private func fakeGateway(
        in directory: ScratchDirectory,
        script: String
    ) throws -> EmbeddedGateway {
        let executable = try directory.write(
            "fake-gateway",
            "#!/bin/sh\n\(script)\n"
        )
        try FileManager.default.setAttributes(
            [.posixPermissions: 0o755],
            ofItemAtPath: executable.path
        )
        return EmbeddedGateway(
            instance: directory.instance,
            binary: GatewayBinary(executable: executable),
            // The fake gateways below never read it; the real one is told where the
            // bundle keeps the SPA, since nothing about a bundle's layout is the
            // gateway binary's to guess.
            webRoot: directory.url.appending(path: "web")
        )
    }

    private func failure(of gateway: EmbeddedGateway) async -> EmbeddedGateway.LaunchFailure? {
        do {
            _ = try await gateway.start()
            Issue.record("the gateway was expected not to start")
            return nil
        } catch let failure as EmbeddedGateway.LaunchFailure {
            return failure
        } catch {
            Issue.record("unexpected error: \(error)")
            return nil
        }
    }
}

/// Catches the supervisor's unexpected-exit callback, which arrives on the main actor
/// at a moment the test does not choose.
@MainActor
private final class Reported {
    private(set) var failure: EmbeddedGateway.LaunchFailure?

    func record(_ failure: EmbeddedGateway.LaunchFailure) {
        self.failure = failure
    }
}
