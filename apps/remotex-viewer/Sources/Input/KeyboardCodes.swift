import AppKit

/// One key as the page's input path wants it: a DOM `code`, a direction, and the
/// two modifier states that change what it means.
struct NativeKeyEvent: Equatable {
    let code: String
    let pressed: Bool
    let caps: Bool
    /// Whether Command was down. Read off the event rather than inferred from the
    /// presses seen so far, because Command may already have been held when the
    /// surface took focus — and it is what tells the page a chord from a keystroke.
    let meta: Bool
}

/// macOS key events, in the alphabet the client speaks.
///
/// This is the whole of the app's keyboard now: a virtual keycode becomes a DOM
/// `code` and goes to the page, which owns the Command-to-Control translation, the
/// bare-Command tap, and the record of what is held (`frontend/src/macKeys.ts`).
/// There was a second implementation of all three here; the page's is the one both
/// clients use, so a chord means the same thing in a browser and in this window,
/// and it is tested once.
///
/// What *cannot* move to the page is above: `NSEvent.addLocalMonitorForEvents` sees
/// ⌘W and ⌘Q before the menu bar does, and WebKit never receives them at all. That
/// is the reason this app exists around the page, and it is the reason the page is
/// told which chords its host can capture.
enum KeyboardCodes {
    /// The events to send for one `NSEvent`, or none for a key with no `code`.
    ///
    /// Modifiers arrive as `flagsChanged` with no direction of their own, so the
    /// direction is read out of the flags — the device-dependent bit for that
    /// physical key, which is the only thing that tells left from right.
    static func events(for event: NSEvent) -> [NativeKeyEvent] {
        guard let code = domCode(for: event.keyCode) else {
            return []
        }
        let caps = event.modifierFlags.contains(.capsLock)
        let meta = event.modifierFlags.contains(.command)
        switch event.type {
        case .keyDown:
            return [NativeKeyEvent(code: code, pressed: true, caps: caps, meta: meta)]
        case .keyUp:
            return [NativeKeyEvent(code: code, pressed: false, caps: caps, meta: meta)]
        case .flagsChanged:
            // Caps Lock has no side mask: the lock state is the flag, so engaging
            // it is a press and disengaging it a release. That is what a browser
            // reports too, and the page turns either into the tap a guest wants.
            if code == "CapsLock" {
                return [
                    NativeKeyEvent(code: code, pressed: caps, caps: caps, meta: meta),
                ]
            }
            guard let mask = sideMask(for: code) else {
                return []
            }
            let pressed = event.modifierFlags.rawValue & mask != 0
            return [
                NativeKeyEvent(code: code, pressed: pressed, caps: caps, meta: meta),
            ]
        default:
            return []
        }
    }

    // Device-dependent bits (NX_DEVICE*_KEYMASK) are the only way to tell the
    // two physical keys of a pair apart.
    static func sideMask(for code: String) -> UInt? {
        switch code {
        case "ShiftLeft": 0x0002
        case "ShiftRight": 0x0004
        case "ControlLeft": 0x0001
        case "ControlRight": 0x2000
        case "AltLeft": 0x0020
        case "AltRight": 0x0040
        case "MetaLeft": 0x0008
        case "MetaRight": 0x0010
        default: nil
        }
    }

    static func domCode(for keyCode: UInt16) -> String? {
        keyCodes[keyCode]
    }

    // macOS virtual keycodes are physical positions, exactly like DOM `code`.
    // This is the inverse of the canonical mac_keycode table for the keys the
    // gateway's RDP and VNC adapters both support.
    private static let keyCodes: [UInt16: String] = [
        0x00: "KeyA", 0x01: "KeyS", 0x02: "KeyD", 0x03: "KeyF",
        0x04: "KeyH", 0x05: "KeyG", 0x06: "KeyZ", 0x07: "KeyX",
        0x08: "KeyC", 0x09: "KeyV", 0x0B: "KeyB", 0x0C: "KeyQ",
        0x0D: "KeyW", 0x0E: "KeyE", 0x0F: "KeyR", 0x10: "KeyY",
        0x11: "KeyT", 0x12: "Digit1", 0x13: "Digit2", 0x14: "Digit3",
        0x15: "Digit4", 0x16: "Digit6", 0x17: "Digit5", 0x18: "Equal",
        0x19: "Digit9", 0x1A: "Digit7", 0x1B: "Minus", 0x1C: "Digit8",
        0x1D: "Digit0", 0x1E: "BracketRight", 0x1F: "KeyO",
        0x20: "KeyU", 0x21: "BracketLeft", 0x22: "KeyI", 0x23: "KeyP",
        0x24: "Enter", 0x25: "KeyL", 0x26: "KeyJ", 0x27: "Quote",
        0x28: "KeyK", 0x29: "Semicolon", 0x2A: "Backslash",
        0x2B: "Comma", 0x2C: "Slash", 0x2D: "KeyN", 0x2E: "KeyM",
        0x2F: "Period", 0x30: "Tab", 0x31: "Space", 0x32: "Backquote",
        0x33: "Backspace", 0x35: "Escape", 0x36: "MetaRight",
        0x37: "MetaLeft", 0x38: "ShiftLeft", 0x39: "CapsLock",
        0x3A: "AltLeft", 0x3B: "ControlLeft", 0x3C: "ShiftRight",
        0x3D: "AltRight", 0x3E: "ControlRight", 0x41: "NumpadDecimal",
        0x43: "NumpadMultiply", 0x45: "NumpadAdd", 0x47: "NumLock",
        0x4B: "NumpadDivide", 0x4C: "NumpadEnter", 0x4E: "NumpadSubtract",
        0x52: "Numpad0", 0x53: "Numpad1", 0x54: "Numpad2",
        0x55: "Numpad3", 0x56: "Numpad4", 0x57: "Numpad5",
        0x58: "Numpad6", 0x59: "Numpad7", 0x5B: "Numpad8",
        0x5C: "Numpad9", 0x60: "F5", 0x61: "F6", 0x62: "F7",
        0x63: "F3", 0x64: "F8", 0x65: "F9", 0x67: "F11",
        0x6D: "F10", 0x6F: "F12", 0x73: "Home", 0x74: "PageUp",
        0x75: "Delete", 0x76: "F4", 0x77: "End", 0x78: "F2",
        0x79: "PageDown", 0x7A: "F1", 0x7B: "ArrowLeft",
        0x7C: "ArrowRight", 0x7D: "ArrowDown", 0x7E: "ArrowUp",
    ]
}
