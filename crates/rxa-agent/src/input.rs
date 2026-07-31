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
//! ## Click count
//!
//! A double-click is not two clicks: it is one click carrying a click state of 2,
//! in `kCGMouseEventClickState`. macOS will guess that state from where and when
//! the events landed if it is left unset, and the guess is what breaks over a
//! network — most visibly on the display the agent makes for itself. So the count
//! is carried on the wire from the client, whose own OS already decided it, for
//! the same reason CapsLock is (below) rather than inferred here.
//!
//! ## Modifiers
//!
//! `CGEventPost` does not apply a modifier to later keystrokes just because its
//! keycode was posted: the flags have to be set explicitly on every event. So
//! the browser's modifier key-downs are tracked in a set and folded into
//! `CGEventFlags` on each event. CapsLock is *not* tracked that way — the
//! browser reports its lock state authoritatively on every key message, so it is
//! read from there instead of inferred.

use std::collections::{HashMap, HashSet};

use log::{debug, info};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventType, CGGetActiveDisplayList, CGMouseButton, CGScrollEventUnit,
    CGWarpMouseCursorPosition,
};
use rxa_proto::keymap;
use rxa_proto::msg::WheelUnit;

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

/// Lines one `DOM_DELTA_PAGE` is spent as.
///
/// macOS has no page unit, so a page has to become some number of lines. This is
/// the screenful most apps take a Page key to mean, and pages are rare enough
/// that being a line or two out costs nothing.
const LINES_PER_PAGE: f32 = 16.0;

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
    /// Mouse buttons currently down, each with the click count of the press that
    /// put it there. Held at all because a move becomes a *drag* rather than a
    /// plain move — the two are different `CGEventType`s and using the wrong one
    /// breaks text selection and window dragging.
    ///
    /// Keyed by button rather than kept as one count for the session: a drag
    /// carries the count of the press *it* started with, which is what makes
    /// dragging out of a double-click select by word rather than by character.
    /// One shared count would let any other button pressed mid-drag overwrite it
    /// and quietly demote the drag to single-click selection.
    buttons: HashMap<u8, u8>,
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
            buttons: HashMap::new(),
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
        let was = self.pointer;
        self.pointer = self.to_global_point(x, y);
        // How far the pointer moved, which a move event carries alongside where
        // it landed. Rounded to whole points because the field is an integer;
        // sub-point movement is reported as none, the same as a mouse that did
        // not move far enough to count a tick.
        let delta = (
            (self.pointer.x - was.x) as i64,
            (self.pointer.y - was.y) as i64,
        );
        let (event_type, button, clicks) = self
            .drag()
            // No button down: a plain move, which has no click to count.
            .unwrap_or((CGEventType::MouseMoved, CGMouseButton::Left, 0));
        self.post(event_type, button, clicks, delta);
    }

    /// The drag a move becomes while a button is held — its event type, its
    /// button, and the click count of the press that started *that* button.
    /// `None` when nothing is down.
    fn drag(&self) -> Option<(CGEventType, CGMouseButton, u8)> {
        // Left drag wins if several buttons are somehow down; that is what a
        // real mouse reports too. The count comes from the button chosen here,
        // so a press on any other one leaves this drag as it found it.
        [
            (0u8, CGEventType::LeftMouseDragged, CGMouseButton::Left),
            (2, CGEventType::RightMouseDragged, CGMouseButton::Right),
            (1, CGEventType::OtherMouseDragged, CGMouseButton::Center),
        ]
        .into_iter()
        .find_map(|(dom, event_type, cg_button)| {
            self.buttons
                .get(&dom)
                .map(|&clicks| (event_type, cg_button, clicks))
        })
    }

    /// Press or release a mouse button. `button` uses the DOM numbering (0/1/2),
    /// `clicks` is the client's click count for this press (1, or 2 for the
    /// second of a double).
    pub fn pointer_button(&mut self, button: u8, pressed: bool, clicks: u8) {
        let Some(cg_button) = mac_button(button) else {
            debug!("input: ignoring unknown mouse button {button}");
            return;
        };
        let event_type = match (cg_button, pressed) {
            (CGMouseButton::Left, true) => CGEventType::LeftMouseDown,
            (CGMouseButton::Left, false) => CGEventType::LeftMouseUp,
            (CGMouseButton::Right, true) => CGEventType::RightMouseDown,
            (CGMouseButton::Right, false) => CGEventType::RightMouseUp,
            // Everything else — middle, and the back and forward buttons a
            // five-button mouse has — is an `OtherMouse` event told apart only
            // by its button number.
            (_, true) => CGEventType::OtherMouseDown,
            (_, false) => CGEventType::OtherMouseUp,
        };
        let clicks = self.note_button(button, pressed, clicks);
        // A press moves nothing, and a real mouse says so.
        self.post(event_type, cg_button, clicks, (0, 0));
    }

    /// Record a press or release, and answer with the click count its event
    /// should carry.
    ///
    /// Split from the posting so the bookkeeping can be tested: everything else
    /// in `pointer_button` ends in `CGEventPost`, which a test has no business
    /// reaching — on a machine whose terminal happens to hold the Accessibility
    /// grant it would click on whatever the developer is looking at.
    fn note_button(&mut self, button: u8, pressed: bool, clicks: u8) -> u8 {
        // Zero would inject a click state of zero, which is a press that counts
        // as no click. The wire floors it too; this is the floor for every caller.
        let clicks = clicks.max(1);
        // Recorded against this button and forgotten when this button comes up,
        // so a drag reads its own press's count for as long as it lasts.
        if pressed {
            self.buttons.insert(button, clicks);
        } else {
            self.buttons.remove(&button);
        }
        clicks
    }

    /// `clicks` becomes the event's click state, which is where `NSEvent`'s
    /// `clickCount` comes from and so the whole of what makes a double-click a
    /// double-click. Without it macOS is left to guess the count from where and
    /// when the events landed, and a click that crossed a network is a poor
    /// imitation of the gesture the person actually made.
    ///
    /// Zero leaves the field alone, which is what a plain move wants — it has no
    /// click to count.
    ///
    /// `delta` is how far the pointer moved to get here, which an app in a
    /// pointer-locked or relative-input mode reads *instead of* the position —
    /// a game or 3D viewer sees a motionless mouse without it.
    fn post(
        &self,
        event_type: CGEventType,
        button: CGMouseButton,
        clicks: u8,
        delta: (i64, i64),
    ) {
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
        if clicks > 0 {
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventClickState,
                i64::from(clicks),
            );
        }
        // Left and right carry their number implicitly; the `OtherMouse` family
        // is *only* told apart by this field, so middle, back and forward all
        // arrive as the same event without it.
        if button.0 > CGMouseButton::Right.0 {
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventButtonNumber,
                i64::from(button.0),
            );
        }
        if delta != (0, 0) {
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventDeltaX,
                delta.0,
            );
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventDeltaY,
                delta.1,
            );
        }
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }

    /// Scroll. `dx`/`dy` are raw DOM deltas, in `unit`.
    pub fn wheel(&self, dx: f32, dy: f32, unit: WheelUnit) {
        let Some(source) = Self::source() else {
            return;
        };
        let (dx, dy) = shift_scrolls_sideways(dx, dy, self.shift_held());
        // A page is spent as lines; the other two map straight onto a native
        // unit, which is the point of carrying the unit at all.
        let (cg_unit, per) = match unit {
            WheelUnit::Pixel => (CGScrollEventUnit::Pixel, 1.0),
            WheelUnit::Line => (CGScrollEventUnit::Line, 1.0),
            WheelUnit::Page => (CGScrollEventUnit::Line, LINES_PER_PAGE),
        };
        // DOM's sign convention is inverted relative to the native one: a DOM
        // `deltaY` is positive scrolling *down*, while a Mac scroll event is
        // positive scrolling *up* (content moving down). The RDP engine hit the
        // same thing (src/rdp.rs). Same for the horizontal axis.
        let amount_y = wheel_amount(-dy, per);
        let amount_x = wheel_amount(-dx, per);
        if amount_y == 0 && amount_x == 0 {
            return;
        }
        let Some(event) = CGEvent::new_scroll_wheel_event2(
            Some(&source),
            cg_unit,
            2,
            amount_y,
            amount_x,
            0,
        ) else {
            debug!("input: could not create a scroll event");
            return;
        };
        CGEvent::set_flags(Some(&event), self.flags());
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }

    /// Whether either Shift is down, which turns a wheel sideways.
    fn shift_held(&self) -> bool {
        self.held.contains("ShiftLeft") || self.held.contains("ShiftRight")
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
        for button in std::mem::take(&mut self.buttons).into_keys() {
            // One click, whatever the press was: nobody is double-clicking a
            // button loose, and the count only matters to the app receiving it.
            self.pointer_button(button, false, 1);
        }
        // Taking the set empties it before the first release is posted, so no
        // release event carries its own flag — a Shift-up flagged with Shift is
        // what leaves an app believing the modifier is still down.
        for code in std::mem::take(&mut self.held) {
            self.key(&code, false, self.caps);
        }
        self.caps = false;
    }
}

