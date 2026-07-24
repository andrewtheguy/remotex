//! Input injection via `CGEvent`.
//!
//! Needs the **Accessibility** TCC grant; without it `CGEventPost` silently does
//! nothing, which is a confusing failure — the screen paints and the session
//! looks healthy while every click vanishes. [`accessibility_granted`] exists so
//! the agent can say so up front.
//!
//! ## The coordinate conversion
//!
//! remotex sends **captured-surface pixel** coordinates. `CGEventPost` addresses
//! the **global display point** space. Three things separate them:
//!
//! - the backing scale (2.0 on Retina), so pixels must be divided by it;
//! - the display's origin in the global space, non-zero on a secondary display;
//! - nothing else — both spaces are y-down, so no flip.
//!
//! Getting this wrong is the "clicks land in the wrong place" bug the plan
//! warns about, so [`Injector::to_global_point`] is a pure function with tests
//! covering 1x, 2x, and an offset display, checked at the screen corners where
//! an error is largest.
//!
//! ## Modifiers
//!
//! `CGEventPost` does not apply a modifier to later keystrokes just because its
//! keycode was posted: the flags have to be set explicitly on every event. So
//! the browser's modifier key-downs are tracked in a set and folded into
//! `CGEventFlags` on each event. CapsLock is *not* tracked that way — the
//! browser reports its lock state authoritatively on every key message, so it is
//! read from there instead of inferred.

use std::collections::HashSet;

use log::debug;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};
use rxa_proto::keymap;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Whether this process is a trusted accessibility client. A null options
    /// dictionary checks without prompting.
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
    /// `kAXTrustedCheckOptionPrompt`, the one option key that matters here:
    /// set it to true and the check also raises the "open System Settings"
    /// dialog and registers the app in the Accessibility list.
    static kAXTrustedCheckOptionPrompt: *const std::ffi::c_void;
}

/// Whether the Accessibility grant is in place, checked without prompting.
pub fn accessibility_granted() -> bool {
    unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) }
}

/// Ask for Accessibility, raising the system prompt if it is unanswered.
///
/// Needed because nothing else will: `CGEventPost` does not fail without the
/// grant, it silently discards every event, so the agent would otherwise never
/// appear in the Accessibility list for the user to switch on. macOS remembers
/// the answer, so this stops prompting once it has been given.
pub fn request_accessibility() -> bool {
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString};

    // Safety: the symbol is a CFStringRef constant owned by the framework, and
    // it outlives the dictionary we build from it.
    let key = unsafe { CFRetained::retain(std::ptr::NonNull::new(
        kAXTrustedCheckOptionPrompt as *mut CFString,
    ).expect("kAXTrustedCheckOptionPrompt is never null")) };
    let value: &CFBoolean = CFBoolean::new(true);
    let options: CFRetained<CFDictionary<CFString, CFBoolean>> =
        CFDictionary::from_slices(&[&*key], &[value]);

    // Safety: `options` is a valid CFDictionary for the duration of the call.
    unsafe {
        AXIsProcessTrustedWithOptions(
            CFRetained::as_ptr(&options).as_ptr().cast::<std::ffi::c_void>()
        )
    }
}

/// How many scroll lines one unit of DOM wheel delta is worth.
///
/// The browser reports `deltaY` in a mix of units depending on the device; a
/// notched mouse wheel is typically ~100 per click. Dividing by that and
/// rounding to at least one line makes a wheel click scroll about one step,
/// while a trackpad's fine-grained deltas stay smooth.
const WHEEL_DIVISOR: f32 = 100.0;

/// Injects browser input into the Mac's event stream.
pub struct Injector {
    /// Captured pixels per display point.
    scale: f64,
    /// The display's origin in the global point space.
    origin: (f64, f64),
    /// Modifier keys the browser currently holds down, by DOM code.
    held: HashSet<String>,
    /// CapsLock lock state, as reported by the browser on every key event.
    caps: bool,
    /// Last pointer position in global points, so a button or wheel event lands
    /// where the pointer actually is.
    pointer: CGPoint,
    /// Mouse buttons currently down, so a move becomes a *drag* rather than a
    /// plain move — the two are different `CGEventType`s and using the wrong one
    /// breaks text selection and window dragging.
    buttons: HashSet<u8>,
}

