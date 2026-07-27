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
    /// The remote's own density, from the same `resize` as `remoteSize`. Both are
    /// needed to lay the desktop out, so both arrive together.
    let guestScale: CGFloat
    let cursor: ServerMessage.Cursor?
    /// Passed in for the same reason as the rest: it decides what the pointer over
    /// the desktop looks like, and `updateNSView` has to run when it changes.
    let isViewOnly: Bool

    func makeCoordinator() -> Coordinator {
        Coordinator(model: model)
    }

    func makeNSView(context: Context) -> RemoteScrollView {
        // Scrollbars of our own — see `RemoteScrollView`, which switches AppKit's off
        // and answers for when they show, where they sit, and what they look like.
        let scrollView = RemoteScrollView(frame: .zero)
        scrollView.drawsBackground = true
        scrollView.backgroundColor = .black
        // Zoom is out of scope: the desktop is shown at its own point size — scaled
        // to the display it is on, never by the user — or it scrolls.
        scrollView.allowsMagnification = false

        guard let renderer = FramebufferRenderer.make() else {
            // No Metal device, or a shader that will not compile. Nothing can be
            // drawn, and saying so beats an unexplained black window — but not from
            // here: this runs inside SwiftUI's update, and setting observed state
            // during one is undefined. The alert goes up on the next turn instead.
            Task { model.showError("This Mac cannot start the remote display renderer.") }
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

    func updateNSView(_ scrollView: RemoteScrollView, context: Context) {
        context.coordinator.apply(remoteSize: remoteSize, guestScale: guestScale)
        context.coordinator.apply(cursor: cursor)
        context.coordinator.apply(isViewOnly: isViewOnly)
    }

    static func dismantleNSView(_ scrollView: RemoteScrollView, coordinator: Coordinator) {
        coordinator.detach()
    }

    @MainActor
    final class Coordinator {
        private let model: AppModel
        private var renderer: FramebufferRenderer?
        private weak var surface: RemoteSurfaceView?
        private var keyboard: KeyboardCapture?
        private var appliedCursor: ServerMessage.Cursor??
        /// Guards the one loop this could have: sizing the document re-tiles the
        /// scroll view, which is what called us.
        private var isLayingOut = false
        private weak var scrollView: RemoteScrollView?

        init(model: AppModel) {
            self.model = model
        }

        func attach(
            renderer: FramebufferRenderer,
            surface: RemoteSurfaceView,
            scrollView: RemoteScrollView
        ) {
            self.renderer = renderer
            self.surface = surface
            self.scrollView = scrollView
            model.attach(renderer: renderer)
            // A local event monitor rather than `keyDown` on the surface: menu key
            // equivalents are consumed by the menu bar before the responder chain,
            // and Command chords have to reach the remote.
            keyboard = KeyboardCapture(model: model, surface: surface)

            // One signal for "the room changed", and it is the layout itself.
            //
            // Frame notifications were watched here before and are not enough. AppKit
            // applies the window's title bar as a top `contentInset` partway through
            // the first layout: with no `NSScroller` left in the view tree for that
            // inset to move, no frame changes and nothing posts — and the first
            // viewport report would keep carrying the title bar, which is a Linux
            // guest's taskbar below the fold for the rest of the session. `tile` runs
            // for the inset, for every window resize, and for a scrollbar appearing.
            //
            // A window-only resize also changes no *observed* state, so SwiftUI never
            // runs `updateNSView`, and the document would keep a size measured
            // against the old window — which is the other half of why this exists.
            scrollView.onTile = { [weak self, weak surface] in
                guard let surface else {
                    return
                }
                self?.apply(remoteSize: surface.remoteSize, guestScale: surface.guestScale)
            }
            // The window side of "Resize to Display". Installed here rather than
            // reached for from the model, which has no window: this coordinator is
            // the only thing that holds one, and it holds it only while the surface
            // is on screen.
            model.fitWindowToRemote = { [weak self] in
                self?.fitWindowToRemote()
            }
            // Deliberately nothing for the window changing display. Every point
            // size here is the remote's own, so a host scale change has no geometry
            // to re-derive — the layer resamples the same desktop for whichever
            // screen the window is on (see `FramebufferView`). It was not always so:
            // while the desktop was laid out in the host's device pixels, a move to
            // a Retina display doubled it inside a document that never grew, and the
            // overflow was not even scrollable.
            report(from: surface)
        }

        /// Report the room available, if there is any yet. Nothing is sent before
        /// the first layout: this runs from `makeNSView`, where the scroll view is
        /// still zero-sized, and a floored 1×1 report would be acted on by an
        /// engine that follows the window.
        private func report(from surface: RemoteSurfaceView) {
            guard let measured = surface.measuredViewport() else {
                return
            }
            model.reportViewport(measured)
        }

        /// Size the document to the remote, lay the scrollbars out around it, and
        /// report the room that is left.
        ///
        /// Re-entrant by nature and guarded once, here: this is called *from* the
        /// scroll view's layout, and it re-tiles at the end because the document's
        /// size is the other thing the scrollbars depend on. Without that tile a
        /// desktop that arrived larger than the window got no scrollbar until the
        /// window was resized by hand — a `resize` message changes the document and
        /// nothing else, and AppKit does not lay the scroll view out for it.
        func apply(remoteSize: DisplayMode?, guestScale: CGFloat) {
            guard let surface, !isLayingOut else {
                return
            }
            isLayingOut = true
            defer { isLayingOut = false }
            // The pointer is sized in the remote's points too, so a density change
            // has to re-derive it from a `cursor` message that has not itself
            // changed — which means stepping past the dedupe in `apply(cursor:)`.
            if surface.guestScale != guestScale {
                appliedCursor = nil
            }
            surface.remoteSize = remoteSize
            surface.guestScale = guestScale
            // Exactly the remote, with nothing added to fill the window: the
            // letterbox is the scroll view's own black background and the middle is
            // where `CenteringClipView` puts a desktop with room to spare. A document
            // padded out to the window is a document that overflows the other axis as
            // soon as one scroller takes its 17pt — which is how a desktop 19pt too
            // wide ended up with a scroller it did not need on each axis.
            //
            // Before the first `resize` there is no remote to size to, and the answer
            // is the room rather than nothing: a zero-sized document is one AppKit
            // stops laying out, so the title bar's `contentInset` — which lands
            // partway through that first layout — would change no frame, notify
            // nobody, and leave the first viewport report carrying the title bar.
            surface.setFrameSize(
                remoteSize.map { RemoteGeometry.pointSize(of: $0, guestScale: guestScale) }
                    ?? surface.enclosingScrollView?.roomForDocument ?? .zero
            )
            scrollView?.tile()
            surface.needsLayout = true
            report(from: surface)
        }

        /// Size the window so the desktop fits it exactly.
        ///
        /// The arithmetic is `RemoteGeometry.windowFrame`, and all of it is there:
        /// what is left here is reading the four measurements off AppKit. The room
        /// is `roomForDocument` — the same measure the viewport report uses, so a
        /// window fitted this way reports the size it is showing.
        ///
        /// A full-screen window is left alone. Its size is the screen's and not
        /// this app's to set, and AppKit would either refuse the frame or take the
        /// window out of full screen to honour it; neither is what the menu item
        /// offers.
        func fitWindowToRemote() {
            guard let surface, let scrollView, let remoteSize = surface.remoteSize,
                  let window = scrollView.window,
                  !window.styleMask.contains(.fullScreen),
                  let screen = window.screen ?? NSScreen.main
            else {
                return
            }
            let frame = RemoteGeometry.windowFrame(
                fitting: RemoteGeometry.pointSize(of: remoteSize, guestScale: surface.guestScale),
                room: scrollView.roomForDocument,
                window: window.frame,
                limit: screen.visibleFrame,
                minimum: window.minSize
            )
            guard frame != window.frame else {
                return
            }
            window.setFrame(frame, display: true)
        }

        /// The doubly-optional `appliedCursor` is the point: "no message yet" and
        /// "a message with a null image" are different states, and only the first
        /// means the remote is drawing its own pointer.
        func apply(cursor: ServerMessage.Cursor?) {
            guard let surface, appliedCursor != .some(cursor) else {
                return
            }
            appliedCursor = .some(cursor)
            surface.apply(
                cursor: RemoteCursor.cursor(
                    for: RemoteCursor.shape(for: cursor, guestScale: surface.guestScale)
                )
            )
        }

        /// The pointer's own dedupe is the surface's — an unchanged value invalidates
        /// no cursor rects.
        func apply(isViewOnly: Bool) {
            surface?.isViewOnly = isViewOnly
        }

        func detach() {
            scrollView?.onTile = nil
            scrollView = nil
            model.fitWindowToRemote = nil
            keyboard?.invalidate()
            keyboard = nil
            model.attach(renderer: nil)
            renderer = nil
            surface = nil
        }
    }
}