/// DOM `MouseEvent.button` to the macOS button, whose numbering it matches
/// everywhere except the middle: DOM counts left/middle/right where macOS counts
/// left/right/center, and the two agree again at 3 and 4 — back and forward.
fn mac_button(button: u8) -> Option<CGMouseButton> {
    match button {
        0 => Some(CGMouseButton::Left),
        1 => Some(CGMouseButton::Center),
        2 => Some(CGMouseButton::Right),
        // No named constant exists for these: `CGMouseButton` is a number, and
        // 3 and 4 are what macOS calls the two side buttons. Passing them
        // through is the whole of what makes Back and Forward work on the
        // remote's browser and Finder windows.
        3 | 4 => Some(CGMouseButton(u32::from(button))),
        _ => None,
    }
}

/// Shift turns a vertical wheel into a horizontal one on macOS — but only for
/// the client that has not done it already.
///
/// A Mac client's own OS applies the rule before the browser ever sees the
/// event, so its delta arrives on the horizontal axis and there is nothing to
/// do. A Windows or Linux client's does not, so the delta arrives vertical and
/// scrolls the wrong way on the remote; setting the Shift flag on the injected
/// event is not enough, because macOS applies the rule to hardware and not to
/// what is posted.
///
/// Keyed on the horizontal delta being empty, which is what distinguishes the
/// two cases without the agent having to know what the client is.
fn shift_scrolls_sideways(dx: f32, dy: f32, shift: bool) -> (f32, f32) {
    if shift && dx == 0.0 && dy != 0.0 {
        (dy, 0.0)
    } else {
        (dx, dy)
    }
}

