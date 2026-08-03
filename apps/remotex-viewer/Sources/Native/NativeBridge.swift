import AppKit
import CRemotexCEF
import Foundation

/// The app's two ends of the seam: what the page posts here, and what this sends
/// back to the page.
///
/// One query function and one `ExecuteJavaScript` call, which is the whole of the
/// app-to-client protocol. There is no frame stream, no envelope framing and no
/// loopback file server here: the page holds the session and this app is not part
/// of that conversation.
///
/// What used to be two more jobs on this class are gone from Swift entirely.
/// Deciding which navigations are allowed and telling the page where its gateway
/// is are both the engine's now — `client::permits` and the scheme handler in
/// `crates/remotex-cef` — because both are about a URL, and the page's URL is
/// something Chromium sees before this side hears about it.
@MainActor
final class NativeBridge: NSObject, CommandSink {
    private weak var model: AppModel?
    /// The browser commands are evaluated against, once there is one.
    private var browser: OpaquePointer?

    init(model: AppModel) {
        self.model = model
    }

    /// Show the client in `view`, and start listening to it.
    ///
    /// The unretained pointer handed across is this object, and it is safe for the
    /// same reason it is unusual: `detach()` closes the browser before this is
    /// released, so nothing can call back into a freed bridge.
    func attach(to view: NSView, gateway: GatewayEndpoint) {
        let context = Unmanaged.passUnretained(self).toOpaque()
        browser = gateway.origin.withCString { origin in
            remotex_cef_create(
                Unmanaged.passUnretained(view).toOpaque(),
                origin,
                { json, context in
                    guard let json, let context else {
                        return
                    }
                    // CEF delivers this on its UI thread, which in this process is
                    // the main thread — the same one AppKit is on. `assumeIsolated`
                    // is the assertion of that, not a way around it.
                    MainActor.assumeIsolated {
                        Unmanaged<NativeBridge>.fromOpaque(context)
                            .takeUnretainedValue()
                            .receive(String(cString: json))
                    }
                },
                context
            )
        }
        if browser == nil {
            model?.pageFailedToLoad("Chromium would not open the client.")
        }
    }

    func detach() {
        if let browser {
            remotex_cef_close(browser)
        }
        browser = nil
    }

    /// Open Chromium's inspector on the page.
    func showDevTools() {
        guard let browser else {
            return
        }
        remotex_cef_show_dev_tools(browser)
    }

    /// Send one command to the page. Dropped silently before the first load, which
    /// is a menu item pressed while the window is still empty.
    func send(_ command: NativeCommand) {
        guard let browser, let script = command.javaScript() else {
            return
        }
        script.withCString { remotex_cef_execute(browser, $0) }
    }

    /// One `NativeEvent`, exactly as `nativeHost.ts` posted it.
    private func receive(_ json: String) {
        guard
            let body = try? JSONSerialization.jsonObject(with: Data(json.utf8)),
            let event = NativeEvent.decode(body)
        else {
            return
        }
        model?.apply(event)
    }
}
