import Foundation

/// `remotex-viewer --probe`: attach to a gateway, print what arrives, exit.
///
/// Measures whether `URLSessionWebSocketTask` answers the gateway's protocol
/// pings, sent every 5 seconds, past its 60-second deadline. `sendPing` cannot
/// substitute: the gateway counts pongs answering its own pings
/// (`HEARTBEAT_INTERVAL` and its grace period are in `src/ws.rs`). It also
/// measures binary frame size because URLSession's 1 MiB default fails the socket
/// when exceeded. The probe uses the raw transport, not `GatewayConnection`, so
/// measurements precede decoding. It starts this app's embedded gateway; callers
/// provide only a target and duration.
@MainActor
enum ProbeCommand {
    static let flag = "--probe"

    /// Run and exit if `--probe` was passed; otherwise return and let the app
    /// start. Never returns in the probe case.
    static func runIfRequested() {
        guard CommandLine.arguments.contains(flag) else {
            return
        }
        Task {
            let ok = await run()
            Foundation.exit(ok ? EXIT_SUCCESS : EXIT_FAILURE)
        }
        // Keep the process alive for the asynchronous probe and URLSession.
        RunLoop.main.run()
    }

    private static func run() async -> Bool {
        let seconds = Int(argument("--probe-seconds") ?? "") ?? 90
        let instance = InstanceDirectory.resolved()
        guard let binary = GatewayBinary.inBundle() else {
            print("probe: no gateway in this bundle — run the probe from remotex.app")
            return false
        }
        let gateway = EmbeddedGateway(instance: instance, binary: binary)
        let handshake: EmbeddedGateway.Handshake
        do {
            handshake = try await gateway.start()
        } catch {
            print("probe: the gateway did not start: \(error.localizedDescription)")
            print(gateway.log())
            return false
        }
        // Do not rely on pipe EOF racing `Foundation.exit`.
        defer { gateway.terminateNow() }

        let location = GatewayLocation.loopback(port: handshake.port)
        print("probe: \(location.url.absoluteString), \(seconds)s")
        let client = GatewayClient(
            gateway: location,
            credential: .token(handshake.token),
            session: URLSession(configuration: .ephemeral)
        )

        do {
            let config = try await client.configuration()
            print("probe: branding=\(config.branding) protocolVersion=\(config.protocolVersion)")

            guard case .claimed(let token) = try await client.claimSession(
                force: true,
                sessionId: nil
            ) else {
                print("probe: could not claim the session slot")
                return false
            }
            // Never print the live session credential.
            print("probe: claimed the session slot")

            let transport = try await client.openSocket(sessionToken: token)
            if let target = argument("--probe-target") {
                // Fail instead of idling at the picker after an encoding failure.
                guard let connect = ClientMessage.connect(target: target).jsonText() else {
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
    /// Mirrors the gateway's `Totals` fields for sender/receiver comparison.
    /// Counts and bytes are both retained because a transport change can move
    /// either independently.
    private struct Counts {
        var control = 0
        var controlBytes = 0
        var binary = 0
        var binaryBytes = 0
        /// Records across all batches, of every kind.
        var records = 0
        /// Records sent as cache slot references.
        var references = 0
        /// Records carrying one H.264 access unit — what a target that streams sends
        /// instead of tiles, counted apart from them because it is not one.
        var units = 0
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
            units += batch?.count { record in
                if case .video = record { true } else { false }
            } ?? 0
            // Keep the largest frame and its tile summary paired.
            if data.count > largestBinary {
                largestBinary = data.count
                largestTile = batch.map { records in
                    let shapes = records.prefix(3).map { record in
                        switch record {
                        case .tile(let tile): "\(tile.w)x\(tile.h) \(tile.format)"
                        case .reference(let slot, _, _): "ref slot \(slot)"
                        case .video(let stream, _, _, let w, let h, _):
                            "\(w)x\(h) h264 stream \(stream)"
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
                // Survival past 60 seconds proves heartbeat handling.
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
                + "carrying \(counts.records) records "
                + "(\(counts.references) cache references, \(counts.units) access units), "
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
}