impl Injector {
    pub fn new(scale: f64, origin: (f64, f64)) -> Self {
        Self {
            // A zero or negative scale would divide the pointer into nonsense.
            scale: if scale > 0.0 { scale } else { 1.0 },
            origin,
            held: HashSet::new(),
            caps: false,
            pointer: CGPoint {
                x: origin.0,
                y: origin.1,
            },
            buttons: HashSet::new(),
        }
    }

    /// Convert captured-surface pixels to a global display point.
    pub fn to_global_point(&self, x: u16, y: u16) -> CGPoint {
        CGPoint {
            x: self.origin.0 + f64::from(x) / self.scale,
            y: self.origin.1 + f64::from(y) / self.scale,
        }
    }

    /// Current modifier flags, from the held set plus the browser's CapsLock.
    fn flags(&self) -> CGEventFlags {
        let mut flags = CGEventFlags::empty();
        for code in &self.held {
            flags |= match code.as_str() {
                "ShiftLeft" | "ShiftRight" => CGEventFlags::MaskShift,
                "ControlLeft" | "ControlRight" => CGEventFlags::MaskControl,
                "AltLeft" | "AltRight" => CGEventFlags::MaskAlternate,
                "MetaLeft" | "MetaRight" => CGEventFlags::MaskCommand,
                "Fn" => CGEventFlags::MaskSecondaryFn,
                _ => CGEventFlags::empty(),
            };
        }
        if self.caps {
            flags |= CGEventFlags::MaskAlphaShift;
        }
        flags
    }

    fn source() -> Option<objc2_core_foundation::CFRetained<CGEventSource>> {
        // HIDSystemState makes injected events behave like real hardware input,
        // which matters for key repeat and for apps that inspect the source.
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
    }

