import AppKit
import Observation
import WebKit

@MainActor
@Observable
final class AppModel {
    private static let gatewayDefaultsKey = "gatewayAddress"
    private static let keyboardOverridesDefaultsKey = "macOSKeyboardOverridesEnabled"

    var gatewayAddress: String
    var macOSKeyboardOverridesEnabled: Bool {
        didSet {
            guard macOSKeyboardOverridesEnabled != oldValue else {
                return
            }
            defaults.set(
                macOSKeyboardOverridesEnabled,
                forKey: Self.keyboardOverridesDefaultsKey
            )
            releaseNativeKeys()
        }
    }
    private(set) var gateway: GatewayLocation
    private(set) var session = ViewerSessionState()
    private(set) var bridgeStatus = BridgeStatus.loading
    private(set) var navigationError: String?

    let clipboard: ClipboardSynchronizer

    @ObservationIgnored
    private weak var webView: WKWebView?
    @ObservationIgnored
    private let defaults: UserDefaults
    @ObservationIgnored
    private var commandQueue: [[String: Any]] = []
    @ObservationIgnored
    private var drainingCommands = false
    @ObservationIgnored
    private var bridgeDeadline: Task<Void, Never>?
    @ObservationIgnored
    private var pressedNativeKeys = NativePressedKeys()

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        let commandLineGateway = Self.commandLineGateway()
        let stored = defaults.string(forKey: Self.gatewayDefaultsKey)
        let initial = commandLineGateway ?? stored ?? "http://127.0.0.1:52380"
        let parsed = (try? GatewayLocation.parse(initial))
            ?? (try! GatewayLocation.parse("http://127.0.0.1:52380"))
        gatewayAddress = parsed.url.absoluteString
        macOSKeyboardOverridesEnabled =
            defaults.object(forKey: Self.keyboardOverridesDefaultsKey) as? Bool ?? true
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
    }

    var macOSKeyboardOverridesActive: Bool {
        macOSKeyboardOverridesEnabled && !session.remoteIsMac
    }

    var macOSKeyboardOverridesLabel: String {
        session.remoteIsMac
            ? "macOS Keyboard Overrides (Not Applicable)"
            : "Enable macOS Keyboard Overrides"
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
        releaseNativeKeys()
        if self.webView === webView {
            self.webView = nil
        }
        bridgeDeadline?.cancel()
        bridgeDeadline = nil
        bridgeStatus = .loading
        clipboard.update(enabled: false)
    }

    func navigationStarted() {
        releaseNativeKeys()
        bridgeDeadline?.cancel()
        bridgeStatus = .loading
        navigationError = nil
        session = ViewerSessionState()
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
        releaseNativeKeys()
        bridgeDeadline?.cancel()
        bridgeStatus = .failed(error.localizedDescription)
        navigationError = error.localizedDescription
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
        let guestChanged = session.remoteIsMac != next.remoteIsMac
        if wasCapturing && (!next.canCaptureKeyboard || guestChanged) {
            releaseNativeKeys()
        }
        session = next
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
            defaults.set(gatewayAddress, forKey: Self.gatewayDefaultsKey)
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

    /// Apply one of the resolutions the remote offered. Unlike `resizeToWindow`
    /// this is a pick off a fixed list — a Mac's virtual display takes nothing
    /// else — so an entry the gateway no longer offers is dropped here rather
    /// than sent and refused.
    func setResolution(_ mode: DisplayMode) {
        guard session.displayModes.contains(mode) else {
            return
        }
        enqueue(["type": "setResolution", "w": mode.w, "h": mode.h])
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
        pressedNativeKeys.record(code: code, pressed: pressed)
        enqueue([
            "type": "key",
            "code": code,
            "pressed": pressed,
            "caps": caps,
        ])
    }

    func releaseNativeKeys() {
        guard pressedNativeKeys.takeForRelease() else {
            return
        }
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
