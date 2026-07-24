//! Maps DOM `KeyboardEvent.code` values to RDP (scan code set 1) make codes.
//!
//! Returns `(scancode, extended)` where `extended` marks keys that are sent
//! with the `E0` prefix (the RDP `EXTENDED` fast-path flag). The scancode is
//! the set-1 make code; the release variant is derived by the caller via the
//! `RELEASE` flag. Assumes a US layout for the MVP — see docs/phase1-mvp.md.

/// Look up the set-1 scancode and extended-key flag for a DOM `code`.
pub fn scancode(code: &str) -> Option<(u8, bool)> {
    let entry = match code {
        // ── Row: escape + function keys ─────────────────────────────────
        "Escape" => (0x01, false),
        "F1" => (0x3B, false),
        "F2" => (0x3C, false),
        "F3" => (0x3D, false),
        "F4" => (0x3E, false),
        "F5" => (0x3F, false),
        "F6" => (0x40, false),
        "F7" => (0x41, false),
        "F8" => (0x42, false),
        "F9" => (0x43, false),
        "F10" => (0x44, false),
        "F11" => (0x57, false),
        "F12" => (0x58, false),

        // ── Number row ──────────────────────────────────────────────────
        "Backquote" => (0x29, false),
        "Digit1" => (0x02, false),
        "Digit2" => (0x03, false),
        "Digit3" => (0x04, false),
        "Digit4" => (0x05, false),
        "Digit5" => (0x06, false),
        "Digit6" => (0x07, false),
        "Digit7" => (0x08, false),
        "Digit8" => (0x09, false),
        "Digit9" => (0x0A, false),
        "Digit0" => (0x0B, false),
        "Minus" => (0x0C, false),
        "Equal" => (0x0D, false),
        "Backspace" => (0x0E, false),

        // ── Top letter row ──────────────────────────────────────────────
        "Tab" => (0x0F, false),
        "KeyQ" => (0x10, false),
        "KeyW" => (0x11, false),
        "KeyE" => (0x12, false),
        "KeyR" => (0x13, false),
        "KeyT" => (0x14, false),
        "KeyY" => (0x15, false),
        "KeyU" => (0x16, false),
        "KeyI" => (0x17, false),
        "KeyO" => (0x18, false),
        "KeyP" => (0x19, false),
        "BracketLeft" => (0x1A, false),
        "BracketRight" => (0x1B, false),
        "Backslash" => (0x2B, false),

        // ── Home letter row ─────────────────────────────────────────────
        "CapsLock" => (0x3A, false),
        "KeyA" => (0x1E, false),
        "KeyS" => (0x1F, false),
        "KeyD" => (0x20, false),
        "KeyF" => (0x21, false),
        "KeyG" => (0x22, false),
        "KeyH" => (0x23, false),
        "KeyJ" => (0x24, false),
        "KeyK" => (0x25, false),
        "KeyL" => (0x26, false),
        "Semicolon" => (0x27, false),
        "Quote" => (0x28, false),
        "Enter" => (0x1C, false),

        // ── Bottom letter row ───────────────────────────────────────────
        "ShiftLeft" => (0x2A, false),
        "KeyZ" => (0x2C, false),
        "KeyX" => (0x2D, false),
        "KeyC" => (0x2E, false),
        "KeyV" => (0x2F, false),
        "KeyB" => (0x30, false),
        "KeyN" => (0x31, false),
        "KeyM" => (0x32, false),
        "Comma" => (0x33, false),
        "Period" => (0x34, false),
        "Slash" => (0x35, false),
        "ShiftRight" => (0x36, false),

        // ── Modifiers + space ───────────────────────────────────────────
        "ControlLeft" => (0x1D, false),
        "AltLeft" => (0x38, false),
        "Space" => (0x39, false),
        "AltRight" => (0x38, true),
        "ControlRight" => (0x1D, true),
        "MetaLeft" => (0x5B, true),
        "MetaRight" => (0x5C, true),
        "ContextMenu" => (0x5D, true),

        // ── Navigation cluster (all E0-extended) ────────────────────────
        "Insert" => (0x52, true),
        "Delete" => (0x53, true),
        "Home" => (0x47, true),
        "End" => (0x4F, true),
        "PageUp" => (0x49, true),
        "PageDown" => (0x51, true),
        "ArrowUp" => (0x48, true),
        "ArrowDown" => (0x50, true),
        "ArrowLeft" => (0x4B, true),
        "ArrowRight" => (0x4D, true),

        // ── Numpad ──────────────────────────────────────────────────────
        "NumLock" => (0x45, false),
        "NumpadDivide" => (0x35, true),
        "NumpadMultiply" => (0x37, false),
        "NumpadSubtract" => (0x4A, false),
        "NumpadAdd" => (0x4E, false),
        "NumpadEnter" => (0x1C, true),
        "NumpadDecimal" => (0x53, false),
        "Numpad0" => (0x52, false),
        "Numpad1" => (0x4F, false),
        "Numpad2" => (0x50, false),
        "Numpad3" => (0x51, false),
        "Numpad4" => (0x4B, false),
        "Numpad5" => (0x4C, false),
        "Numpad6" => (0x4D, false),
        "Numpad7" => (0x47, false),
        "Numpad8" => (0x48, false),
        "Numpad9" => (0x49, false),

        // ── Misc ────────────────────────────────────────────────────────
        "ScrollLock" => (0x46, false),

        _ => return None,
    };

    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::scancode;

    #[test]
    fn maps_common_letters() {
        assert_eq!(scancode("KeyA"), Some((0x1E, false)));
        assert_eq!(scancode("KeyZ"), Some((0x2C, false)));
        assert_eq!(scancode("Enter"), Some((0x1C, false)));
        assert_eq!(scancode("Space"), Some((0x39, false)));
    }

    #[test]
    fn extended_keys_are_flagged() {
        assert_eq!(scancode("ArrowUp"), Some((0x48, true)));
        assert_eq!(scancode("ArrowRight"), Some((0x4D, true)));
        assert_eq!(scancode("ControlRight"), Some((0x1D, true)));
        assert_eq!(scancode("Delete"), Some((0x53, true)));
        assert_eq!(scancode("NumpadEnter"), Some((0x1C, true)));
    }

    #[test]
    fn left_and_right_modifiers_differ_by_extended_flag() {
        // Same base scancode, but the right-hand key is E0-extended.
        assert_eq!(scancode("ControlLeft"), Some((0x1D, false)));
        assert_eq!(scancode("ControlRight"), Some((0x1D, true)));
    }

    #[test]
    fn unmapped_code_returns_none() {
        assert_eq!(scancode("MediaPlayPause"), None);
        assert_eq!(scancode("F13"), None);
        assert_eq!(scancode(""), None);
    }
}
