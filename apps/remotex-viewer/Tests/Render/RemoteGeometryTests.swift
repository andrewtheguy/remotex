import CoreGraphics
import Foundation
import Testing
@testable import RemotexViewer

struct RemoteGeometryTests {
    /// The remote's own points, not the host's: a Retina remote's 2560 pixels are
    /// the 1280 points its desktop is laid out in, and the same framebuffer from a
    /// 1x remote is 2560 points wide. What the host does with those points — one
    /// texel per device pixel, or scaled either way — is the host's business, and is
    /// what keeps a desktop the same physical size on any display.
    @Test
    func aRemoteIsSizedInPointsByItsOwnDensity() {
        let remote = DisplayMode(w: 2560, h: 1600)
        #expect(
            RemoteGeometry.pointSize(of: remote, guestScale: 2)
                == CGSize(width: 1280, height: 800)
        )
        #expect(
            RemoteGeometry.pointSize(of: remote, guestScale: 1)
                == CGSize(width: 2560, height: 1600)
        )
        // A scale of zero would divide by nothing; 1 is the sane reading.
        #expect(
            RemoteGeometry.pointSize(of: remote, guestScale: 0)
                == CGSize(width: 2560, height: 1600)
        )
    }

    /// The chrome — title bar, insets, whatever else is between the frame and the
    /// document — is whatever the window is bigger than its room by, and it is
    /// carried across unchanged. The top-left corner stays put, as it does when a
    /// window is dragged by its opposite corner.
    @Test
    func fittingTheWindowGivesTheDocumentExactlyTheRemote() {
        let frame = RemoteGeometry.windowFrame(
            fitting: CGSize(width: 1280, height: 800),
            room: CGSize(width: 960, height: 640),
            window: CGRect(x: 100, y: 200, width: 1000, height: 700),
            limit: CGRect(x: 0, y: 0, width: 3000, height: 2000),
            minimum: CGSize(width: 900, height: 640)
        )
        // 40pt of chrome across, 60 down, and the same document-to-frame difference
        // after the resize as before it.
        #expect(frame.size == CGSize(width: 1320, height: 860))
        #expect(frame.minX == 100)
        #expect(frame.maxY == 900, "the top edge has not moved")
    }

    /// A remote larger than the screen is ordinary — a 1x 3840×2160 desktop is
    /// 3840×2160 points — and the honest answer is the largest window that fits,
    /// with the scrollbars that implies.
    @Test
    func aRemoteBiggerThanTheScreenFillsWhatThereIs() {
        let visible = CGRect(x: 0, y: 25, width: 1512, height: 920)
        let frame = RemoteGeometry.windowFrame(
            fitting: CGSize(width: 3840, height: 2160),
            room: CGSize(width: 960, height: 640),
            window: CGRect(x: 100, y: 200, width: 1000, height: 700),
            limit: visible,
            minimum: CGSize(width: 900, height: 640)
        )
        #expect(frame == visible)
    }

    /// The window's own floor beats both the remote and the screen: AppKit will not
    /// hand out a smaller window, so describing one would describe a window nobody
    /// gets.
    @Test
    func theWindowsMinimumWinsOverEverything() {
        let minimum = CGSize(width: 900, height: 640)
        let tiny = RemoteGeometry.windowFrame(
            fitting: CGSize(width: 320, height: 200),
            room: CGSize(width: 960, height: 640),
            window: CGRect(x: 0, y: 0, width: 1000, height: 700),
            limit: CGRect(x: 0, y: 0, width: 3000, height: 2000),
            minimum: minimum
        )
        #expect(tiny.size == minimum)

        let cramped = RemoteGeometry.windowFrame(
            fitting: CGSize(width: 3840, height: 2160),
            room: CGSize(width: 760, height: 540),
            window: CGRect(x: 0, y: 0, width: 800, height: 600),
            limit: CGRect(x: 0, y: 0, width: 800, height: 600),
            minimum: minimum
        )
        #expect(cramped.size == minimum, "a screen smaller than the minimum window")
    }

    /// Nothing to measure means nothing to do. A scroll view before its first layout
    /// has no room, and a window mid-layout can hand out a NaN.
    @Test
    func anUnmeasurableWindowIsLeftAlone() {
        let window = CGRect(x: 10, y: 20, width: 1000, height: 700)
        let limit = CGRect(x: 0, y: 0, width: 3000, height: 2000)
        let minimum = CGSize(width: 900, height: 640)
        #expect(
            RemoteGeometry.windowFrame(
                fitting: CGSize(width: 1280, height: 800),
                room: .zero,
                window: window,
                limit: limit,
                minimum: minimum
            ) == window
        )
        #expect(
            RemoteGeometry.windowFrame(
                fitting: CGSize(width: CGFloat.nan, height: 800),
                room: CGSize(width: 960, height: 640),
                window: window,
                limit: limit,
                minimum: minimum
            ) == window
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

    /// The room asked for is in the remote's pixels: a Retina Mac has to be given
    /// two per point to fill the same window, while a remote whose pixels are its
    /// points is asked for the points themselves — no matter how dense the display
    /// the window happens to be on.
    @Test
    func aViewportIsMeasuredInTheRemotesPixels() {
        #expect(
            RemoteGeometry.viewport(clip: CGSize(width: 720, height: 450), guestScale: 2)
                == DisplayMode(w: 1440, h: 900)
        )
        #expect(
            RemoteGeometry.viewport(clip: CGSize(width: 1280, height: 800), guestScale: 1)
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
            guestScale: 3
        )
        #expect(huge == DisplayMode(w: 65535, h: 65535))

        // Zero is refused as well: a zero-size desktop is nobody's intent, and a
        // window can measure zero mid-layout.
        #expect(
            RemoteGeometry.viewport(clip: .zero, guestScale: 2) == DisplayMode(w: 1, h: 1)
        )
        #expect(
            RemoteGeometry.viewport(
                clip: CGSize(width: -10, height: 5),
                guestScale: 1
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
                guestScale: 2
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
