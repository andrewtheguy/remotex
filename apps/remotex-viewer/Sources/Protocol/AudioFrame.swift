import Foundation

/// A server -> client binary frame carrying sound: one wave buffer's worth of Opus
/// packets.
///
/// Layout (little-endian, matching `audio` in `src/protocol.rs`):
///
/// ```text
/// offset 0: u8  frame kind, always 0x03 (audio)
/// offset 1: u8  flags, always 0
/// offset 2: u16 packet count
/// offset 4: packets, each u16 length | length bytes
/// ```
///
/// The header is deliberately the same four bytes as `BatchFrame`'s, so a reader of
/// one finds nothing surprising in the other, and the two are told apart by the kind
/// byte alone (`GatewayConnection.receiveLoop`).
///
/// **Its own parser, not a port of the SPA's.** `decodeAudioFrame` in
/// `frontend/src/protocol.ts` reads the same bytes and this deliberately does not
/// share it: a wrong parser agrees with itself, so the value of a second one is that
/// it was written from the layout above rather than from the first one.
///
/// Lengths are on the wire because an Opus packet does not carry its own size, and
/// the count is there because records that are self-delimiting would otherwise make a
/// *truncated* frame parse cleanly as a complete but shorter one.
enum AudioFrame {
    static let frameKind: UInt8 = 0x03
    static let headerLength = 4
    static let packetHeaderLength = 2

    /// The frame's packets in wire order, or nil for anything malformed or of another
    /// kind.
    ///
    /// Same all-or-nothing contract as `BatchFrame.decode`, for a different reason.
    /// There it is that half a repaint leaves the framebuffer wrong until something
    /// else corrects it; here it is that a partly-read frame would hand the decoder a
    /// packet stream with a hole in it, and Opus packets carry inter-packet state — so
    /// the cost of guessing is not one bad 20 ms but a decoder running on wrong
    /// history.
    static func decode(_ frame: Data) -> [Data]? {
        guard frame.count >= headerLength else {
            return nil
        }
        // Read through the raw buffer rather than by subscript: `frame` may be a
        // slice, whose indices start wherever it was cut from, and the u16 fields are
        // not aligned, so the loads have to be unaligned ones.
        return frame.withUnsafeBytes { raw -> [Data]? in
            guard raw[0] == frameKind, raw[1] == 0 else {
                return nil
            }
            let count = Int(u16(raw, 2))
            var packets = [Data]()
            packets.reserveCapacity(count)
            var at = headerLength
            while at < raw.count {
                guard at + packetHeaderLength <= raw.count else {
                    return nil
                }
                let length = Int(u16(raw, at))
                let start = at + packetHeaderLength
                guard start + length <= raw.count else {
                    return nil
                }
                // Re-based to index 0, so nothing downstream has to know this came out
                // of the middle of a larger buffer.
                packets.append(Data(raw[start..<(start + length)]))
                at = start + length
            }
            return packets.count == count ? packets : nil
        }
    }

    private static func u16(_ raw: UnsafeRawBufferPointer, _ at: Int) -> UInt16 {
        UInt16(littleEndian: raw.loadUnaligned(fromByteOffset: at, as: UInt16.self))
    }
}
