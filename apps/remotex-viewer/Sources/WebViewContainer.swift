import SwiftUI
import WebKit

struct WebViewContainer: NSViewRepresentable {
    let model: AppModel

    func makeCoordinator() -> Coordinator {
        Coordinator(model: model)
    }

    func makeNSView(context: Context) -> WKWebView {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .default()
        configuration.preferences.isElementFullscreenEnabled = true

        let controller = WKUserContentController()
        let bridge = NativeBridge(model: model)
        controller.addScriptMessageHandler(
            bridge,
            contentWorld: .page,
            name: NativeBridge.handlerName
        )
        controller.addUserScript(
            WKUserScript(
                source: Self.hostDescriptorScript(),
                injectionTime: .atDocumentStart,
                forMainFrameOnly: true,
                in: .page
            )
        )
        configuration.userContentController = controller

        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.navigationDelegate = bridge
        webView.allowsMagnification = true
        webView.allowsBackForwardNavigationGestures = false

        context.coordinator.bridge = bridge
        context.coordinator.keyboard = KeyboardCapture(model: model, webView: webView)
        model.attach(webView: webView)
        webView.load(URLRequest(url: model.gateway.url))
        return webView
    }

    func updateNSView(_ webView: WKWebView, context: Context) {}

    static func dismantleNSView(_ webView: WKWebView, coordinator: Coordinator) {
        webView.stopLoading()
        webView.navigationDelegate = nil
        webView.configuration.userContentController.removeScriptMessageHandler(
            forName: NativeBridge.handlerName,
            contentWorld: .page
        )
        coordinator.model?.detach(webView: webView)
        coordinator.keyboard?.invalidate()
        coordinator.keyboard = nil
        coordinator.bridge = nil
    }

    @MainActor
    final class Coordinator {
        weak var model: AppModel?
        var bridge: NativeBridge?
        var keyboard: KeyboardCapture?

        init(model: AppModel) {
            self.model = model
        }
    }

    private static func hostDescriptorScript() -> String {
        let descriptor: [String: Any] = [
            "bridgeVersion": ProductInfo.bridgeVersion,
            "viewerVersion": ProductInfo.version,
        ]
        let data = try! JSONSerialization.data(withJSONObject: descriptor, options: [.sortedKeys])
        let json = String(decoding: data, as: UTF8.self)
        return """
        Object.defineProperty(window, "__remotexNativeHost", {
          value: Object.freeze(\(json)),
          configurable: false,
          enumerable: false,
          writable: false
        });
        """
    }
}
