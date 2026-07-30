import AppKit
import SwiftUI

/// Hosts the point-sized remote surface in a scrolling AppKit view.
///
/// Content stays inside the safe area so title-bar chrome is never mistaken for
/// clickable remote pixels. Oversized desktops scroll instead of scaling to fit.
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
    /// Whether the window's toolbar gives way to the desktop.
    ///
    /// Its 8pt is the smaller half of what this buys. The larger half is full
    /// screen: macOS keeps the title bar pinned there for as long as a toolbar is
    /// shown, and auto-hides it as soon as none is — so with this on, a full-screen
    /// desktop reaches the top of the screen and the chrome comes back on a trip to
    /// the top edge.
    let hidesToolbar: Bool

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
        context.coordinator.apply(hidesToolbar: hidesToolbar)
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

            // `tile` observes content insets, window resizing, and scroller changes
            // that frame notifications and SwiftUI state do not reliably expose.
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
            // Read on demand rather than pushed, for the same reason and from the
            // same place: only this coordinator holds a window, and the density the
            // remote is asked to match has to be the one this window is on *now*.
            model.hostScaleReader = { [weak surface] in
                surface?.window?.backingScaleFactor ?? 1
            }
            // Host display changes affect layer rasterization, not remote geometry.
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

        /// Size the document, retile scrollbars, and report remaining room. Guard
        /// re-entry because this is also called from scroll-view layout.
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

        /// Reapply toolbar visibility because SwiftUI may replace the toolbar.
        func apply(hidesToolbar: Bool) {
            guard let toolbar = scrollView?.window?.toolbar else {
                return
            }
            toolbar.isVisible = !hidesToolbar
        }

        func detach() {
            // Put the toolbar back with the surface that took it away: a login screen
            // under a bare title bar would be this coordinator's doing outliving it.
            apply(hidesToolbar: false)
            scrollView?.onTile = nil
            scrollView = nil
            model.fitWindowToRemote = nil
            model.hostScaleReader = nil
            keyboard?.invalidate()
            keyboard = nil
            model.attach(renderer: nil)
            renderer = nil
            surface = nil
        }
    }
}
