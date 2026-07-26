import Foundation

/// `remotex-viewer --probe`: attach to a gateway, print what arrives, exit.
///
/// This exists to settle two assumptions the rest of the client is built on,
/// both of which live inside `URLSessionWebSocketTask` where no unit test can
/// reach them:
///
/// 1. **Does URLSession answer the gateway's pings?** The gateway sends a
///    protocol Ping every 5s and kills the engine after 60s without a Pong
///    (`HEARTBEAT_INTERVAL` and the grace period in src/ws.rs). It uses protocol
///    pings precisely because a browser answers them in its network stack, and
///    `URLSessionWebSocketTask` is expected to do the same — but if it does not,
///    `sendPing` would not help either, since the gateway counts Pongs to *its*
///    pings. Idling here past 60s is the answer.
/// 2. **How large do strips actually get?** `maximumMessageSize` defaults to
///    1 MiB, and going past it fails the whole socket rather than dropping the
///    frame. The largest payload seen is printed, so the headroom is a measured
///    number rather than a guess.
///
/// Deliberately built on `GatewayClient` and the raw transport rather than on
/// `GatewayConnection`: both questions are about the socket, and a diagnostic
/// that reports frame sizes needs the bytes before anything decodes them.
enum ProbeCommand {
    static let flag = "--probe"

    /// Run and exit if `--probe` was passed; otherwise return and let the app
    /// start. Never returns in the probe case.
    static func runIfRequested() {
        guard CommandLine.arguments.contains(flag) else {
            return
        }
        Task.detached {
            let ok = await run()
            Foundation.exit(ok ? EXIT_SUCCESS : EXIT_FAILURE)
        }
        // The probe's own work is asynchronous, and URLSession delivers on its
        // own queues; running the main loop keeps the process alive until the
        // task above exits it.
        RunLoop.main.run()
    }

    private static func run() async -> Bool {
        let address = argument("--gateway") ?? "http://127.0.0.1:52380"
        let seconds = Int(argument("--probe-seconds") ?? "") ?? 90
        guard let gateway = try? GatewayLocation.parse(address) else {
            print("probe: \(address) is not a usable gateway address")
            return false
        }
        print("probe: \(gateway.url.absoluteString), \(seconds)s")
        let client = GatewayClient(gateway: gateway)

        do {
            let config = try await client.configuration()
            print("probe: branding=\(config.branding) protocolVersion=\(config.protocolVersion)")

            if try await !client.isAuthenticated() {
                guard let username = environment("REMOTEX_PROBE_USERNAME"),
                      let password = environment("REMOTEX_PROBE_PASSWORD")
                else {
                    print(
                        "probe: not signed in; set REMOTEX_PROBE_USERNAME and REMOTEX_PROBE_PASSWORD"
                    )
                    return false
                }
                let outcome = try await client.logIn(username: username, password: password)
                guard outcome == .ok else {
                    print("probe: login refused (\(outcome))")
                    return false
                }
            }
            print("probe: signed in")

            guard case .claimed(let token) = try await client.claimSession(
                force: true,
                sessionId: nil
            ) else {
                print("probe: could not claim the session slot")
                return false
            }
            print("probe: claimed \(token)")

            let transport = try await client.openSocket(sessionToken: token)
            if let target = argument("--probe-target") {
                try await transport.send(
                    ClientMessage.connect(target: target).jsonText() ?? ""
                )
                print("probe: connecting to \(target)")
            }
            return await pump(transport, seconds: seconds)
        } catch {
            print("probe: \(error.localizedDescription)")
            return false
        }
    }

    private static func pump(_ transport: any WebSocketTransport, seconds: Int) async -> Bool {
        let started = ContinuousClock.now
        let deadline = started + .seconds(seconds)
        var control = 0
        var tiles = 0
        var largestPayload = 0
        var largestTile = "none"

        let stopwatch = Task {
            try? await Task.sleep(for: .seconds(seconds))
            transport.cancel()
        }
        defer { stopwatch.cancel() }

        while ContinuousClock.now < deadline {
            let frame: WebSocketFrame
            do {
                frame = try await transport.receive()
            } catch {
                let elapsed = ContinuousClock.now - started
                print("probe: socket ended after \(elapsed) — closeCode=\(transport.closeCode.map(String.init) ?? "none")")
                // Surviving past the gateway's 60s heartbeat deadline is the
                // answer to question 1; ending before it is a failure.
                let survived = elapsed > .seconds(60)
                summarize(
                    control: control,
                    tiles: tiles,
                    largestPayload: largestPayload,
                    largestTile: largestTile,
                    survivedHeartbeat: survived
                )
                return survived
            }
            switch frame {
            case .text(let text):
                control += 1
                print("probe: control \(text.prefix(200))")
            case .binary(let data):
                tiles += 1
                largestPayload = max(largestPayload, data.count)
                if let tile = TileFrame.decode(data) {
                    largestTile = "\(tile.w)x\(tile.h) \(tile.format)"
                } else {
                    print("probe: undecodable \(data.count)-byte binary frame")
                }
            }
        }

        print("probe: idled the full \(seconds)s with the socket up")
        summarize(
            control: control,
            tiles: tiles,
            largestPayload: largestPayload,
            largestTile: largestTile,
            survivedHeartbeat: true
        )
        return true
    }

    private static func summarize(
        control: Int,
        tiles: Int,
        largestPayload: Int,
        largestTile: String,
        survivedHeartbeat: Bool
    ) {
        print("probe: \(control) control frames, \(tiles) tiles")
        print("probe: largest binary frame \(largestPayload) bytes (\(largestTile))")
        print("probe: default message limit is 1 MiB; this build allows \(URLSessionWebSocketTransport.maximumMessageSize)")
        print("probe: heartbeat answered by URLSession: \(survivedHeartbeat ? "yes" : "NO")")
    }

    private static func argument(_ name: String) -> String? {
        let arguments = CommandLine.arguments
        guard let index = arguments.firstIndex(of: name),
              arguments.indices.contains(index + 1)
        else {
            return nil
        }
        return arguments[index + 1]
    }

    private static func environment(_ name: String) -> String? {
        ProcessInfo.processInfo.environment[name].flatMap { $0.isEmpty ? nil : $0 }
    }
}