/// Convert a DOM wheel delta to `per`-scaled scroll units, keeping any nonzero
/// delta worth at least one so a small trackpad flick is not swallowed.
fn wheel_amount(delta: f32, per: f32) -> i32 {
    if delta == 0.0 || !delta.is_finite() {
        return 0;
    }
    let amount = (delta * per).round();
    if amount == 0.0 {
        if delta > 0.0 { 1 } else { -1 }
    } else {
        amount as i32
    }
}

/// Where the pointer was before a session moved it, so it can be put back.
///
/// A Mac has **one** pointer for every display it has, and [`Injector`] moves it:
/// a client's mouse position is posted as an absolute location in the global point
/// space (see [`Injector::to_global_point`]), so while a session shares the display
/// the agent made, the pointer lives on a screen nobody sitting at the Mac can see.
/// That much is the price of one pointer and cannot be fixed here.
///
/// What can be fixed is the state a session *leaves behind*. Nothing moved the
/// pointer back when a session ended, so the person at the Mac was left with no
/// pointer on their own screen — and, depending on how the displays are arranged,
/// sometimes no way to walk it back. The pointer is clamped to the union of the
/// display rectangles, so two displays of different heights do not share every row:
/// measured on the test VM with the agent's 1600x1000 display at (0,0) and the
/// Mac's own 800x600 screen at (-800,0), a pointer anywhere below y=600 had no
/// leftward path onto the real screen at all. It had to go up first, which is not
/// something anyone guesses while their pointer is invisible.
///
/// Recorded per session rather than once per process because the answer is "wherever
/// that person was working", which is only knowable at the moment the session takes
/// the pointer away from them.
pub struct PointerHome {
    /// The agent's own display — the only screen a session can strand it on.
    owned: u32,
    /// Where the pointer was, if that was anywhere worth returning it to.
    saved: Option<CGPoint>,
}

