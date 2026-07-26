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
        let window: NSWindow
        let scrollView: NSScrollView
        let surface: RemoteSurfaceView
        private let coordinator: RemoteSurfaceHost.Coordinator

        init() throws {
            let renderer = try #require(FramebufferRenderer.make(), "needs a Metal device")
            model = Self.makeModel()
            window = NSWindow(
                contentRect: CGRect(x: 0, y: 0, width: 300, height: 200),
                styleMask: [.titled, .resizable],
                backing: .buffered,
                defer: false
            )
            scrollView = NSScrollView(frame: .zero)
            scrollView.hasVerticalScroller = true
            scrollView.hasHorizontalScroller = true
            scrollView.autohidesScrollers = true
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
}
