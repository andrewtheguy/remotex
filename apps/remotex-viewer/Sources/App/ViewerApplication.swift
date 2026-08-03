import AppKit

/// The `NSApplication` Chromium requires, named by `NSPrincipalClass` in
/// `Info.plist`.
///
/// CEF will not run under a plain `NSApplication`. It asks the running app whether
/// it is currently inside `-sendEvent:` — the `CefAppProtocol` conformance below —
/// because Chromium reaches into the event loop from its own message pump and has
/// to be able to tell a re-entrant dispatch from a fresh one. There is no way to
/// supply that from outside `NSApp`, which is why this subclass exists and why it
/// is named in the plist rather than installed at startup: `NSApplication.shared`
/// reads the principal class, and by the time any of our code runs the instance
/// already exists.
///
/// Nothing else belongs here. The menu bar, the key monitor and the model are all
/// `ViewerApplicationDelegate`'s.
final class ViewerApplication: NSApplication {
    private var handlingSendEvent = false

    override func sendEvent(_ event: NSEvent) {
        let wasHandling = handlingSendEvent
        if !wasHandling {
            handlingSendEvent = true
        }
        super.sendEvent(event)
        if !wasHandling {
            handlingSendEvent = false
        }
    }

    /// `CrAppProtocol`, by name. Declared `@objc` because Chromium looks it up as a
    /// selector rather than through any Swift type.
    @objc var isHandlingSendEvent: Bool {
        handlingSendEvent
    }

    @objc(setHandlingSendEvent:)
    func setHandlingSendEvent(_ handling: Bool) {
        handlingSendEvent = handling
    }

    /// Quit by closing the browser, not by `exit()`.
    ///
    /// `NSApplication`'s own `terminate:` ends the process without leaving the run
    /// loop, and Chromium depends on leaving it to shut down in order. So the
    /// engine is stopped first and the ordinary termination follows — which is what
    /// keeps `applicationWillTerminate` reachable, and with it the gateway's own
    /// stop. (The gateway's real guarantee is still the stdin pipe: the kernel
    /// closes this process's end however it dies. This only makes the ordinary quit
    /// immediate.)
    override func terminate(_ sender: Any?) {
        MainActor.assumeIsolated {
            ChromiumHost.stop()
        }
        super.terminate(sender)
    }
}
