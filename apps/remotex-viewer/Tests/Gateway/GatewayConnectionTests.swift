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
    /// The invariant the whole receive loop is shaped around. Tiles decode
    /// asynchronously; a `resize` that overtook the tiles queued ahead of it
    /// would blit stale pixels into a freshly allocated texture, and two
    /// reordered tiles leave the older one on screen. Tiles carry no delta state,
    /// so nothing downstream can repair either.
    ///
    /// Audio is in the stream because it shares the invariant for a second reason: an
    /// `audioFormat` that arrived after the packets it configures configures nothing,
    /// and Opus packets carry inter-packet state, so a reordered pair is a decoder
    /// running on wrong history.
    @Test
    func framesReachTheSinkInArrivalOrderAcrossTheAsyncTileDecode() async throws {
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
            $0.hasPrefix("control:") || $0.hasPrefix("tiles:") || $0.hasPrefix("audio:")
        }
        #expect(
            interesting == [
                "control:connected(mac)",
                "control:resize(64x64@2.0x)",
                "tiles:0,0,2x2",
                "control:audioFormat(opus)",
                "audio:240",
                "tiles:8,16,2x2",
                "control:remoteOs(true)",
                "audio:12|1",
                "tiles:32,48,2x2",
                "control:picker",
            ]
        )
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

        let delivered = sink.trace.filter {
            $0.hasPrefix("control:") || $0.hasPrefix("tiles:") || $0.hasPrefix("audio:")
        }
        #expect(delivered == ["tiles:0,0,2x2", "control:picker"])
        await connection.stop()
    }

    /// A batch reaches the sink as one event, in wire order.
    ///
    /// One event per frame is what lets the renderer ask for a single redraw per
    /// frame; delivering tile by tile would put that back to one per tile, and no
    /// pixel assertion downstream could tell the difference.
    @Test
    func aBatchReachesTheSinkAsOneEventInWireOrder() async throws {
        let batch = batchFrame([
            try tileRecord(x: 0, y: 0),
            try tileRecord(x: 8, y: 0),
            try tileRecord(x: 16, y: 32),
        ])
        let transport = FakeWebSocketTransport(
            inbound: [
                .text(#"{"type":"resize","w":64,"h":64,"scale":1.0}"#),
                .binary(batch),
                .text(#"{"type":"picker"}"#),
            ],
            closeCode: nil
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        #expect(
            sink.trace.filter { $0.hasPrefix("tiles:") }
                == ["tiles:0,0,2x2|8,0,2x2|16,32,2x2"]
        )
        await connection.stop()
    }

    /// One record this build cannot decode must not cost the rest of its batch:
    /// those tiles decoded, and the pixels they cover would stay stale until
    /// something else happened to repaint them.
    @Test
    func anUndecodableRecordIsDroppedWithoutItsBatch() async throws {
        // A structurally valid record whose payload is not an image.
        var garbage = Data([BatchFrame.opTile, TileFormat.png.rawValue])
        for value: UInt16 in [BatchFrame.noSlot, 8, 8, 2, 2] {
            garbage.append(UInt8(value & 0xFF))
            garbage.append(UInt8(value >> 8))
        }
        garbage.append(contentsOf: [0x03, 0x00, 0x00, 0x00, 0xDE, 0xAD, 0xBE])
        let batch = batchFrame([
            try tileRecord(x: 0, y: 0),
            garbage,
            try tileRecord(x: 16, y: 0),
        ])
        let transport = FakeWebSocketTransport(
            inbound: [
                .text(#"{"type":"resize","w":64,"h":64,"scale":1.0}"#),
                .binary(batch),
                .text(#"{"type":"picker"}"#),
            ],
            closeCode: nil
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        #expect(
            sink.trace.filter { $0.hasPrefix("tiles:") } == ["tiles:0,0,2x2|16,0,2x2"]
        )
        await connection.stop()
    }

    /// A video target has to be refused out loud. This build cannot decode H.264, and
    /// a video target sends nothing else — so dropping the records the way an
    /// undecodable *tile* is dropped would leave a desktop that simply never paints,
    /// with the reason only in the log.
    @Test
    func aVideoTargetIsRefusedByNameRatherThanShowingNothing() async throws {
        let batch = batchFrame([
            tileRecord(x: 0, y: 0, w: 64, h: 64, format: .h264, payload: Data([0, 0, 0, 1, 0x67]))
        ])
        let transport = FakeWebSocketTransport(
            inbound: [
                .text(#"{"type":"resize","w":64,"h":64,"scale":1.0}"#),
                .binary(batch),
                .binary(batch),
                // A sentinel behind the second batch. Waiting on the rejection alone
                // would be satisfied by the *first* batch, so "said once" would pass
                // without the second batch having been looked at — which is the half
                // of the latch worth testing. The receive loop is strictly ordered,
                // so this control message arriving means both batches are done.
                .text(#"{"type":"picker"}"#),
            ],
            closeCode: nil
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        let reasons = sink.events.compactMap { event -> String? in
            if case .rejected(let reason) = event { reason } else { nil }
        }
        #expect(reasons.count == 1, "one unusable target should say so once, not once a batch")
        #expect(reasons[0].contains("H.264"), "the message should name what arrived")
        #expect(reasons[0].contains("browser"), "the message should say where it does work")
        #expect(
            !sink.trace.contains { $0.hasPrefix("tiles:") },
            "an access unit reached the renderer"
        )
        await connection.stop()
    }

    /// The tile cache's whole claim, from the side that has to honour it: seven
    /// bytes become the pixels a slot already holds, at a new position.
    ///
    /// Asserted on the decoded bytes and not only on geometry, because resolving a
    /// reference to the wrong slot would put the same rectangle in the trace. Both
    /// lifetimes are covered: a slot filled by an earlier batch, which is what the
    /// cache exists for, and one filled earlier in the reference's own batch, which
    /// the gateway emits because records are applied in order.
    @Test
    func aReferenceRedrawsTheCachedPixelsAtItsOwnPosition() async throws {
        let stored = batchFrame([try tileRecord(x: 0, y: 0, red: 0x11, slot: 3)])
        let reused = batchFrame([
            referenceRecord(slot: 3, x: 8, y: 16),
            try tileRecord(x: 0, y: 32, red: 0x22, slot: 4),
            referenceRecord(slot: 4, x: 24, y: 32),
        ])
        let transport = FakeWebSocketTransport(
            inbound: [
                .text(#"{"type":"resize","w":64,"h":64,"scale":1.0}"#),
                .binary(stored),
                .binary(reused),
                .text(#"{"type":"picker"}"#),
            ],
            closeCode: nil
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        #expect(
            sink.trace.filter { $0.hasPrefix("tiles:") } == [
                "tiles:0,0,2x2",
                "tiles:8,16,2x2|0,32,2x2|24,32,2x2",
            ]
        )

        let painted = sink.events.compactMap { event -> [DecodedTile]? in
            if case .tiles(let tiles) = event { tiles } else { nil }
        }
        try #require(painted.count == 2)
        let cached = try #require(painted[0].first)
        let second = painted[1]
        #expect(!cached.bgra.isEmpty)
        #expect(second[0].bgra == cached.bgra, "slot 3's bytes, at 8,16")
        #expect(second[2].bgra == second[1].bgra, "slot 4's, filled this same batch")
        #expect(second[1].bgra != cached.bgra, "different colours, so different slots")
        await connection.stop()
    }

    /// A reference into a slot this client does not hold cannot be recovered from
    /// here: the gateway believes the slot is full, and nothing else would ever
    /// correct that. So the client says so — once per batch, because fifty stale
    /// references are one disagreement, not fifty.
    @Test
    func referencesIntoAnEmptyCacheAskForOneResetPerBatch() async throws {
        let batch = batchFrame([
            try tileRecord(x: 0, y: 0, slot: 1),
            referenceRecord(slot: 9, x: 8, y: 0), // never filled
            referenceRecord(slot: 10, x: 16, y: 0), // nor this
            referenceRecord(slot: 1, x: 24, y: 0), // filled by this very batch
        ])
        let transport = FakeWebSocketTransport(
            inbound: [
                .text(#"{"type":"resize","w":64,"h":64,"scale":1.0}"#),
                .binary(batch),
                .text(#"{"type":"picker"}"#),
            ],
            closeAfterDraining: false
        )
        let gateway = FakeGateway(claims: [.claimed("tok-1")], sockets: [transport])
        let sink = RecordingSink()
        let connection = GatewayConnection(gateway: gateway, sink: sink)

        await connection.start()
        await sink.wait { $0.contains { if case .control(.picker) = $0 { true } else { false } } }

        // A sentinel behind whatever the batch enqueued: the outbound queue is
        // FIFO, so a `refresh` that reached the socket means every reset the batch
        // asked for has too. Waiting on a count instead would be asserting that
        // nothing more arrived, over a wall clock — true on a fast machine and a
        // flake on a slow one.
        connection.send(.refresh)
        await sink.wait { _ in transport.sentFrames.contains { $0.contains("refresh") } }

        let sent = try transport.sentFrames.map { frame in
            try #require(
                JSONSerialization.jsonObject(with: Data(frame.utf8)) as? [String: Any]
            )
        }
        #expect(
            sent.compactMap { $0["type"] as? String }
                .filter { $0 == "cacheReset" || $0 == "refresh" } == ["cacheReset", "refresh"]
        )
        // And the record naming a slot this batch filled itself still painted: the
        // cache is emptied after the pass over the records, not in the middle of
        // one, or a miss would take a hit down with it.
        #expect(
            sink.trace.filter { $0.hasPrefix("tiles:") } == ["tiles:0,0,2x2|24,0,2x2"]
        )
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

        let delivered = sink.trace.filter { $0.hasPrefix("control:") || $0.hasPrefix("tiles:") }
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
