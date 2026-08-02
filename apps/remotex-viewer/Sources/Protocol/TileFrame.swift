import Foundation

/// A tile payload's codec, from the `TILE` record's format byte.
///
/// PNG is the gateway's lossless default; a target on the fixed-quality render
/// dial sends JPEG or WebP instead. All three decode through the same ImageIO
/// path (WebP since macOS 11, and the app's minimum is 15). `Tile::FORMAT_PNG`,
/// `Tile::FORMAT_JPEG`, `Tile::FORMAT_WEBP` in `src/protocol.rs`.
///
/// A byte outside these cases makes `BatchFrame.decode` fail — a refusal — rather
/// than handing bytes to a decoder that will mangle them. The version check in
/// `GatewayClient` is what catches a codec change *first* and legibly; this is the
/// backstop.
enum TileFormat: UInt8, Sendable, Equatable {
    case png = 1
    case jpeg = 2
    case webp = 3
    /// One H.264 access unit, from a target on `render_type = "video"`.
    ///
    /// **This viewer cannot decode it.** It is a case here anyway, and that is the
    /// whole point of it: an unrecognised format byte makes `BatchFrame.decode`
    /// return nil, and the caller drops the *entire batch* — so without this a video
    /// target would show a frozen desktop and say nothing about why. Recognising it
    /// is what lets `GatewayConnection` refuse the session by name instead.
    ///
    /// Decoding it is planned; see docs/roadmap.md, which records what it would take
    /// (VideoToolbox, and a checked-in fixture because macOS cannot encode H.264
    /// through ImageIO). The browser decodes it today.
    case h264 = 4
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
    /// not".
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
/// TILE (op 0x01):     u8 format | u16 slot | u16 x | u16 y | u16 w | u16 h
///                     | u32 len | payload[len]
/// TILE_REF (op 0x02): u16 slot | u16 x | u16 y
/// ```
///
/// One frame carries however many tiles were ready at once, which is what a full
/// repaint needs: a desktop is dozens of cells, and one WebSocket frame each
/// costs a receive, a decode and a paint apiece.
enum BatchFrame {
    static let frameKind: UInt8 = 0x02
    static let headerLength = 4
    static let opTile: UInt8 = 0x01
    static let opTileRef: UInt8 = 0x02
    static let tileHeaderLength = 16
    static let tileRefLength = 7
    /// `slot` meaning "draw this and do not remember it".
    static let noSlot: UInt16 = 0xFFFF
    /// How many tiles this client keeps. Part of the wire contract
    /// (`batch::SLOT_COUNT`): a slot at or above it is a malformed record rather
    /// than a reason to grow an array, which keeps this client's memory a function
    /// of the protocol instead of of what a gateway chooses to send.
    static let slotCount: UInt16 = 256

    /// One record of a batch: pixels to draw and keep, or a position to redraw
    /// something already kept at.
    enum Record: Sendable, Equatable {
        case tile(TileFrame)
        case reference(slot: UInt16, x: UInt16, y: UInt16)

        /// Whether this record carries a video frame rather than a picture.
        ///
        /// Only a tile can: an access unit is never cached into a slot, so a
        /// reference can never name one (`Slots::place` in `src/wire.rs`).
        var isVideo: Bool {
            switch self {
            case .tile(let frame): frame.format == .h264
            case .reference: false
            }
        }
    }

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
    static func decode(_ frame: Data) -> [Record]? {
        guard frame.count >= headerLength else {
            return nil
        }
        // Read through the raw buffer rather than by subscript: `frame` may be a
        // slice, whose indices start wherever it was cut from, and the multi-byte
        // fields are not aligned, so the loads have to be unaligned ones.
        return frame.withUnsafeBytes { raw -> [Record]? in
            guard raw[0] == frameKind, raw[1] == 0 else {
                return nil
            }
            let count = Int(u16(raw, 2))

            var records: [Record] = []
            records.reserveCapacity(count)
            var at = headerLength
            while at < raw.count {
                guard let parsed = raw[at] == opTileRef
                    ? reference(raw, at)
                    : tile(raw, at)
                else {
                    return nil
                }
                records.append(parsed.record)
                at = parsed.next
            }
            return records.count == count ? records : nil
        }
    }

    private static func u16(_ raw: UnsafeRawBufferPointer, _ at: Int) -> UInt16 {
        UInt16(littleEndian: raw.loadUnaligned(fromByteOffset: at, as: UInt16.self))
    }

    private static func reference(
        _ raw: UnsafeRawBufferPointer,
        _ at: Int
    ) -> (record: Record, next: Int)? {
        guard at + tileRefLength <= raw.count else {
            return nil
        }
        let slot = u16(raw, at + 1)
        guard slot < slotCount else {
            return nil
        }
        return (
            .reference(slot: slot, x: u16(raw, at + 3), y: u16(raw, at + 5)),
            at + tileRefLength
        )
    }

    private static func tile(
        _ raw: UnsafeRawBufferPointer,
        _ at: Int
    ) -> (record: Record, next: Int)? {
        guard at + tileHeaderLength <= raw.count, raw[at] == opTile,
              let format = TileFormat(rawValue: raw[at + 1])
        else {
            return nil
        }
        let slot = u16(raw, at + 2)
        guard slot == noSlot || slot < slotCount else {
            return nil
        }
        let length = Int(
            UInt32(littleEndian: raw.loadUnaligned(fromByteOffset: at + 12, as: UInt32.self))
        )
        let start = at + tileHeaderLength
        guard start + length <= raw.count else {
            return nil
        }
        return (
            .tile(
                TileFrame(
                    format: format,
                    slot: slot,
                    x: u16(raw, at + 4),
                    y: u16(raw, at + 6),
                    w: u16(raw, at + 8),
                    h: u16(raw, at + 10),
                    // Re-based to index 0, so nothing downstream has to know this
                    // came out of the middle of a larger buffer.
                    payload: Data(raw[start..<(start + length)])
                )
            ),
            start + length
        )
    }
}
