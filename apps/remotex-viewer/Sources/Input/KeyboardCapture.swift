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
        guard model.canCaptureKeyboardNow else {
            return false
        }
        if event.modifierFlags.contains(.command),
           let code = KeyboardTranslator.domCode(for: event.keyCode),
           ["KeyQ", "KeyW", "Comma"].contains(code)
        {
            return false
        }

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
