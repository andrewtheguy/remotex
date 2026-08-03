import AppKit
import SwiftUI
import WebKit

/// Hosts the client: a `WKWebView` showing the SPA the bundled gateway serves.
///
/// What stays on this side of the boundary is everything a web page has no
/// business owning — the window, the toolbar, keyboard capture, and the geometry
/// **Resize to Display** is built on. Everything else, including the whole session,
/// is the page's.
struct RemoteWebHost: NSViewRepresentable {
    let model: AppModel
    /// The gateway to show: its loopback origin and the launch token that gets past
    /// its door.
    let gateway: GatewayEndpoint
    /// Whether the window's toolbar gives way to the desktop.
    ///
    /// Its 8pt is the smaller half of what this buys. The larger half is full
    /// screen: macOS keeps the title bar pinned there for as long as a toolbar is
    /// shown, and auto-hides it as soon as none is — so with this on, a full-screen
    /// desktop reaches the top of the screen and the chrome comes back on a trip to
    /// the top edge.
    let hidesToolbar: Bool

    func makeCoordinator() -> Coordinator {
        Coordinator(model: model, gateway: gateway)
    }

    func makeNSView(context: Context) -> RemoteWebView {
        let configuration = WKWebViewConfiguration()
        // No gesture requirement, unlike a browser tab. This is what lets **Remote ›
        // Enable Audio** start sound from a menu item: a menu press is not a user
        // activation as far as WebKit is concerned, and without this the page's
        // `AudioContext` would come up suspended with nothing on screen to say so.
        configuration.mediaTypesRequiringUserActionForPlayback = []
        // A store of this instance's own, and a *persistent* one: the client keeps
        // its three remembered preferences in `localStorage`, so a non-persistent
        // store forgets the Command-translation override and both "if compatible"
        // defaults at every launch — silently, since nothing on screen says a
        // preference was dropped.
        //
        // The cookie in it is replaced at every launch by the token this launch
        // minted, and the claim lives in `sessionStorage`, which is per web view
        // whatever the store does. So persistence costs nothing and buys the
        // preferences. Keyed off the instance directory, which is what keeps
        // `--instance-dir` isolating these too — WebKit keeps the store in this
        // app's container rather than in that directory.
        if let identifier = model.dataStoreIdentifier {
            configuration.websiteDataStore = WKWebsiteDataStore(forIdentifier: identifier)
        }
        let webView = RemoteWebView(frame: .zero, configuration: configuration)
        webView.allowsMagnification = false
        webView.allowsBackForwardNavigationGestures = false
        #if DEBUG
        webView.isInspectable = true
        #endif
        context.coordinator.attach(webView: webView)
        return webView
    }

    func updateNSView(_ webView: RemoteWebView, context: Context) {
        context.coordinator.apply(hidesToolbar: hidesToolbar)
    }

    static func dismantleNSView(_ webView: RemoteWebView, coordinator: Coordinator) {
        coordinator.detach()
    }

    @MainActor
    final class Coordinator {
        private let model: AppModel
        private let gateway: GatewayEndpoint
        private weak var webView: RemoteWebView?
        private var bridge: NativeBridge?
        private var keyboard: KeyboardCapture?

        init(model: AppModel, gateway: GatewayEndpoint) {
            self.model = model
            self.gateway = gateway
        }

        func attach(webView: RemoteWebView) {
            self.webView = webView
            // A local event monitor rather than letting WebKit see keys: menu key
            // equivalents are consumed by the menu bar before any responder, and a
            // focused desktop has to be given every Command chord. The web view
            // never receives a key event, which is why the page's own keyboard
            // listeners never fire here and the keys arrive over the bridge
            // instead.
            keyboard = KeyboardCapture(model: model, surface: webView)

            // The window side of "Resize to Display". Installed here rather than
            // reached for from the model, which has no window: this coordinator is
            // the only thing that holds one, and it holds it only while the surface
            // is on screen.
            model.fitWindowToRemote = { [weak self] in
                self?.fitWindowToRemote()
            }
            // Read on demand rather than pushed, for the same reason and from the
            // same place: the density in the Display menu's readout has to be the
            // one this window is on *now*.
            model.hostScaleReader = { [weak webView] in
                webView?.window?.backingScaleFactor ?? 1
            }
            load(into: webView)
        }

