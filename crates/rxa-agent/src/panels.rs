//! The agent's modal panels: everything the GUI needs beyond a menu item.
//!
//! [`config`] is the settings dialog — one panel holding every setting the agent
//! has. [`error`] reports the failures a menu cannot show on its own.
//! [`startup_failure`] is [`error`] from before there is a menu at all, which is
//! the only thing standing between a failed launch and an app that appears to do
//! nothing when opened.
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
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication,
    NSApplicationActivationPolicy, NSButton, NSControlStateValueOff, NSControlStateValueOn, NSFont,
    NSTextAlignment, NSTextField, NSView,
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
    /// Whether to give this Mac an extra display of the agent's own making.
    pub virtual_display: bool,
    /// That display's size, `WIDTHxHEIGHT` in points. Kept whether or not it is
    /// switched on, so turning it off and back on does not lose it.
    pub virtual_size: String,
}

/// The settings dialog: listen address, the display list, the virtual display,
/// pre-shared key.
///
/// Returns what the user typed if they saved, and `None` if they cancelled. The
/// draft is not validated here — the caller owns the rules, and the point of
/// handing back exactly what was typed is that it can re-open this dialog on the
/// same values when one of them is refused, instead of making the user retype a
/// key to fix a port.
pub fn config(mtm: MainThreadMarker, current: &Draft, displays: &[String]) -> Option<Draft> {
    let alert = alert(
        mtm,
        "remotex-agent Settings",
        "The pre-shared key is the entire credential: the same value must appear as `psk` \
         on the matching [[targets]] entry in the gateway's remotex.toml.\n\nSaving a \
         change restarts the agent, which drops any connection in progress — the gateway \
         reconnects on its own.",
        NSAlertStyle::Informational,
    );

    // Listen address, the display list, the virtual display switch, its size,
    // the key. Only the list is taller than one row, and only because it grows
    // with the number of screens attached.
    let list_height = (displays.len().max(1) as f64) * LABEL_HEIGHT;
    let heights = [
        ROW_HEIGHT,
        list_height,
        ROW_HEIGHT,
        ROW_HEIGHT,
        ROW_HEIGHT,
    ];
    let height =
        heights.iter().sum::<f64>() + (heights.len() as f64 - 1.0) * ROW_GAP;
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(WIDTH, height)),
    );
    // AppKit's origin is bottom-left, so rows are laid out upwards and named
    // downwards: listen on top, then the displays, the virtual switch and its
    // size, then the key.
    let mut tops = [0.0; 5];
    let mut cursor = height;
    for (top, row_height) in tops.iter_mut().zip(heights) {
        cursor -= row_height;
        *top = cursor;
        cursor -= ROW_GAP;
    }
    let row = |n: usize| tops[n];

    view.addSubview(&label(mtm, "Listen address", row(0)));
    let listen = field(mtm, &current.listen, row(0), WIDTH - CONTROL_X, false);
    view.addSubview(&listen);

    // Read-only, and that is the design rather than a shortcut: which display a
    // session shares is picked in the viewer or the browser, by whoever is
    // looking at it, and can change several times while this dialog is closed.
    // A control here would be a second opinion about a decision this process
    // does not own. What it is good for is answering "what is there to pick
    // from", which is exactly what the list says.
    view.addSubview(&label(mtm, "Displays", row(1)));
    let list = NSTextField::labelWithString(&NSString::from_str(&displays.join("\n")), mtm);
    list.setFrame(NSRect::new(
        NSPoint::new(CONTROL_X, row(1)),
        NSSize::new(WIDTH - CONTROL_X, list_height),
    ));
    list.setToolTip(Some(&NSString::from_str(
        "Every display this Mac can share. Which one a session shares is chosen \
         from the remotex viewer or the browser, not here.",
    )));
    view.addSubview(&list);

    let virtual_display = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Add a private 2x display"),
            None,
            None,
            mtm,
        )
    };
    virtual_display.setFrame(NSRect::new(
        NSPoint::new(CONTROL_X, row(2)),
        NSSize::new(WIDTH - CONTROL_X, ROW_HEIGHT),
    ));
    virtual_display.setState(if current.virtual_display {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    virtual_display.setToolTip(Some(&NSString::from_str(
        "Give this Mac an extra display that nobody is sitting in front of. It \
         joins the list above — the Mac's own screens stay shareable.",
    )));
    view.addSubview(&virtual_display);

    // Only meaningful with the box above ticked, and left enabled regardless: a
    // field that greys out as a checkbox is toggled is more startling than one
    // whose value simply does not apply yet.
    //
    // "Initial" is in the label rather than only the tooltip, because the field
    // otherwise reads as the display's current resolution — which it stops being
    // the moment anyone changes that display in System Settings.
    view.addSubview(&label(mtm, "Initial size", row(3)));
    let virtual_size = field(mtm, &current.virtual_size, row(3), WIDTH - CONTROL_X, false);
    virtual_size.setToolTip(Some(&NSString::from_str(
        "The size the virtual display is created at the first time this Mac sees \
         it, in points, WIDTHxHEIGHT — and the largest mode macOS can ever render \
         on it at 2x.\n\nAfter that its resolution is the Mac's, like any other \
         screen: change it in System Settings > Displays, where macOS also \
         remembers it. Editing this will not move a display already arranged \
         there.",
    )));
    view.addSubview(&virtual_size);

    view.addSubview(&label(mtm, "Pre-shared key", row(4)));
    let psk = field(
        mtm,
        &current.psk,
        row(4),
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
        NSPoint::new(WIDTH - BUTTON_WIDTH, row(4)),
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
    Some(Draft {
        listen: listen.stringValue().to_string().trim().to_owned(),
        psk: psk.stringValue().to_string().trim().to_owned(),
        virtual_display: virtual_display.state() == NSControlStateValueOn,
        virtual_size: virtual_size.stringValue().to_string().trim().to_owned(),
    })
}

/// Report a failure from before the menu bar exists, and give up.
///
/// Same panel as [`error`], with the activation policy set first. That normally
/// happens in [`crate::menubar::run`], which a failing startup never reaches — and
/// without it macOS gives the agent a Dock tile and a menu of its own for as long
/// as the panel is up, which is a strange last impression for an app that is about
/// to exit.
pub fn startup_failure(mtm: MainThreadMarker, title: &str, body: &str) {
    NSApplication::sharedApplication(mtm)
        .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    error(mtm, title, body);
}

/// Report a failure: one button, and nothing to decide.
pub fn error(mtm: MainThreadMarker, title: &str, body: &str) {
    let alert = alert(mtm, title, body, NSAlertStyle::Critical);
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
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
