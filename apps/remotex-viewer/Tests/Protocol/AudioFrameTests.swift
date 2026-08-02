import Foundation
import Testing
@testable import RemotexViewer

/// The audio frame's parser, against bytes built here rather than by the gateway.
///
/// Hand-built on purpose: this is the layout in `src/protocol.rs` written out a second
/// time, so a parser that drifted from it disagrees with these bytes instead of
/// agreeing with a fixture the same drift produced. `audioSchedule.test.ts` and the
/// canvas page's own decoder are the other
/// half — real packets from the gateway's own encoder — and between them the wire is
/// checked both for its framing and for its contents.
struct AudioFrameTests {
    @Test
    func aFrameYieldsItsPacketsInOrder() {
        let packets = AudioFrame.decode(frame(packets: [Data([1, 2, 3]), Data([4, 5])]))
        #expect(packets == [Data([1, 2, 3]), Data([4, 5])])
    }

    /// One packet is the tone harness's cadence (20 ms buffers); nine or ten is a live
    /// host's (32 KiB wave buffers). Both are ordinary, and neither is special-cased.
    @Test(arguments: [1, 9, 10, 64])
    func anyPacketCountRoundTrips(count: Int) {
        let sent = (0 ..< count).map { Data(repeating: UInt8($0 % 251), count: 240 + $0) }
        #expect(AudioFrame.decode(frame(packets: sent)) == sent)
    }

    /// Well formed, and nothing to play. The gateway does not send these — it skips an
    /// empty buffer — but "zero packets" is representable, so the parser has to answer
    /// for it rather than the caller guessing which of the two nils it got.
    @Test
    func anEmptyFrameIsWellFormedAndCarriesNothing() {
        #expect(AudioFrame.decode(frame(packets: [])) == [])
    }

    /// A batch frame is the realistic wrong kind, and it must not be half-parsed as
    /// audio: its `u16` record count would read as a packet length.
    @Test
    func aBatchFrameIsNotAnAudioFrame() {
        var batch = Data([BatchFrame.frameKind, 0, 1, 0])
        batch.append(contentsOf: [BatchFrame.opTileRef, 0, 0, 0, 0, 0, 0])
        #expect(AudioFrame.decode(batch) == nil)
    }

    @Test
    func unknownFlagsAreRefused() {
        var frame = self.frame(packets: [Data([9])])
        frame[1] = 1
        #expect(AudioFrame.decode(frame) == nil)
    }

    /// The count is what makes truncation detectable at all: packets are
    /// self-delimiting, so a short read would otherwise parse cleanly as a complete
    /// frame that simply carried fewer.
    @Test
    func aTruncatedFrameIsRefusedRatherThanReadShort() {
        let whole = frame(packets: [Data([1, 2, 3]), Data([4, 5, 6])])
        // Every prefix short of the whole thing: one of them cuts the header, one cuts
        // a length field in half, one cuts a payload.
        for length in 0 ..< whole.count {
            #expect(
                AudioFrame.decode(whole.prefix(length)) == nil,
                "a \(length)-byte prefix decoded"
            )
        }
        #expect(AudioFrame.decode(whole) != nil)
    }

    /// The count disagreeing with the packets present, in both directions. This is the
    /// header lying rather than the frame being cut, which truncation cannot produce.
    @Test(arguments: [0, 1, 3, 0xFFFF])
    func aCountThatDisagreesIsRefused(claimed: Int) {
        var frame = self.frame(packets: [Data([1]), Data([2])])
        frame.replaceSubrange(2 ..< 4, with: withUnsafeBytes(of: UInt16(claimed).littleEndian) {
            Data($0)
        })
        #expect(AudioFrame.decode(frame) == nil)
    }

    @Test
    func aHeaderWithNoFrameBehindItIsRefused() {
        #expect(AudioFrame.decode(Data()) == nil)
        #expect(AudioFrame.decode(Data([AudioFrame.frameKind, 0, 0])) == nil)
    }

    /// Data handed to the parser may be a slice of a larger buffer, whose indices start
    /// wherever it was cut from. A parser reading by subscript from 0 crashes or reads
    /// the wrong bytes; one reading through the raw buffer does not.
    @Test
    func aSlicedFrameParsesLikeAWholeOne() {
        let sent = [Data([7, 7, 7]), Data([8])]
        var padded = Data([0xAA, 0xBB, 0xCC])
        padded.append(frame(packets: sent))
        #expect(AudioFrame.decode(padded.dropFirst(3)) == sent)
    }

    // MARK: - The layout, written out from src/protocol.rs

    private func frame(packets: [Data]) -> Data {
        var frame = Data([AudioFrame.frameKind, 0])
        frame.append(u16(packets.count))
        for packet in packets {
            frame.append(u16(packet.count))
            frame.append(packet)
        }
        return frame
    }

    private func u16(_ value: Int) -> Data {
        withUnsafeBytes(of: UInt16(value).littleEndian) { Data($0) }
    }
}
