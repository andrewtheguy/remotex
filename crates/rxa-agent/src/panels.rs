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
//! spent real effort not having (see the module docs in `main.rs`), for a
//! handful of fields. An alert with an accessory view is a handful of fields.
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

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, Message as _, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication,
    NSApplicationActivationPolicy, NSButton, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSTextAlignment, NSTextField, NSView,
};
use objc2_foundation::{
    NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};

/// Width of an accessory view, in points.
///
/// What sets it is the key rows: a 50-character key has to fit on one line in a
/// 12pt monospaced font, because one that scrolls is one somebody copies half of
/// by hand. Menlo 12 advances ~7.23pt per character, so 50 of them need ~361pt
/// plus the field's insets — and what a row leaves for its control is
/// `WIDTH - CONTROL_X`, which is why widening [`LABEL_WIDTH`] means widening
/// this too. Both were measured on screen: at 490 against a 126pt label the last
/// character of the key was cut off.
const WIDTH: f64 = 540.0;

/// One row of the settings dialog: label on the left, control on the right.
const ROW_HEIGHT: f64 = 24.0;
const ROW_GAP: f64 = 10.0;
/// Wide enough for the longest caption, "This Mac's public key", which at 490
/// against 126pt rendered as "This Mac's public ke".
const LABEL_WIDTH: f64 = 150.0;
const CONTROL_X: f64 = LABEL_WIDTH + 8.0;
/// A single line of label text, so a caption can be centred against its control
/// rather than sitting at the top of a 24pt frame.
const LABEL_HEIGHT: f64 = 17.0;
/// The key buttons, on a row of their own under both key rows: **Copy** (this
/// Mac's public key), **Import…** and **Regenerate identity**. Under rather than
/// beside so each key keeps the full width — a key that scrolls is a key somebody
/// copies half of by hand, which is also what [`WIDTH`] is sized for.
///
/// In the order they are reached for, left to right: reading this Mac's key out
/// is the routine visit, and the two that replace its identity are together at
/// the far end.
const COPY_WIDTH: f64 = 78.0;
const IMPORT_WIDTH: f64 = 84.0;
const REGENERATE_WIDTH: f64 = 144.0;
const BUTTON_GAP: f64 = 8.0;

/// Every setting the agent has, as the dialog reads and writes it.
///
/// A plain struct rather than [`crate::config::Config`] because a draft is
/// allowed to be nonsense — it is whatever the user has typed, and the caller
/// validates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub listen: String,
    /// This Mac's private key. Carried through the dialog but never shown in it
    /// and never typed into: the only thing that changes it is **Regenerate
    /// identity**. What the dialog *displays* is the public half derived from
    /// it, which is not a secret.
    pub private_key: String,
    /// The gateway's public key, as pasted in. Empty means unpaired.
    pub gateway_public_key: String,
    /// Whether to give this Mac an extra display of the agent's own making.
    pub virtual_display: bool,
    /// That display's size, `WIDTHxHEIGHT` in points. Kept whether or not it is
    /// switched on, so turning it off and back on does not lose it.
    pub virtual_size: String,
}

impl Draft {
    /// The public key matching [`Draft::private_key`], or an empty string if
    /// that key is nonsense — which only a hand-edited config can manage, and
    /// which the dialog shows as a blank rather than refusing to open over.
    fn public_key(&self) -> String {
        rxa_proto::key::public_text_of(rxa_proto::key::Role::Agent, &self.private_key)
            .unwrap_or_default()
    }
}

