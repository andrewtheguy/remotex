import Foundation
import OSLog
import WebKit

/// The page's side of the conversation, and the rules about where it may go.
///
/// Everything the page sends is input or a report about audio; nothing it sends
/// changes the session. That asymmetry is deliberate — the app owns the
/// gateway socket, the claim, the picker and the menus, so a page that is
/// reloaded, wedged or replaced costs a repaint and nothing else.
@MainActor
final class CanvasBridge: NSObject {
    static let handlerName = "remotexCanvas"

    private let model: AppModel
    /// The one origin this page may be at. Anything else is refused rather than
    /// loaded: the page has no links, so a navigation is either a bug or a
    /// remote's clipboard content having found its way somewhere it should not.
    private let origin: URL
    private let log = Logger(subsystem: "dev.remotex.viewer", category: "canvas")

    init(model: AppModel, origin: URL) {
        self.model = model
        self.origin = origin
    }
}

extension CanvasBridge: WKScriptMessageHandler {
    nonisolated func userContentController(
        _ controller: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        // `body` is JSON-serializable by construction (WebKit converts it), so
        // it round-trips through `JSONSerialization` into the typed decoder
        // rather than being read key by key.
        guard JSONSerialization.isValidJSONObject(message.body),
              let json = try? JSONSerialization.data(withJSONObject: message.body),
              let event = CanvasEvent.decode(json)
        else {
            return
        }
        MainActor.assumeIsolated {
            apply(event)
        }
    }

    private func apply(_ event: CanvasEvent) {
        switch event {
        case .ready(let secureContext, let audioDecoder, let room, let content):
            model.noteCanvasRoom(room, content: content)
            noteReady(secureContext: secureContext, audioDecoder: audioDecoder)
        case .pointer(let x, let y):
            model.sendPointer(x: x, y: y)
        case .button(let button, let pressed, let clicks):
            model.sendMouseButton(button, pressed: pressed, clicks: clicks)
        case .wheel(let dx, let dy, let unit):
            model.sendWheel(dx: dx, dy: dy, unit: unit)
        case .cacheReset:
            model.resetTileCache()
        case .audioState(_, let error):
            if let error {
                model.audio.playbackFailed(error)
            }
        case .videoState(let error):
            // Raised rather than logged. A video target sends no still tiles to
            // fall back on, so the alternative to an alert is a black window
            // with the reason only in the system log.
            if let error {
                log.error("canvas page cannot show video: \(error, privacy: .public)")
                model.showError(error)
            }
        }
    }

    /// The page came up. Both halves of this are load-bearing and neither fails
    /// loudly on its own, which is why they are checked here rather than trusted:
    /// without a secure context there is no `AudioDecoder` at all, and remote
    /// sound would simply never arrive with nothing in any log to say why.
    /// Nothing is re-primed from here: `CanvasServer`'s attach callback fires for
    /// the same event — the page posts this from the stream opening — and doing it
    /// twice would ask the gateway for two full repaints.
    private func noteReady(secureContext: Bool, audioDecoder: Bool) {
        guard !secureContext || !audioDecoder else {
            return
        }
        log.error(
            """
            canvas page came up degraded: secureContext=\(secureContext, privacy: .public) \
            audioDecoder=\(audioDecoder, privacy: .public)
            """
        )
        model.showError(
            secureContext
                ? "This build's remote display cannot decode audio."
                : "The remote display did not load from a secure origin, so audio cannot play."
        )
    }
}

extension CanvasBridge: WKNavigationDelegate {
    nonisolated func webView(
        _ webView: WKWebView,
        decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void
    ) {
        MainActor.assumeIsolated {
            let url = navigationAction.request.url
            let allowed = url.map(sameOrigin) ?? false
            if !allowed {
                log.warning("refused canvas navigation to \(url?.scheme ?? "?", privacy: .public)")
            }
            decisionHandler(allowed ? .allow : .cancel)
        }
    }

    nonisolated func webView(
        _ webView: WKWebView,
        didFailProvisionalNavigation navigation: WKNavigation!,
        withError error: any Error
    ) {
        MainActor.assumeIsolated {
            log.error("canvas page did not load: \(error.localizedDescription, privacy: .public)")
            model.showError("The remote display did not load.")
        }
    }

    private func sameOrigin(_ url: URL) -> Bool {
        url.scheme == origin.scheme && url.host == origin.host && url.port == origin.port
    }
}
