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
            // Not the token itself: this prints to a terminal and gets pasted into
            // bug reports, and it is a live credential for the session slot.
            print("probe: claimed the session slot")

            let transport = try await client.openSocket(sessionToken: token)
            if let target = argument("--probe-target") {
                // An empty frame is one the gateway drops, and the probe would
                // then idle against the picker while reporting it was connecting.
                guard let connect = ClientMessage.connect(target: target, force: false).jsonText() else {
                    print("probe: could not encode a connect for \(target)")
                    return false
                }
                try await transport.send(connect)
                print("probe: connecting to \(target)")
            }
            return await pump(transport, seconds: seconds)
        } catch {
            print("probe: \(error.localizedDescription)")
            return false
        }
    }

    /// What arrived on the socket.
    ///
    /// Deliberately shaped like the gateway's own `Totals` line (`src/ws.rs`) so
    /// the two can be read against each other: that side reports what it sent,
    /// this side what a real client received. Bytes as well as counts, because a
    /// transport change can move one without the other.
    private struct Counts {
        var control = 0
        var controlBytes = 0
        var binary = 0
        var binaryBytes = 0
        /// Tile records across every batch, which is the number that used to equal
        /// `binary` one-for-one. Seeing the two apart is the whole point.
        var records = 0
        /// References among them: records the gateway sent as a slot and a position
        /// because this client already had the pixels.
        var references = 0
        var largestBinary = 0
        var largestTile = "none"

        mutating func text(_ text: String) {
            control += 1
            controlBytes += text.utf8.count
        }

        mutating func binaryFrame(_ data: Data, records batch: [BatchFrame.Record]?) {
            binary += 1
            binaryBytes += data.count
            records += batch?.count ?? 0
            references += batch?.count { record in
                if case .reference = record { true } else { false }
            } ?? 0
            // Both halves have to come from the *same* frame, or the summary pairs
            // the largest frame with whatever happened to arrive last — which is
            // the number this probe exists to report.
            if data.count > largestBinary {
                largestBinary = data.count
                largestTile = batch.map { records in
                    let shapes = records.prefix(3).map { record in
                        switch record {
                        case .tile(let tile): "\(tile.w)x\(tile.h) \(tile.format)"
                        case .reference(let slot, _, _): "ref slot \(slot)"
                        }
                    }
                    let more = records.count > 3 ? ", +\(records.count - 3) more" : ""
                    return "\(records.count) records: \(shapes.joined(separator: ", "))\(more)"
                } ?? "undecodable"
            }
        }
    }

    private static func pump(_ transport: any WebSocketTransport, seconds: Int) async -> Bool {
        let started = ContinuousClock.now
        let deadline = started + .seconds(seconds)
        var counts = Counts()

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
                summarize(counts, survivedHeartbeat: survived)
                return survived
            }
            switch frame {
            case .text(let text):
                counts.text(text)
                print("probe: control \(text.prefix(200))")
            case .binary(let data):
                let records = BatchFrame.decode(data)
                if records == nil {
                    print("probe: undecodable \(data.count)-byte binary frame")
                }
                counts.binaryFrame(data, records: records)
            }
        }

        print("probe: idled the full \(seconds)s with the socket up")
        summarize(counts, survivedHeartbeat: true)
        return true
    }

    private static func summarize(_ counts: Counts, survivedHeartbeat: Bool) {
        print(
            "probe: \(counts.binary) binary frames / \(counts.binaryBytes) bytes "
                + "carrying \(counts.records) tile records "
                + "(\(counts.references) cache references), "
                + "\(counts.control) control frames / \(counts.controlBytes) bytes"
        )
        print("probe: largest binary frame \(counts.largestBinary) bytes (\(counts.largestTile))")
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
