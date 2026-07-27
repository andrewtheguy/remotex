import AppKit
import ImageIO

/// The remote pointer.
///
/// Receiving any `cursor` message means the client owns pointer rendering from
/// then on. Engines whose server composites the pointer into the framebuffer —
/// RDP, and VNC servers that ignore the Cursor pseudo-encoding — never send one,
/// which is why the default is `hidden` rather than the system arrow: the remote
/// is already drawing a pointer into the pixels, and two would be worse than one.
enum RemoteCursor {
    /// What the pointer should look like over the framebuffer.
    ///
    /// Split from the `NSCursor` it becomes so the decision is a value: which of
    /// the three cases a message means, and what its size and hotspot work out
    /// to, are then testable without AppKit. (Which is also necessary —
    /// `#expect(a === b)` on an `NSCursor` traps inside Swift Testing.)
    enum Shape: Equatable {
        /// The remote draws its own pointer, so ours is transparent.
        case hidden
        /// The remote hid its pointer, or sent one that could not be read. A
        /// generic arrow beats an invisible pointer on a remote desktop — this is
        /// where the web client painted a fallback of its own.
        case fallbackArrow
        /// The remote's shape, already converted to points.
        case image(png: Data, size: CGSize, hotSpot: CGPoint)
    }

    /// Decide what a `cursor` message means. Nil means none has arrived.
    ///
    /// `guestScale` — the remote's density, the same one the desktop is laid out
    /// with — converts the payload's *pixels* into the points AppKit wants.
    /// Without it a Retina remote's pointer comes out at double size with its
    /// hotspot displaced by half the image, and next to a desktop drawn at the
    /// remote's own size it would be a pointer twice as large as the windows it
    /// points at.
    static func shape(for payload: ServerMessage.Cursor?, guestScale: CGFloat) -> Shape {
        guard let payload else {
            return .hidden
        }
        let scale = guestScale > 0 ? guestScale : 1
        guard let base64 = payload.image,
              let png = Data(base64Encoded: base64),
              payload.w > 0,
              payload.h > 0,
              isImage(png)
        else {
            return .fallbackArrow
        }
        return .image(
            png: png,
            size: CGSize(
                width: CGFloat(payload.w) / scale,
                height: CGFloat(payload.h) / scale
            ),
            hotSpot: CGPoint(
                x: CGFloat(payload.hx) / scale,
                y: CGFloat(payload.hy) / scale
            )
        )
    }

    private static func isImage(_ data: Data) -> Bool {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil) else {
            return false
        }
        return CGImageSourceGetType(source) != nil
    }

    /// A fully transparent cursor, for when the remote draws its own.
    ///
    /// A cursor rect rather than `NSCursor.hide()`, which is a *balanced global
    /// stack* — trivially unbalanced across reconnects and screen changes, and
    /// what it leaves behind when it goes wrong is an invisible pointer
    /// everywhere in the app.
    ///
    /// Built from a bitmap rather than by drawing into an `NSImage`: `lockFocus`
    /// needs a graphics context, which a process without an app does not have,
    /// and it traps rather than failing.
    @MainActor
    static let hidden: NSCursor = {
        let image = NSImage(size: NSSize(width: 1, height: 1))
        if let transparent = NSBitmapImageRep(
            bitmapDataPlanes: nil,
            pixelsWide: 1,
            pixelsHigh: 1,
            bitsPerSample: 8,
            samplesPerPixel: 4,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: .deviceRGB,
            bytesPerRow: 4,
            bitsPerPixel: 32
        ) {
            // Freshly allocated planes are zeroed, so this is already a fully
            // transparent pixel.
            image.addRepresentation(transparent)
        }
        return NSCursor(image: image, hotSpot: .zero)
    }()

    @MainActor
    static func cursor(for shape: Shape) -> NSCursor {
        switch shape {
        case .hidden:
            return hidden
        case .fallbackArrow:
            return .arrow
        case .image(let png, let size, let hotSpot):
            guard let image = NSImage(data: png) else {
                return .arrow
            }
            image.size = size
            return NSCursor(image: image, hotSpot: hotSpot)
        }
    }
}
