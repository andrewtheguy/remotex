import CoreGraphics
import Foundation

/// The coordinate arithmetic between the window and the remote framebuffer.
///
/// Pure and separate because this is where "clicks land in the wrong place" bugs
/// live, and none of it needs a window or a GPU to check.
///
/// Everything here is in the *remote's* terms: its pixels, and its own points —
/// `guestScale` framebuffer pixels to one of them, which is what `resize` carries
/// (1 for VNC, RDP and a 1x Mac; 2 for a Retina Mac). The host display's backing
/// scale appears nowhere on purpose. A remote is laid out at its own point size
/// and the host resamples that to whatever screen the window is on, so a desktop
/// keeps its physical size when the window moves between a Retina display and a
/// 1x one — and nothing here has to be re-derived when it does.
enum RemoteGeometry {
    /// The remote's size in points: its own logical size, whatever display the
    /// window showing it is on.
    ///
    /// A view of this size holding a framebuffer-sized drawable is what scales the
    /// remote — up on a host denser than the remote (a 1x guest on a Retina Mac,
    /// blurry as every remote desktop client is), down on a host coarser than it (a
    /// Retina guest on a 1x display, which would otherwise be drawn at twice its
    /// size).
    static func pointSize(of remote: DisplayMode, guestScale: CGFloat) -> CGSize {
        let scale = guestScale > 0 ? guestScale : 1
        return CGSize(width: CGFloat(remote.w) / scale, height: CGFloat(remote.h) / scale)
    }

    /// The window frame that gives the remote exactly its own point size: the
    /// desktop fitting, no letterbox and no scrollbars.
    ///
    /// The complement of "Resize to Window", and the only half of the pair
    /// available for a remote whose resolution is not this client's to set. What
    /// cannot be moved on that side is met by moving this one.
    ///
    /// Taken as a delta on the *room* rather than as a content size, so the title
    /// bar, the scroll view's insets, and anything else between the window's frame
    /// and the document need no accounting here — `room` is what the desktop
    /// actually gets, measured through all of it.
    ///
    /// Anchored at the top-left, as a window resized by its bottom-right corner is:
    /// the origin is bottom-left, so a height change moves it. Then held inside
    /// `limit` (the screen's visible frame) and no smaller than `minimum` (the
    /// window's own floor), because a remote bigger than the screen is ordinary — a
    /// 1x 3840×2160 desktop is 3840×2160 points — and the answer there is the
    /// largest window that fits, with the scrollbars that implies.
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

    /// The viewport to report for a visible area of `size` points: the size the
    /// remote desktop would have to be to fill it exactly, in *remote* pixels.
    ///
    /// The room times the remote's own density, so a remote that follows the
    /// window comes back with a desktop that fits and is presented one point per
    /// point. Reporting the host's device pixels instead — which this did before
    /// clients scaled their output — asks a remote to grow to the host's density
    /// and lay its UI out at half size on a Retina screen.
    ///
    /// Floored at 1 and capped at 65535 per axis: the gateway's
    /// `w`/`h` are u16 and it *rejects* an out-of-range report rather than
    /// clamping it, logging and dropping the frame — so an unclamped report does
    /// not resize anything and leaves nothing to find. Zero is refused too; a
    /// zero-size desktop is nobody's intent.
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

    private static func clamp(_ value: CGFloat, limit: UInt16) -> Int32 {
        guard value.isFinite else {
            return 0
        }
        let highest = CGFloat(limit) - 1
        return Int32(min(max(value.rounded(), 0), max(highest, 0)))
    }
}
