import WebKit

/// The web view's two ends of the seam: the handler the page posts to, and the
/// navigation policy around it.
///
/// One `WKScriptMessageHandler` and one `evaluateJavaScript` call, which is the
/// whole of the app-to-client protocol. There is no frame stream, no envelope
/// framing and no loopback file server here any more: the page is served by the
/// gateway on the same origin it talks to, so everything about the session is a
/// conversation this app is not part of.
@MainActor
final class NativeBridge: NSObject, CommandSink {
    /// The name the page posts to: `window.webkit.messageHandlers.remotexNative`.
    /// Its presence is also how the page knows it is not in a browser.
    static let handlerName = "remotexNative"

    private weak var model: AppModel?
    /// The view commands are evaluated against. Weak: the bridge is retained by the
    /// web view's user-content controller, which the view owns.
    weak var webView: WKWebView?
    /// The origin the page was loaded from. Any navigation elsewhere is refused.
    private let origin: URL

    init(model: AppModel, origin: URL) {
        self.model = model
        self.origin = origin
    }

    /// Send one command to the page. Dropped silently before the first load, which
    /// is a menu item pressed while the window is still empty.
    func send(_ command: NativeCommand) {
        guard let webView, let script = command.javaScript() else {
            return
        }
        webView.evaluateJavaScript(script)
    }
}

extension NativeBridge: WKScriptMessageHandler {
    nonisolated func userContentController(
        _ controller: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        // Inside the hop, all of it: `WKScriptMessage.body` is main-actor isolated,
        // so reading it out here to decode first is a concurrency violation rather
        // than a saving. WebKit delivers this on the main thread, which is what
        // `assumeIsolated` asserts.
        MainActor.assumeIsolated {
            guard let event = NativeEvent.decode(message.body) else {
                return
            }
            model?.apply(event)
        }
    }
}

extension NativeBridge: WKNavigationDelegate {
    /// The page may talk to its own gateway and nowhere else.
    ///
    /// A remote desktop is a stream of somebody else's pixels and text, and the
    /// clipboard bridge carries their strings into this process. None of it should
    /// be able to send this window somewhere — so a navigation off the origin is
    /// refused rather than opened, here and not in a browser's tab-level policy,
    /// because there is no tab and no address bar to notice with.
    nonisolated func webView(
        _ webView: WKWebView,
        decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void
    ) {
        MainActor.assumeIsolated {
            let url = navigationAction.request.url
            let sameOrigin =
                url?.scheme == origin.scheme
                    && url?.host == origin.host
                    && url?.port == origin.port
            decisionHandler(sameOrigin ? .allow : .cancel)
        }
    }

    /// A load that never started. Reported, because on loopback it means the
    /// gateway stopped answering between the handshake and the request — except
    /// when it is a cancellation, which is not a failure at all.
    ///
    /// Cancellations are routine here and one of them is this class's own doing:
    /// the policy above answers `.cancel` for a navigation off the origin, and
    /// WebKit delivers that refusal back through this callback. Reporting it would
    /// put "the gateway stopped answering" on screen for a page that is loaded and
    /// working. A load superseded by the next one — a relaunch, a redirect — comes
    /// through the same way.
    nonisolated func webView(
        _ webView: WKWebView,
        didFailProvisionalNavigation navigation: WKNavigation!,
        withError error: any Error
    ) {
        let failure = error as NSError
        if failure.domain == NSURLErrorDomain, failure.code == NSURLErrorCancelled {
            return
        }
        MainActor.assumeIsolated {
            model?.pageFailedToLoad(error.localizedDescription)
        }
    }
}
