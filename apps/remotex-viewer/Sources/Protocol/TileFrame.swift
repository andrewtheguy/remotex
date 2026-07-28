import Foundation

enum TileFormat: UInt8, Sendable, Equatable {
    case png = 1
    case jpeg = 2
}

/// One dirty rectangle of the framebuffer, as one `TILE` record inside a batch
/// frame. `Tile` in `src/protocol.rs`.
///
/// There is no delta encoding and no inter-frame state: every tile overwrites its
/// rectangle outright. Records are at most `CELL_H` (64) rows tall, but nothing
/// here depends on that.
struct TileFrame: Sendable, Equatable {
    let format: TileFormat
    /// Where the gateway wants this remembered, or `BatchFrame.noSlot` for "do
    /// not". Parsed but unused until this client keeps a tile cache.
    let slot: UInt16
    let x: UInt16
    let y: UInt16
    let w: UInt16
    let h: UInt16
    let payload: Data
}

/// A server -> client binary frame: a batch of records.
///
/// Layout (little-endian, matching `batch` in `src/protocol.rs`):
///
/// ```text
/// offset 0: u8  frame kind, always 0x02 (batch)
/// offset 1: u8  flags, always 0
/// offset 2: u16 record count
/// offset 4: records, back to back
///
/// TILE (op 0x01): u8 format | u16 slot | u16 x | u16 y | u16 w | u16 h
///                 | u32 len | payload[len]
/// ```
///
/// One frame carries however many tiles were ready at once, which is what a full
/// repaint needs: a desktop is dozens of cells, and one WebSocket frame each
/// costs a receive, a decode and a paint apiece.
enum BatchFrame {
    static let frameKind: UInt8 = 0x02
    static let headerLength = 4
    static let opTile: UInt8 = 0x01
    static let tileHeaderLength = 16
    /// `slot` meaning "draw this and do not remember it".
    static let noSlot: UInt16 = 0xFFFF

    /// Parse a binary frame into its tile records, or nil for anything malformed
    /// or of an unknown kind, op or format.
    ///
    /// Same contract as `decodeBatchFrame` in `frontend/src/protocol.ts`: a bad
    /// frame yields nothing at all rather than the records read before the bad
    /// one. Half a repaint leaves the framebuffer in a state nothing corrects,
    /// where dropping the frame costs one refresh.
    ///
    /// The record count in the header is what makes a *truncated* frame
    /// detectable: records are self-delimiting, so without it a short read would
    /// parse cleanly as a complete but smaller batch.
    static func decode(_ frame: Data) -> [TileFrame]? {
        guard frame.count >= headerLength else {
            return nil
        }
        // Read through the raw buffer rather than by subscript: `frame` may be a
        // slice, whose indices start wherever it was cut from, and the multi-byte
        // fields are not aligned, so the loads have to be unaligned ones.
        return frame.withUnsafeBytes { raw -> [TileFrame]? in
            guard raw[0] == frameKind, raw[1] == 0 else {
                return nil
            }
            let u16 = { (at: Int) -> UInt16 in
                UInt16(littleEndian: raw.loadUnaligned(fromByteOffset: at, as: UInt16.self))
            }
            let count = Int(u16(2))

            var tiles: [TileFrame] = []
            tiles.reserveCapacity(count)
            var at = headerLength
            while at < raw.count {
                guard at + tileHeaderLength <= raw.count, raw[at] == opTile,
                      let format = TileFormat(rawValue: raw[at + 1])
                else {
                    return nil
                }
                let length = Int(
                    UInt32(
                        littleEndian: raw.loadUnaligned(
                            fromByteOffset: at + 12,
                            as: UInt32.self
                        )
                    )
                )
                let start = at + tileHeaderLength
                guard start + length <= raw.count else {
                    return nil
                }
                tiles.append(
                    TileFrame(
                        format: format,
                        slot: u16(at + 2),
                        x: u16(at + 4),
                        y: u16(at + 6),
                        w: u16(at + 8),
                        h: u16(at + 10),
                        // Re-based to index 0, so nothing downstream has to know
                        // this came out of the middle of a larger buffer.
                        payload: Data(raw[start..<(start + length)])
                    )
                )
                at = start + length
            }
            return tiles.count == count ? tiles : nil
        }
    }
}
