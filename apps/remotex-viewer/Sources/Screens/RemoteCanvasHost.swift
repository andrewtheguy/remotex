import AppKit
import SwiftUI
import WebKit

/// Hosts the remote surface: a `WKWebView` showing the canvas page the app's own
/// loopback listener serves.
///
/// What stays on this side of the boundary is everything a web page has no
/// business owning — the window, the toolbar, keyboard capture, and the two
/// measurements the resize items are built on. What crossed it is the drawing.
struct RemoteCanvasHost: NSViewRepresentable {
    let model: AppModel
    /// Whether the window's toolbar gives way to the desktop.
    ///
    /// Its 8pt is the smaller half of what this buys. The larger half is full
    /// screen: macOS keeps the title bar pinned there for as long as a toolbar is
    /// shown, and auto-hides it as soon as none is — so with this on, a
    /// full-screen desktop reaches the top of the screen and the chrome comes
    /// back on a trip to the top edge.
    let hidesToolbar: Bool

    func makeCoordinator() -> Coordinator {
        Coordinator(model: model)
    }

    func makeNSView(context: Context) -> CanvasWebView {
        let configuration = WKWebViewConfiguration()
        // No gesture requirement, unlike a browser tab: the Remote menu's toggle
        // is the consent, and it was given before any of this loaded.
        configuration.mediaTypesRequiringUserActionForPlayback = []
        let webView = CanvasWebView(frame: .zero, configuration: configuration)
        webView.allowsMagnification = false
        webView.allowsBackForwardNavigationGestures = false
        #if DEBUG
        webView.isInspectable = true
        #endif
        context.coordinator.attach(webView: webView)
        return webView
    }

    func updateNSView(_ webView: CanvasWebView, context: Context) {
        context.coordinator.apply(hidesToolbar: hidesToolbar)
    }

    static func dismantleNSView(_ webView: CanvasWebView, coordinator: Coordinator) {
        coordinator.detach()
    }

    @MainActor
    final class Coordinator {
        private let model: AppModel
        private weak var webView: CanvasWebView?
        private var server: CanvasServer?
        private var bridge: CanvasBridge?
        private var keyboard: KeyboardCapture?

        init(model: AppModel) {
            self.model = model
        }

        func attach(webView: CanvasWebView) {
            self.webView = webView
            // A local event monitor rather than letting WebKit see keys: menu key
            // equivalents are consumed by the menu bar before any responder, and
            // Command chords have to reach the remote. The web view never
            // receives a key event, which is also why the page has no keyboard
            // handling in it.
            keyboard = KeyboardCapture(model: model, surface: webView)

            // The window side of "Resize to Display". Installed here rather than
            // reached for from the model, which has no window: this coordinator
            // is the only thing that holds one, and it holds it only while the
            // surface is on screen.
            model.fitWindowToRemote = { [weak self] in
                self?.fitWindowToRemote()
            }
            // Read on demand rather than pushed, for the same reason and from the
            // same place: only this coordinator holds a window, and the density
            // the remote is asked to match has to be the one this window is on
            // *now*.
            model.hostScaleReader = { [weak webView] in
                webView?.window?.backingScaleFactor ?? 1
            }
            webView.onFrameChange = { [weak self] in
                self?.reportViewport()
            }
            start()
        }

        /// Bring up the listener, then point the web view at it. Asynchronous
        /// because the port is the kernel's to name, and a URL handed out before
        /// the bind is one that cannot load.
        private func start() {
            let server = CanvasServer(onAttach: { [weak self] in
                Task { @MainActor in
                    self?.model.canvasAttached()
                }
            })
            self.server = server
            Task { @MainActor [weak self] in
                do {
                    let address = try await server.start()
                    self?.load(server: server, address: address)
                } catch {
                    self?.model.showError(
                        "This build cannot start the remote display (\(error))."
                    )
                }
            }
        }

        private func load(server: CanvasServer, address: CanvasServer.Address) {
            guard let webView else {
                // The surface went away while the listener was binding. Nothing
                // to report: `detach` has already stopped the server.
                return
            }
            // Read off the web view rather than held from `makeNSView`.
            // `WKWebView` *copies* the configuration it is initialised with, so
            // the controller built there is released with the original — holding
            // a weak reference to it meant this method found nil and returned
            // before loading anything, which looks exactly like a blank desktop.
            let controller = webView.configuration.userContentController
            // Source iteration: point the web view at `bun run dev` and keep the
            // stream, which is what the page reads either way. The same shape as
            // `REMOTEX_DEV_BACKEND` for the SPA.
            let dev = ProcessInfo.processInfo.environment["REMOTEX_VIEWER_DEV_URL"]
                .flatMap(URL.init(string:))
            let url = address.documentURL(base: dev)
            let bridge = CanvasBridge(model: model, origin: url)
            self.bridge = bridge
            controller.add(bridge, name: CanvasBridge.handlerName)
            webView.navigationDelegate = bridge
            model.attach(canvas: server)
            webView.load(URLRequest(url: url))
            reportViewport()
        }

