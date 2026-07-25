import AppKit
import Observation
import WebKit

@MainActor
@Observable
final class AppModel {
    private static let gatewayDefaultsKey = "gatewayAddress"

    var gatewayAddress: String
    private(set) var gateway: GatewayLocation
    private(set) var session = ViewerSessionState()
    private(set) var bridgeStatus = BridgeStatus.loading
    private(set) var navigationError: String?
    var keyboardCaptureEnabled = true {
        didSet {
            if !keyboardCaptureEnabled {
                releaseNativeKeys()
            }
        }
    }

    let clipboard: ClipboardSynchronizer

    @ObservationIgnored
    private weak var webView: WKWebView?
    @ObservationIgnored
    private var commandQueue: [[String: Any]] = []
    @ObservationIgnored
    private var drainingCommands = false
    @ObservationIgnored
    private var bridgeDeadline: Task<Void, Never>?

    init() {
        let commandLineGateway = Self.commandLineGateway()
        let stored = UserDefaults.standard.string(forKey: Self.gatewayDefaultsKey)
        let initial = commandLineGateway ?? stored ?? "http://127.0.0.1:52380"
        let parsed = (try? GatewayLocation.parse(initial))
            ?? (try! GatewayLocation.parse("http://127.0.0.1:52380"))
        gatewayAddress = parsed.url.absoluteString
        gateway = parsed
        clipboard = ClipboardSynchronizer()
        clipboard.sendCommand = { [weak self] command in
            self?.enqueue(command)
        }
    }

    deinit {
        bridgeDeadline?.cancel()
    }

    var canCaptureKeyboardNow: Bool {
        bridgeStatus == .ready
            && session.canCaptureKeyboard
            && keyboardCaptureEnabled
    }

    var windowTitle: String {
        if let target = session.connectedTarget {
            "\(target) — remotex"
        } else {
            "remotex"
        }
    }

    func attach(webView: WKWebView) {
        self.webView = webView
    }

    func detach(webView: WKWebView) {
        if self.webView === webView {
            self.webView = nil
        }
        bridgeDeadline?.cancel()
        bridgeDeadline = nil
        bridgeStatus = .loading
        releaseNativeKeys()
        clipboard.update(enabled: false)
    }

    func navigationStarted() {
        bridgeDeadline?.cancel()
        bridgeStatus = .loading
        navigationError = nil
        session = ViewerSessionState()
        releaseNativeKeys()
        clipboard.update(enabled: false)
    }

    func navigationFinished() {
        bridgeDeadline?.cancel()
        bridgeDeadline = Task { [weak self] in
            try? await Task.sleep(for: .seconds(3))
            guard !Task.isCancelled, let self, self.bridgeStatus == .loading else {
                return
            }
            self.bridgeStatus = .incompatible(
                "The viewer and gateway frontend did not complete bridge version \(ProductInfo.bridgeVersion)."
            )
        }
    }

    func navigationFailed(_ error: Error) {
        bridgeDeadline?.cancel()
        bridgeStatus = .failed(error.localizedDescription)
        navigationError = error.localizedDescription
        releaseNativeKeys()
        clipboard.update(enabled: false)
    }

    func acceptHandshake(bridgeVersion: Int, appVersion: String) -> (accepted: Bool, reason: String?) {
        guard bridgeVersion == ProductInfo.bridgeVersion else {
            let reason = "Bridge version \(bridgeVersion) does not match \(ProductInfo.bridgeVersion)."
            bridgeStatus = .incompatible(reason)
            return (false, reason)
        }
        guard appVersion == ProductInfo.version else {
            let reason = "Gateway \(appVersion) does not match viewer \(ProductInfo.version)."
            bridgeStatus = .incompatible(reason)
            return (false, reason)
        }
        bridgeDeadline?.cancel()
        bridgeDeadline = nil
        bridgeStatus = .ready
        return (true, nil)
    }

    func apply(session next: ViewerSessionState) {
        let wasCapturing = session.canCaptureKeyboard
        session = next
        if wasCapturing && !next.canCaptureKeyboard {
            releaseNativeKeys()
        }
        clipboard.update(
            enabled: bridgeStatus == .ready
                && next.screen == .desktop
                && next.connectionStatus == .connected
                && next.canClipboard
        )
    }

    func applyGatewayAddress() {
        do {
            let next = try GatewayLocation.parse(gatewayAddress)
            gatewayAddress = next.url.absoluteString
            gateway = next
            UserDefaults.standard.set(gatewayAddress, forKey: Self.gatewayDefaultsKey)
            loadGateway()
        } catch {
            navigationError = error.localizedDescription
        }
    }

    func loadGateway() {
        navigationStarted()
        webView?.load(URLRequest(url: gateway.url))
    }

    func reload() {
        navigationStarted()
        webView?.reload()
    }

    func resizeToWindow() {
        guard session.canResize else {
            return
        }
        enqueue(["type": "resize"])
    }

    func switchTarget() {
        guard session.screen == .desktop else {
            return
        }
        enqueue(["type": "switchTarget"])
    }

    func logout() {
        guard session.screen == .picker || session.screen == .desktop else {
            return
        }
        enqueue(["type": "logout"])
    }

    func takeOver() {
        guard session.connectionStatus == .busy || session.connectionStatus == .takenOver else {
            return
        }
        enqueue(["type": "takeOver"])
    }

    func sendKey(code: String, pressed: Bool, caps: Bool) {
        enqueue([
            "type": "key",
            "code": code,
            "pressed": pressed,
            "caps": caps,
        ])
    }

    func releaseNativeKeys() {
        enqueue(["type": "releaseKeys"])
    }

    func showError(_ message: String) {
        navigationError = message
    }

    func clearError() {
        navigationError = nil
    }

    private func enqueue(_ command: [String: Any]) {
        guard bridgeStatus == .ready else {
            return
        }
        commandQueue.append(command)
        guard !drainingCommands else {
            return
        }
        drainingCommands = true
        Task { [weak self] in
            await self?.drainCommandQueue()
        }
    }

    private func drainCommandQueue() async {
        while !commandQueue.isEmpty {
            let command = commandQueue.removeFirst()
            guard let webView else {
                commandQueue.removeAll()
                break
            }
            do {
                let result = try await webView.callAsyncJavaScript(
                    "return window.__remotexNativeDispatch(command);",
                    arguments: ["command": command],
                    in: nil,
                    contentWorld: .page
                )
                if let reply = result as? [String: Any],
                   reply["ok"] as? Bool != true,
                   let error = reply["error"] as? String
                {
                    navigationError = error
                }
            } catch {
                navigationError = "Native command failed: \(error.localizedDescription)"
                commandQueue.removeAll()
                break
            }
        }
        drainingCommands = false
    }

    private static func commandLineGateway() -> String? {
        let arguments = ProcessInfo.processInfo.arguments
        guard let flag = arguments.firstIndex(of: "--gateway"),
              arguments.indices.contains(flag + 1)
        else {
            return nil
        }
        return arguments[flag + 1]
    }
}
