//! The agent's modal panels: everything the GUI needs beyond a menu item.
//!
//! Three of them. [`config`] is the settings dialog — one panel holding every
//! setting the agent has — and [`error`] and [`confirm`] are the two answers a
//! menu cannot give on its own: report a failure, and ask before doing something
//! the user would rather have been asked about.
//!
//! It is deliberately `NSAlert` and nothing more. A settings *window* would be a
//! window controller, a nib, and a Dock icon's worth of behaviour the agent has
//! spent real effort not having (see the module docs in `main.rs`), for three
//! fields. An alert with an accessory view is three fields.
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

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, Message as _, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSButton, NSFont,
    NSPopUpButton, NSTextAlignment, NSTextField, NSView,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// Width of an accessory view, in points.
///
/// Sized so a 49-character pre-shared key fits its field on one line in a 12pt
/// monospaced font: a key that wrapped or scrolled would invite copying half of
/// it by hand.
const WIDTH: f64 = 470.0;

/// One row of the settings dialog: label on the left, control on the right.
const ROW_HEIGHT: f64 = 24.0;
const ROW_GAP: f64 = 10.0;
const LABEL_WIDTH: f64 = 104.0;
const CONTROL_X: f64 = LABEL_WIDTH + 8.0;
/// A single line of label text, so a caption can be centred against its control
/// rather than sitting at the top of a 24pt frame.
const LABEL_HEIGHT: f64 = 17.0;
/// The **Regenerate** button beside the key field.
const BUTTON_WIDTH: f64 = 100.0;

/// Every setting the agent has, as the dialog reads and writes it.
///
/// A plain struct rather than [`crate::config::Config`] because a draft is
/// allowed to be nonsense — it is whatever the user has typed, and the caller
/// validates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub listen: String,
    pub psk: String,
    pub display: usize,
}

/// One entry for the dialog's display menu: the index the config stores, and how
/// to describe it.
pub struct DisplayChoice {
    pub index: usize,
    pub label: String,
}

