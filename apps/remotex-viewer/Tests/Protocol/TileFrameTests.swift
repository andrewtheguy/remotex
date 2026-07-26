import Foundation
import Testing
@testable import RemotexViewer

struct TileFrameTests {
    @Test
    func aWellFormedHeaderDecodes() throws {
        let payload: [UInt8] = [0xDE, 0xAD, 0xBE, 0xEF]
        let tile = try #require(
            TileFrame.decode(frame(format: 2, x: 16, y: 320, w: 1280, h: 64, payload: payload))
        )
        #expect(tile.format == .jpeg)
        #expect(tile.x == 16)
        #expect(tile.y == 320)
        #expect(tile.w == 1280)
        #expect(tile.h == 64)
        #expect(Array(tile.payload) == payload)
    }

    /// Coordinates are u16 and full-width strips reach the ceiling on real
    /// desktops, so the top of the range must not wrap or sign-extend.
    @Test
    func coordinatesAtTheUInt16CeilingSurvive() throws {
        let tile = try #require(
            TileFrame.decode(
                frame(format: 1, x: 0xFFFF, y: 0xFFFF, w: 0xFFFF, h: 0xFFFF, payload: [0x01])
            )
        )
        #expect(tile.x == 65535)
        #expect(tile.y == 65535)
        #expect(tile.w == 65535)
        #expect(tile.h == 65535)
    }

    @Test
    func aFrameShorterThanTheHeaderIsRejected() {
        for count in 0 ..< TileFrame.headerLength {
            #expect(TileFrame.decode(Data(repeating: 0x01, count: count)) == nil)
        }
    }

    @Test
    func anUnknownFrameKindIsRejected() {
        for kind: UInt8 in [0x00, 0x02, 0xFF] {
            #expect(TileFrame.decode(frame(kind: kind, format: 1, w: 8, h: 8)) == nil)
        }
    }

    @Test
    func anUnknownFormatIsRejected() {
        for format: UInt8 in [0x00, 0x03, 0xFF] {
            #expect(TileFrame.decode(frame(format: format, w: 8, h: 8)) == nil)
        }
    }

    /// An empty payload is structurally a frame; whether those bytes are an
    /// image is `TileDecoder`'s question, not this one's.
    @Test
    func aHeaderWithNoPayloadDecodesToAnEmptyPayload() throws {
        let tile = try #require(TileFrame.decode(frame(format: 1, w: 8, h: 8)))
        #expect(tile.payload.isEmpty)
    }

    /// A `Data` cut out of a larger buffer keeps the indices it was cut at, so a
    /// decoder that reads by subscript would misparse one. This is the test for
    /// that, and it is the reason the header is read through a raw buffer.
    @Test
    func aSliceWithANonZeroStartIndexDecodesTheSameWay() throws {
        let payload: [UInt8] = [0x11, 0x22, 0x33]
        let whole = frame(format: 1, x: 7, y: 9, w: 64, h: 32, payload: payload)
        var padded = Data(repeating: 0xAA, count: 5)
        padded.append(whole)
        let slice = padded.dropFirst(5)

        #expect(slice.startIndex != 0, "the slice has to be offset for this to test anything")
        #expect(TileFrame.decode(slice) == TileFrame.decode(whole))
        let tile = try #require(TileFrame.decode(slice))
        #expect(tile.x == 7)
        #expect(tile.y == 9)
        #expect(Array(tile.payload) == payload)
    }

    private func frame(
        kind: UInt8 = TileFrame.frameKind,
        format: UInt8 = 1,
        x: UInt16 = 0,
        y: UInt16 = 0,
        w: UInt16 = 0,
        h: UInt16 = 0,
        payload: [UInt8] = []
    ) -> Data {
        var data = Data([kind, format])
        for value in [x, y, w, h] {
            data.append(UInt8(value & 0xFF))
            data.append(UInt8(value >> 8))
        }
        data.append(contentsOf: payload)
        return data
    }
}