impl PointerHome {
    /// Note where the pointer is, before a session starts moving it.
    ///
    /// `None` when the agent has no display of its own: every display is then one
    /// the person at the Mac can see, so a session cannot hide the pointer and there
    /// is nothing to undo.
    pub fn note(owned: Option<u32>) -> Option<Self> {
        let owned = owned?;
        let now = CGEvent::location(None);
        // Already on our display means the previous session left it there and this
        // one inherited it. Saving that would make the restore a no-op, so it is
        // left empty and [`restore`] falls back to a screen someone can see.
        let saved = (!contains(CGDisplayBounds(owned), now)).then_some(now);
        Some(Self { owned, saved })
    }

    /// A home that remembers nothing, for the moment the display is *created*.
    ///
    /// Deliberately not "note the position first, then restore it". Creating the
    /// display rearranges the others: on the test VM the new display takes the
    /// global origin and the Mac's own screen moves from (0,0) to (-800,0), so a
    /// pointer that never moved is suddenly inside the new display's rectangle —
    /// which is how it gets swallowed in the first place. A position noted
    /// beforehand would name a spot that is now on the new display, making the
    /// restore a no-op. The middle of a real screen is the only answer that still
    /// means what it said.
    pub fn for_new_display(owned: u32) -> Self {
        Self { owned, saved: None }
    }

    /// Put the pointer back, if this session is what took it away.
    ///
    /// Consumes itself: a restore is the end of a session, and doing it twice would
    /// move a pointer the person at the Mac has since moved themselves.
    pub fn restore(self) {
        let others: Vec<CGRect> = active_displays()
            .into_iter()
            .filter(|id| *id != self.owned)
            .map(|id| CGDisplayBounds(id))
            .collect();
        let now = CGEvent::location(None);
        let Some(to) = destination(now, self.saved, CGDisplayBounds(self.owned), &others) else {
            return;
        };
        // No `CGAssociateMouseAndMouseCursorPosition` after it: a warp suppresses
        // mouse events for a fraction of a second and then resumes on its own, and
        // nobody is racing this — the session that was driving the pointer has just
        // ended.
        let err = CGWarpMouseCursorPosition(to);
        // `info`, not `debug`: this moves the pointer out from under whoever is at
        // the Mac, and the log is where they will look to find out why. It only
        // fires when a warp actually happened.
        info!(
            "input: pointer left on display {}; moved to ({}, {}) (warp {})",
            self.owned, to.x as i32, to.y as i32, err.0
        );
    }
}

