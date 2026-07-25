//! DOM `KeyboardEvent.code` → macOS virtual keycode (`kVK_*`), US layout.
//!
//! The same shape as the gateway's `keymap::scancode` / `keymap::keysym`
//! (`src/keymap.rs`): one `match` table, `None` for keys the agent should drop.
//!
//! It lives in `rxa-proto` rather than in the agent crate **specifically so it
//! is unit tested off a Mac** — the agent crate never compiles in local dev or
//! on a Linux CI runner, so anything testable has to live outside it.
//!
//! macOS virtual keycodes are *positional*, not symbolic: 0x00 is "the key
//! where A sits on a US ANSI keyboard". That makes them a direct match for DOM
//! `code`, which is also positional — so unlike the X11 keysym path there is no
//! Shift-state resolution to do here. The agent sends the keycode plus the real
//! modifier flags and the Mac's own layout produces the character, which means a
//! non-US layout selected on the Mac keeps working.
//!
//! Values are the `kVK_*` constants from `HIToolbox/Events.h`.

/// Look up the macOS virtual keycode for a DOM `code`.
pub fn mac_keycode(code: &str) -> Option<u16> {
    let key = match code {
        // ── Letters (kVK_ANSI_*) ────────────────────────────────────────
        "KeyA" => 0x00,
        "KeyS" => 0x01,
        "KeyD" => 0x02,
        "KeyF" => 0x03,
        "KeyH" => 0x04,
        "KeyG" => 0x05,
        "KeyZ" => 0x06,
        "KeyX" => 0x07,
        "KeyC" => 0x08,
        "KeyV" => 0x09,
        "KeyB" => 0x0B,
        "KeyQ" => 0x0C,
        "KeyW" => 0x0D,
        "KeyE" => 0x0E,
        "KeyR" => 0x0F,
        "KeyY" => 0x10,
        "KeyT" => 0x11,
        "KeyO" => 0x1F,
        "KeyU" => 0x20,
        "KeyI" => 0x22,
        "KeyP" => 0x23,
        "KeyL" => 0x25,
        "KeyJ" => 0x26,
        "KeyK" => 0x28,
        "KeyN" => 0x2D,
        "KeyM" => 0x2E,

        // ── Number row ──────────────────────────────────────────────────
        "Digit1" => 0x12,
        "Digit2" => 0x13,
        "Digit3" => 0x14,
        "Digit4" => 0x15,
        "Digit6" => 0x16,
        "Digit5" => 0x17,
        "Digit9" => 0x19,
        "Digit7" => 0x1A,
        "Digit8" => 0x1C,
        "Digit0" => 0x1D,
        "Equal" => 0x18,
        "Minus" => 0x1B,

        // ── Punctuation ─────────────────────────────────────────────────
        "BracketRight" => 0x1E,
        "BracketLeft" => 0x21,
        "Quote" => 0x27,
        "Semicolon" => 0x29,
        "Backslash" => 0x2A,
        "Comma" => 0x2B,
        "Slash" => 0x2C,
        "Period" => 0x2F,
        "Backquote" => 0x32,
        // The extra key on ISO keyboards, between Left Shift and Z.
        "IntlBackslash" => 0x0A,

        // ── Editing and whitespace ──────────────────────────────────────
        // kVK_Delete is Backspace; kVK_ForwardDelete is the Delete key.
        "Enter" => 0x24,
        "Tab" => 0x30,
        "Space" => 0x31,
        "Backspace" => 0x33,
        "Escape" => 0x35,
        "Delete" => 0x75,
        "Help" => 0x72,
        "Insert" => 0x72, // no Insert on Mac keyboards; Help occupies the slot

        // ── Modifiers ───────────────────────────────────────────────────
        // Meta is Command, Alt is Option — the browser reports the physical
        // key and the Mac's Command/Option assignment matches positionally.
        "MetaRight" => 0x36,
        "MetaLeft" => 0x37,
        "ShiftLeft" => 0x38,
        "CapsLock" => 0x39,
        "AltLeft" => 0x3A,
        "ControlLeft" => 0x3B,
        "ShiftRight" => 0x3C,
        "AltRight" => 0x3D,
        "ControlRight" => 0x3E,
        "Fn" => 0x3F,
        "ContextMenu" => 0x6E,

        // ── Numpad ──────────────────────────────────────────────────────
        "NumpadDecimal" => 0x41,
        "NumpadMultiply" => 0x43,
        "NumpadAdd" => 0x45,
        // A Mac keypad has Clear where a PC has NumLock.
        "NumLock" => 0x47,
        "NumpadClear" => 0x47,
        "NumpadDivide" => 0x4B,
        "NumpadEnter" => 0x4C,
        "NumpadSubtract" => 0x4E,
        "NumpadEqual" => 0x51,
        "Numpad0" => 0x52,
        "Numpad1" => 0x53,
        "Numpad2" => 0x54,
        "Numpad3" => 0x55,
        "Numpad4" => 0x56,
        "Numpad5" => 0x57,
        "Numpad6" => 0x58,
        "Numpad7" => 0x59,
        "Numpad8" => 0x5B,
        "Numpad9" => 0x5C,

        // ── Function keys ───────────────────────────────────────────────
        "F1" => 0x7A,
        "F2" => 0x78,
        "F3" => 0x63,
        "F4" => 0x76,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F8" => 0x64,
        "F9" => 0x65,
        "F10" => 0x6D,
        "F11" => 0x67,
        "F12" => 0x6F,
        "F13" => 0x69,
        "F14" => 0x6B,
        "F15" => 0x71,
        "F16" => 0x6A,
        "F17" => 0x40,
        "F18" => 0x4F,
        "F19" => 0x50,
        "F20" => 0x5A,

        // ── Navigation ──────────────────────────────────────────────────
        "Home" => 0x73,
        "PageUp" => 0x74,
        "End" => 0x77,
        "PageDown" => 0x79,
        "ArrowLeft" => 0x7B,
        "ArrowRight" => 0x7C,
        "ArrowDown" => 0x7D,
        "ArrowUp" => 0x7E,

        // ── Media ───────────────────────────────────────────────────────
        "AudioVolumeUp" => 0x48,
        "AudioVolumeDown" => 0x49,
        "AudioVolumeMute" => 0x4A,

        _ => return None,
    };
    Some(key)
}

