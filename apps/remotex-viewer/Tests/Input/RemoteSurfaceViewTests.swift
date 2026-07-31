import AppKit
import Testing
@testable import RemotexViewer

/// How much room the remote has, and — the half that actually broke — whether a
/// window resize is noticed at all.
@MainActor
struct RemoteSurfaceViewTests {
    /// The measurement is the scroll view's visible area, not this view's own
    /// size, which is at least the remote's and would report the desktop's
    /// current size back as the space available for it.
    @Test
    func theViewportIsTheVisibleAreaInRemotePixels() throws {
        let harness = try Harness()
        harness.resize(to: CGSize(width: 640, height: 480))

        #expect(
            harness.surface.measuredViewport()
                == harness.expectedViewport(width: 640, height: 480)
        )
    }

    /// A document view larger than the window must not inflate the measurement:
    /// reporting the desktop's own size back as the room available for it is how
    /// a desktop that grew once would never shrink again.
    @Test
    func aRemoteLargerThanTheWindowStillReportsTheWindow() throws {
        let harness = try Harness()
        harness.resize(to: CGSize(width: 640, height: 480))
        harness.surface.setFrameSize(CGSize(width: 3000, height: 2000))

        #expect(
            harness.surface.measuredViewport()
                == harness.expectedViewport(width: 640, height: 480)
        )
    }

    /// `RemoteGeometry` floors at 1 because the gateway rejects a zero, but an
    /// engine that follows the window would take that literally and resize the
    /// remote to 1×1. So before there is anything to measure there is nothing to
    /// report — which is the state `makeNSView` attaches in.
    @Test
    func nothingIsMeasuredBeforeTheFirstLayout() throws {
        let renderer = try #require(FramebufferRenderer.make(), "needs a Metal device")
        let model = Harness.makeModel()
        let scrollView = RemoteScrollView(frame: .zero)
        let surface = RemoteSurfaceView(model: model, renderer: renderer)
        scrollView.documentView = surface
        let coordinator = RemoteSurfaceHost.Coordinator(model: model)
        coordinator.attach(renderer: renderer, surface: surface, scrollView: scrollView)
        defer { coordinator.detach() }

        #expect(surface.measuredViewport() == nil)
        #expect(model.viewportSize == nil, "and nothing was reported")
    }

    /// The regression. `NSClipView` posts `boundsDidChange` when it *scrolls* —
    /// the origin moves and no size changes — while a window resize changes its
    /// frame and the bounds size follows without a bounds notification. Watching
    /// the bounds meant a resize never reached the model, so a VNC target never
    /// followed the window.
    @Test
    func resizingTheWindowReportsTheNewViewport() async throws {
        let harness = try Harness()

        harness.resize(to: CGSize(width: 800, height: 600))
        let first = try await harness.reportedViewport()
        harness.resize(to: CGSize(width: 500, height: 400))
        let second = try await harness.reportedViewport(otherThan: first)

        #expect(first == harness.expectedViewport(width: 800, height: 600))
        #expect(second == harness.expectedViewport(width: 500, height: 400))
    }

    /// A legacy scroller (the style macOS uses once a mouse is attached) takes
    /// 17pt off the clip view when the remote overflows. Measuring that would make
    /// an engine that follows the window resize to the smaller size, hide the
    /// scrollers, be told the full size again, and flip between the two forever.
    ///
    /// Driven by shrinking the clip view directly rather than by provoking a real
    /// scroller: whether AppKit tiles one depends on the order the frames were set
    /// in, and the rule under test is ours, not AppKit's. That the shrink is 17pt
    /// and real was established separately against a live scroll view.
    @Test
    func aClipViewNarrowedByAScrollerDoesNotReadAsLessRoom() throws {
        let harness = try Harness()
        harness.resize(to: CGSize(width: 800, height: 600))
        let full = try #require(harness.surface.measuredViewport())

        harness.scrollView.contentView.setFrameSize(CGSize(width: 783, height: 583))

        #expect(harness.surface.measuredViewport() == full)
        #expect(full == harness.expectedViewport(width: 800, height: 600))
    }

    /// The bug behind a Linux guest's taskbar being unreachable. A window with a
    /// title bar gets a top `contentInset` for it (`automaticallyAdjustsContentInsets`),
    /// and the scroll view's `bounds` include that inset — so a desktop sized to
    /// them hangs its last 52pt below the fold, with no scroll extent to reach it,
    /// and an RDP "Resize to Window" comes back larger than the window every time.
    @Test
    func theTitleBarsContentInsetIsNotReportedAsRoom() throws {
        let harness = try Harness()
        harness.resize(to: CGSize(width: 900, height: 700))
        harness.inset(top: 52)

        #expect(
            harness.surface.measuredViewport()
                == harness.expectedViewport(width: 900, height: 648)
        )
        // And the document is floored on the same room, so a remote that fits it
        // exactly cannot overflow and raise a scroller.
        harness.apply(remote: harness.remote(width: 900, height: 648))
        #expect(harness.surface.frame.size == CGSize(width: 900, height: 648))
    }

    /// The inset lands *after* the first layout, without the scroll view's own frame
    /// changing — so the first report is made against the whole frame and something
    /// has to notice. Nothing did: the desktop kept the size that included the title
    /// bar for the rest of the session.
    @Test
    func aContentInsetThatArrivesLateIsReported() async throws {
        let harness = try Harness()
        harness.resize(to: CGSize(width: 900, height: 700))
        let first = try await harness.reportedViewport()
        #expect(first == harness.expectedViewport(width: 900, height: 700))

        harness.inset(top: 52)

        let second = try await harness.reportedViewport(otherThan: first)
        #expect(second == harness.expectedViewport(width: 900, height: 648))
    }

    /// The two above set the inset themselves, which tests our arithmetic and takes
    /// AppKit's word for the rest. This one takes nothing on faith: automatic
    /// insets left on, a real window and a real layout, and the assertion is that
    /// whatever inset AppKit decided on is out of the *reported* size. Without it,
    /// the shipped bug — the report carrying the title bar — passes every test in
    /// this file.
    @Test
    func appKitInsetsForTheTitleBarOfItsOwnAccord() async throws {
        let harness = try Harness(automaticContentInsets: true)
        harness.resize(to: CGSize(width: 900, height: 700))

        let inset = harness.scrollView.contentInsets.top
        #expect(inset > 0, "a titled window insets its scroll view for the title bar")

        // Its own number, not one written here: the title bar's height is AppKit's
        // to decide and has changed across releases.
        let expected = harness.expectedViewport(width: 900, height: 700 - inset)
        #expect(harness.surface.measuredViewport() == expected)
        // Polled to the expected value rather than taking the first report: the
        // inset lands partway through the layout, so the report before it is a real
        // state this passes through.
        for _ in 0 ..< 200 where harness.model.viewportSize != expected {
            try await Task.sleep(for: .milliseconds(5))
        }
        #expect(harness.model.viewportSize == expected)
    }

    /// The toolbar is the desktop's for as long as one is showing.
    ///
    /// In a window that is 8pt; in full screen it is the whole strip, because macOS
    /// keeps the title bar pinned there while a toolbar is shown and auto-hides it
    /// when none is. Re-applied on every SwiftUI update, so a rebuilt toolbar does
    /// not come back up over the desktop — and put back on `detach`, so a login
    /// screen does not inherit a bare title bar from the session before it.
    @Test
    func theToolbarGivesWayToTheDesktopAndComesBack() throws {
        let harness = try Harness()
        harness.window.toolbar = NSToolbar()
        #expect(harness.window.toolbar?.isVisible == true)

        harness.coordinator.apply(hidesToolbar: true)
        #expect(harness.window.toolbar?.isVisible == false)

        harness.coordinator.apply(hidesToolbar: true)
        #expect(harness.window.toolbar?.isVisible == false, "and again is still hidden")

        harness.coordinator.apply(hidesToolbar: false)
        #expect(harness.window.toolbar?.isVisible == true)

        harness.coordinator.apply(hidesToolbar: true)
        harness.coordinator.detach()
        #expect(harness.window.toolbar?.isVisible == true)
    }

    /// A window-only resize changes no observed state, so SwiftUI never runs
    /// `updateNSView`; the room has to be re-measured and re-reported anyway, and a
    /// remote smaller than the window has to stay in the middle of the space it grew
    /// into rather than sitting where it was.
    @Test
    func growingTheWindowLeavesASmallerRemoteCentred() async throws {
        let harness = try Harness()
        harness.resize(to: CGSize(width: 600, height: 400))
        harness.apply(remote: harness.remote(width: 400, height: 300))
        let before = try await harness.reportedViewport()

        harness.resize(to: CGSize(width: 900, height: 700))
        let after = try await harness.reportedViewport(otherThan: before)

        #expect(after == harness.expectedViewport(width: 900, height: 700))
        // The document is the remote and nothing more — padding it out to the window
        // is what used to raise a scroller on the axis that fit — so being centred is
        // the clip view's doing, and it is read off the clip.
        #expect(harness.surface.frame.size == CGSize(width: 400, height: 300))
        let clip = harness.scrollView.contentView
        #expect(clip.bounds.origin.x == (400 - clip.bounds.width) / 2)
        #expect(clip.bounds.origin.y == (300 - clip.bounds.height) / 2)
    }

    /// The reference is Microsoft Remote Desktop: a desktop that does not fit gets a
    /// scroller on the axis it overflows, *beside* the picture, and nothing on the
    /// axis that fits. Ours raised both — the document was padded out to the room, so
    /// the horizontal scroller's 17pt made it overflow vertically too — and with
    /// them a white corner box the reference never shows.
    @Test
    func aRemoteWiderThanTheWindowScrollsOnThatAxisAlone() throws {
        let harness = try Harness()
        harness.resize(to: CGSize(width: 900, height: 700))
        // 19pt too wide and well short vertically: the near-miss.
        // No window resize and no tile of our own: a desktop that arrives larger than
        // the window has to raise its own scrollbar, which is what it did not do.
        harness.apply(remote: harness.remote(width: 919, height: 500))

        #expect(harness.scrollView.visibleBars.horizontal)
        #expect(!harness.scrollView.visibleBars.vertical)
        // The scroller took its width out of the room it is drawn beside, and the
        // desktop is centred in what is left.
        let clip = harness.scrollView.contentView
        #expect(clip.frame.height < 700)
        #expect(clip.bounds.origin.y == (500 - clip.bounds.height) / 2)
    }

    /// The black band above every desktop, and its last rows below the fold.
    ///
    /// AppKit asks the clip view where the document may sit while the clip is still
    /// the whole frame — the title bar's inset has not been taken off yet — so the
    /// centring answer is for a clip 52pt taller than the real one, and the origin it
    /// keeps is half that. Nothing asked again once the clip was resized.
    ///
    /// Automatic insets on, because the inset *is* the trigger: with the room and the
    /// frame the same size there is nothing to centre wrongly.
    @Test
    func aDesktopThatFillsTheWindowSitsFlushAgainstIt() throws {
        let harness = try Harness(automaticContentInsets: true)
        harness.resize(to: CGSize(width: 900, height: 700))
        #expect(harness.scrollView.contentInsets.top > 0, "the title bar insets the room")

        // What an engine that follows the window comes back with: exactly the room.
        harness.apply(remote: try #require(harness.surface.measuredViewport()))

        #expect(harness.scrollView.contentView.bounds.origin == .zero)
        #expect(!harness.scrollView.visibleBars.horizontal)
        #expect(!harness.scrollView.visibleBars.vertical)
    }

    /// A desktop that fits shows none at all — the state every engine that follows
    /// the window settles in, and the one the reference client shows bare.
    @Test
    func aRemoteThatFitsShowsNoScrollers() throws {
        let harness = try Harness()
        harness.resize(to: CGSize(width: 900, height: 700))
        harness.apply(remote: harness.remote(width: 900, height: 700))

        #expect(!harness.scrollView.visibleBars.horizontal)
        #expect(!harness.scrollView.visibleBars.vertical)
    }

    /// Moving the window between displays of different scale is the case that used
    /// to double or halve the desktop: the layout was in the host's device pixels,
    /// so a 1x guest on a Retina display came out at half its size and a document
    /// derived from the scale left behind overflowed into space that was not even
    /// scrollable. The desktop is laid out in the *remote's* points now, so the move
    /// is the host's to absorb and there is nothing here to re-derive.
    @Test
    func theHostsDisplayDensityChangesNothing() throws {
        let harness = try Harness()
        harness.window.scale = 1
        harness.resize(to: CGSize(width: 900, height: 700))
        // Larger than the room, so the document is the remote's own point size and
        // any scale it had been derived from would be readable off it.
        harness.apply(remote: DisplayMode(w: 4_000, h: 3_000))
        let document = harness.surface.frame.size
        let reported = harness.surface.measuredViewport()
        #expect(document == CGSize(width: 4_000, height: 3_000))

        // Onto a Retina display. Same desktop, same size, same request.
        harness.window.scale = 2
        harness.resize(to: CGSize(width: 900, height: 700))

        #expect(harness.surface.frame.size == document)
        #expect(harness.surface.measuredViewport() == reported)
    }

    /// A Retina remote draws two framebuffer pixels per point of its own desktop,
    /// so it is presented at half its pixel size — on a Retina host that is one
    /// texel per device pixel, and on a 1x one it is scaled down rather than shown
    /// at twice the size it is meant to be. The window here is deliberately 1x so
    /// that a layout derived from the *host* instead could not pass.
    @Test
    func aRetinaRemoteIsLaidOutAtItsOwnPointSize() throws {
        let harness = try Harness()
        harness.window.scale = 1
        harness.resize(to: CGSize(width: 900, height: 700))
        harness.apply(remote: DisplayMode(w: 4_000, h: 3_000), guestScale: 2)

        #expect(harness.surface.frame.size == CGSize(width: 2_000, height: 1_500))
        // And the room is reported in the remote's pixels, which is what a desktop
        // that fills a 900x700pt window at that density would have to be.
        #expect(
            harness.surface.measuredViewport() == DisplayMode(w: 1_800, h: 1_400)
        )
    }

    /// Scrolling changes what is visible, not how much room there is.
    @Test
    func scrollingDoesNotChangeTheViewport() async throws {
        let harness = try Harness()
        harness.resize(to: CGSize(width: 800, height: 600))
        let reported = try await harness.reportedViewport()
        harness.surface.setFrameSize(CGSize(width: 3000, height: 2000))

        harness.scrollView.contentView.scroll(to: CGPoint(x: 120, y: 90))
        harness.scrollView.reflectScrolledClipView(harness.scrollView.contentView)

        #expect(harness.surface.measuredViewport() == reported)
        #expect(harness.model.viewportSize == reported)
    }

    /// A real scroll view, surface and window, wired the way `RemoteSurfaceHost`
    /// wires them. The notification wiring is the thing under test, so it is the
    /// one part that cannot be faked.
    @MainActor
    private struct Harness {
        let model: AppModel
        let window: ScaledWindow
        let scrollView: RemoteScrollView
        let surface: RemoteSurfaceView
        let coordinator: RemoteSurfaceHost.Coordinator

        /// `automaticContentInsets` off by default, so the room is the frame and
        /// every size written below is the one that is measured. AppKit would
        /// otherwise add this window's title bar as a top inset partway through the
        /// first layout — which is a thing to test deliberately, and
        /// `appKitInsetsForTheTitleBarOfItsOwnAccord` is where it is, rather than
        /// having it arrive underneath every other number.
        init(automaticContentInsets: Bool = false) throws {
            let renderer = try #require(FramebufferRenderer.make(), "needs a Metal device")
            model = Self.makeModel()
            window = ScaledWindow(
                contentRect: CGRect(x: 0, y: 0, width: 300, height: 200),
                styleMask: [.titled, .resizable],
                backing: .buffered,
                defer: false
            )
            scrollView = RemoteScrollView(frame: .zero)
            scrollView.automaticallyAdjustsContentInsets = automaticContentInsets
            surface = RemoteSurfaceView(model: model, renderer: renderer)
            scrollView.documentView = surface
            window.contentView = scrollView
            coordinator = RemoteSurfaceHost.Coordinator(model: model)
            coordinator.attach(renderer: renderer, surface: surface, scrollView: scrollView)
        }

        static func makeModel() -> AppModel {
            AppModel(
                clipboard: ClipboardSynchronizer(
                    pasteboard: NSPasteboard.withUniqueName(),
                    startsPolling: false
                )
            )
        }

        func resize(to size: CGSize) {
            scrollView.setFrameSize(size)
            scrollView.layoutSubtreeIfNeeded()
        }

        /// What AppKit does for a title bar, done deliberately: the frame is
        /// untouched and the room inside it shrinks.
        func inset(top: CGFloat) {
            scrollView.contentInsets = NSEdgeInsets(
                top: top, left: 0, bottom: 0, right: 0
            )
            scrollView.tile()
            scrollView.layoutSubtreeIfNeeded()
        }

        /// What the host does on a `resize`, both halves of it together.
        func apply(remote: DisplayMode, guestScale: CGFloat = 1) {
            coordinator.apply(remoteSize: remote, guestScale: guestScale)
        }

        /// A remote whose desktop is `width`×`height` of its own points — the same
        /// rule as the report below, read the other way.
        func remote(width: CGFloat, height: CGFloat) -> DisplayMode {
            expectedViewport(width: width, height: height)
        }

        /// Room in points to the remote pixels it is reported as, through the same
        /// rule the surface uses. The surface's density rather than the window's, so
        /// which display the tests run on cannot change the answer.
        func expectedViewport(width: CGFloat, height: CGFloat) -> DisplayMode {
            RemoteGeometry.viewport(
                clip: CGSize(width: width, height: height),
                guestScale: surface.guestScale
            )
        }

        /// Reports are delivered on the main *queue*, so they land after this
        /// test's current turn. Polled rather than slept on, so the wait is only as
        /// long as the delivery.
        func reportedViewport(otherThan previous: DisplayMode? = nil) async throws -> DisplayMode {
            for _ in 0..<200 {
                if let size = model.viewportSize, size != previous {
                    return size
                }
                try await Task.sleep(for: .milliseconds(5))
            }
            throw ReportNeverArrived()
        }
    }

    private struct ReportNeverArrived: Error {}

    /// A window whose backing scale can be told what to be, since a test cannot
    /// move one between displays. Nil is the display's own, so every test that does
    /// not care reads the real thing.
    private final class ScaledWindow: NSWindow {
        var scale: CGFloat?

        override var backingScaleFactor: CGFloat {
            scale ?? super.backingScaleFactor
        }
    }
}