/// The settings dialog: listen address, display, pre-shared key.
///
/// Returns what the user typed if they saved, and `None` if they cancelled. The
/// draft is not validated here — the caller owns the rules, and the point of
/// handing back exactly what was typed is that it can re-open this dialog on the
/// same values when one of them is refused, instead of making the user retype a
/// key to fix a port.
pub fn config(
    mtm: MainThreadMarker,
    current: &Draft,
    displays: &[DisplayChoice],
) -> Option<Draft> {
    let alert = alert(
        mtm,
        "remotex-agent Settings",
        "The pre-shared key is the entire credential: the same value must appear as `psk` \
         on the matching [[targets]] entry in the gateway's remotex.toml.\n\nSaving a \
         change restarts the agent, which drops any connection in progress — the gateway \
         reconnects on its own.",
        NSAlertStyle::Informational,
    );

    let rows = 3.0;
    let height = rows * ROW_HEIGHT + (rows - 1.0) * ROW_GAP;
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(WIDTH, height)),
    );
    // AppKit's origin is bottom-left, so rows are laid out upwards and named
    // downwards: listen on top, then display, then the key.
    let row = |n: f64| (rows - 1.0 - n) * (ROW_HEIGHT + ROW_GAP);

    view.addSubview(&label(mtm, "Listen address", row(0.0)));
    let listen = field(mtm, &current.listen, row(0.0), WIDTH - CONTROL_X, false);
    view.addSubview(&listen);

    view.addSubview(&label(mtm, "Display", row(1.0)));
    let display = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        NSRect::new(
            NSPoint::new(CONTROL_X, row(1.0)),
            NSSize::new(WIDTH - CONTROL_X, ROW_HEIGHT),
        ),
        false,
    );
    for choice in displays {
        display.addItemWithTitle(&NSString::from_str(&choice.label));
    }
    // The configured display can be one that is not attached, and a popup with
    // nothing selected would silently save a different screen than the one shown.
    let selected = displays
        .iter()
        .position(|choice| choice.index == current.display)
        .unwrap_or(0);
    display.selectItemAtIndex(selected as isize);
    view.addSubview(&display);

    view.addSubview(&label(mtm, "Pre-shared key", row(2.0)));
    let psk = field(
        mtm,
        &current.psk,
        row(2.0),
        WIDTH - CONTROL_X - BUTTON_WIDTH - 6.0,
        true,
    );
    view.addSubview(&psk);
    // Owns the Regenerate button's action. `buttonWithTitle:target:action:` holds
    // its target weakly, so this has to outlive `runModal` below — which is why
    // it is a named local and not a temporary.
    let regenerator = Regenerator::new(mtm, psk.clone());
    let regenerate = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Regenerate"),
            Some(&regenerator.as_object()),
            Some(sel!(regenerate:)),
            mtm,
        )
    };
    // Exactly the row, like every other control here: anything hanging outside
    // the accessory view's bounds is at the mercy of the alert's layout.
    regenerate.setFrame(NSRect::new(
        NSPoint::new(WIDTH - BUTTON_WIDTH, row(2.0)),
        NSSize::new(BUTTON_WIDTH, ROW_HEIGHT),
    ));
    // A new key lands in the field rather than in the file: nothing is saved
    // until Save, so a regenerate can still be abandoned with Cancel.
    regenerate.setToolTip(Some(&NSString::from_str(
        "Put a fresh key in the field. Nothing is saved until you press Save.",
    )));
    view.addSubview(&regenerate);

    alert.setAccessoryView(Some(&view));
    alert.addButtonWithTitle(&NSString::from_str("Save"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    // Without this nothing is focused and the first keystroke goes nowhere, which
    // reads as a dialog that has ignored you.
    alert.window().setInitialFirstResponder(Some(&listen));

    if alert.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    let chosen = usize::try_from(display.indexOfSelectedItem())
        .ok()
        .and_then(|position| displays.get(position))
        .map_or(current.display, |choice| choice.index);
    Some(Draft {
        listen: listen.stringValue().to_string().trim().to_owned(),
        psk: psk.stringValue().to_string().trim().to_owned(),
        display: chosen,
    })
}

/// Report a failure: one button, and nothing to decide.
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

/// The **Regenerate** button's target: puts a fresh key in the field it holds.
///
/// A whole class for one button, because an `NSButton` action has to be a
/// selector on an Objective-C object — there is nowhere to hang a Rust closure.
struct RegeneratorIvars {
    psk: Retained<NSTextField>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `Regenerator` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    // AppKit sends the action on the main thread, and the ivar is a main-thread
    // only object.
    #[thread_kind = MainThreadOnly]
    #[name = "RxaRegenerator"]
    #[ivars = RegeneratorIvars]
    struct Regenerator;

    unsafe impl NSObjectProtocol for Regenerator {}

    impl Regenerator {
        #[unsafe(method(regenerate:))]
        fn regenerate(&self, _sender: Option<&AnyObject>) {
            let psk = rxa_proto::psk::generate();
            self.ivars().psk.setStringValue(&NSString::from_str(&psk));
        }
    }
);

impl Regenerator {
    fn new(mtm: MainThreadMarker, psk: Retained<NSTextField>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RegeneratorIvars { psk });
        unsafe { msg_send![super(this), init] }
    }

    fn as_object(&self) -> Retained<AnyObject> {
        let this: Retained<Self> = self.retain();
        // Safety: upcasting a subclass of NSObject to AnyObject.
        unsafe { Retained::cast_unchecked(this) }
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

/// A settings row's caption: right-aligned against its control, and centred on
/// it.
///
/// Its frame is one line tall and offset into the row, not the row's full height:
/// a label draws its text at the top of whatever frame it is given, so a 24pt one
/// would sit visibly above the field it names — and a frame taller than the row
/// risks being clipped by the alert's own layout.
fn label(mtm: MainThreadMarker, text: &str, y: f64) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setAlignment(NSTextAlignment::Right);
    label.setFrame(NSRect::new(
        NSPoint::new(0.0, y + (ROW_HEIGHT - LABEL_HEIGHT) / 2.0),
        NSSize::new(LABEL_WIDTH, LABEL_HEIGHT),
    ));
    label
}

fn field(
    mtm: MainThreadMarker,
    value: &str,
    y: f64,
    width: f64,
    monospaced: bool,
) -> Retained<NSTextField> {
    let field = NSTextField::textFieldWithString(&NSString::from_str(value), mtm);
    field.setFrame(NSRect::new(
        NSPoint::new(CONTROL_X, y),
        NSSize::new(width, ROW_HEIGHT),
    ));
    if monospaced {
        // The key may have to be compared character by character against the
        // gateway's config file.
        field.setFont(NSFont::userFixedPitchFontOfSize(12.0).as_deref());
    }
    field
}

/// Bring the agent forward, so a modal from a menu bar item is actually visible.
/// See the module docs.
fn activate(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).activate();
}
