import AppKit

@MainActor
final class ClipboardSynchronizer {
    var sendCommand: (([String: Any]) -> Void)?

    private let pasteboard = NSPasteboard.general
    private var timer: Timer?
    private var enabled = false
    private var observedChangeCount = 0
    private var lastFromRemote: String?
    private var lastToRemote: String?

    isolated deinit {
        timer?.invalidate()
        timer = nil
    }

    func update(enabled: Bool) {
        guard self.enabled != enabled else {
            return
        }
        self.enabled = enabled
        if enabled {
            observedChangeCount = pasteboard.changeCount
            sendCommand?(["type": "clipboardRequest"])
            pushLocalClipboard(force: false)
            let timer = Timer(timeInterval: 0.4, repeats: true) {
                [weak self] _ in
                MainActor.assumeIsolated {
                    self?.poll()
                }
            }
            RunLoop.main.add(timer, forMode: .common)
            self.timer = timer
        } else {
            timer?.invalidate()
            timer = nil
            lastFromRemote = nil
            lastToRemote = nil
        }
    }

    func receiveRemote(_ text: String) {
        guard enabled, !text.isEmpty else {
            return
        }
        lastFromRemote = text
        if text == lastToRemote {
            return
        }
        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)
        observedChangeCount = pasteboard.changeCount
    }

    func pushLocalClipboard(force: Bool) {
        guard enabled, let text = pasteboard.string(forType: .string), !text.isEmpty else {
            return
        }
        guard force || (text != lastFromRemote && text != lastToRemote) else {
            return
        }
        lastToRemote = text
        sendCommand?([
            "type": "clipboard",
            "text": text,
        ])
    }

    func synchronizeNow() {
        guard enabled else {
            return
        }
        pushLocalClipboard(force: true)
        sendCommand?(["type": "clipboardRequest"])
    }

    private func poll() {
        guard enabled, pasteboard.changeCount != observedChangeCount else {
            return
        }
        observedChangeCount = pasteboard.changeCount
        pushLocalClipboard(force: false)
    }
}
