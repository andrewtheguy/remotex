//! The agent's modal panels: everything the GUI needs beyond a menu item.
//!
//! Showing a key, taking an address and asking "are you sure" are the three
//! things a menu cannot do on its own, and they are the whole reason this module
//! exists. It is deliberately `NSAlert` and nothing more — a settings *window*
//! for three settings would be a nib's worth of AppKit, a window controller, and
//! a Dock icon's worth of behaviour the agent has spent real effort not having
//! (see the module docs in `main.rs`).
//!
//! ## Every panel activates the app first
//!
//! An accessory app (`LSUIElement`) is never the active application. A modal it
//! puts up without activating opens *behind* whatever the user is looking at —
//! invisible, and, being modal, not reachable from the menu bar item that opened
//! it. That is a hang as far as anyone can tell. So [`activate`] runs first,
//! every time.
//!
//! ## Modal from a menu action is fine
//!
//! `runModal` spins its own run loop, which would be a problem *inside* a menu
//! tracking loop. It is not one here: AppKit closes the menu and unwinds its
//! tracking before it sends the item's action, so by the time any of this runs
//! the menu is gone. The cursor timer keeps firing throughout, because it is
//! registered in `NSRunLoopCommonModes` (see [`crate::menubar`]).

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSAlertStyle, NSApplication,
    NSFont, NSTextField,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// Width of an accessory view, in points.
///
/// Wide enough for a 49-character pre-shared key on one line in a 12pt
/// monospaced font, which is the longest thing any panel here shows. A key that
/// wrapped mid-string would invite copying half of it by hand.
const ACCESSORY_WIDTH: f64 = 400.0;

/// What the user chose in [`secret`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secret {
    Copy,
    Regenerate,
    Close,
}

/// Tell the user something, with nothing to decide.
pub fn message(mtm: MainThreadMarker, title: &str, body: &str) {
    let alert = alert(mtm, title, body, NSAlertStyle::Informational);
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
}

/// Report a failure. Same shape as [`message`], louder icon.
pub fn error(mtm: MainThreadMarker, title: &str, body: &str) {
    let alert = alert(mtm, title, body, NSAlertStyle::Critical);
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
}

/// Ask before doing something the user cannot undo.
///
/// `confirm_label` names the action rather than saying "OK", because a button
/// that says what it does is the difference between reading the dialog and
/// dismissing it.
pub fn confirm(mtm: MainThreadMarker, title: &str, body: &str, confirm_label: &str) -> bool {
    let alert = alert(mtm, title, body, NSAlertStyle::Warning);
    alert.addButtonWithTitle(&NSString::from_str(confirm_label));
    // Second, so Escape and the default Return both land on Cancel's side of a
    // decision the user may not have meant to make.
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.runModal() == NSAlertFirstButtonReturn
}

/// Take one line of text, pre-filled with the current value.
///
/// Returns `None` if the user cancelled, and the trimmed text otherwise — even
/// when it is unchanged or empty. Deciding what an empty answer means belongs to
/// the caller, which is the only thing that knows.
pub fn prompt(
    mtm: MainThreadMarker,
    title: &str,
    body: &str,
    current: &str,
    save_label: &str,
) -> Option<String> {
    let alert = alert(mtm, title, body, NSAlertStyle::Informational);
    let field = NSTextField::textFieldWithString(&NSString::from_str(current), mtm);
    field.setFrame(accessory_frame(24.0));
    alert.setAccessoryView(Some(&field));
    alert.addButtonWithTitle(&NSString::from_str(save_label));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    // Without this the field is not focused and the first keystroke goes
    // nowhere, which reads as a dialog that has ignored you.
    alert.window().setInitialFirstResponder(Some(&field));

    if alert.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    Some(field.stringValue().to_string().trim().to_owned())
}

/// Show a secret, and offer the two things one can do with it.
///
/// The value is in a selectable field rather than the alert's own body text, so
/// it can be selected and copied by hand as well as with the button — some
/// people trust their own selection more than a clipboard they cannot see.
pub fn secret(mtm: MainThreadMarker, title: &str, body: &str, value: &str) -> Secret {
    let alert = alert(mtm, title, body, NSAlertStyle::Informational);

    let label = NSTextField::labelWithString(&NSString::from_str(value), mtm);
    label.setSelectable(true);
    // Monospaced: this is a string somebody may have to compare character by
    // character against the gateway's config file.
    label.setFont(NSFont::userFixedPitchFontOfSize(12.0).as_deref());
    label.setFrame(accessory_frame(20.0));
    alert.setAccessoryView(Some(&label));

    alert.addButtonWithTitle(&NSString::from_str("Copy"));
    alert.addButtonWithTitle(&NSString::from_str("Regenerate…"));
    alert.addButtonWithTitle(&NSString::from_str("Close"));
    let response = alert.runModal();
    if response == NSAlertFirstButtonReturn {
        Secret::Copy
    } else if response == NSAlertSecondButtonReturn {
        Secret::Regenerate
    } else {
        Secret::Close
    }
}

fn alert(
    mtm: MainThreadMarker,
    title: &str,
    body: &str,
    style: NSAlertStyle,
) -> Retained<NSAlert> {
    activate(mtm);
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(style);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(body));
    alert
}

fn accessory_frame(height: f64) -> NSRect {
    NSRect::new(NSPoint::ZERO, NSSize::new(ACCESSORY_WIDTH, height))
}

/// Bring the agent forward, so a modal from a menu bar item is actually visible.
/// See the module docs.
fn activate(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).activate();
}
