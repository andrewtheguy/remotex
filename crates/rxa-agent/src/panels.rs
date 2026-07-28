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

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, Message as _, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication,
    NSApplicationActivationPolicy, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSTextAlignment, NSTextField, NSView,
};
use objc2_foundation::{
    NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};

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
/// The key's three buttons, on the row under the field: **Copy**, **Edit**,
/// **Regenerate**. Under it rather than beside it so the field keeps the full
/// width — a key that scrolls is a key somebody copies half of by hand, which is
/// also what [`WIDTH`] is sized for.
const COPY_WIDTH: f64 = 78.0;
const EDIT_WIDTH: f64 = 78.0;
const REGENERATE_WIDTH: f64 = 110.0;
const BUTTON_GAP: f64 = 8.0;

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
///
/// `in_force` is the key the running process is actually authenticating with,
/// which is not always the one in `current` — see the warning it raises below.
/// It is read, never written: this dialog edits the file.
pub fn config(
    mtm: MainThreadMarker,
    current: &Draft,
    displays: &[String],
    in_force: &str,
) -> Option<Draft> {
    let mut body = String::from(
        "The pre-shared key is the entire credential: the same value must appear as `psk` \
         on the matching [[targets]] entry in the gateway's remotex.toml.\n\nSaving a \
         change restarts the agent, which drops any connection in progress — the gateway \
         reconnects on its own.",
    );
    // Said out loud rather than left in a tooltip, because Copy is here now and
    // the difference between these two keys is the difference between a gateway
    // that connects and one that is refused. Saving a key re-execs into it, so
    // they normally agree; they differ only when that did not happen — a re-exec
    // that failed, or a file edited by hand.
    if in_force != current.psk.trim() {
        body.push_str(
            "\n\n⚠︎ The key below is the one in the config file, and it never took effect: \
             this agent is still authenticating with the previous one. Quit remotex-agent \
             and open it again to start using the key below.",
        );
    }
    let alert = alert(mtm, "remotex-agent Settings", &body, NSAlertStyle::Informational);

    // Listen address, the display list, the virtual display switch, its size,
    // the key, and the key's buttons. Only the list is taller than one row, and
    // only because it grows with the number of screens attached.
    let list_height = (displays.len().max(1) as f64) * LABEL_HEIGHT;
    let heights = [
        ROW_HEIGHT,
        list_height,
        ROW_HEIGHT,
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
    // size, then the key and what can be done to it.
    let mut tops = [0.0; 6];
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
    // the moment anyone changes that display, in System Settings or with a
    // client's Resize to window.
    view.addSubview(&label(mtm, "Initial size", row(3)));
    let virtual_size = field(mtm, &current.virtual_size, row(3), WIDTH - CONTROL_X, false);
    virtual_size.setToolTip(Some(&NSString::from_str(
        "The size the virtual display is created at the first time this Mac sees \
         it, in points, WIDTHxHEIGHT, no smaller than 800x600 — and the largest \
         mode macOS can ever render on it at 2x.\n\nTo change its size later, use \
         \"Resize to window\" in the browser or the viewer, which asks this \
         display to match the window it is being shown in. It can also be resized \
         in System Settings > Displays like any other screen, but every size is \
         listed twice there — HiDPI and \"(low resolution)\" — and one much \
         smaller than this drops out of HiDPI either way, so it comes back soft \
         or oversized. macOS remembers whichever size it ends up at and restores \
         it, so editing this will not move a display already arranged.",
    )));
    view.addSubview(&virtual_size);

    // Locked until Edit says otherwise, and it looks locked: a grey fill instead
    // of a field's white one, an abbreviated key, and no selection. The key is
    // the whole credential and it is already right — the overwhelmingly common
    // visit to this dialog reads it onto a gateway, and a field one keystroke
    // away from silently becoming a *different* key, with the agent then
    // answering nobody, is a poor thing to leave under a cursor.
    //
    // Abbreviated because the way to take a key out of here is Copy. Shown whole
    // it invites a drag-select, which on a 49-character key in a field that
    // barely fits it is how three characters get left behind — and a key that is
    // wrong by three characters fails as a checksum, with nothing to say which
    // half was mistyped. Copy is exact, and it is the only way out while locked.
    view.addSubview(&label(mtm, "Pre-shared key", row(4)));
    let psk = field(mtm, &abbreviate(&current.psk), row(4), WIDTH - CONTROL_X, true);
    view.addSubview(&psk);

    // Owns all three buttons' actions, and the real key behind that abbreviation.
    // `buttonWithTitle:target:action:` holds its target weakly, so this has to
    // outlive `runModal` below — which is why it is a named local and not a
    // temporary.
    let actions = KeyActions::new(mtm, psk.clone(), &current.psk);
    actions.set_locked(true);
    // Right-aligned as one group under the field, in the order they are reached
    // for: read it, then unlock, then replace.
    let regenerate_x = WIDTH - REGENERATE_WIDTH;
    let edit_x = regenerate_x - BUTTON_GAP - EDIT_WIDTH;
    let copy_x = edit_x - BUTTON_GAP - COPY_WIDTH;

    let copy = button(mtm, "Copy", &actions, sel!(copyKey:), copy_x, row(5), COPY_WIDTH);
    copy.setToolTip(Some(&NSString::from_str(
        "Put the whole key on the clipboard, to paste as `psk` on the gateway's rxa \
         target. The field above shows an abbreviation of it.",
    )));
    view.addSubview(&copy);

    let edit = button(mtm, "Edit", &actions, sel!(unlockKey:), edit_x, row(5), EDIT_WIDTH);
    edit.setToolTip(Some(&NSString::from_str(
        "Show the key in full and unlock it for typing, and enable Regenerate. \
         Changing it means changing the gateway's copy to match, or nothing can \
         connect.",
    )));
    view.addSubview(&edit);

    let regenerate = button(
        mtm,
        "Regenerate",
        &actions,
        sel!(regenerate:),
        regenerate_x,
        row(5),
        REGENERATE_WIDTH,
    );
    // Disabled until Edit, for the same reason the field is locked: this one is
    // worse, being a single click from a key nothing else in the world knows.
    regenerate.setEnabled(false);
    // A new key lands in the field rather than in the file: nothing is saved
    // until Save, so a regenerate can still be abandoned with Cancel.
    regenerate.setToolTip(Some(&NSString::from_str(
        "Press Edit first. Puts a fresh key in the field — nothing is saved until you \
         press Save.",
    )));
    view.addSubview(&regenerate);
    actions.arm(copy.clone(), edit.clone(), regenerate.clone());

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
        // Never the field's text: while locked that is an abbreviation, and
        // saving it would replace the credential with an ellipsis.
        psk: actions.key(),
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

/// How long Copy says it copied.
const COPIED_FOR: f64 = 1.2;

/// The key row's three buttons' target: copy it, unlock it, replace it — and the
/// keeper of the real key, which is not what the field shows while locked.
///
/// A whole class for three buttons, because an `NSButton` action has to be a
/// selector on an Objective-C object — there is nowhere to hang a Rust closure.
struct KeyActionsIvars {
    psk: Retained<NSTextField>,
    /// The key itself. Authoritative while locked, when the field holds an
    /// abbreviation of it; from Edit onwards the field is authoritative, because
    /// the user may have typed or regenerated something else. [`KeyActions::key`]
    /// is the one place that decides which.
    full: RefCell<String>,
    locked: Cell<bool>,
    /// The row's buttons. Filled in by [`KeyActions::arm`] rather than at
    /// construction: each of them takes *this* object as its target, so none of
    /// them can exist before it does.
    buttons: RefCell<Option<Buttons>>,
}

struct Buttons {
    copy: Retained<NSButton>,
    edit: Retained<NSButton>,
    regenerate: Retained<NSButton>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `KeyActions` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    // AppKit sends the actions on the main thread, and the ivars are main-thread
    // only objects.
    #[thread_kind = MainThreadOnly]
    #[name = "RxaKeyActions"]
    #[ivars = KeyActionsIvars]
    struct KeyActions;

    unsafe impl NSObjectProtocol for KeyActions {}

    impl KeyActions {
        /// The whole key, never the field's text — which is an abbreviation
        /// while locked, and would be a credential three characters short.
        #[unsafe(method(copyKey:))]
        fn copy_key(&self, _sender: Option<&AnyObject>) {
            let wrote = crate::pasteboard::write(&self.key());
            // Nothing else changes on screen when this works, and a clipboard is
            // not somewhere you can look to check. Without a word from the
            // button, the way to find out whether it copied is to paste it
            // somewhere and see — which for a credential means putting it
            // somewhere it should not be.
            self.say(if wrote { "Copied" } else { "Failed" });
        }

        /// Put the button back, a moment later. See [`KeyActions::say`].
        #[unsafe(method(restoreCopy:))]
        fn restore_copy(&self, _timer: Option<&AnyObject>) {
            if let Some(buttons) = self.ivars().buttons.borrow().as_ref() {
                buttons.copy.setTitle(&NSString::from_str("Copy"));
            }
        }

        /// One way, and only for as long as this dialog is up: Cancel discards
        /// whatever the unlock allowed, and the next visit starts locked again.
        #[unsafe(method(unlockKey:))]
        fn unlock_key(&self, _sender: Option<&AnyObject>) {
            let ivars = self.ivars();
            // In full now: it can be read carefully, and it has to be what any
            // typing starts from.
            let full = ivars.full.borrow().clone();
            ivars.psk.setStringValue(&NSString::from_str(&full));
            self.set_locked(false);
            if let Some(buttons) = ivars.buttons.borrow().as_ref() {
                buttons.regenerate.setEnabled(true);
                // Nothing left for it to do, and leaving it live would suggest
                // there were a way back other than Cancel.
                buttons.edit.setEnabled(false);
            }
            // Otherwise the click that unlocked the field leaves focus on a
            // button, and the next keystroke goes nowhere.
            if let Some(window) = ivars.psk.window() {
                window.makeFirstResponder(Some(&ivars.psk));
            }
        }

        /// Only reachable after Edit, so the field is showing a whole key and is
        /// the authority on it — no need to touch `full`.
        #[unsafe(method(regenerate:))]
        fn regenerate(&self, _sender: Option<&AnyObject>) {
            let psk = rxa_proto::psk::generate();
            self.ivars().psk.setStringValue(&NSString::from_str(&psk));
        }
    }
);

impl KeyActions {
    fn new(mtm: MainThreadMarker, psk: Retained<NSTextField>, key: &str) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(KeyActionsIvars {
            psk,
            full: RefCell::new(key.trim().to_owned()),
            locked: Cell::new(true),
            buttons: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Hand over the buttons the actions act on, once they exist.
    fn arm(&self, copy: Retained<NSButton>, edit: Retained<NSButton>, regenerate: Retained<NSButton>) {
        *self.ivars().buttons.borrow_mut() = Some(Buttons { copy, edit, regenerate });
    }

    /// Dress the field for locked or unlocked.
    ///
    /// The background is the whole of it visually: a text field is white because
    /// it is somewhere to type, and this one is not until Edit. Selection goes
    /// with it — a drag-select of an abbreviation is a key that fails its
    /// checksum at the gateway for a reason nothing on screen explains, so while
    /// locked the only way to take the key out is Copy.
    fn set_locked(&self, locked: bool) {
        let ivars = self.ivars();
        ivars.locked.set(locked);
        let field = &ivars.psk;
        field.setEditable(!locked);
        field.setSelectable(!locked);
        let background = if locked {
            NSColor::windowBackgroundColor()
        } else {
            NSColor::textBackgroundColor()
        };
        field.setBackgroundColor(Some(&background));
    }

    /// The key as it stands: the stored one while locked, the field's from Edit
    /// onwards.
    fn key(&self) -> String {
        let ivars = self.ivars();
        if ivars.locked.get() {
            ivars.full.borrow().clone()
        } else {
            ivars.psk.stringValue().to_string().trim().to_owned()
        }
    }

    /// Let the Copy button report, by becoming the report for a moment.
    ///
    /// `NSRunLoopCommonModes`, because this dialog is a modal run loop and a
    /// timer left in the default mode would not fire until the dialog closed —
    /// see [`crate::menubar`], which registers its cursor timer for the same
    /// reason. The timer is not kept: it fires once and releases its target,
    /// where holding it here would be this object holding a timer holding this
    /// object. Two clicks in quick succession therefore restore on the first
    /// one's schedule, which costs a fifth of a second of a button saying
    /// "Copy" while it might have said "Copied".
    fn say(&self, title: &str) {
        let Some(button) = self
            .ivars()
            .buttons
            .borrow()
            .as_ref()
            .map(|buttons| buttons.copy.clone())
        else {
            return;
        };
        button.setTitle(&NSString::from_str(title));
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                COPIED_FOR,
                &self.as_object(),
                sel!(restoreCopy:),
                None,
                false,
            )
        };
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    }

    fn as_object(&self) -> Retained<AnyObject> {
        let this: Retained<Self> = self.retain();
        // Safety: upcasting a subclass of NSObject to AnyObject.
        unsafe { Retained::cast_unchecked(this) }
    }
}

/// A key short enough to be glanced at rather than read, and still enough of it
/// to tell two apart.
///
/// Both ends, not just the head: a key is copied to compare against one already
/// on a gateway, and the tail is what says they are the same key rather than the
/// same prefix. The ellipsis is the point — nobody mistakes this for something to
/// transcribe.
fn abbreviate(key: &str) -> String {
    const HEAD: usize = 12;
    const TAIL: usize = 4;
    let chars: Vec<char> = key.chars().collect();
    // Nothing to gain by abbreviating something already this short — and a
    // "shortening" that is longer than the original would be a strange thing to
    // show for a key somebody has hand-edited into nonsense.
    if chars.len() <= HEAD + TAIL + 1 {
        return key.to_owned();
    }
    let head: String = chars[..HEAD].iter().collect();
    let tail: String = chars[chars.len() - TAIL..].iter().collect();
    format!("{head}…{tail}")
}

/// One of the key row's buttons: same target, same row, different width.
fn button(
    mtm: MainThreadMarker,
    title: &str,
    target: &KeyActions,
    action: objc2::runtime::Sel,
    x: f64,
    y: f64,
    width: f64,
) -> Retained<NSButton> {
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(&target.as_object()),
            Some(action),
            mtm,
        )
    };
    // Exactly the row, like every other control here: anything hanging outside
    // the accessory view's bounds is at the mercy of the alert's layout.
    button.setFrame(NSRect::new(
        NSPoint::new(x, y),
        NSSize::new(width, ROW_HEIGHT),
    ));
    button
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What the locked field shows. The rest of this module needs a window
    /// server; this part is a string, and it is the part that could quietly go
    /// wrong — an abbreviation that is a *plausible* key is one somebody
    /// transcribes.
    #[test]
    fn an_abbreviated_key_cannot_be_mistaken_for_one() {
        let key = rxa_proto::psk::generate();
        let short = abbreviate(&key);

        assert!(short.contains('…'), "{short}");
        assert!(short.len() < key.len(), "{short} is no shorter than the key");
        assert_ne!(short, key);
        // Both ends, because comparing against a gateway's copy is what this is
        // for: a prefix alone cannot tell two keys apart, and every key here
        // shares the same one.
        assert!(key.starts_with(&short[..short.find('…').unwrap()]), "{short}");
        let tail = &short[short.find('…').unwrap() + '…'.len_utf8()..];
        assert!(key.ends_with(tail), "{short}");
        assert!(key.starts_with(rxa_proto::psk::PREFIX), "the shared prefix");
    }

    /// Nothing worth hiding, and a "shortening" longer than its input would be a
    /// strange thing to show for a key somebody has hand-edited into nonsense.
    #[test]
    fn something_already_short_is_left_alone() {
        for value in ["", "rxap", "not-a-key"] {
            assert_eq!(abbreviate(value), value);
        }
    }
}
