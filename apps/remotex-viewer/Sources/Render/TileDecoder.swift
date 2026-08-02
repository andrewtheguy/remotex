import CoreGraphics
import Foundation
import ImageIO

/// A tile's pixels, ready for `MTLTexture.replaceRegion`.
///
/// BGRA8, one row after another **top-down**, `w * 4` bytes per row — the layout
/// `.bgra8Unorm` wants, in the row order Metal's texture coordinates want (row 0
/// is the top). See `TileDecoder` for why that row order costs a context flip.
struct DecodedTile: Sendable, Equatable {
    let x: UInt16
    let y: UInt16
    let w: UInt16
    let h: UInt16
    let bgra: [UInt8]
}

/// Turns encoded tile payloads into BGRA pixels, off the main thread.
///
/// An actor so decodes are serial: the receive loop awaits each one before
/// pulling the next frame, which is what preserves the gateway's arrival order
/// across an async decode. Tiles overwrite their rectangles with no delta state,
/// so a reordered pair leaves stale pixels on screen.
actor TileDecoder {
    /// Decode one tile, or nil if the payload is unusable.
    ///
    /// Everything goes through a `CGContext` rather than reading the decoded
    /// image's own bytes, because those bytes differ per payload: a JPEG decodes as
    /// three-channel RGB while a PNG can carry an alpha channel, and ImageIO is free
    /// to hand back either. Drawing into a context of a known layout normalizes
    /// both, and it is the only place the row order can be fixed.
    ///
    /// The format byte is read for exactly one thing: telling a picture from a video
    /// frame. Among the three still codecs it stays unread, because ImageIO
    /// identifies a payload from its own container — so which of PNG, JPEG and WebP
    /// arrived makes no difference here, and `BatchFrame.decode` has already refused
    /// anything that is none of them.
    ///
    /// `.h264` is not a picture and has no ImageIO path at all. It returns nil here,
    /// and `GatewayConnection` refuses the session by name rather than letting a
    /// video target look like a desktop that stopped repainting.
    func decode(_ frame: TileFrame) -> DecodedTile? {
        guard frame.format != .h264 else {
            return nil
        }
        let w = Int(frame.w)
        let h = Int(frame.h)
        guard w > 0, h > 0 else {
            return nil
        }
        // Each tile is seen exactly once, so ImageIO's cache is pure overhead.
        guard let source = CGImageSourceCreateWithData(
            frame.payload as CFData,
            [kCGImageSourceShouldCache: false] as CFDictionary
        ),
            let image = CGImageSourceCreateImageAtIndex(source, 0, nil)
        else {
            return nil
        }
        // The header decides where these pixels go, so a payload that disagrees
        // with it is dropped rather than scaled into the rectangle: scaling would
        // smear one bad tile across the screen, where dropping costs one repaint.
        guard image.width == w, image.height == h else {
            return nil
        }

        var bgra = [UInt8](repeating: 0, count: w * h * 4)
        let drawn = bgra.withUnsafeMutableBytes { buffer -> Bool in
            // noneSkipFirst | byteOrder32Little is byte order B,G,R,X — a
            // byte-for-byte match for .bgra8Unorm, so the upload needs no
            // swizzle. Alpha is discarded rather than premultiplied because
            // screen tiles are opaque by construction: both ends encode from
            // packed RGB888 with no alpha channel to carry (`encode_png` in
            // `src/protocol.rs`, `encode.rs` in the agent), and the gateway's own
            // roundtrip test asserts a payload does not come back with one.
            //
            // Device RGB, not sRGB: it skips a colour-match transform per tile,
            // and the texture format applies no transfer function either, which
            // matches what an untagged canvas did.
            guard let context = CGContext(
                data: buffer.baseAddress,
                width: w,
                height: h,
                bitsPerComponent: 8,
                bytesPerRow: w * 4,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.noneSkipFirst.rawValue
                    | CGBitmapInfo.byteOrder32Little.rawValue
            ) else {
                return false
            }
            // No flip. CoreGraphics' user space is bottom-left, but a bitmap
            // context's *memory* is raster order — row 0 is the top — and
            // `draw` fills the rect accordingly, so the buffer comes out in the
            // tile's own row order. Adding the usual translate/scale flip here
            // inverts it instead, which `TileDecoderTests` pins down.
            //
            // Getting this right matters more than for a full-frame blit: tiles
            // are placed into the texture by their own y, so an upside-down
            // buffer cannot be corrected downstream by flipping the sampler —
            // every strip would land in the wrong band.
            context.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
            return true
        }
        guard drawn else {
            return nil
        }
        return DecodedTile(x: frame.x, y: frame.y, w: frame.w, h: frame.h, bgra: bgra)
    }
}
