import AppKit
import WebKit

struct RemoteClipboardPush: Equatable {
    let text: String
    let changedAtMs: Int64?
}

struct ClipboardFetchResult: Equatable {
    let requestID: String
    /// `nil` when the web side answered that the remote clipboard could not be
    /// read, so the panel can stop waiting on its own deadline.
    let text: String?
    let changedAtMs: Int64?
}

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
              let messageURL = message.frameInfo.request.url,
              model.gateway.origin.contains(messageURL),
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
            guard handshakeAccepted,
                  let payload = Self.decodeRemoteClipboardPush(body)
            else {
                replyHandler(nil, "invalid clipboard")
                return
            }
            model.clipboard.receiveRemotePush(payload.text)
            replyHandler(["accepted": true], nil)
        case "clipboardFetchResult":
            guard handshakeAccepted,
                  let payload = Self.decodeClipboardFetchResult(body)
            else {
                replyHandler(nil, "invalid clipboard fetch result")
                return
            }
            if let text = payload.text {
                model.clipboard.receiveFetchResult(
                    requestID: payload.requestID,
                    text: text,
                    changedAtMs: payload.changedAtMs
                )
            } else {
                model.clipboard.fetchUnavailable(requestID: payload.requestID)
            }
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

    static func decodeMode(_ value: Any?) -> DisplayMode? {
        guard let mode = value as? [String: Any],
              let w = mode["w"] as? Int,
              let h = mode["h"] as? Int
        else {
            return nil
        }
        return DisplayMode(w: w, h: h)
    }

    /// Anything malformed is dropped rather than failing the whole state
    /// update: a missing menu entry costs one item, a rejected state costs the
    /// viewer every capability it carries.
    static func decodeModes(_ value: Any?) -> [DisplayMode] {
        guard let list = value as? [Any] else {
            return []
        }
        return list.compactMap(decodeMode)
    }

    static func decodeState(_ value: [String: Any]) -> ViewerSessionState? {
        guard let screenName = value["screen"] as? String,
              let screen = ViewerScreen(rawValue: screenName),
              let remoteIsMac = value["remoteIsMac"] as? Bool,
              let canResize = value["canResize"] as? Bool,
              let canClipboard = value["canClipboard"] as? Bool,
              let canCaptureKeyboard = value["canCaptureKeyboard"] as? Bool
        else {
            return nil
        }
        let status = (value["connectionStatus"] as? String)
            .flatMap(ViewerConnectionStatus.init(rawValue:))
        return ViewerSessionState(
            screen: screen,
            connectionStatus: status,
            connectedTarget: value["connectedTarget"] as? String,
            remoteIsMac: remoteIsMac,
            displayModes: decodeModes(value["displayModes"]),
            remoteSize: decodeMode(value["remoteSize"]),
            canResize: canResize,
            canClipboard: canClipboard,
            canCaptureKeyboard: canCaptureKeyboard
        )
    }

    static func decodeRemoteClipboardPush(
        _ value: [String: Any]
    ) -> RemoteClipboardPush? {
        guard let text = value["text"] as? String,
              let changedAtMs = decodeChangedAtMs(value)
        else {
            return nil
        }
        return RemoteClipboardPush(text: text, changedAtMs: changedAtMs)
    }

    static func decodeClipboardFetchResult(
        _ value: [String: Any]
    ) -> ClipboardFetchResult? {
        guard let requestID = value["requestId"] as? String, !requestID.isEmpty else {
            return nil
        }
        // A null text is the failure shape and carries no timestamp: the fetch
        // resolved with nothing to show.
        if value["text"] is NSNull {
            return ClipboardFetchResult(
                requestID: requestID,
                text: nil,
                changedAtMs: nil
            )
        }
        guard let text = value["text"] as? String,
              let changedAtMs = decodeChangedAtMs(value)
        else {
            return nil
        }
        return ClipboardFetchResult(
            requestID: requestID,
            text: text,
            changedAtMs: changedAtMs
        )
    }

    /// A nullable timestamp is required on both v5 clipboard event shapes.
    /// The outer optional is decoding success; the inner optional is JSON
    /// null, used when the remote content predates this session.
    private static func decodeChangedAtMs(
        _ value: [String: Any]
    ) -> Int64?? {
        guard value.keys.contains("changedAtMs") else {
            return nil
        }
        let raw = value["changedAtMs"]
        if raw is NSNull {
            return .some(nil)
        }
        guard !(raw is Bool),
              let number = raw as? NSNumber,
              number.doubleValue.isFinite,
              number.doubleValue >= 0,
              // `Double(Int64.max)` rounds up to 2^63, which `int64Value`
              // cannot represent, so the bound has to be exclusive.
              number.doubleValue < Double(Int64.max),
              number.doubleValue.rounded(.towardZero) == number.doubleValue
        else {
            return nil
        }
        return .some(number.int64Value)
    }
}
