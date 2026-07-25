@preconcurrency import AppKit
import WebKit

@MainActor
final class KeyboardCapture {
    private weak var model: AppModel?
    private weak var webView: WKWebView?
    private var translator = KeyboardTranslator()
    private var monitor: Any?
    private var observers: [NSObjectProtocol] = []
    private var suppressedKeyUps = Set<UInt16>()

    init(model: AppModel, webView: WKWebView) {
        self.model = model
        self.webView = webView
        monitor = NSEvent.addLocalMonitorForEvents(
            matching: [.keyDown, .keyUp, .flagsChanged]
        ) { [weak self] event in
            let consumed = MainActor.assumeIsolated {
                self?.consume(event) ?? false
            }
            return consumed ? nil : event
        }
        let center = NotificationCenter.default
        observers.append(
            center.addObserver(
                forName: NSApplication.didResignActiveNotification,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated {
                    self?.releaseAll()
                }
            }
        )
        observers.append(
            center.addObserver(
                forName: NSWindow.didResignKeyNotification,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self, self.webView?.window?.isKeyWindow == false else {
                        return
                    }
                    self.releaseAll()
                }
            }
        )
    }

    func invalidate() {
        releaseAll()
        if let monitor {
            NSEvent.removeMonitor(monitor)
            self.monitor = nil
        }
        for observer in observers {
            NotificationCenter.default.removeObserver(observer)
        }
        observers.removeAll()
    }

    private func consume(_ event: NSEvent) -> Bool {
        guard let model, let webView, event.window === webView.window else {
            return false
        }
        if event.type == .keyUp, suppressedKeyUps.remove(event.keyCode) != nil {
            return true
        }
        if isCaptureToggle(event), model.session.canCaptureKeyboard {
            guard model.keyboardCaptureEnabled else {
                // Let the SwiftUI menu key equivalent turn capture back on.
                return false
            }
            if event.type == .keyDown {
                suppressedKeyUps.insert(event.keyCode)
                releaseAll()
                model.keyboardCaptureEnabled = false
            }
            return true
        }
        guard model.canCaptureKeyboardNow else {
            return false
        }

        if event.type == .keyDown,
           KeyboardTranslator.domCode(for: event.keyCode) == "KeyV",
           event.modifierFlags.contains(.command)
        {
            model.clipboard.pushLocalClipboard(force: true)
        }
        for translated in translator.translate(
            event,
            mapCommandToControl: model.session.guestOS != .macos
        ) {
            model.sendKey(
                code: translated.code,
                pressed: translated.pressed,
                caps: translated.caps
            )
        }
        return true
    }

    private func releaseAll() {
        translator.reset()
        model?.releaseNativeKeys()
    }

    private func isCaptureToggle(_ event: NSEvent) -> Bool {
        guard event.type == .keyDown || event.type == .keyUp else {
            return false
        }
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        return event.keyCode == 0x35
            && modifiers.contains([.control, .option, .command])
    }
}
