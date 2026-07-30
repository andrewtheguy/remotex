@preconcurrency import AppKit

@MainActor
final class KeyboardCapture {
    private weak var model: AppModel?
    private weak var surface: NSView?
    private var translator = KeyboardTranslator()
    private var monitor: Any?
    private var observers: [NSObjectProtocol] = []
    private var firstResponderObservation: NSKeyValueObservation?
    private weak var observedWindow: NSWindow?
    private var isInvalidated = false

    init(model: AppModel, surface: NSView) {
        self.model = model
        self.surface = surface
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
                    guard let self, self.surface?.window?.isKeyWindow == false else {
                        return
                    }
                    self.releaseAll()
                }
            }
        )
        Task { [weak self] in
            await Task.yield()
            self?.updateWindowObservation()
        }
    }

    func invalidate() {
        // Before the teardown, not after: the deferred setup in `init` may still
        // be waiting its turn, and `RemoteSurfaceHost` holds this until it has
        // built the next one.
        isInvalidated = true
        releaseAll()
        firstResponderObservation?.invalidate()
        firstResponderObservation = nil
        observedWindow = nil
        if let monitor {
            NSEvent.removeMonitor(monitor)
            self.monitor = nil
        }
        for observer in observers {
            NotificationCenter.default.removeObserver(observer)
        }
        observers.removeAll()
    }

    func updateWindowObservation() {
        // Retired. The observation this would install outlives `invalidate`, and
        // the surface it reports first responders for is no longer this one's.
        guard !isInvalidated else {
            return
        }
        guard let window = surface?.window else {
            // Detached: the observation would otherwise keep the old window
            // alive and keep reporting its first responder as ours.
            firstResponderObservation?.invalidate()
            firstResponderObservation = nil
            observedWindow = nil
            return
        }
        guard observedWindow !== window else {
            return
        }
        firstResponderObservation?.invalidate()
        observedWindow = window
        firstResponderObservation = window.observe(
            \.firstResponder,
            options: [.initial, .new]
        ) { [weak self] window, _ in
            MainActor.assumeIsolated {
                self?.firstResponderChanged(window.firstResponder)
            }
        }
    }

    /// Whether `responder` is the remote surface or something inside it.
    ///
    /// The gate that keeps typing into the clipboard card's editor from being
    /// forwarded to the remote: an `NSTextView` is not the surface.
    static func capturesFirstResponder(
        _ responder: NSResponder?,
        inside surface: NSView
    ) -> Bool {
        guard let view = responder as? NSView else {
            return false
        }
        return view === surface || view.isDescendant(of: surface)
    }

    private func consume(_ event: NSEvent) -> Bool {
        guard let model, let surface, event.window === surface.window else {
            return false
        }
        guard Self.capturesFirstResponder(
            surface.window?.firstResponder,
            inside: surface
        ) else {
            releaseAll()
            return false
        }
        // Nothing to capture for: no desktop, or no first frame yet. Whatever the
        // translator was part way through goes with it: a Command left pending, or
        // the synthetic Control a mapped chord rides on, would otherwise be resumed
        // against a remote whose releases have already been sent.
        guard model.canCaptureKeyboardNow else {
            releaseAll()
            return false
        }
        // Nothing is held back from here on, Command-Q and Command-W included: a
        // focused desktop gets every chord this Mac is given, and what the system
        // keeps for itself (Command-Tab, Command-Space, the screen shot chords,
        // anything bound in System Settings) it never delivers here to begin with.
        // Exempting a few would make the meaning of a chord depend on which one it
        // was, and Quit in particular was a keystroke that ended the session while
        // the user was typing at the guest. Moving focus off the desktop is the way
        // to get the whole keyboard back.
        let mapCommandToControl = model.macOSKeyboardOverridesActive
        if event.type == .keyDown,
           KeyboardTranslator.domCode(for: event.keyCode) == "KeyV",
           event.modifierFlags.contains(.command),
           mapCommandToControl || model.session.remoteIsMac
        {
            model.clipboard.pushLocalClipboard(force: true)
        }
        for translated in translator.translate(
            event,
            mapCommandToControl: mapCommandToControl
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
        model?.releaseInput()
    }

    private func firstResponderChanged(_ responder: NSResponder?) {
        guard let surface,
              !Self.capturesFirstResponder(responder, inside: surface)
        else {
            return
        }
        releaseAll()
    }
}
