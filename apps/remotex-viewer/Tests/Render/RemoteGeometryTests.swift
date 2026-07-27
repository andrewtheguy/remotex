import CoreGraphics
import Foundation
import Testing
@testable import RemotexViewer

struct RemoteGeometryTests {
    /// One texel per device pixel: a 2× display shows a 2560-wide desktop in
    /// 1280 points.
    @Test
    func aRemoteIsSizedInPointsByTheBackingScale() {
        let remote = DisplayMode(w: 2560, h: 1600)
        #expect(
            RemoteGeometry.pointSize(of: remote, backingScale: 2)
                == CGSize(width: 1280, height: 800)
        )
        #expect(
            RemoteGeometry.pointSize(of: remote, backingScale: 1)
                == CGSize(width: 2560, height: 1600)
        )
        // A scale of zero would divide by nothing; 1 is the sane reading.
        #expect(
            RemoteGeometry.pointSize(of: remote, backingScale: 0)
                == CGSize(width: 2560, height: 1600)
        )
    }

    @Test
    func aPointMapsToTheRemotePixelUnderIt() {
        let remote = DisplayMode(w: 1000, h: 500)
        let surface = CGSize(width: 500, height: 250)

        #expect(RemoteGeometry.remotePoint(.zero, in: surface, remote: remote) == (0, 0))
        #expect(
            RemoteGeometry.remotePoint(
                CGPoint(x: 250, y: 125),
                in: surface,
                remote: remote
            ) == (500, 250)
        )
    }

    /// The bottom-right pixel is `w-1, h-1`, not `w, h` — and this is what a drag
    /// that runs off the edge lands on. Unclamped, those coordinates would be off
    /// the framebuffer.
    @Test
    func theFarCornersClampInsideTheFramebuffer() {
        let remote = DisplayMode(w: 800, h: 600)
        let surface = CGSize(width: 800, height: 600)

        #expect(
            RemoteGeometry.remotePoint(
                CGPoint(x: 800, y: 600),
                in: surface,
                remote: remote
            ) == (799, 599)
        )
        #expect(
            RemoteGeometry.remotePoint(
                CGPoint(x: 5_000, y: 5_000),
                in: surface,
                remote: remote
            ) == (799, 599)
        )
        // A drag that leaves the top-left has to keep reporting, too.
        #expect(
            RemoteGeometry.remotePoint(
                CGPoint(x: -40, y: -40),
                in: surface,
                remote: remote
            ) == (0, 0)
        )
    }

    @Test
    func aOnePixelRemoteHasExactlyOneAddressablePixel() {
        let remote = DisplayMode(w: 1, h: 1)
        let surface = CGSize(width: 100, height: 100)
        for point in [CGPoint.zero, CGPoint(x: 50, y: 50), CGPoint(x: 100, y: 100)] {
            #expect(RemoteGeometry.remotePoint(point, in: surface, remote: remote) == (0, 0))
        }
    }

    @Test
    func aViewportIsMeasuredInDevicePixels() {
        #expect(
            RemoteGeometry.viewport(clip: CGSize(width: 720, height: 450), backingScale: 2)
                == DisplayMode(w: 1440, h: 900)
        )
        #expect(
            RemoteGeometry.viewport(clip: CGSize(width: 1280, height: 800), backingScale: 1)
                == DisplayMode(w: 1280, h: 800)
        )
    }

    /// The gateway's `w`/`h` are u16 and it rejects an out-of-range report rather
    /// than clamping it — logging and dropping the frame, so nothing resizes and
    /// there is nothing to find. Clamping here is what makes that unreachable.
    @Test
    func aViewportIsClampedIntoTheGatewaysRange() {
        let huge = RemoteGeometry.viewport(
            clip: CGSize(width: 40_000, height: 40_000),
            backingScale: 3
        )
        #expect(huge == DisplayMode(w: 65535, h: 65535))

        // Zero is refused as well: a zero-size desktop is nobody's intent, and a
        // window can measure zero mid-layout.
        #expect(
            RemoteGeometry.viewport(clip: .zero, backingScale: 2) == DisplayMode(w: 1, h: 1)
        )
        #expect(
            RemoteGeometry.viewport(
                clip: CGSize(width: -10, height: 5),
                backingScale: 1
            ) == DisplayMode(w: 1, h: 5)
        )
    }

    /// A window mid-layout can hand out non-finite geometry, and `UInt16(...)` of
    /// a NaN traps.
    @Test
    func nonFiniteGeometryDoesNotTrap() {
        #expect(
            RemoteGeometry.viewport(
                clip: CGSize(width: CGFloat.nan, height: CGFloat.infinity),
                backingScale: 2
            ) == DisplayMode(w: 1, h: 1)
        )
        let remote = DisplayMode(w: 100, h: 100)
        #expect(
            RemoteGeometry.remotePoint(
                CGPoint(x: CGFloat.nan, y: CGFloat.infinity),
                in: CGSize(width: 100, height: 100),
                remote: remote
            ) == (0, 0)
        )
    }

    /// A surface that has not been laid out yet has no scale to divide by.
    @Test
    func anEmptySurfaceMapsToTheOrigin() {
        let remote = DisplayMode(w: 640, h: 480)
        #expect(
            RemoteGeometry.remotePoint(CGPoint(x: 10, y: 10), in: .zero, remote: remote)
                == (10, 10)
        )
    }
}
