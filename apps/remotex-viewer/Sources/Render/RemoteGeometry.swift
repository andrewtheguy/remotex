import CoreGraphics
import Foundation

/// The coordinate arithmetic between the window and the remote framebuffer.
///
/// Pure and separate because this is where "clicks land in the wrong place" bugs
/// live, and none of it needs a window or a GPU to check.
enum RemoteGeometry {
    /// The remote's size in points, for a view showing it at one texel per
    /// device pixel.
    static func pointSize(of remote: DisplayMode, backingScale: CGFloat) -> CGSize {
        let scale = backingScale > 0 ? backingScale : 1
        return CGSize(width: CGFloat(remote.w) / scale, height: CGFloat(remote.h) / scale)
    }

    /// Map a point in the framebuffer view's own coordinates — flipped, so the
    /// origin is top-left as the DOM's is — to a remote pixel.
    ///
    /// Clamped to the framebuffer, which is what keeps a drag that runs off the
    /// edge reporting positions the gateway will accept instead of ones it
    /// rejects. Rounded rather than truncated so the pixel under the pointer is
    /// the nearest one.
    static func remotePoint(
        _ point: CGPoint,
        in surface: CGSize,
        remote: DisplayMode
    ) -> (x: Int32, y: Int32) {
        let scaleX = surface.width > 0 ? CGFloat(remote.w) / surface.width : 1
        let scaleY = surface.height > 0 ? CGFloat(remote.h) / surface.height : 1
        return (
            x: clamp(point.x * scaleX, limit: remote.w),
            y: clamp(point.y * scaleY, limit: remote.h)
        )
    }

    /// The viewport to report for a visible area of `size` points.
    ///
    /// Device pixels, floored at 1 and capped at 65535 per axis: the gateway's
    /// `w`/`h` are u16 and it *rejects* an out-of-range report rather than
    /// clamping it, logging and dropping the frame — so an unclamped report does
    /// not resize anything and leaves nothing to find. Zero is refused too; a
    /// zero-size desktop is nobody's intent.
    static func viewport(clip size: CGSize, backingScale: CGFloat) -> DisplayMode {
        let scale = backingScale > 0 ? backingScale : 1
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

    private static func clamp(_ value: CGFloat, limit: UInt16) -> Int32 {
        guard value.isFinite else {
            return 0
        }
        let highest = CGFloat(limit) - 1
        return Int32(min(max(value.rounded(), 0), max(highest, 0)))
    }
}
