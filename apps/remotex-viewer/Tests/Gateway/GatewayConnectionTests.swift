import Foundation
import Testing
@testable import RemotexViewer

/// The `connected` this suite attaches with. Named because every field is required —
/// the gateway sends them all and this build refuses a message missing any — so it is
/// noise in a test about ordering.
private let connectedJSON = #"""
{"type":"connected","name":"mac","protocol":"vnc","resize":false,\#
"clipboard":true,"audio":true}
"""#

/// A real `audioFormat`, `head` included: the base64 is the gateway's own 19-byte
/// `OpusHead` from `protocol.rs`'s pinned test, so a decoder built from this is built
/// from what the wire carries.
private let audioFormatJSON = #"""
{"type":"audioFormat","codec":"opus","sampleRate":48000,"channels":2,\#
"head":"T3B1c0hlYWQBAjgBRKwAAAAAAA=="}
"""#

@MainActor
struct GatewayConnectionTests {
    /// The invariant the whole receive loop is shaped around, and the only one
    /// this layer still owes now that decoding is the canvas page's: everything
    /// off the socket reaches the sink in the order it arrived, control messages
    /// and binary frames interleaved as the gateway sent them.
    ///
    /// It matters at both ends of the pipe. A `resize` that overtook the tiles
    /// queued ahead of it paints stale pixels into a freshly sized canvas, and
    /// tiles carry no delta state, so nothing downstream can repair it. An
    /// `audioFormat` that arrived after the packets it configures configures
    /// nothing, and Opus packets carry inter-packet state, so a reordered pair is
    /// a decoder running on wrong history.
    @Test
    func everythingReachesTheSinkInArrivalOrder() async throws {
        let transport = FakeWebSocketTransport(
            inbound: [
                .text(connectedJSON),
                .text(#"{"type":"resize","w":64,"h":64,"scale":2.0}"#),
                .binary(try tileFrame(x: 0, y: 0)),
                .text(audioFormatJSON),
                .binary(audioFrame([Data(repeating: 7, count: 240)])),
                .binary(try tileFrame(x: 8, y: 16)),
                .text(#"{"type":"remoteOs","macos":true}"#),
                .binary(audioFrame([Data(repeating: 8, count: 12), Data([9])])),
                .binary(try tileFrame(x: 32, y: 48)),
                .text(#"{"type":"picker"}"#),
            ],
            closeCode: nil
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        let interesting = sink.trace.filter {
            $0.hasPrefix("control:") || $0.hasPrefix("frame:")
        }
        #expect(
            interesting == [
                "control:connected(mac)",
                "control:resize(64x64@2.0x)",
                "frame:2,\(try tileFrame(x: 0, y: 0).count)",
                "control:audioFormat(opus)",
                "frame:3,\(audioFrame([Data(repeating: 7, count: 240)]).count)",
                "frame:2,\(try tileFrame(x: 8, y: 16).count)",
                "control:remoteOs(true)",
                "frame:3,\(audioFrame([Data(repeating: 8, count: 12), Data([9])]).count)",
                "frame:2,\(try tileFrame(x: 32, y: 48).count)",
                "control:picker",
            ]
        )
        await connection.stop()
    }

    /// A binary frame is handed on byte for byte. The page parses it with the same
    /// code the browser SPA uses, so anything rewritten here would be a second
    /// implementation of a format only one side reads.
    @Test
    func aBinaryFrameIsForwardedVerbatim() async throws {
        let batch = batchFrame([
            try tileRecord(x: 0, y: 0),
            referenceRecord(slot: 3, x: 8, y: 16),
        ])
        let audio = audioFrame([Data(repeating: 7, count: 240), Data([1, 2, 3])])
        let transport = FakeWebSocketTransport(
            inbound: [.binary(batch), .binary(audio), .text(#"{"type":"picker"}"#)],
            closeCode: nil
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        let forwarded = sink.events.compactMap { event -> Data? in
            if case .frame(let data) = event { data } else { nil }
        }
        #expect(forwarded == [batch, audio])
        await connection.stop()
    }

    /// A binary frame of a kind this build has no parser for is one dropped frame.
    ///
    /// Worth its own case because the two binary kinds are now told apart by their
    /// first byte: before audio existed every binary frame went to the tile parser, so
    /// this frame would have been reported as a malformed *batch* — a wrong diagnosis
    /// for a newer gateway, which is what the version check exists to say.
    @Test
    func aBinaryFrameOfAnUnknownKindIsDroppedAlone() async throws {
        let transport = FakeWebSocketTransport(
            inbound: [
                .binary(Data([0x04, 0, 1, 0, 5, 5, 5])),
                .binary(try tileFrame(x: 0, y: 0)),
                .text(#"{"type":"picker"}"#),
            ],
            closeCode: nil
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        let batch = try tileFrame(x: 0, y: 0)
        let delivered = sink.trace.filter {
            $0.hasPrefix("control:") || $0.hasPrefix("frame:")
        }
        #expect(delivered == ["frame:2,\(batch.count)", "control:picker"])
        await connection.stop()
    }

    @Test
    func aClaimedSlotOpensASocketWithItsOwnToken() async throws {
        let transport = FakeWebSocketTransport(
            inbound: [.text(#"{"type":"picker"}"#)],
            closeCode: nil
        )
        let gateway = FakeGateway(claims: [.claimed("tok-42")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        #expect(gateway.socketTokens == ["tok-42"])
        #expect(gateway.claimCalls.count == 1)
        #expect(gateway.claimCalls[0].force == false)
        #expect(gateway.claimCalls[0].sessionId == nil)
        await connection.stop()
    }

    /// An eviction must not race the user: no re-claim happens behind their back,
    /// and taking the session back is an explicit forced claim.
    @Test
    func anEvictionStopsAndDoesNotReclaimUntilAskedTo() async throws {
        let evicted = FakeWebSocketTransport(
            inbound: [.text(#"{"type":"picker"}"#)],
            closeCode: 4001
        )
        let reclaimed = FakeWebSocketTransport(
            inbound: [.text(#"{"type":"picker"}"#)],
            closeCode: nil
        )
        let gateway = FakeGateway(
            claims: [.claimed("tok-1"), .claimed("tok-2")],
            sockets: [evicted, reclaimed]
        )
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        // The status alone is enough to wait for: a transition reaches the sink in
        // one hop, so the cleanup below has landed with it.
        await sink.wait { events in
            events.contains { if case .status(.takenOver) = $0 { true } else { false } }
        }
        #expect(gateway.claimCalls.count == 1, "an eviction must not re-claim on its own")

        // The framebuffer is dropped and input released, and any clipboard fetch
        // in flight is failed rather than left waiting out its own deadline.
        let afterEviction = sink.trace.drop { $0 != "status:takenOver" }
        #expect(afterEviction.contains("failFetch"))
        #expect(afterEviction.contains("clear"))
        #expect(afterEviction.contains("release"))

        await connection.start(force: true)
        await sink.wait { _ in gateway.claimCalls.count == 2 }
        #expect(gateway.claimCalls[1].force == true)
        #expect(
            gateway.claimCalls[1].sessionId == "tok-1",
            "the take-back replays this viewer's own token"
        )
        await connection.stop()
    }

    /// A drop re-claims with the token this process already holds, which is what
    /// lets it reattach to its own session instead of prompting for a takeover.
    @Test
    func aDropReclaimsWithTheTokenItAlreadyHolds() async throws {
        let dropped = FakeWebSocketTransport(
            inbound: [.text(#"{"type":"picker"}"#)],
            closeCode: 1_006
        )
        let reattached = FakeWebSocketTransport(
            inbound: [.text(#"{"type":"picker"}"#)],
            closeCode: nil
        )
        let gateway = FakeGateway(
            claims: [.claimed("tok-1"), .claimed("tok-1")],
            sockets: [dropped, reattached]
        )
        let sink = RecordingSink()
        let connection = GatewayConnection(
            gateway: gateway,
            sink: sink,
            policy: ReconnectPolicy(baseMilliseconds: 20, capMilliseconds: 20)
        )

        await connection.start()
        // `>=`, not `==`: once the scripted sockets run out the driver keeps
        // retrying, so the count overshoots between polls.
        await sink.wait { _ in gateway.claimCalls.count >= 2 }
        #expect(gateway.claimCalls[1].force == false)
        #expect(gateway.claimCalls[1].sessionId == "tok-1")
        await sink.wait { events in
            events.filter { if case .status(.connected) = $0 { true } else { false } }.count >= 2
        }
        await connection.stop()
    }

    @Test
    func aBusySlotReportsBusyAndWaits() async throws {
        let gateway = FakeGateway(claims: [.busy], sockets: [])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .status(.busy) = $0 { true } else { false } } }
        #expect(gateway.socketTokens.isEmpty, "no socket is opened for a slot we do not hold")
        await connection.stop()
    }

    @Test
    func a401ReportsUnauthorizedRatherThanRetrying() async throws {
        let gateway = FakeGateway(claims: [.unauthorized], sockets: [])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .unauthorized = $0 { true } else { false } } }
        #expect(gateway.claimCalls.count == 1)
        await connection.stop()
    }

    @Test
    func queuedMessagesReachTheSocketInOrder() async throws {
        let transport = FakeWebSocketTransport(
            inbound: [.text(#"{"type":"picker"}"#)],
            closeAfterDraining: false
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        connection.send(.key(code: "KeyA", pressed: true, caps: false))
        connection.send(.key(code: "KeyA", pressed: false, caps: false))
        connection.send(.connect(target: "mac"))
        await sink.wait { _ in transport.sentFrames.count == 3 }

        // Parsed, not compared as text: JSONEncoder does not promise a key order,
        // and the gateway's internally-tagged deserialization does not need one.
        // What must hold is the order of the *messages* — a press and its release
        // arriving the other way round leaves a modifier down on the remote.
        let sent = try transport.sentFrames.map { frame in
            try #require(
                JSONSerialization.jsonObject(with: Data(frame.utf8)) as? [String: Any]
            )
        }
        #expect(sent.map { $0["type"] as? String } == ["key", "key", "connect"])
        #expect(sent.map { $0["pressed"] as? Bool } == [true, false, nil])
        #expect(sent[2]["target"] as? String == "mac")
        await connection.stop()
    }

    /// A frame this build cannot read is one dropped frame, not a dropped
    /// session — the same call the gateway makes on a client message it cannot
    /// read. And an unknown *type* is not even that: it is delivered.
    @Test
    func undecodableFramesAreSkippedWithoutStallingTheLoop() async throws {
        let transport = FakeWebSocketTransport(
            inbound: [
                .text("this is not json"),
                .text(#"{"type":"resize","w":1}"#),
                .binary(Data([0x09, 0x09])),
                .text(#"{"type":"aNewMessage","v":1}"#),
                .text(#"{"type":"picker"}"#),
            ],
            closeCode: nil
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        let delivered = sink.trace.filter { $0.hasPrefix("control:") || $0.hasPrefix("frame:") }
        #expect(delivered == ["control:unsupported(aNewMessage)", "control:picker"])
        await connection.stop()
    }

    @Test
    func stoppingCancelsTheSocket() async throws {
        let transport = FakeWebSocketTransport(
            inbound: [.text(#"{"type":"picker"}"#)],
            closeCode: nil
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }
        await connection.stop()
        #expect(transport.wasCancelled)
    }
}
