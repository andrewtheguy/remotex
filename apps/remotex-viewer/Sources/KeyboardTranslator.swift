import AppKit

struct TranslatedKeyEvent: Equatable {
    let code: String
    let pressed: Bool
    let caps: Bool
}

struct KeyboardTranslator {
    private var pendingCommandCodes = Set<UInt16>()
    private var forwardedCommandCodes = Set<UInt16>()
    private var commandWasUsed = false
    private var translatedCommandKeys = Set<UInt16>()
    private var syntheticControlHeld = false

    mutating func translate(
        _ event: NSEvent,
        mapCommandToControl: Bool = true
    ) -> [TranslatedKeyEvent] {
        let caps = event.modifierFlags.contains(.capsLock)
        switch event.type {
        case .flagsChanged:
            return translateModifier(
                event,
                caps: caps,
                mapCommandToControl: mapCommandToControl
            )
        case .keyDown:
            return translateKey(
                event,
                pressed: true,
                caps: caps,
                mapCommandToControl: mapCommandToControl
            )
        case .keyUp:
            return translateKey(
                event,
                pressed: false,
                caps: caps,
                mapCommandToControl: mapCommandToControl
            )
        default:
            return []
        }
    }

    mutating func reset() {
        pendingCommandCodes.removeAll()
        forwardedCommandCodes.removeAll()
        translatedCommandKeys.removeAll()
        commandWasUsed = false
        syntheticControlHeld = false
    }

    private mutating func translateModifier(
        _ event: NSEvent,
        caps: Bool,
        mapCommandToControl: Bool
    ) -> [TranslatedKeyEvent] {
        guard let code = Self.domCode(for: event.keyCode) else {
            return []
        }
        if code == "CapsLock" {
            return [
                TranslatedKeyEvent(code: code, pressed: true, caps: caps),
                TranslatedKeyEvent(code: code, pressed: false, caps: caps),
            ]
        }
        if code == "MetaLeft" || code == "MetaRight" {
            if !mapCommandToControl {
                guard let mask = Self.sideMask(for: code) else {
                    return []
                }
                return [
                    TranslatedKeyEvent(
                        code: code,
                        pressed: event.modifierFlags.rawValue & mask != 0,
                        caps: caps
                    ),
                ]
            }
            return translateCommandModifier(event, code: code, caps: caps)
        }
        guard let mask = Self.sideMask(for: code) else {
            return []
        }
        return [
            TranslatedKeyEvent(
                code: code,
                pressed: event.modifierFlags.rawValue & mask != 0,
                caps: caps
            ),
        ]
    }

    private mutating func translateCommandModifier(
        _ event: NSEvent,
        code: String,
        caps: Bool
    ) -> [TranslatedKeyEvent] {
        guard let mask = Self.sideMask(for: code) else {
            return []
        }
        let pressed = event.modifierFlags.rawValue & mask != 0
        if pressed {
            pendingCommandCodes.insert(event.keyCode)
            return []
        }

        let wasPending = pendingCommandCodes.remove(event.keyCode) != nil
        if forwardedCommandCodes.remove(event.keyCode) != nil {
            return [TranslatedKeyEvent(code: code, pressed: false, caps: caps)]
        }
        guard wasPending else {
            return []
        }
        if !commandWasUsed && pendingCommandCodes.isEmpty {
            return [
                TranslatedKeyEvent(code: code, pressed: true, caps: caps),
                TranslatedKeyEvent(code: code, pressed: false, caps: caps),
            ]
        }
        if pendingCommandCodes.isEmpty {
            commandWasUsed = false
        }
        return []
    }

    private mutating func translateKey(
        _ event: NSEvent,
        pressed: Bool,
        caps: Bool,
        mapCommandToControl: Bool
    ) -> [TranslatedKeyEvent] {
        guard let code = Self.domCode(for: event.keyCode) else {
            return []
        }
        if mapCommandToControl,
           pressed,
           event.modifierFlags.contains(.command),
           Self.commandMapsToControl(code)
        {
            commandWasUsed = true
            translatedCommandKeys.insert(event.keyCode)
            var translated: [TranslatedKeyEvent] = []
            if !syntheticControlHeld {
                syntheticControlHeld = true
                translated.append(
                    TranslatedKeyEvent(code: "ControlLeft", pressed: true, caps: caps)
                )
            }
            translated.append(TranslatedKeyEvent(code: code, pressed: true, caps: caps))
            return translated
        }
        if !pressed, translatedCommandKeys.remove(event.keyCode) != nil {
            var translated = [TranslatedKeyEvent(code: code, pressed: false, caps: caps)]
            if translatedCommandKeys.isEmpty, syntheticControlHeld {
                syntheticControlHeld = false
                translated.append(
                    TranslatedKeyEvent(code: "ControlLeft", pressed: false, caps: caps)
                )
            }
            return translated
        }

        var translated: [TranslatedKeyEvent] = []
        if mapCommandToControl, pressed, event.modifierFlags.contains(.command) {
            commandWasUsed = true
            for keyCode in pendingCommandCodes.subtracting(forwardedCommandCodes) {
                guard let commandCode = Self.domCode(for: keyCode) else {
                    continue
                }
                forwardedCommandCodes.insert(keyCode)
                translated.append(
                    TranslatedKeyEvent(code: commandCode, pressed: true, caps: caps)
                )
            }
        }
        translated.append(TranslatedKeyEvent(code: code, pressed: pressed, caps: caps))
        return translated
    }

    private static func commandMapsToControl(_ code: String) -> Bool {
        switch code {
        case "KeyA", "KeyC", "KeyF", "KeyL", "KeyN", "KeyO", "KeyP", "KeyR",
             "KeyS", "KeyT", "KeyV", "KeyW", "KeyX", "KeyZ":
            true
        default:
            false
        }
    }

    // Device-dependent bits (NX_DEVICE*_KEYMASK) are the only way to tell the
    // two physical keys of a pair apart.
    private static func sideMask(for code: String) -> UInt? {
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
    // This is the inverse of rxa-proto's mac_keycode table for the canonical
    // keys the gateway's RDP and VNC adapters both support.
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
