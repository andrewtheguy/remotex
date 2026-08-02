import CoreGraphics
import Foundation

/// Pure coordinate arithmetic between window points and remote pixels.
///
/// `guestScale` is remote framebuffer pixels per remote point. Host backing
/// scale is deliberately excluded; AppKit rasterizes the remote's point-sized
/// view for the screen containing the window.
enum RemoteGeometry {
    /// Remote logical size, independent of the host display.
    static func pointSize(of remote: DisplayMode, guestScale: CGFloat) -> CGSize {
        let scale = guestScale > 0 ? guestScale : 1
        return CGSize(width: CGFloat(remote.w) / scale, height: CGFloat(remote.h) / scale)
    }

    /// Fit the window around a point-sized document using measured content room.
    /// Preserve the top-left anchor, visible-screen bounds, and window minimum.
    static func windowFrame(
        fitting document: CGSize,
        room: CGSize,
        window: CGRect,
        limit: CGRect,
        minimum: CGSize
    ) -> CGRect {
        // A window mid-layout, or one whose scroll view has no room yet, has no
        // usable chrome to derive. Leaving it alone is the honest answer.
        let measurements = [
            document.width, document.height, room.width, room.height,
            window.minX, window.maxY, window.width, window.height,
            limit.minX, limit.minY, limit.maxX, limit.maxY,
            minimum.width, minimum.height,
        ]
        guard document.width >= 1, document.height >= 1, room.width >= 1, room.height >= 1,
              measurements.allSatisfy(\.isFinite)
        else {
            return window
        }
        let size = CGSize(
            width: fit(
                document.width + window.width - room.width,
                least: minimum.width,
                most: limit.width
            ),
            height: fit(
                document.height + window.height - room.height,
                least: minimum.height,
                most: limit.height
            )
        )
        return CGRect(
            origin: CGPoint(
                x: max(limit.minX, min(window.minX, limit.maxX - size.width)),
                y: max(limit.minY, min(window.maxY - size.height, limit.maxY - size.height))
            ),
            size: size
        )
    }

    /// `least` wins a fight with `most`: a window AppKit will not shrink past its
    /// own minimum is the size that actually results, so returning anything smaller
    /// would describe a window nobody gets.
    private static func fit(_ value: CGFloat, least: CGFloat, most: CGFloat) -> CGFloat {
        let ceiling = max(most, least)
        return min(max(value.rounded(), least), ceiling)
    }

    /// Convert visible host points to remote pixels, clamped to the nonzero
    /// `u16` wire range.
    static func viewport(clip size: CGSize, guestScale: CGFloat) -> DisplayMode {
        let scale = guestScale > 0 ? guestScale : 1
        return DisplayMode(
            w: pixels(size.width * scale),
            h: pixels(size.height * scale)
        )
    }

    private static func pixels(_ value: CGFloat) -> UInt16 {
        guard value.isFinite else {
            return 1
        }
        return UInt16(min(max(value.rounded(), 1), CGFloat(UInt16.max)))
    }
}