        /// Report the room available.
        ///
        /// The web view's own bounds, which is the whole measurement — a page
        /// scrolls inside them and cannot change them. The native surface this
        /// replaced had to derive the same number from an `NSScrollView` while
        /// discounting scrollers that were about to appear, and getting that
        /// wrong is what made a desktop 19pt too wide oscillate.
        private func reportViewport() {
            guard let webView else {
                return
            }
            let room = webView.bounds.size
            guard room.width >= 1, room.height >= 1 else {
                return
            }
            model.reportViewport(
                RemoteGeometry.viewport(clip: room, guestScale: model.session.remoteScale)
            )
        }

        /// Size the window so the desktop fits it exactly.
        ///
        /// The arithmetic is `RemoteGeometry.windowFrame`, and all of it is
        /// there: what is left here is reading the measurements off AppKit.
        ///
        /// One pass, and one is enough. `doc + (window - bounds)` is a fixed
        /// point: applying it again measures the same chrome and computes the
        /// same frame. A second pass was tried against a scroll-bar problem and
        /// could only compound it, because the only thing that made the two
        /// passes differ was a measurement taken before layout had settled.
        ///
        /// A full-screen window is left alone. Its size is the screen's and not
        /// this app's to set, and AppKit would either refuse the frame or take
        /// the window out of full screen to honour it; neither is what the menu
        /// item offers.
        func fitWindowToRemote() {
            guard let webView, let remoteSize = model.session.remoteSize,
                  let window = webView.window,
                  !window.styleMask.contains(.fullScreen),
                  let screen = window.screen ?? NSScreen.main
            else {
                return
            }
            // The web view's bounds, and deliberately *not* the page's content
            // box. What this arithmetic wants is the window chrome —
            // `window - room` — and the bounds give exactly that, scroll bars or
            // no scroll bars. The content box subtracts whatever the bars are
            // taking, which then gets added to the desktop as though it were
            // chrome: fitting a 1280x800 desktop from a 1000x700 window with
            // both bars up asked for 1295x847 instead of 1280x832, and a second
            // pass on top of that reached 1590x855. Fitting to the bounds lands
            // the view on the desktop's own size, where nothing overflows and
            // the bars have no reason to be.
            let room = webView.bounds.size
            let frame = RemoteGeometry.windowFrame(
                fitting: RemoteGeometry.pointSize(
                    of: remoteSize,
                    guestScale: model.session.remoteScale
                ),
                room: room,
                window: window.frame,
                limit: screen.visibleFrame,
                minimum: window.minSize
            )
            model.trace(
                "fit: room \(Int(room.width))x\(Int(room.height)) "
                    + "window \(Int(window.frame.width))x\(Int(window.frame.height)) "
                    + "-> \(Int(frame.width))x\(Int(frame.height))"
            )
            guard frame != window.frame else {
                return
            }
            window.setFrame(frame, display: true)
        }

        /// Reapply toolbar visibility because SwiftUI may replace the toolbar.
        func apply(hidesToolbar: Bool) {
            guard let toolbar = webView?.window?.toolbar else {
                return
            }
            toolbar.isVisible = !hidesToolbar
        }

        func detach() {
            // Put the toolbar back with the surface that took it away: a login
            // screen under a bare title bar would be this coordinator's doing
            // outliving it.
            apply(hidesToolbar: false)
            webView?.onFrameChange = nil
            webView?.navigationDelegate = nil
            webView?.configuration.userContentController
                .removeScriptMessageHandler(forName: CanvasBridge.handlerName)
            model.fitWindowToRemote = nil
            model.hostScaleReader = nil
            keyboard?.invalidate()
            keyboard = nil
            model.attach(canvas: nil)
            server?.stop()
            server = nil
            bridge = nil
            webView = nil
        }
    }
}

/// A `WKWebView` that says when its frame changed.
///
/// `frameDidChange` notifications need `postsFrameChangedNotifications`, and a
/// subclass overriding the one method is less machinery than an observer whose
/// registration has to be undone.
final class CanvasWebView: WKWebView {
    var onFrameChange: (() -> Void)?

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        onFrameChange?()
    }
}
