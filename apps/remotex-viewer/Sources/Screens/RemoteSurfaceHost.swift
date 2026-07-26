import AppKit
import SwiftUI

/// Puts the remote surface in a scroll view and keeps it in step with the model.
///
/// The scroll view is what handles a remote larger than the window: the desktop
/// is shown at its own size and scrolls, rather than being scaled down to fit.
struct RemoteSurfaceHost: NSViewRepresentable {
    let model: AppModel
    /// Passed in rather than read off the model inside `updateNSView`, so the
    /// enclosing `body` is what observes them and SwiftUI actually calls the
    /// update when they change.
    let remoteSize: DisplayMode?
    let cursor: ServerMessage.Cursor?

    func makeCoordinator() -> Coordinator {
        Coordinator(model: model)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSScrollView()
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = true
        scrollView.backgroundColor = .black
        // Zoom is out of scope: the desktop is shown at one texel per device pixel
        // or it scrolls.
        scrollView.allowsMagnification = false

        guard let renderer = FramebufferRenderer.make() else {
            // No Metal device, or a shader that will not compile. Nothing can be
            // drawn, and saying so beats an unexplained black window.
            model.showError("This Mac cannot start the remote display renderer.")
            return scrollView
        }
        let surface = RemoteSurfaceView(model: model, renderer: renderer)
        scrollView.documentView = surface
        context.coordinator.attach(
            renderer: renderer,
            surface: surface,
            scrollView: scrollView
        )
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        context.coordinator.apply(remoteSize: remoteSize)
        context.coordinator.apply(cursor: cursor)
    }

    static func dismantleNSView(_ scrollView: NSScrollView, coordinator: Coordinator) {
        coordinator.detach()
    }

    @MainActor
    final class Coordinator {
        private let model: AppModel
        private var renderer: FramebufferRenderer?
        private weak var surface: RemoteSurfaceView?
        private var keyboard: KeyboardCapture?
        private var observers: [NSObjectProtocol] = []
        private var appliedCursor: ServerMessage.Cursor??

        init(model: AppModel) {
            self.model = model
        }

        func attach(
            renderer: FramebufferRenderer,
            surface: RemoteSurfaceView,
            scrollView: NSScrollView
        ) {
            self.renderer = renderer
            self.surface = surface
            model.attach(renderer: renderer)
            // A local event monitor rather than `keyDown` on the surface: menu key
            // equivalents are consumed by the menu bar before the responder chain,
            // and Command chords have to reach the remote.
            keyboard = KeyboardCapture(model: model, surface: surface)

            // The visible area changing is the only thing that changes how much
            // room the remote has. `contentView` posts this once its own bounds
            // change, which covers window resizes and scroller appearance alike.
            scrollView.contentView.postsBoundsChangedNotifications = true
            observers.append(
                NotificationCenter.default.addObserver(
                    forName: NSView.boundsDidChangeNotification,
                    object: scrollView.contentView,
                    queue: .main
                ) { [weak self, weak surface] _ in
                    MainActor.assumeIsolated {
                        guard let self, let surface else {
                            return
                        }
                        surface.needsLayout = true
                        self.model.reportViewport(surface.measuredViewport())
                    }
                }
            )
            model.reportViewport(surface.measuredViewport())
        }

        func apply(remoteSize: DisplayMode?) {
            guard let surface else {
                return
            }
            surface.remoteSize = remoteSize
            // At least the visible area, so a remote smaller than the window still
            // fills it with margin instead of leaving the scroll view's own
            // background showing through.
            let visible = surface.enclosingScrollView?.contentView.bounds.size ?? .zero
            let scale = surface.window?.backingScaleFactor ?? 1
            let wanted = remoteSize.map {
                RemoteGeometry.pointSize(of: $0, backingScale: scale)
            } ?? .zero
            surface.setFrameSize(
                CGSize(
                    width: max(wanted.width, visible.width),
                    height: max(wanted.height, visible.height)
                )
            )
        }

        /// The doubly-optional `appliedCursor` is the point: "no message yet" and
        /// "a message with a null image" are different states, and only the first
        /// means the remote is drawing its own pointer.
        func apply(cursor: ServerMessage.Cursor?) {
            guard let surface, appliedCursor != .some(cursor) else {
                return
            }
            appliedCursor = .some(cursor)
            let scale = surface.window?.backingScaleFactor ?? 1
            surface.apply(
                cursor: RemoteCursor.cursor(
                    for: RemoteCursor.shape(for: cursor, backingScale: scale)
                )
            )
        }

        func detach() {
            for observer in observers {
                NotificationCenter.default.removeObserver(observer)
            }
            observers.removeAll()
            keyboard?.invalidate()
            keyboard = nil
            model.attach(renderer: nil)
            renderer = nil
            surface = nil
        }
    }
}
