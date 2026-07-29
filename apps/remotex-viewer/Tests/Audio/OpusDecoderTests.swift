import AVFoundation
import Foundation
import Testing
@testable import RemotexViewer

/// The decoder, against packets the **gateway's own encoder** produced.
///
/// This is the one test in either codebase where the two ends of the audio path meet:
/// the Rust side encodes and the Swift side decodes, so a disagreement about the rate,
/// the framing, the channel order or the pre-skip fails here. Everything else is each
/// end agreeing with itself — `opus_stream`'s round trip decodes what it just encoded
/// with libopus, and `AudioFrameTests` parses bytes it built.
///
/// The fixtures are hard-panned (tone left, silence right) because a channel *swap* and
/// a blend pass every other assertion. `write_swift_opus_fixtures` in
/// `src/opus_stream.rs` writes them; regenerate after changing the encoder.
struct OpusDecoderTests {
    /// 44 100 Hz stereo in, 48 kHz Opus out, and 40 packets of 20 ms.
    private static let expectedPackets = 40

    @Test
    func theGatewaysOwnPacketsDecodeToStereoPCM() throws {
        let decoder = try #require(OpusDecoder(format: try fixtureFormat()))
        let packets = try opusFixturePackets()
        #expect(packets.count >= 20, "the fixture is too short to be worth decoding")

        let buffer = try #require(decoder.decode(packets), "the system decoder refused")
        #expect(buffer.format.sampleRate == 48_000)
        #expect(buffer.format.channelCount == 2)
        // 960 frames per packet, less the pre-skip and the converter's own priming.
        let nominal = AVAudioFrameCount(packets.count) * 960
        #expect(buffer.frameLength > nominal - 1000)
        #expect(buffer.frameLength <= nominal)
    }

    /// The assertion the fixture is hard-panned *for*. Both channels present and one of
    /// them quiet is the only shape that rules out a blend, a swap, and a decoder that
    /// read the deinterleaved output as interleaved.
    @Test
    func theToneStaysOnTheLeftAndTheRightStaysSilent() throws {
        let decoder = try #require(OpusDecoder(format: try fixtureFormat()))
        let buffer = try #require(decoder.decode(try opusFixturePackets()))
        let channels = try #require(buffer.floatChannelData)

        let left = rms(channels[0], count: Int(buffer.frameLength))
        let right = rms(channels[1], count: Int(buffer.frameLength))
        #expect(left > 0.05, "the left channel should carry the tone, got \(left)")
        #expect(right * 10 < left, "the right channel should be far quieter: \(right) vs \(left)")
    }

    /// Fed the way a session feeds it — one call per wave buffer, on one decoder — the
    /// audio must come out continuous. What this catches is the converter losing frames
    /// on *every* call rather than only priming on the first: at five calls a second
    /// that is a skew that grows for as long as the session lasts, and one long listen
    /// is the only other way to notice.
    @Test
    func frameAfterFrameLosesNothingBeyondTheFirstCallsPriming() throws {
        let decoder = try #require(OpusDecoder(format: try fixtureFormat()))
        let packets = try opusFixturePackets()
        // Nine packets a call, which is a 32 KiB RDP wave buffer's worth.
        let frames = stride(from: 0, to: packets.count - 9, by: 9).map {
            Array(packets[$0 ..< ($0 + 9)])
        }
        #expect(frames.count >= 3, "not enough frames to see a per-call loss")

        var lengths: [AVAudioFrameCount] = []
        for frame in frames {
            lengths.append(try #require(decoder.decode(frame)).frameLength)
        }
        // The first call pays the priming and the pre-skip; every one after it returns
        // its packets in full.
        for length in lengths.dropFirst() {
            #expect(length == 9 * 960, "a later call returned \(length)")
        }
        #expect(lengths[0] < 9 * 960, "the first call should pay the pre-skip")
    }

    /// There is one codec on this wire and no fallback in either direction, so a gateway
    /// describing something else is refused here — no sound and a line in the log —
    /// rather than having its packets fed to a decoder built for Opus.
    @Test
    func anUnknownCodecIsRefusedRatherThanGuessed() throws {
        let head = try opusFixtureHead()
        #expect(
            OpusDecoder(
                format: .init(codec: "vorbis", sampleRate: 48_000, channels: 2, head: head)
            ) == nil
        )
        #expect(
            OpusDecoder(
                format: .init(codec: "opus", sampleRate: 48_000, channels: 2, head: head)
            ) != nil
        )
    }

    /// A head too short to hold a pre-skip costs 6.5 ms of near-silence, not a refusal
    /// to play: the sound is what the user asked for and the delay is inaudible.
    @Test
    func aHeadWithNoPreSkipInItStillPlays() throws {
        let decoder = try #require(
            OpusDecoder(
                format: .init(codec: "opus", sampleRate: 48_000, channels: 2, head: Data([1, 2]))
            )
        )
        let buffer = try #require(decoder.decode(try opusFixturePackets()))
        #expect(buffer.frameLength > 0)
    }

    // MARK: - Fixtures

    private func fixtureFormat() throws -> ServerMessage.AudioFormat {
        .init(codec: "opus", sampleRate: 48_000, channels: 2, head: try opusFixtureHead())
    }

    private func opusFixtureHead() throws -> Data {
        let head = try fixture("head")
        #expect(head.count == 19)
        #expect(head.prefix(8) == Data("OpusHead".utf8))
        return head
    }

    /// The fixture's packets, read with the wire's own `u16` framing — the same layout
    /// `AudioFrame` parses, so the fixture needs no format of its own.
    private func opusFixturePackets() throws -> [Data] {
        let blob = try fixture("packets")
        var packets: [Data] = []
        var at = 0
        while at + 2 <= blob.count {
            let length = Int(blob[blob.startIndex + at]) | (Int(blob[blob.startIndex + at + 1]) << 8)
            at += 2
            guard at + length <= blob.count else {
                Issue.record("the fixture's framing is truncated")
                return packets
            }
            packets.append(blob.subdata(in: (blob.startIndex + at) ..< (blob.startIndex + at + length)))
            at += length
        }
        #expect(packets.count == Self.expectedPackets)
        return packets
    }

    private func fixture(_ name: String) throws -> Data {
        let url = try #require(
            Bundle.module.url(forResource: "Fixtures/opus/\(name)", withExtension: "bin"),
            """
            missing fixture opus/\(name).bin — regenerate with
            `cargo test --lib -- --ignored --nocapture swift_opus_fixtures`
            """
        )
        return try Data(contentsOf: url)
    }

    private func rms(_ samples: UnsafePointer<Float>, count: Int) -> Double {
        guard count > 0 else {
            return 0
        }
        var total = 0.0
        for index in 0 ..< count {
            total += Double(samples[index]) * Double(samples[index])
        }
        return (total / Double(count)).squareRoot()
    }
}
