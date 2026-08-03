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

    // Nothing about quitting is here any more, and that is the fix rather than a
    // tidy-up. Stopping the engine on this stack meant stopping it from inside
    // `terminate:`, where the run loop is not turning — and a CEF browser's close is
    // half AppKit's work, so it simply never finished. The quit is the delegate's
    // now: `applicationShouldTerminate` answers `terminateLater`, which is AppKit's
    // own way of saying "keep the run loop going, I will tell you when".
}