/// Where to send the pointer, or `None` to leave it alone. Pure so the rules can be
/// tested without a WindowServer, which is the whole of the logic worth testing —
/// the rest is three CoreGraphics calls.
///
/// `others` is every active display except the agent's, in the order the system
/// lists them.
fn destination(
    now: CGPoint,
    saved: Option<CGPoint>,
    owned: CGRect,
    others: &[CGRect],
) -> Option<CGPoint> {
    // Not on our display: either the session never moved it there or the person at
    // the Mac has already recovered it. Either way, moving it now would be the rude
    // one of the two mistakes available.
    if !contains(owned, now) {
        return None;
    }
    // Back where they were working, but only if that screen is still there — a
    // display can be unplugged, or resized, while a session holds the pointer.
    if let Some(saved) = saved
        && others.iter().any(|rect| contains(*rect, saved))
    {
        return Some(saved);
    }
    // Otherwise the middle of the first display someone could be sitting at. A Mac
    // whose *only* display is ours gets `None`: there is nowhere better to be.
    others.first().map(centre)
}

/// The active displays, ours included, in system order. Empty if the list cannot be
/// read, which leaves [`PointerHome::restore`] doing nothing rather than guessing.
fn active_displays() -> Vec<u32> {
    let mut count: u32 = 0;
    if unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) }.0 != 0 || count == 0 {
        return Vec::new();
    }
    let mut ids = vec![0u32; count as usize];
    if unsafe { CGGetActiveDisplayList(count, ids.as_mut_ptr(), &mut count) }.0 != 0 {
        return Vec::new();
    }
    ids.truncate(count as usize);
    ids
}

/// Half-open on both axes, matching how the WindowServer tiles displays edge to
/// edge: a point on one display's right edge belongs to the next one along.
fn contains(rect: CGRect, point: CGPoint) -> bool {
    point.x >= rect.origin.x
        && point.x < rect.origin.x + rect.size.width
        && point.y >= rect.origin.y
        && point.y < rect.origin.y + rect.size.height
}