        /// Put the launch token in the cookie store, then load the page.
        ///
        /// The order is the whole of it, and the wait is not optional: the store is
        /// written asynchronously, and a document loaded before the cookie is in it
        /// arrives unauthenticated — which the page reports as a gateway that will
        /// not have it, on a first launch, once.
        ///
        /// A cookie rather than the `Authorization` header this app used to send,
        /// because the requests that matter are not this app's: the page issues its
        /// own `fetch` calls and opens its own `ws://` sockets, and neither can be
        /// given a header from out here. See `src/auth.rs`.
        private func load(into webView: RemoteWebView) {
            let bridge = NativeBridge(model: model, origin: gateway.url)
            bridge.webView = webView
            self.bridge = bridge
            webView.configuration.userContentController
                .add(bridge, name: NativeBridge.handlerName)
            webView.navigationDelegate = bridge
            model.attach(bridge: bridge)

            let store = webView.configuration.websiteDataStore.httpCookieStore
            store.setCookie(gateway.cookie) { [weak self, weak webView] in
                MainActor.assumeIsolated {
                    guard let self, let webView else {
                        return
                    }
                    webView.load(URLRequest(url: self.gateway.url))
                }
            }
        }

        /// Size the window so the desktop fits it exactly.
        ///
        /// The arithmetic is `RemoteGeometry.windowFrame`, and all of it is there:
        /// what is left here is reading the measurements off AppKit.
        ///
        /// One pass, and one is enough. `doc + (window - bounds)` is a fixed point:
        /// applying it again measures the same chrome and computes the same frame.
        /// A second pass was tried against a scroll-bar problem and could only
        /// compound it, because the only thing that made the two passes differ was a
        /// measurement taken before layout had settled.
        ///
        /// A full-screen window is left alone. Its size is the screen's and not this
        /// app's to set, and AppKit would either refuse the frame or take the window
        /// out of full screen to honour it; neither is what the menu item offers.
        func fitWindowToRemote() {
            guard let webView, let remoteSize = model.state.remoteSize,
                  let window = webView.window,
                  !window.styleMask.contains(.fullScreen),
                  let screen = window.screen ?? NSScreen.main
            else {
                return
            }
            // The web view's bounds, and deliberately *not* the page's content box.
            // What this arithmetic wants is the window chrome — `window - room` —
            // and the bounds give exactly that, scroll bars or no scroll bars. The
            // content box subtracts whatever the bars are taking, which then gets
            // added to the desktop as though it were chrome: fitting a 1280x800
            // desktop from a 1000x700 window with both bars up asked for 1295x847
            // instead of 1280x832, and a second pass on top of that reached
            // 1590x855.
            let room = webView.bounds.size
            let frame = RemoteGeometry.windowFrame(
                fitting: RemoteGeometry.pointSize(
                    of: remoteSize,
                    guestScale: model.state.remoteScale
                ),
                room: room,
                window: window.frame,
                limit: screen.visibleFrame,
                minimum: window.minSize
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
            // Put the toolbar back with the surface that took it away: a launch
            // screen under a bare title bar would be this coordinator's doing
            // outliving it.
            apply(hidesToolbar: false)
            webView?.navigationDelegate = nil
            webView?.configuration.userContentController
                .removeScriptMessageHandler(forName: NativeBridge.handlerName)
            keyboard?.invalidate()
            keyboard = nil
            if let bridge {
                model.release(bridge: bridge)
            }
            model.fitWindowToRemote = nil
            model.hostScaleReader = nil
            bridge = nil
            webView = nil
        }
    }
}

/// Where the page is and what gets it past the door.
struct GatewayEndpoint: Equatable {
    let port: UInt16
    let token: String

    var url: URL {
        // Force-unwrapped over a literal with a `UInt16` in it: there is no port
        // this cannot spell, and an optional here would be a branch with no case.
        URL(string: "http://127.0.0.1:\(port)/")!
    }

    /// The `remotex_session` cookie the gateway checks, spelled the way a login
    /// would have set it — minus `Secure`, which loopback HTTP is not, and which
    /// Safari drops on sight over plain HTTP even on localhost.
    var cookie: HTTPCookie {
        // Same reasoning as `url`: every field is supplied and none can be wrong.
        HTTPCookie(properties: [
            .name: "remotex_session",
            .value: token,
            .domain: "127.0.0.1",
            .path: "/",
        ])!
    }
}

/// A `WKWebView` that says when its frame changed.
///
/// Kept for one reason: the page reports the viewport itself from its own window
/// size, so nothing measures the surface any more — but full screen and a toolbar
/// appearing still change the bounds under a desktop, and **Resize to Display**
/// reads them.
final class RemoteWebView: WKWebView {
    var onFrameChange: (() -> Void)?

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        onFrameChange?()
    }
}
