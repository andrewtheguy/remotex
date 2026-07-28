import Foundation
import Testing
@testable import RemotexViewer

struct TileFrameTests {
    /// The tile records of a batch, for the cases that are only about tiles.
    /// References are covered on their own below.
    private func tileRecords(_ frame: Data) -> [TileFrame]? {
        guard let records = BatchFrame.decode(frame) else {
            return nil
        }
        return records.compactMap { record in
            guard case .tile(let tile) = record else {
                return nil
            }
            return tile
        }
    }

    @Test
    func aWellFormedRecordDecodes() throws {
        let payload: [UInt8] = [0xDE, 0xAD, 0xBE, 0xEF]
        let tiles = try #require(
            tileRecords(batch(tile(format: 2, slot: 5, x: 16, y: 320, w: 1280, h: 64, payload: payload)))
        )
        #expect(tiles.count == 1)
        let tile = tiles[0]
        #expect(tile.format == .jpeg)
        #expect(tile.slot == 5)
        #expect(tile.x == 16)
        #expect(tile.y == 320)
        #expect(tile.w == 1280)
        #expect(tile.h == 64)
        #expect(Array(tile.payload) == payload)
    }

    /// The point of the envelope: many records, one frame, in order.
    @Test
    func everyRecordInABatchDecodesInOrder() throws {
        let tiles = try #require(
            tileRecords(
                batch(
                    tile(y: 0, w: 320, h: 64, payload: [0x01]),
                    tile(y: 64, w: 320, h: 64, payload: [0x02, 0x03]),
                    tile(y: 128, w: 320, h: 64, payload: [])
                )
            )
        )
        #expect(tiles.map(\.y) == [0, 64, 128])
        #expect(tiles.map { Array($0.payload) } == [[0x01], [0x02, 0x03], []])
    }

    /// Coordinates are u16 and full-width strips reach the ceiling on real
    /// desktops, so the top of the range must not wrap or sign-extend.
    @Test
    func coordinatesAtTheUInt16CeilingSurvive() throws {
        let tiles = try #require(
            tileRecords(
                batch(
                    tile(format: 1, x: 0xFFFF, y: 0xFFFF, w: 0xFFFF, h: 0xFFFF, payload: [0x01])
                )
            )
        )
        #expect(tiles[0].x == 65535)
        #expect(tiles[0].y == 65535)
        #expect(tiles[0].w == 65535)
        #expect(tiles[0].h == 65535)
        #expect(tiles[0].slot == BatchFrame.noSlot)
    }

    @Test
    func aFrameShorterThanTheHeaderIsRejected() {
        for count in 0 ..< BatchFrame.headerLength {
            #expect(BatchFrame.decode(Data(repeating: 0x02, count: count)) == nil)
        }
    }

    /// An empty batch is well formed and means nothing to paint.
    @Test
    func anEmptyBatchDecodesToNoRecords() throws {
        let records = try #require(BatchFrame.decode(batch()))
        #expect(records.isEmpty)
    }

    @Test
    func anUnknownFrameKindIsRejected() {
        for kind: UInt8 in [0x00, 0x01, 0x03, 0xFF] {
            #expect(BatchFrame.decode(batch(kind: kind, tile(w: 8, h: 8))) == nil)
        }
    }

    /// Reserved bits are *rejected*, not ignored. A client that skipped a flag it
    /// did not understand could never be told anything by one later.
    @Test
    func nonZeroFlagsAreRejected() {
        for flags: UInt8 in [0x01, 0x80, 0xFF] {
            #expect(BatchFrame.decode(batch(flags: flags, tile(w: 8, h: 8))) == nil)
        }
    }

    @Test
    func anUnknownRecordOpIsRejected() {
        for op: UInt8 in [0x00, 0x02, 0xFF] {
            #expect(BatchFrame.decode(batch(tile(op: op, w: 8, h: 8))) == nil)
        }
    }

    @Test
    func anUnknownFormatIsRejected() {
        for format: UInt8 in [0x00, 0x03, 0xFF] {
            #expect(BatchFrame.decode(batch(tile(format: format, w: 8, h: 8))) == nil)
        }
    }

    /// A truncated frame is only detectable because the header carries a count:
    /// records are self-delimiting, so a short read would otherwise parse cleanly
    /// as a complete but smaller batch and paint half a repaint.
    @Test
    func aTruncatedFrameIsRejectedRatherThanPartlyApplied() {
        let whole = batch(
            tile(y: 0, w: 8, h: 8, payload: [0x01, 0x02, 0x03, 0x04]),
            tile(y: 64, w: 8, h: 8, payload: [0x05, 0x06, 0x07, 0x08])
        )
        for cut in 1 ..< whole.count {
            #expect(
                BatchFrame.decode(whole.prefix(cut)) == nil,
                "a frame cut at \(cut) of \(whole.count) bytes must not decode"
            )
        }
    }

    /// A payload length that claims more than the frame holds must not be trusted
    /// into a read past the end.
    @Test
    func aPayloadLengthPastTheEndOfTheFrameIsRejected() {
        var record = tile(w: 8, h: 8, payload: [0x01, 0x02])
        // Overwrite the u32 length with something far larger than the payload.
        record.replaceSubrange(12 ..< 16, with: [0xFF, 0xFF, 0x00, 0x00])
        #expect(BatchFrame.decode(batch(record)) == nil)
    }

    /// An empty payload is structurally a record; whether those bytes are an
    /// image is `TileDecoder`'s question, not this one's.
    @Test
    func aRecordWithNoPayloadDecodesToAnEmptyPayload() throws {
        let decoded = try #require(tileRecords(batch(tile(format: 1, w: 8, h: 8))))
        #expect(decoded[0].payload.isEmpty)
    }

    /// A `Data` cut out of a larger buffer keeps the indices it was cut at, so a
    /// decoder that read by subscript would misparse one. This is the test for
    /// that, and it is the reason the frame is read through a raw buffer.
    @Test
    func aSliceWithANonZeroStartIndexDecodesTheSameWay() throws {
        let payload: [UInt8] = [0x11, 0x22, 0x33]
        let whole = batch(tile(format: 1, x: 7, y: 9, w: 64, h: 32, payload: payload))
        var padded = Data(repeating: 0xAA, count: 5)
        padded.append(whole)
        let slice = padded.dropFirst(5)

        #expect(slice.startIndex != 0, "the slice has to be offset for this to test anything")
        #expect(BatchFrame.decode(slice) == BatchFrame.decode(whole))
        let decoded = try #require(tileRecords(slice))
        #expect(decoded[0].x == 7)
        #expect(decoded[0].y == 9)
        #expect(Array(decoded[0].payload) == payload)
    }

    /// A reference decodes to a slot and a position, and nothing else — the size
    /// and the pixels come from whatever the client kept there.
    @Test
    func aReferenceDecodesToItsSlotAndPosition() throws {
        let records = try #require(BatchFrame.decode(batch(reference(slot: 7, x: 320, y: 64))))
        #expect(records == [.reference(slot: 7, x: 320, y: 64)])
    }

    /// References and tiles mix freely in one batch, in wire order: a reference may
    /// name a slot a tile earlier in the same batch has just filled.
    @Test
    func aBatchMixesTilesAndReferencesInOrder() throws {
        let records = try #require(
            BatchFrame.decode(
                batch(
                    tile(slot: 3, x: 0, y: 0, w: 8, h: 8, payload: [0x01]),
                    reference(slot: 3, x: 8, y: 0),
                    tile(slot: 4, x: 0, y: 8, w: 8, h: 8, payload: [0x02])
                )
            )
        )
        #expect(records.count == 3)
        #expect(records[1] == .reference(slot: 3, x: 8, y: 0))
        guard case .tile(let last) = records[2] else {
            Issue.record("the third record should be a tile")
            return
        }
        #expect(last.slot == 4)
    }

    /// A slot outside the fixed cache is a malformed record, not a reason to grow an
    /// array — that is what keeps this client's memory a function of the protocol
    /// rather than of what a gateway sends it.
    @Test
    func aSlotOutsideTheCacheIsRejected() {
        #expect(BatchFrame.decode(batch(reference(slot: BatchFrame.slotCount, x: 0, y: 0))) == nil)
        #expect(BatchFrame.decode(batch(reference(slot: 0xFFFE, x: 0, y: 0))) == nil)
        #expect(
            BatchFrame.decode(batch(tile(slot: BatchFrame.slotCount, w: 8, h: 8))) == nil,
            "including on a tile that asks to be stored there"
        )
        // `noSlot` is the exception: it is "do not remember this", not a slot.
        #expect(BatchFrame.decode(batch(tile(slot: BatchFrame.noSlot, w: 8, h: 8))) != nil)
    }

    @Test
    func aTruncatedReferenceIsRejected() {
        let whole = batch(reference(slot: 1, x: 2, y: 3))
        for cut in 1 ..< whole.count {
            #expect(
                BatchFrame.decode(whole.prefix(cut)) == nil,
                "a frame cut at \(cut) of \(whole.count) bytes must not decode"
            )
        }
    }

    /// One `TILE_REF` record, as bytes.
    private func reference(slot: UInt16, x: UInt16, y: UInt16) -> Data {
        var data = Data([BatchFrame.opTileRef])
        for value in [slot, x, y] {
            data.append(UInt8(value & 0xFF))
            data.append(UInt8(value >> 8))
        }
        return data
    }

    /// One `TILE` record, as bytes.
    private func tile(
        op: UInt8 = BatchFrame.opTile,
        format: UInt8 = 1,
        slot: UInt16 = BatchFrame.noSlot,
        x: UInt16 = 0,
        y: UInt16 = 0,
        w: UInt16 = 0,
        h: UInt16 = 0,
        payload: [UInt8] = []
    ) -> Data {
        var data = Data([op, format])
        for value in [slot, x, y, w, h] {
            data.append(UInt8(value & 0xFF))
            data.append(UInt8(value >> 8))
        }
        let length = UInt32(payload.count)
        for shift in [0, 8, 16, 24] {
            data.append(UInt8((length >> shift) & 0xFF))
        }
        data.append(contentsOf: payload)
        return data
    }

    /// A batch frame wrapping `records`, whose count the header reports honestly.
    private func batch(
        kind: UInt8 = BatchFrame.frameKind,
        flags: UInt8 = 0,
        _ records: Data...
    ) -> Data {
        var data = Data([kind, flags])
        let count = UInt16(records.count)
        data.append(UInt8(count & 0xFF))
        data.append(UInt8(count >> 8))
        for record in records {
            data.append(record)
        }
        return data
    }
}
