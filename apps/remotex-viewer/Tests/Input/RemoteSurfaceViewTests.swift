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
    func theViewportIsTheVisibleAreaInDevicePixels() throws {
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
        let scrollView = NSScrollView(frame: .zero)
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
        harness.coordinator.apply(remoteSize: harness.remote(width: 900, height: 648))
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

    /// A window-only resize changes no observed state, so SwiftUI never runs
    /// `updateNSView` and the surface would keep a frame measured against the old
    /// window — leaving a remote smaller than the window off-centre in the space it
    /// grew into, with input mapped against the stale frame.
    @Test
    func growingTheWindowGrowsTheSurfaceUnderASmallerRemote() async throws {
        let harness = try Harness()
        harness.resize(to: CGSize(width: 600, height: 400))
        harness.coordinator.apply(remoteSize: harness.remote(width: 400, height: 300))
        let before = try await harness.reportedViewport()

        harness.resize(to: CGSize(width: 900, height: 700))
        _ = try await harness.reportedViewport(otherThan: before)

        #expect(harness.surface.frame.size == CGSize(width: 900, height: 700))
    }

    /// The same hole as the resize above, reached the other way: moving the window
    /// between displays of different scale changes no observed state either, so
    /// `updateNSView` never runs and the document keeps a frame derived from the
    /// scale it left behind. The framebuffer *does* lay itself out at the new one —
    /// so a remote at 2× inside a 1× document overflows into space that is not even
    /// scrollable, which is a non-Retina guest's taskbar below the fold on a Retina
    /// host, with no way to scroll to it.
    @Test
    func aBackingScaleChangeReDerivesTheDocument() throws {
        let harness = try Harness()
        harness.window.scale = 1
        harness.resize(to: CGSize(width: 900, height: 700))
        // Larger than the room at either scale, so the document is the remote's own
        // point size both times and the scale it was derived from is readable off it.
        harness.coordinator.apply(remoteSize: DisplayMode(w: 4_000, h: 3_000))
        #expect(harness.surface.frame.size == CGSize(width: 4_000, height: 3_000))

        // Onto a Retina display: the same remote is half the points it was.
        harness.window.scale = 2
        harness.surface.backingScaleChanged()

        #expect(harness.surface.frame.size == CGSize(width: 2_000, height: 1_500))
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
        let scrollView: NSScrollView
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
            scrollView = NSScrollView(frame: .zero)
            scrollView.hasVerticalScroller = true
            scrollView.hasHorizontalScroller = true
            scrollView.autohidesScrollers = true
            scrollView.automaticallyAdjustsContentInsets = automaticContentInsets
            surface = RemoteSurfaceView(model: model, renderer: renderer)
            scrollView.documentView = surface
            window.contentView = scrollView
            coordinator = RemoteSurfaceHost.Coordinator(model: model)
            coordinator.attach(renderer: renderer, surface: surface, scrollView: scrollView)
        }

        static func makeModel() -> AppModel {
            AppModel(
                defaults: UserDefaults(
                    suiteName: "RemoteSurfaceViewTests.\(UUID().uuidString)"
                )!,
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

        /// A remote whose size in *points* on this window's display is
        /// `width`×`height` — the same points-to-pixels rule, read the other way.
        func remote(width: CGFloat, height: CGFloat) -> DisplayMode {
            expectedViewport(width: width, height: height)
        }

        /// Points to pixels through the same rule the surface uses, so a display
        /// with any backing scale gives the same answer.
        func expectedViewport(width: CGFloat, height: CGFloat) -> DisplayMode {
            RemoteGeometry.viewport(
                clip: CGSize(width: width, height: height),
                backingScale: window.backingScaleFactor
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
