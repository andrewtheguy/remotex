import AppKit
import WebKit

@MainActor
final class NativeBridge: NSObject, WKScriptMessageHandlerWithReply, WKNavigationDelegate {
    static let handlerName = "remotexNative"

    private weak var model: AppModel?
    private var handshakeAccepted = false

    init(model: AppModel) {
        self.model = model
    }

    func resetHandshake() {
        handshakeAccepted = false
    }

    func userContentController(
        _ userContentController: WKUserContentController,
        didReceive message: WKScriptMessage,
        replyHandler: @escaping @MainActor @Sendable (Any?, String?) -> Void
    ) {
        guard message.frameInfo.isMainFrame,
              let model,
              model.gateway.origin.contains(message.frameInfo.request.url ?? model.gateway.url),
              let body = message.body as? [String: Any],
              let type = body["type"] as? String
        else {
            replyHandler(nil, "message rejected")
            return
        }

        switch type {
        case "ready":
            let bridgeVersion = body["bridgeVersion"] as? Int ?? -1
            let appVersion = body["appVersion"] as? String ?? ""
            let result = model.acceptHandshake(
                bridgeVersion: bridgeVersion,
                appVersion: appVersion
            )
            handshakeAccepted = result.accepted
            var reply: [String: Any] = ["accepted": result.accepted]
            if let reason = result.reason {
                reply["reason"] = reason
            }
            replyHandler(reply, nil)
        case "state":
            guard handshakeAccepted,
                  let state = body["state"] as? [String: Any],
                  let decoded = Self.decodeState(state)
            else {
                replyHandler(nil, "invalid state")
                return
            }
            model.apply(session: decoded)
            replyHandler(["accepted": true], nil)
        case "remoteClipboard":
            guard handshakeAccepted, let text = body["text"] as? String else {
                replyHandler(nil, "invalid clipboard")
                return
            }
            model.clipboard.receiveRemote(text)
            replyHandler(["accepted": true], nil)
        default:
            replyHandler(nil, "unknown message type")
        }
    }

    func webView(
        _ webView: WKWebView,
        decidePolicyFor navigationAction: WKNavigationAction
    ) async -> WKNavigationActionPolicy {
        guard let model, let url = navigationAction.request.url else {
            return .cancel
        }
        if navigationAction.targetFrame?.isMainFrame == false {
            return .allow
        }
        if model.gateway.origin.contains(url) || url.absoluteString == "about:blank" {
            return .allow
        }
        NSWorkspace.shared.open(url)
        return .cancel
    }

    func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
        resetHandshake()
        model?.navigationStarted()
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        model?.navigationFinished()
    }

    func webView(
        _ webView: WKWebView,
        didFail navigation: WKNavigation!,
        withError error: Error
    ) {
        model?.navigationFailed(error)
    }

    func webView(
        _ webView: WKWebView,
        didFailProvisionalNavigation navigation: WKNavigation!,
        withError error: Error
    ) {
        model?.navigationFailed(error)
    }

    private static func decodeState(_ value: [String: Any]) -> ViewerSessionState? {
        guard let screenName = value["screen"] as? String,
              let screen = ViewerScreen(rawValue: screenName),
              let canResize = value["canResize"] as? Bool,
              let canClipboard = value["canClipboard"] as? Bool,
              let canCaptureKeyboard = value["canCaptureKeyboard"] as? Bool
        else {
            return nil
        }
        let status = (value["connectionStatus"] as? String)
            .flatMap(ViewerConnectionStatus.init(rawValue:))
        let guestOS = (value["guestOS"] as? String)
            .flatMap(GuestOS.init(rawValue:))
        guard screen != .desktop || guestOS != nil else {
            return nil
        }
        return ViewerSessionState(
            screen: screen,
            connectionStatus: status,
            connectedTarget: value["connectedTarget"] as? String,
            guestOS: guestOS,
            canResize: canResize,
            canClipboard: canClipboard,
            canCaptureKeyboard: canCaptureKeyboard
        )
    }
}