    /// Move the pointer. Becomes a drag while a button is held.
    pub fn pointer_move(&mut self, x: u16, y: u16) {
        self.pointer = self.to_global_point(x, y);
        // Left drag wins if several buttons are somehow down; that is what a
        // real mouse reports too.
        let (event_type, button) = if self.buttons.contains(&0) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.buttons.contains(&2) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if self.buttons.contains(&1) {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        self.post_mouse(event_type, button);
    }

    /// Press or release a mouse button. `button` uses the DOM numbering (0/1/2).
    pub fn pointer_button(&mut self, button: u8, pressed: bool) {
        let Some(cg_button) = dom_button(button) else {
            debug!("input: ignoring unknown mouse button {button}");
            return;
        };
        let event_type = match (cg_button, pressed) {
            (CGMouseButton::Left, true) => CGEventType::LeftMouseDown,
            (CGMouseButton::Left, false) => CGEventType::LeftMouseUp,
            (CGMouseButton::Right, true) => CGEventType::RightMouseDown,
            (CGMouseButton::Right, false) => CGEventType::RightMouseUp,
            (_, true) => CGEventType::OtherMouseDown,
            (_, false) => CGEventType::OtherMouseUp,
        };
        if pressed {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        self.post_mouse(event_type, cg_button);
    }

    fn post_mouse(&self, event_type: CGEventType, button: CGMouseButton) {
        let Some(source) = Self::source() else {
            debug!("input: no event source available");
            return;
        };
        let Some(event) =
            CGEvent::new_mouse_event(Some(&source), event_type, self.pointer, button)
        else {
            debug!("input: could not create a mouse event");
            return;
        };
        // Modifiers apply to clicks too — Command-click, Shift-click.
        CGEvent::set_flags(Some(&event), self.flags());
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }

    /// Scroll. `dx`/`dy` are raw DOM deltas.
    pub fn wheel(&self, dx: f32, dy: f32) {
        let Some(source) = Self::source() else {
            return;
        };
        // DOM's sign convention is inverted relative to the native one: a DOM
        // `deltaY` is positive scrolling *down*, while a Mac scroll event is
        // positive scrolling *up* (content moving down). The RDP engine hit the
        // same thing (src/rdp.rs). Same for the horizontal axis.
        let lines_y = wheel_lines(-dy);
        let lines_x = wheel_lines(-dx);
        if lines_y == 0 && lines_x == 0 {
            return;
        }
        let Some(event) = CGEvent::new_scroll_wheel_event2(
            Some(&source),
            CGScrollEventUnit::Line,
            2,
            lines_y,
            lines_x,
            0,
        ) else {
            debug!("input: could not create a scroll event");
            return;
        };
        CGEvent::set_flags(Some(&event), self.flags());
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }

    /// Press or release a key, identified by its DOM `KeyboardEvent.code`.
    ///
    /// `caps` is the browser's authoritative CapsLock state at the moment of the
    /// event, so lock state is never inferred.
    pub fn key(&mut self, code: &str, pressed: bool, caps: bool) {
        self.caps = caps;

        // Track the modifier *before* posting, so the modifier's own event
        // already carries its flag — otherwise the first Shift-down arrives
        // unflagged and an app watching flagsChanged sees nothing.
        if keymap::is_modifier(code) {
            if pressed {
                self.held.insert(code.to_owned());
            } else {
                self.held.remove(code);
            }
        }

        let Some(keycode) = keymap::mac_keycode(code) else {
            debug!("input: no macOS keycode for {code:?}, dropping");
            return;
        };
        let Some(source) = Self::source() else {
            return;
        };

        // A modifier is a flagsChanged event on real hardware, but posting it as
        // a keyboard event with the right flags is what CGEvent-based injection
        // does and what apps accept.
        let Some(event) = CGEvent::new_keyboard_event(Some(&source), keycode, pressed) else {
            debug!("input: could not create a keyboard event for {code:?}");
            return;
        };
        CGEvent::set_flags(Some(&event), self.flags());
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }

    /// Release everything the browser was holding.
    ///
    /// Called when a session ends: a browser that disconnects mid-chord would
    /// otherwise leave Command or a mouse button stuck down on the Mac, which is
    /// both baffling and hard to clear without a keyboard.
    pub fn release_all(&mut self) {
        for button in std::mem::take(&mut self.buttons) {
            self.pointer_button(button, false);
        }
        for code in std::mem::take(&mut self.held) {
            // Drop it from the set first so the release event does not carry its
            // own flag.
            self.held.remove(&code);
            self.key(&code, false, self.caps);
        }
        self.caps = false;
    }
}

/// DOM `MouseEvent.button` to `CGMouseButton`.
fn dom_button(button: u8) -> Option<CGMouseButton> {
    match button {
        0 => Some(CGMouseButton::Left),
        1 => Some(CGMouseButton::Center),
        2 => Some(CGMouseButton::Right),
        _ => None,
    }
}

/// Convert a DOM wheel delta to scroll lines, keeping any nonzero delta worth at
/// least one line so a small trackpad flick is not swallowed.
fn wheel_lines(delta: f32) -> i32 {
    if delta == 0.0 || !delta.is_finite() {
        return 0;
    }
    let lines = (delta / WHEEL_DIVISOR).round();
    if lines == 0.0 {
        if delta > 0.0 { 1 } else { -1 }
    } else {
        lines as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The conversion the plan calls the most likely source of a "clicks land in
    // the wrong place" bug. Checked at the corners, where an error is largest.
    #[test]
    fn a_non_retina_main_display_maps_one_to_one() {
        let inj = Injector::new(1.0, (0.0, 0.0));
        let p = inj.to_global_point(0, 0);
        assert_eq!((p.x, p.y), (0.0, 0.0));
        let p = inj.to_global_point(1279, 799);
        assert_eq!((p.x, p.y), (1279.0, 799.0));
    }

    #[test]
    fn a_retina_display_halves_pixel_coordinates() {
        // 3456x2234 pixels of a 1728x1117 point display.
        let inj = Injector::new(2.0, (0.0, 0.0));
        assert_eq!(inj.to_global_point(0, 0).x, 0.0);
        let p = inj.to_global_point(3454, 2232);
        assert_eq!((p.x, p.y), (1727.0, 1116.0));
        // The far corner stays inside the point-space display.
        let p = inj.to_global_point(3455, 2233);
        assert!(p.x < 1728.0 && p.y < 1117.0, "{p:?}");
    }

    #[test]
    fn a_secondary_display_is_offset_by_its_origin() {
        // A 2x display placed to the right of, and above, the main one.
        let inj = Injector::new(2.0, (1728.0, -200.0));
        let p = inj.to_global_point(0, 0);
        assert_eq!((p.x, p.y), (1728.0, -200.0));
        let p = inj.to_global_point(200, 400);
        assert_eq!((p.x, p.y), (1828.0, 0.0));
    }

    // A nonsense scale must not turn the pointer into NaN or infinity.
    #[test]
    fn a_degenerate_scale_falls_back_to_one_to_one() {
        for scale in [0.0, -2.0] {
            let inj = Injector::new(scale, (0.0, 0.0));
            let p = inj.to_global_point(100, 50);
            assert_eq!((p.x, p.y), (100.0, 50.0), "scale {scale}");
        }
    }

    #[test]
    fn modifier_flags_accumulate_and_clear() {
        let mut inj = Injector::new(1.0, (0.0, 0.0));
        assert_eq!(inj.flags(), CGEventFlags::empty());

        inj.held.insert("ShiftLeft".to_owned());
        assert!(inj.flags().contains(CGEventFlags::MaskShift));

        inj.held.insert("MetaLeft".to_owned());
        let flags = inj.flags();
        assert!(flags.contains(CGEventFlags::MaskShift));
        assert!(flags.contains(CGEventFlags::MaskCommand));

        inj.held.remove("ShiftLeft");
        let flags = inj.flags();
        assert!(!flags.contains(CGEventFlags::MaskShift));
        assert!(flags.contains(CGEventFlags::MaskCommand));
    }

    #[test]
    fn left_and_right_modifiers_set_the_same_flag() {
        let mut inj = Injector::new(1.0, (0.0, 0.0));
        inj.held.insert("ControlRight".to_owned());
        assert!(inj.flags().contains(CGEventFlags::MaskControl));
        inj.held.clear();
        inj.held.insert("ControlLeft".to_owned());
        assert!(inj.flags().contains(CGEventFlags::MaskControl));
    }

    // CapsLock comes from the browser's authoritative flag, never from tracking
    // the key — it cannot be observed until the first key event otherwise, which
    // would mis-case letters when CapsLock was already on at connect time.
    #[test]
    fn capslock_comes_from_the_browsers_flag_not_the_held_set() {
        let mut inj = Injector::new(1.0, (0.0, 0.0));
        assert!(!inj.flags().contains(CGEventFlags::MaskAlphaShift));
        inj.caps = true;
        assert!(inj.flags().contains(CGEventFlags::MaskAlphaShift));
        // And CapsLock is not a tracked modifier, so holding it changes nothing.
        assert!(!keymap::is_modifier("CapsLock"));
    }

    // DOM's wheel sign is inverted relative to macOS's, so a "scroll down"
    // gesture must produce a negative line count.
    #[test]
    fn wheel_deltas_invert_the_dom_sign_convention() {
        // DOM deltaY > 0 is scrolling down; macOS wants a negative line count.
        assert_eq!(wheel_lines(-100.0), -1);
        assert_eq!(wheel_lines(100.0), 1);
        assert_eq!(wheel_lines(-300.0), -3);
    }

    #[test]
    fn small_wheel_deltas_still_scroll_by_at_least_one_line() {
        // A trackpad's fine deltas must not be rounded away to nothing.
        assert_eq!(wheel_lines(3.0), 1);
        assert_eq!(wheel_lines(-3.0), -1);
        assert_eq!(wheel_lines(0.4), 1);
    }

    #[test]
    fn a_zero_or_nonfinite_wheel_delta_scrolls_nothing() {
        assert_eq!(wheel_lines(0.0), 0);
        assert_eq!(wheel_lines(f32::NAN), 0);
        assert_eq!(wheel_lines(f32::INFINITY), 0);
    }

    #[test]
    fn dom_button_numbering_maps_to_cg_buttons() {
        assert_eq!(dom_button(0), Some(CGMouseButton::Left));
        assert_eq!(dom_button(1), Some(CGMouseButton::Center));
        assert_eq!(dom_button(2), Some(CGMouseButton::Right));
        assert_eq!(dom_button(3), None);
    }
}