fn centre(rect: &CGRect) -> CGPoint {
    CGPoint {
        x: rect.origin.x + rect.size.width / 2.0,
        y: rect.origin.y + rect.size.height / 2.0,
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

    /// The count a drag reports, or `None` when the move is a plain move.
    fn drag_clicks(inj: &Injector) -> Option<u8> {
        inj.drag().map(|(_, _, clicks)| clicks)
    }

    // The count on the press is the count on its release, and the drag in
    // between reads it — that is what makes dragging out of a double-click
    // select by word rather than by character.
    #[test]
    fn a_press_leaves_its_click_count_for_the_drag_and_takes_it_back_on_release() {
        let mut inj = Injector::new(1.0, (0.0, 0.0));
        assert_eq!(drag_clicks(&inj), None, "no press, no drag");

        assert_eq!(inj.note_button(0, true, 2), 2);
        assert_eq!(drag_clicks(&inj), Some(2), "what a drag from here reports");

        assert_eq!(inj.note_button(0, false, 2), 2, "the release counts too");
        assert_eq!(drag_clicks(&inj), None, "nothing held, nothing to drag");
    }

    // A left double-click drag with another button chorded into it: the drag is
    // still the left button's, so it keeps the left button's count throughout.
    // One count shared across buttons would have let the right press overwrite
    // it and silently demote the selection from by-word to by-character.
    #[test]
    fn the_count_survives_until_the_last_button_is_up() {
        let mut inj = Injector::new(1.0, (0.0, 0.0));
        inj.note_button(0, true, 2);
        assert_eq!(drag_clicks(&inj), Some(2));

        inj.note_button(2, true, 1);
        assert_eq!(
            drag_clicks(&inj),
            Some(2),
            "the left drag is untouched by a right press"
        );
        inj.note_button(2, false, 1);
        assert_eq!(drag_clicks(&inj), Some(2), "and by its release");

        inj.note_button(0, false, 2);
        assert_eq!(drag_clicks(&inj), None);
    }

    // The left button wins a chord, so a right press *before* it does not become
    // the drag — and does not lend it a count either.
    #[test]
    fn a_left_press_takes_over_a_drag_a_right_button_started() {
        let mut inj = Injector::new(1.0, (0.0, 0.0));
        inj.note_button(2, true, 1);
        assert_eq!(drag_clicks(&inj), Some(1));
        inj.note_button(0, true, 2);
        assert_eq!(drag_clicks(&inj), Some(2), "now the left button's drag");
    }

    // Zero reaches `kCGMouseEventClickState` as a press that counts as no click.
    #[test]
    fn a_zero_click_count_is_floored_at_one() {
        let mut inj = Injector::new(1.0, (0.0, 0.0));
        assert_eq!(inj.note_button(0, true, 0), 1);
        assert_eq!(drag_clicks(&inj), Some(1));
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
    // gesture must produce a negative count.
    #[test]
    fn wheel_deltas_invert_the_dom_sign_convention() {
        // DOM deltaY > 0 is scrolling down; macOS wants a negative count.
        assert_eq!(wheel_amount(-100.0, 1.0), -100);
        assert_eq!(wheel_amount(100.0, 1.0), 100);
        assert_eq!(wheel_amount(-300.0, 1.0), -300);
    }

    // The bug the unit exists for: a trackpad's few-pixel deltas were divided
    // into lines and every one of them came out as a whole line, so a glide
    // scrolled in jumps. In pixels they are spent as themselves.
    #[test]
    fn a_trackpads_small_deltas_stay_small() {
        assert_eq!(wheel_amount(3.0, 1.0), 3);
        assert_eq!(wheel_amount(-3.0, 1.0), -3);
        assert_eq!(wheel_amount(12.5, 1.0), 13);
    }

    #[test]
    fn a_delta_too_small_to_round_still_scrolls_by_one() {
        // Rounding a fine delta away would let a slow gesture scroll nothing
        // at all, however long it went on.
        assert_eq!(wheel_amount(0.4, 1.0), 1);
        assert_eq!(wheel_amount(-0.4, 1.0), -1);
    }

    #[test]
    fn a_page_is_spent_as_lines() {
        assert_eq!(wheel_amount(1.0, LINES_PER_PAGE), LINES_PER_PAGE as i32);
        assert_eq!(wheel_amount(-2.0, LINES_PER_PAGE), -2 * LINES_PER_PAGE as i32);
    }

    #[test]
    fn a_zero_or_nonfinite_wheel_delta_scrolls_nothing() {
        assert_eq!(wheel_amount(0.0, 1.0), 0);
        assert_eq!(wheel_amount(f32::NAN, 1.0), 0);
        assert_eq!(wheel_amount(f32::INFINITY, 1.0), 0);
    }

    // A Mac client's OS already turned the wheel sideways, so doing it again
    // here would turn it back.
    #[test]
    fn shift_only_turns_a_wheel_sideways_for_a_client_that_did_not() {
        // A non-Mac client: vertical delta, Shift held.
        assert_eq!(shift_scrolls_sideways(0.0, -120.0, true), (-120.0, 0.0));
        // A Mac client: its OS already moved the delta across.
        assert_eq!(shift_scrolls_sideways(-120.0, 0.0, true), (-120.0, 0.0));
        // No Shift, no swap.
        assert_eq!(shift_scrolls_sideways(0.0, -120.0, false), (0.0, -120.0));
        // A diagonal trackpad gesture is left alone: it already has both axes,
        // and folding one into the other would lose the horizontal part.
        assert_eq!(shift_scrolls_sideways(-3.0, -7.0, true), (-3.0, -7.0));
    }

    #[test]
    fn dom_button_numbering_maps_to_cg_buttons() {
        assert_eq!(mac_button(0), Some(CGMouseButton::Left));
        assert_eq!(mac_button(1), Some(CGMouseButton::Center));
        assert_eq!(mac_button(2), Some(CGMouseButton::Right));
        assert_eq!(mac_button(5), None);
    }

    // Back and forward are `OtherMouse` events told apart only by their number,
    // and DOM numbers them the same way macOS does.
    #[test]
    fn the_side_buttons_keep_their_numbers() {
        assert_eq!(mac_button(3), Some(CGMouseButton(3)));
        assert_eq!(mac_button(4), Some(CGMouseButton(4)));
        // And they are past the two that carry their number implicitly, which
        // is what makes `post` stamp it on.
        for button in [1u8, 3, 4] {
            let mac = mac_button(button).expect("a known button");
            assert!(mac.0 > CGMouseButton::Right.0, "button {button}");
        }
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect {
            origin: CGPoint { x, y },
            size: objc2_core_foundation::CGSize {
                width: w,
                height: h,
            },
        }
    }

    fn point(x: f64, y: f64) -> CGPoint {
        CGPoint { x, y }
    }

    /// The measured arrangement on the test VM: the agent's display at the origin,
    /// the Mac's own smaller screen to its left.
    const OWNED: fn() -> CGRect = || rect(0.0, 0.0, 1600.0, 1000.0);
    const REAL: fn() -> CGRect = || rect(-800.0, 0.0, 800.0, 600.0);

    #[test]
    fn a_pointer_left_on_the_agents_display_goes_back_where_the_session_found_it() {
        assert_eq!(
            destination(point(859.0, 943.0), Some(point(-400.0, 300.0)), OWNED(), &[REAL()]),
            Some(point(-400.0, 300.0))
        );
    }

    /// The row that has no leftward path to the real screen is exactly the case the
    /// restore exists for, so it must not be treated as "already fine".
    #[test]
    fn a_pointer_below_the_real_screens_bottom_edge_is_still_stranded() {
        assert!(destination(point(859.0, 943.0), None, OWNED(), &[REAL()]).is_some());
    }

    #[test]
    fn a_pointer_on_a_real_screen_is_left_alone() {
        assert_eq!(
            destination(point(-400.0, 300.0), Some(point(-400.0, 300.0)), OWNED(), &[REAL()]),
            None
        );
    }

    #[test]
    fn nothing_saved_falls_back_to_the_middle_of_the_first_real_screen() {
        assert_eq!(
            destination(point(10.0, 10.0), None, OWNED(), &[REAL()]),
            Some(point(-400.0, 300.0))
        );
    }

    /// A display that went away while the session held the pointer: the saved point
    /// is on no screen now, so returning it there would strand the pointer off every
    /// display instead of on the wrong one.
    #[test]
    fn a_saved_point_on_a_screen_that_is_gone_falls_back_to_one_that_is_there() {
        assert_eq!(
            destination(point(10.0, 10.0), Some(point(-9000.0, 50.0)), OWNED(), &[REAL()]),
            Some(point(-400.0, 300.0))
        );
    }

    /// A saved point that is itself on the agent's display would make the restore a
    /// no-op. `note` already declines to save one, and the rule holds here too.
    #[test]
    fn a_saved_point_on_the_agents_own_display_is_not_a_destination() {
        assert_eq!(
            destination(point(10.0, 10.0), Some(point(20.0, 20.0)), OWNED(), &[REAL()]),
            Some(point(-400.0, 300.0))
        );
    }

    #[test]
    fn a_mac_whose_only_display_is_the_agents_has_nowhere_to_put_the_pointer() {
        assert_eq!(destination(point(10.0, 10.0), None, OWNED(), &[]), None);
    }

    /// Displays tile edge to edge, so the shared edge belongs to exactly one of
    /// them. Without the half-open rule a pointer at x=0 would read as being on both
    /// the real screen and ours.
    #[test]
    fn a_display_owns_its_top_left_edge_and_not_its_bottom_right() {
        assert!(contains(REAL(), point(-800.0, 0.0)));
        assert!(!contains(REAL(), point(0.0, 0.0)));
        assert!(contains(OWNED(), point(0.0, 0.0)));
        assert!(!contains(OWNED(), point(1600.0, 1000.0)));
    }
}