/// Whether a DOM `code` is a modifier the agent tracks in `CGEventFlags`
/// rather than (only) posting as a keystroke.
///
/// `CGEventPost` will not apply a modifier to subsequent keystrokes just
/// because its keycode was posted: the flags have to be set explicitly on each
/// event. The agent keeps a modifier set keyed on these codes; this predicate is
/// the one place that decides membership, so the agent and its tests agree.
pub fn is_modifier(code: &str) -> bool {
    matches!(
        code,
        "ShiftLeft"
            | "ShiftRight"
            | "ControlLeft"
            | "ControlRight"
            | "AltLeft"
            | "AltRight"
            | "MetaLeft"
            | "MetaRight"
            | "Fn"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Spot-check against HIToolbox/Events.h. The letters are the easiest place
    // to get this wrong, because the table is positional on the original
    // Macintosh keyboard and so looks shuffled.
    #[test]
    fn letters_match_the_kvk_ansi_table() {
        assert_eq!(mac_keycode("KeyA"), Some(0x00));
        assert_eq!(mac_keycode("KeyS"), Some(0x01));
        assert_eq!(mac_keycode("KeyZ"), Some(0x06));
        assert_eq!(mac_keycode("KeyQ"), Some(0x0C));
        assert_eq!(mac_keycode("KeyM"), Some(0x2E));
    }

    // The digit block is not in numeric order — 5 and 6 are swapped relative
    // to the obvious guess, as are 7/8/9.
    #[test]
    fn digits_follow_the_shuffled_hardware_order() {
        assert_eq!(mac_keycode("Digit1"), Some(0x12));
        assert_eq!(mac_keycode("Digit5"), Some(0x17));
        assert_eq!(mac_keycode("Digit6"), Some(0x16));
        assert_eq!(mac_keycode("Digit7"), Some(0x1A));
        assert_eq!(mac_keycode("Digit8"), Some(0x1C));
        assert_eq!(mac_keycode("Digit9"), Some(0x19));
        assert_eq!(mac_keycode("Digit0"), Some(0x1D));
    }

    // The one that silently eats an hour: kVK_Delete (0x33) is *Backspace*,
    // and the Delete key is kVK_ForwardDelete (0x75).
    #[test]
    fn backspace_and_delete_are_not_swapped() {
        assert_eq!(mac_keycode("Backspace"), Some(0x33));
        assert_eq!(mac_keycode("Delete"), Some(0x75));
    }

    #[test]
    fn left_and_right_modifiers_have_distinct_keycodes() {
        assert_eq!(mac_keycode("ShiftLeft"), Some(0x38));
        assert_eq!(mac_keycode("ShiftRight"), Some(0x3C));
        assert_eq!(mac_keycode("ControlLeft"), Some(0x3B));
        assert_eq!(mac_keycode("ControlRight"), Some(0x3E));
        assert_eq!(mac_keycode("AltLeft"), Some(0x3A));
        assert_eq!(mac_keycode("AltRight"), Some(0x3D));
        // Command: Left is 0x37, Right is 0x36 — also not in the obvious order.
        assert_eq!(mac_keycode("MetaLeft"), Some(0x37));
        assert_eq!(mac_keycode("MetaRight"), Some(0x36));
    }

    // Function keys are scattered, not sequential.
    #[test]
    fn function_keys_are_not_sequential() {
        assert_eq!(mac_keycode("F1"), Some(0x7A));
        assert_eq!(mac_keycode("F2"), Some(0x78));
        assert_eq!(mac_keycode("F3"), Some(0x63));
        assert_eq!(mac_keycode("F10"), Some(0x6D));
        assert_eq!(mac_keycode("F11"), Some(0x67));
        assert_eq!(mac_keycode("F12"), Some(0x6F));
        assert_eq!(mac_keycode("F20"), Some(0x5A));
    }

    #[test]
    fn arrows_and_navigation_map() {
        assert_eq!(mac_keycode("ArrowUp"), Some(0x7E));
        assert_eq!(mac_keycode("ArrowDown"), Some(0x7D));
        assert_eq!(mac_keycode("ArrowLeft"), Some(0x7B));
        assert_eq!(mac_keycode("ArrowRight"), Some(0x7C));
        assert_eq!(mac_keycode("Home"), Some(0x73));
        assert_eq!(mac_keycode("PageDown"), Some(0x79));
    }

    #[test]
    fn numpad_maps_and_enter_differs_from_return() {
        assert_eq!(mac_keycode("Numpad0"), Some(0x52));
        assert_eq!(mac_keycode("Numpad8"), Some(0x5B));
        assert_eq!(mac_keycode("NumpadDecimal"), Some(0x41));
        // kVK_ANSI_KeypadEnter is a different key from kVK_Return.
        assert_eq!(mac_keycode("Enter"), Some(0x24));
        assert_eq!(mac_keycode("NumpadEnter"), Some(0x4C));
    }

    #[test]
    fn unmapped_codes_return_none() {
        assert_eq!(mac_keycode(""), None);
        assert_eq!(mac_keycode("F21"), None);
        assert_eq!(mac_keycode("MediaPlayPause"), None);
        // DOM codes are case-sensitive and digits are Digit1, not Key1.
        assert_eq!(mac_keycode("keya"), None);
        assert_eq!(mac_keycode("Key1"), None);
    }

    // Two DOM codes deliberately share a keycode (Insert/Help, NumLock/Clear);
    // everything else must be one-to-one, or two keys on the browser side would
    // collapse into one on the Mac.
    #[test]
    fn the_table_has_no_unintended_duplicate_keycodes() {
        const CODES: &[&str] = &[
            "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK",
            "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV",
            "KeyW", "KeyX", "KeyY", "KeyZ", "Digit0", "Digit1", "Digit2", "Digit3", "Digit4",
            "Digit5", "Digit6", "Digit7", "Digit8", "Digit9", "Minus", "Equal", "BracketLeft",
            "BracketRight", "Backslash", "Semicolon", "Quote", "Comma", "Period", "Slash",
            "Backquote", "IntlBackslash", "Enter", "Tab", "Space", "Backspace", "Escape", "Delete",
            "Help", "MetaLeft", "MetaRight", "ShiftLeft", "ShiftRight", "ControlLeft",
            "ControlRight", "AltLeft", "AltRight", "CapsLock", "Fn", "ContextMenu", "F1", "F2",
            "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
            "F16", "F17", "F18", "F19", "F20", "Home", "End", "PageUp", "PageDown", "ArrowUp",
            "ArrowDown", "ArrowLeft", "ArrowRight", "Numpad0", "Numpad1", "Numpad2", "Numpad3",
            "Numpad4", "Numpad5", "Numpad6", "Numpad7", "Numpad8", "Numpad9", "NumpadDecimal",
            "NumpadAdd", "NumpadSubtract", "NumpadMultiply", "NumpadDivide", "NumpadEnter",
            "NumpadEqual", "NumpadClear", "AudioVolumeUp", "AudioVolumeDown", "AudioVolumeMute",
        ];
        let mut seen: Vec<(u16, &str)> = Vec::new();
        for code in CODES {
            let key = mac_keycode(code).unwrap_or_else(|| panic!("{code} should be mapped"));
            if let Some((_, first)) = seen.iter().find(|(k, _)| *k == key) {
                panic!("{code} and {first} both map to 0x{key:02x}");
            }
            seen.push((key, code));
        }
        // The two intentional aliases, asserted rather than merely allowed.
        assert_eq!(mac_keycode("Insert"), mac_keycode("Help"));
        assert_eq!(mac_keycode("NumLock"), mac_keycode("NumpadClear"));
    }

    #[test]
    fn modifier_set_is_exactly_the_flag_carrying_keys() {
        for code in [
            "ShiftLeft",
            "ShiftRight",
            "ControlLeft",
            "ControlRight",
            "AltLeft",
            "AltRight",
            "MetaLeft",
            "MetaRight",
            "Fn",
        ] {
            assert!(is_modifier(code), "{code} should be a tracked modifier");
        }
        // CapsLock is deliberately *not* here: the browser sends its lock state
        // as an authoritative flag on every key event (see `GatewayMsg::Key`),
        // so the agent never tracks it as a held modifier.
        for code in ["CapsLock", "KeyA", "Enter", "F1", ""] {
            assert!(!is_modifier(code), "{code} should not be a tracked modifier");
        }
    }
}
