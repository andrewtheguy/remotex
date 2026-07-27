import Foundation

enum TileFormat: UInt8, Sendable, Equatable {
    case png = 1
    case jpeg = 2
}

/// One dirty rectangle of the framebuffer, as it arrives in a binary WebSocket
/// frame. `Tile` in `src/protocol.rs`; layout (little-endian):
///
/// ```text
/// offset 0:  u8  frame kind, always 0x01
/// offset 1:  u8  format: 1 = PNG, 2 = JPEG
/// offset 2:  u16 x
/// offset 4:  u16 y
/// offset 6:  u16 w
/// offset 8:  u16 h
/// offset 10: payload (a complete PNG or JPEG stream)
/// ```
///
/// There is no delta encoding and no inter-frame state: every tile overwrites
/// its rectangle outright. Strips are at most `STRIP_ROWS` (64) rows tall, but
/// nothing here depends on that.
struct TileFrame: Sendable, Equatable {
    static let frameKind: UInt8 = 0x01
    static let headerLength = 10

    let format: TileFormat
    let x: UInt16
    let y: UInt16
    let w: UInt16
    let h: UInt16
    let payload: Data

    /// Parse a binary frame, or nil for anything malformed or of an unknown
    /// kind or format — the same contract as `decodeTileFrame` in
    /// `frontend/src/protocol.ts`, whose callers drop what they can't read.
    static func decode(_ frame: Data) -> TileFrame? {
        guard frame.count >= headerLength else {
            return nil
        }
        // Read through the raw buffer rather than by subscript: `frame` may be a
        // slice, whose indices start wherever it was cut from, and offsets 2/4/6/8
        // are not 2-byte aligned so the loads have to be unaligned ones.
        let header = frame.withUnsafeBytes { raw in
            (
                kind: raw[0],
                format: raw[1],
                x: UInt16(littleEndian: raw.loadUnaligned(fromByteOffset: 2, as: UInt16.self)),
                y: UInt16(littleEndian: raw.loadUnaligned(fromByteOffset: 4, as: UInt16.self)),
                w: UInt16(littleEndian: raw.loadUnaligned(fromByteOffset: 6, as: UInt16.self)),
                h: UInt16(littleEndian: raw.loadUnaligned(fromByteOffset: 8, as: UInt16.self))
            )
        }
        guard header.kind == frameKind,
              let format = TileFormat(rawValue: header.format)
        else {
            return nil
        }
        return TileFrame(
            format: format,
            x: header.x,
            y: header.y,
            w: header.w,
            h: header.h,
            // Re-based to index 0, so nothing downstream has to know this came
            // out of the middle of a larger buffer.
            payload: Data(frame.dropFirst(headerLength))
        )
    }
}
