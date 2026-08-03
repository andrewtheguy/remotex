import AppKit
import CRemotexCEF
import SwiftUI

/// Hosts the client: a Chromium browser showing the SPA out of this bundle.
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

    /// A plain container, and plain on purpose: Chromium adds its own view under
    /// this one and `client::fill_parent` gives that view an autoresizing mask, so
    /// keeping the two the same size is AppKit's job and nothing here has to watch
    /// for it.
    func makeNSView(context: Context) -> NSView {
        let container = NSView()
        context.coordinator.attach(container: container)
        return container
    }

    func updateNSView(_ container: NSView, context: Context) {
        context.coordinator.apply(hidesToolbar: hidesToolbar)
    }

    static func dismantleNSView(_ container: NSView, coordinator: Coordinator) {
        coordinator.detach()
    }

    @MainActor
    final class Coordinator {
        private let model: AppModel
        private let gateway: GatewayEndpoint
        private weak var container: NSView?
        private var bridge: NativeBridge?
        private var keyboard: KeyboardCapture?

        init(model: AppModel, gateway: GatewayEndpoint) {
            self.model = model
            self.gateway = gateway
        }

        func attach(container: NSView) {
            self.container = container
            // A local event monitor rather than letting Chromium see keys: menu key
            // equivalents are consumed by the menu bar before any responder, and a
            // focused desktop has to be given every Command chord. The browser never
            // receives a key event, which is why the page's own keyboard listeners
            // never fire here and the keys arrive over the bridge instead.
            keyboard = KeyboardCapture(model: model, surface: container)

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
            model.hostScaleReader = { [weak container] in
                container?.window?.backingScaleFactor ?? 1
            }
            load(into: container)
        }

        /// Put the launch token in the cookie jar, then show the client.
        ///
        /// The order is the whole of it, and the wait is not optional: a page loaded
        /// before the cookie is in the jar arrives unauthenticated — which the page
        /// reports as a gateway that will not have it, on a first launch, once.
        ///
        /// A cookie rather than the `Authorization` header this app used to send,
        /// because the requests that matter are not this app's: the page issues its
        /// own `fetch` calls and opens its own `ws://` sockets, and neither can be
        /// given a header from out here. See `src/auth.rs`.
        ///
        /// The page is loaded from `remotex://app` rather than from the gateway, and
        /// that is what makes the client's remembered preferences survive a
        /// relaunch: `localStorage` is keyed by *origin*, and this one is a property
        /// of the bundle rather than of whichever port the kernel handed out this
        /// time. The gateway is then only a backend, and the page is told where it
        /// is — by the scheme handler, as `index.html` is served.
        private func load(into container: NSView) {
            let bridge = NativeBridge(model: model)
            self.bridge = bridge
            model.attach(bridge: bridge)

            let gateway = self.gateway
            gateway.origin.withCString { origin in
                gateway.token.withCString { token in
                    remotex_cef_set_cookie(origin, token, nil, nil)
                }
            }
            bridge.attach(to: container, gateway: gateway)
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
            guard let container, let remoteSize = model.state.remoteSize,
                  let window = container.window,
                  !window.styleMask.contains(.fullScreen),
                  let screen = window.screen ?? NSScreen.main
            else {
                return
            }
            // The container's bounds, and deliberately *not* the page's content box.
            // What this arithmetic wants is the window chrome — `window - room` —
            // and the bounds give exactly that, scroll bars or no scroll bars. The
            // content box subtracts whatever the bars are taking, which then gets
            // added to the desktop as though it were chrome: fitting a 1280x800
            // desktop from a 1000x700 window with both bars up asked for 1295x847
            // instead of 1280x832, and a second pass on top of that reached
            // 1590x855.
            let room = container.bounds.size
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
            guard let toolbar = container?.window?.toolbar else {
                return
            }
            toolbar.isVisible = !hidesToolbar
        }

        func detach() {
            // Put the toolbar back with the surface that took it away: a launch
            // screen under a bare title bar would be this coordinator's doing
            // outliving it.
            apply(hidesToolbar: false)
            keyboard?.invalidate()
            keyboard = nil
            if let bridge {
                bridge.detach()
                model.release(bridge: bridge)
            }
            model.fitWindowToRemote = nil
            model.hostScaleReader = nil
            bridge = nil
            container = nil
        }
    }
}

/// Where the page's gateway is and what gets it past the door.
struct GatewayEndpoint: Equatable {
    let port: UInt16
    let token: String

    /// The gateway's origin, with no trailing slash — the shape
    /// `frontend/src/gateway.ts` expects, and the shape the cookie is set against.
    ///
    /// Force-unwrapped over a literal with a `UInt16` in it: there is no port this
    /// cannot spell, and an optional here would be a branch with no case.
    var origin: String {
        "http://127.0.0.1:\(port)"
    }

    var url: URL {
        URL(string: "\(origin)/")!
    }
}