/// The settings dialog: listen address, the display list, the virtual display,
/// and the two halves of the pairing.
///
/// Returns what the user typed if they saved, and `None` if they cancelled. The
/// draft is not validated here — the caller owns the rules, and the point of
/// handing back exactly what was typed is that it can re-open this dialog on the
/// same values when one of them is refused, instead of making the user retype a
/// key to fix a port.
///
/// `in_force` is the pairing the running process is actually using, which is not
/// always the one in `current` — see the warning it raises below. It is read,
/// never written: this dialog edits the file.
pub fn config(
    mtm: MainThreadMarker,
    current: &Draft,
    displays: &[String],
    in_force: &InForce,
) -> Option<Draft> {
    // Kept short on purpose: this is an NSAlert, so the body pushes the rows
    // down, and the whole panel has to fit a small screen — the test Mac's is
    // 800x600. The detail lives in the rows' own tooltips.
    let mut body = String::from(
        "Pairing is two public keys, one each way, and neither is a secret. Copy this \
         Mac's onto the gateway as `agent_public_key`; paste the gateway's own — from \
         `remotex rxa-pubkey` — below.\n\nSaving restarts the agent, dropping any \
         connection in progress. The gateway reconnects on its own.",
    );
    if current.gateway_public_key.trim().is_empty() {
        body.push_str("\n\n⚠︎ Unpaired: no gateway key is set, so every connection is refused.");
    }
    // Said out loud rather than left in a tooltip, because the difference
    // between the saved pairing and the running one is the difference between a
    // gateway that connects and one that is refused. Saving re-execs into the
    // new values, so they normally agree; they differ only when that did not
    // happen — a re-exec that failed, or a file edited by hand.
    if !in_force.matches(current) {
        body.push_str(
            "\n\n⚠︎ The keys below are the ones in the config file, and they never took \
             effect: this agent is still using the previous pairing. Quit remotex-agent \
             and open it again to start using these.",
        );
    }
    let alert = alert(mtm, "remotex-agent Settings", &body, NSAlertStyle::Informational);

    // Listen address, the display list, the virtual display switch, its size,
    // this Mac's public key, the gateway's, and the key buttons. Only the list
    // is taller than one row, and only because it grows with the number of
    // screens attached.
    let list_height = (displays.len().max(1) as f64) * LABEL_HEIGHT;
    let heights = [
        ROW_HEIGHT,
        list_height,
        ROW_HEIGHT,
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
    // size, then the two keys and what can be done to them.
    let mut tops = [0.0; 7];
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

    // Shown whole, and a label rather than a field: this is derived from the
    // private key and there is nothing to type into it, so there is no state to
    // lock and no white box to suggest otherwise. Whole because it is not a
    // secret — which is the entire reason the protocol stopped using a shared
    // one — and because reading it against the gateway's copy is the common
    // visit to this dialog. Copy is still there, being exact where a drag-select
    // over 50 monospaced characters is not.
    view.addSubview(&label(mtm, "This Mac's public key", row(4)));
    let public_key = NSTextField::labelWithString(&NSString::from_str(&current.public_key()), mtm);
    public_key.setFont(NSFont::userFixedPitchFontOfSize(12.0).as_deref());
    public_key.setSelectable(true);
    public_key.setFrame(NSRect::new(
        NSPoint::new(CONTROL_X, row(4)),
        NSSize::new(WIDTH - CONTROL_X, ROW_HEIGHT),
    ));
    public_key.setToolTip(Some(&NSString::from_str(
        "Paste this as `agent_public_key` on the matching [[targets]] entry in the \
         gateway's remotex.toml. Not a secret — the private key it comes from never \
         leaves this Mac.",
    )));
    view.addSubview(&public_key);

    // An ordinary editable field: this one is pasted in, and it is not a secret
    // either, so nothing about it wants hiding or locking.
    view.addSubview(&label(mtm, "Gateway public key", row(5)));
    let gateway = field(
        mtm,
        &current.gateway_public_key,
        row(5),
        WIDTH - CONTROL_X,
        true,
    );
    gateway.setToolTip(Some(&NSString::from_str(
        "The one gateway this Mac answers, from `remotex rxa-pubkey` on that server. \
         Leave it empty to unpair, which makes this agent refuse every connection.",
    )));
    view.addSubview(&gateway);

    // Owns both buttons' actions and the private key behind the label above.
    // `buttonWithTitle:target:action:` holds its target weakly, so this has to
    // outlive `runModal` below — which is why it is a named local and not a
    // temporary.
    let actions = KeyActions::new(mtm, public_key.clone(), &current.private_key);
    // Right-aligned as one group under the keys, in the order they are reached
    // for: read this Mac's key, or — rarely — replace it, with one it had before
    // or with a new one.
    let regenerate_x = WIDTH - REGENERATE_WIDTH;
    let import_x = regenerate_x - BUTTON_GAP - IMPORT_WIDTH;
    let copy_x = import_x - BUTTON_GAP - COPY_WIDTH;

    let copy = button(mtm, "Copy", &actions, sel!(copyKey:), copy_x, row(6), COPY_WIDTH);
    copy.setToolTip(Some(&NSString::from_str(
        "Put this Mac's public key on the clipboard, to paste as `agent_public_key` on \
         the gateway's rxa target.",
    )));
    view.addSubview(&copy);

    let import = button(
        mtm,
        "Import…",
        &actions,
        sel!(importIdentity:),
        import_x,
        row(6),
        IMPORT_WIDTH,
    );
    import.setToolTip(Some(&NSString::from_str(
        "Give this Mac an identity it has held before — after a reinstall, or when it \
         replaces another Mac — so the gateways that already know that public key need \
         no change. Asks for the private key; this dialog never shows one.",
    )));
    view.addSubview(&import);

    let regenerate = button(
        mtm,
        "Regenerate identity",
        &actions,
        sel!(regenerateIdentity:),
        regenerate_x,
        row(6),
        REGENERATE_WIDTH,
    );
    // Confirmed rather than disabled behind an unlock step: there is no field to
    // unlock any more, and what makes this dangerous is not a stray keystroke
    // but what it means — every gateway paired with this Mac stops being able to
    // reach it until its config is updated. That is a question, not a lock.
    regenerate.setToolTip(Some(&NSString::from_str(
        "Replace this Mac's identity with a fresh keypair. Asks first, and nothing is \
         saved until you press Save — but once saved, the gateway refuses this Mac \
         until its new public key is pasted there.",
    )));
    view.addSubview(&regenerate);
    actions.arm(copy.clone());

    alert.setAccessoryView(Some(&view));
    alert.addButtonWithTitle(&NSString::from_str("Save"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    // Without this nothing is focused and the first keystroke goes nowhere, which
    // reads as a dialog that has ignored you. The gateway field when there is no
    // pairing yet: on a fresh Mac that is the one thing left to do.
    let first = if current.gateway_public_key.trim().is_empty() {
        &gateway
    } else {
        &listen
    };
    alert.window().setInitialFirstResponder(Some(first));

    if alert.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    Some(Draft {
        listen: listen.stringValue().to_string().trim().to_owned(),
        // From the actions, not the label: the label shows the *public* half,
        // and Regenerate may have replaced what it was derived from.
        private_key: actions.private_key(),
        gateway_public_key: gateway.stringValue().to_string().trim().to_owned(),
        virtual_display: virtual_display.state() == NSControlStateValueOn,
        virtual_size: virtual_size.stringValue().to_string().trim().to_owned(),
    })
}

/// The pairing the running agent is actually using, for the warning [`config`]
/// raises when the file has moved on without a restart.
pub struct InForce {
    pub private_key: String,
    pub gateway_public_key: String,
}

impl InForce {
    /// Whether a draft describes what the agent is already doing.
    ///
    /// Both halves, because either one changing is a pairing that has not taken
    /// effect: a regenerated identity the gateway has not been told about, and a
    /// newly pasted gateway key, fail in exactly the same way.
    fn matches(&self, draft: &Draft) -> bool {
        self.private_key == draft.private_key.trim()
            && self.gateway_public_key == draft.gateway_public_key.trim()
    }
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

/// The key buttons' target: copy this Mac's public key, or replace the identity
/// behind it — and the keeper of the private key, which nothing on screen shows.
///
/// A whole class for two buttons, because an `NSButton` action has to be a
/// selector on an Objective-C object — there is nowhere to hang a Rust closure.
struct KeyActionsIvars {
    /// The label showing the public key, updated in place by a regenerate.
    public_key: Retained<NSTextField>,
    /// The private key the label is derived from. Authoritative throughout:
    /// there is no field it could disagree with, because it is never displayed.
    private_key: RefCell<String>,
    /// The Copy button, which reports into its own title. Filled in by
    /// [`KeyActions::arm`] rather than at construction: it takes *this* object
    /// as its target, so it cannot exist before this does.
    copy: RefCell<Option<Retained<NSButton>>>,
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
        /// This Mac's public key. The label shows it whole, so this only saves a
        /// drag-select — but an exact one, over 50 monospaced characters where
        /// three left behind would fail as a checksum on the gateway.
        #[unsafe(method(copyKey:))]
        fn copy_key(&self, _sender: Option<&AnyObject>) {
            let wrote = crate::pasteboard::write(&self.public_key());
            // Nothing else changes on screen when this works, and a clipboard is
            // not somewhere you can look to check.
            self.say(if wrote { "Copied" } else { "Failed" });
        }

        /// Put the button back, a moment later. See [`KeyActions::say`].
        #[unsafe(method(restoreCopy:))]
        fn restore_copy(&self, _timer: Option<&AnyObject>) {
            if let Some(copy) = self.ivars().copy.borrow().as_ref() {
                copy.setTitle(&NSString::from_str("Copy"));
            }
        }

        /// Take an identity this Mac has held before, or one belonging to the Mac
        /// it is replacing.
        ///
        /// Write-only, and that is the whole shape of it: this dialog accepts a
        /// private key and has no way to show one. The field comes up empty
        /// rather than holding the key in force, so what is typed can only ever
        /// be a replacement — there is nothing here to read back out.
        ///
        /// Validated by kind as well as checksum before it is accepted, so the
        /// mistake this catches is the likely one: `rxap` is this Mac's own
        /// public key, one row up and one Copy away.
        #[unsafe(method(importIdentity:))]
        fn import_identity(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let Some(entered) = prompt(
                mtm,
                "Import an identity",
                "Paste this Mac's private key (rxas…). It replaces the identity below, \
                 and the gateway keeps working only if the key you paste is one it \
                 already knows — that is what importing is for.\n\nNothing is saved \
                 until you press Save.",
                "Import",
            ) else {
                return;
            };
            let private_key = match rxa_proto::key::parse_private(
                rxa_proto::key::Role::Agent,
                &entered,
            ) {
                Ok(_) => entered,
                Err(e) => {
                    error(mtm, "That is not an agent private key", &format!("{e}"));
                    return;
                }
            };
            let public_key =
                rxa_proto::key::public_text_of(rxa_proto::key::Role::Agent, &private_key)
                    .expect("just parsed");
            let ivars = self.ivars();
            *ivars.private_key.borrow_mut() = private_key;
            ivars
                .public_key
                .setStringValue(&NSString::from_str(&public_key));
        }

        /// Mint a new identity for this Mac, after asking.
        ///
        /// Asking, because the cost is not local: every gateway paired with this
        /// Mac stops being able to reach it until its `agent_public_key` is
        /// updated, and nothing on this machine can tell the user that has
        /// happened. The new key lands in this dialog only — Cancel still
        /// abandons it.
        #[unsafe(method(regenerateIdentity:))]
        fn regenerate_identity(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            if !confirm(
                mtm,
                "Replace this Mac's identity?",
                "A fresh keypair is generated and the old one is discarded. The gateway \
                 will refuse this Mac until you paste the new public key into its \
                 `agent_public_key`.\n\nNothing is saved until you press Save.",
                "Replace",
            ) {
                return;
            }
            let ivars = self.ivars();
            let private_key = rxa_proto::key::generate_private(rxa_proto::key::Role::Agent);
            let public_key =
                rxa_proto::key::public_text_of(rxa_proto::key::Role::Agent, &private_key)
                    .expect("a key just generated parses");
            *ivars.private_key.borrow_mut() = private_key;
            ivars
                .public_key
                .setStringValue(&NSString::from_str(&public_key));
        }
    }
);

impl KeyActions {
    fn new(
        mtm: MainThreadMarker,
        public_key: Retained<NSTextField>,
        private_key: &str,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(KeyActionsIvars {
            public_key,
            private_key: RefCell::new(private_key.trim().to_owned()),
            copy: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Hand over the Copy button, once it exists.
    fn arm(&self, copy: Retained<NSButton>) {
        *self.ivars().copy.borrow_mut() = Some(copy);
    }

    /// This Mac's private key as it stands — the original, or whatever a
    /// regenerate replaced it with.
    fn private_key(&self) -> String {
        self.ivars().private_key.borrow().clone()
    }

    /// What the label is showing, which is the public half of the above.
    fn public_key(&self) -> String {
        self.ivars().public_key.stringValue().to_string()
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
        let Some(button) = self.ivars().copy.borrow().clone() else {
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

/// Ask before doing something whose cost lands somewhere else.
///
/// `confirm` rather than `error`'s single button, and the destructive verb is on
/// the button rather than "OK": the sheet is read by someone who has already
/// clicked once, and the second click should say what it does.
///
/// Cancel is the default, so Return does nothing.
fn confirm(mtm: MainThreadMarker, title: &str, body: &str, verb: &str) -> bool {
    let alert = alert(mtm, title, body, NSAlertStyle::Warning);
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.addButtonWithTitle(&NSString::from_str(verb));
    alert.runModal() != NSAlertFirstButtonReturn
}

/// Ask for one value, on its own, and hand back what was typed.
///
/// A second modal over the settings dialog rather than another row in it: what it
/// takes is a *secret*, and the row it would sit in is the one place this dialog
/// deliberately never shows one. A field that appears only when asked for, empty
/// every time, and is gone again on OK cannot be mistaken for a display of the
/// key in force.
///
/// Returns `None` on Cancel, and never an empty string — nothing is a cancel by
/// another name.
fn prompt(mtm: MainThreadMarker, title: &str, body: &str, verb: &str) -> Option<String> {
    let alert = alert(mtm, title, body, NSAlertStyle::Informational);
    let field = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
    field.setFrame(NSRect::new(
        NSPoint::ZERO,
        NSSize::new(WIDTH, ROW_HEIGHT),
    ));
    // Monospaced like the key rows behind it: a key is compared character by
    // character, including one being pasted in.
    field.setFont(NSFont::userFixedPitchFontOfSize(12.0).as_deref());
    alert.setAccessoryView(Some(&field));
    alert.addButtonWithTitle(&NSString::from_str(verb));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    // Or the paste has nowhere to land and the panel looks inert.
    alert.window().setInitialFirstResponder(Some(&field));

    if alert.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    let value = field.stringValue().to_string().trim().to_owned();
    (!value.is_empty()).then_some(value)
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
    use rxa_proto::key::{self, Role};

    fn draft(private_key: String, gateway_public_key: String) -> Draft {
        Draft {
            listen: "0.0.0.0:52381".to_owned(),
            private_key,
            gateway_public_key,
            virtual_display: false,
            virtual_size: "1280x800".to_owned(),
        }
    }

    fn gateway_public_key() -> String {
        key::public_text_of(Role::Gateway, &key::generate_private(Role::Gateway)).unwrap()
    }

    /// What the dialog puts on screen for this Mac. The rest of this module
    /// needs a window server; this part is a string, and it is the part that
    /// could quietly go wrong — showing the *private* key here would put a
    /// secret on screen and on the clipboard behind the Copy button.
    #[test]
    fn the_dialog_shows_the_public_half_and_never_the_private_one() {
        let private_key = key::generate_private(Role::Agent);
        let draft = draft(private_key.clone(), gateway_public_key());
        let shown = draft.public_key();

        assert!(shown.starts_with("rxap"), "{shown}");
        assert_ne!(shown, private_key);
        assert!(!shown.contains(&private_key));
        assert_eq!(
            shown,
            key::public_text_of(Role::Agent, &private_key).unwrap()
        );
    }

    /// A hand-edited config can hold nonsense where a key belongs. The dialog is
    /// how it gets fixed, so it has to open — showing a blank rather than
    /// refusing, or worse, echoing the nonsense back as if it were a key.
    #[test]
    fn a_private_key_that_is_nonsense_shows_as_nothing() {
        for bad in ["", "rxasnope", "hunter2"] {
            let draft = draft(bad.to_owned(), gateway_public_key());
            assert_eq!(draft.public_key(), "", "{bad}");
        }
    }

    /// The warning that says a saved change has not taken effect. Either half of
    /// the pairing moving is the same failure, so both have to count.
    #[test]
    fn a_pairing_that_has_not_taken_effect_is_noticed_either_way() {
        let (private_key, gateway) = (key::generate_private(Role::Agent), gateway_public_key());
        let running = InForce {
            private_key: private_key.clone(),
            gateway_public_key: gateway.clone(),
        };
        assert!(running.matches(&draft(private_key.clone(), gateway.clone())));
        // Whitespace round a pasted value is the user's typing, not a change —
        // `Settings::apply` trims it before it ever reaches the file.
        assert!(running.matches(&draft(private_key.clone(), format!("  {gateway}\n"))));

        // A regenerated identity the gateway has not been told about.
        assert!(!running.matches(&draft(key::generate_private(Role::Agent), gateway)));
        // A newly pasted gateway key.
        assert!(!running.matches(&draft(private_key, gateway_public_key())));
    }
}
