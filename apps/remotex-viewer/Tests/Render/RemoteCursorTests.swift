import CoreGraphics
import Foundation
import ImageIO
import Testing
import UniformTypeIdentifiers
@testable import RemotexViewer

/// Only the decision is tested, which is where all of the logic is. Constructing
/// the `NSCursor` from a `Shape` is a three-case switch — and `#expect(a === b)`
/// on an `NSCursor` traps inside Swift Testing, so comparing cursors is not an
/// option even where it would be meaningful.
struct RemoteCursorTests {
    /// The payload is in remote *pixels* and AppKit wants points. Without the
    /// divide a Retina remote's pointer is twice the right size with its hotspot
    /// displaced by half the image.
    @Test
    func aShapeIsSizedAndAnchoredInPoints() throws {
        let png = try pngData(width: 32, height: 48)
        let payload = ServerMessage.Cursor(
            image: png.base64EncodedString(),
            w: 32,
            h: 48,
            hx: 8,
            hy: 12
        )

        #expect(
            RemoteCursor.shape(for: payload, guestScale: 2)
                == .image(
                    png: png,
                    size: CGSize(width: 16, height: 24),
                    hotSpot: CGPoint(x: 4, y: 6)
                )
        )
        #expect(
            RemoteCursor.shape(for: payload, guestScale: 1)
                == .image(
                    png: png,
                    size: CGSize(width: 32, height: 48),
                    hotSpot: CGPoint(x: 8, y: 12)
                )
        )
    }

    /// No message has arrived, so the remote is compositing its own pointer into
    /// the framebuffer and ours has to be out of the way.
    @Test
    func noMessageMeansTheRemoteDrawsItsOwn() {
        #expect(RemoteCursor.shape(for: nil, guestScale: 2) == .hidden)
    }

    /// A null image is the remote hiding its pointer — a different thing from
    /// never having sent one, and the reason the model holds an optional of an
    /// optional at the boundary.
    @Test
    func aNullImageMeansTheRemoteHidItsPointer() {
        let payload = ServerMessage.Cursor(image: nil, w: 0, h: 0, hx: 0, hy: 0)
        #expect(RemoteCursor.shape(for: payload, guestScale: 2) == .fallbackArrow)
    }

    @Test
    func anUnreadablePayloadFallsBackRatherThanShowingNothing() throws {
        let notBase64 = ServerMessage.Cursor(
            image: "not base64 at all!!",
            w: 16,
            h: 16,
            hx: 0,
            hy: 0
        )
        #expect(RemoteCursor.shape(for: notBase64, guestScale: 1) == .fallbackArrow)

        // Valid base64 that is not an image.
        let notAnImage = ServerMessage.Cursor(
            image: Data([0x00, 0x01, 0x02, 0x03]).base64EncodedString(),
            w: 16,
            h: 16,
            hx: 0,
            hy: 0
        )
        #expect(RemoteCursor.shape(for: notAnImage, guestScale: 1) == .fallbackArrow)
    }

    /// A zero dimension would divide the hotspot into nothing.
    @Test
    func aZeroSizedShapeFallsBack() throws {
        let png = try pngData(width: 8, height: 8).base64EncodedString()
        for payload in [
            ServerMessage.Cursor(image: png, w: 0, h: 8, hx: 0, hy: 0),
            ServerMessage.Cursor(image: png, w: 8, h: 0, hx: 0, hy: 0),
        ] {
            #expect(RemoteCursor.shape(for: payload, guestScale: 1) == .fallbackArrow)
        }
    }

    /// A scale of zero would divide by nothing.
    @Test
    func aZeroGuestScaleReadsAsOne() throws {
        let png = try pngData(width: 10, height: 10)
        let payload = ServerMessage.Cursor(
            image: png.base64EncodedString(),
            w: 10,
            h: 10,
            hx: 5,
            hy: 5
        )
        #expect(
            RemoteCursor.shape(for: payload, guestScale: 0)
                == .image(
                    png: png,
                    size: CGSize(width: 10, height: 10),
                    hotSpot: CGPoint(x: 5, y: 5)
                )
        )
    }

    private func pngData(width: Int, height: Int) throws -> Data {
        let bytes = [UInt8](repeating: 0x80, count: width * height * 4)
        let provider = try #require(CGDataProvider(data: Data(bytes) as CFData))
        let image = try #require(
            CGImage(
                width: width,
                height: height,
                bitsPerComponent: 8,
                bitsPerPixel: 32,
                bytesPerRow: width * 4,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                provider: provider,
                decode: nil,
                shouldInterpolate: false,
                intent: .defaultIntent
            )
        )
        let data = NSMutableData()
        let destination = try #require(
            CGImageDestinationCreateWithData(data, UTType.png.identifier as CFString, 1, nil)
        )
        CGImageDestinationAddImage(destination, image, nil)
        #expect(CGImageDestinationFinalize(destination))
        return data as Data
    }
}
