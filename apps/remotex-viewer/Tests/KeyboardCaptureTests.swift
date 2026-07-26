import AppKit
import Testing
import WebKit
@testable import RemotexViewer

struct KeyboardCaptureTests {
    @Test
    @MainActor
    func webKitRespondersAreCapturedButANativeClipboardEditorIsNot() {
        let webView = WKWebView(frame: .zero)
        let webContentResponder = FocusableView(frame: .zero)
        webView.addSubview(webContentResponder)
        let nativeClipboardEditor = NSTextView(frame: .zero)

        #expect(
            KeyboardCapture.capturesFirstResponder(
                webContentResponder,
                inside: webView
            )
        )
        #expect(
            !KeyboardCapture.capturesFirstResponder(
                nativeClipboardEditor,
                inside: webView
            )
        )
        #expect(!KeyboardCapture.capturesFirstResponder(nil, inside: webView))
    }
}

private final class FocusableView: NSView {
    override var acceptsFirstResponder: Bool { true }
}
