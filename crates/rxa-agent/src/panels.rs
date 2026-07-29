//! The agent's modal panels: everything the GUI needs beyond a menu item.
//!
//! [`config`] is the settings dialog — one panel holding every setting the agent
//! has. [`error`] reports the failures a menu cannot show on its own.
//! [`startup_failure`] is [`error`] from before there is a menu at all, which is
//! the only thing standing between a failed launch and an app that appears to do
//! nothing when opened.
//!
//! Settings is an `NSPanel`, with a title bar and content owned completely here.
//! It used to be an `NSAlert` with an accessory view, but an alert reserves room
//! for its application icon and moves that icon above the title when the
//! accessory is wide. Hiding the image does not recover the reserved room. A
//! normal panel has no alert-icon slot, so the useful explanation and the form
//! both fit without trying to alter AppKit's private alert layout.
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
    NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSFont, NSModalResponseCancel,
    NSModalResponseOK, NSPanel, NSTextAlignment, NSTextField, NSView,
    NSWindowButton, NSWindowStyleMask,
};
use objc2_foundation::{
    NSPoint, NSRange, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString,
    NSTimer,
};

/// Width of the settings form, in points.
///
/// What sets it is the key rows: a 50-character key has to fit on one line in a
/// 12pt monospaced font, because one that scrolls is one somebody copies half of
/// by hand. Menlo 12 advances ~7.23pt per character, so 50 of them need ~361pt
/// plus the field's insets. `540` was the measured minimum for that field alone;
/// this is wider so its Copy button can sit on the same row without shortening
/// the value, making the button's target unambiguous. The resulting 670pt panel
/// still fits the 800pt-wide test Mac.
const WIDTH: f64 = 630.0;

/// The ordinary window chrome is outside this rectangle; these are the panel's
/// content margins and its bottom action row.
const PANEL_MARGIN: f64 = 20.0;
const PANEL_WIDTH: f64 = WIDTH + PANEL_MARGIN * 2.0;
const PANEL_TOP_MARGIN: f64 = 16.0;
const PANEL_BOTTOM_MARGIN: f64 = 12.0;
const PANEL_BUTTON_HEIGHT: f64 = 32.0;
const PANEL_BUTTON_WIDTH: f64 = 90.0;
const PANEL_CONTENT_GAP: f64 = 12.0;

/// One row of the settings dialog: label on the left, control on the right.
///
/// Kept compact for the 1x 800x600 test Mac, where a tall panel can put its
/// actions behind the Dock. Unlike the old alert, the panel spends no vertical
/// space on an application icon.
const ROW_HEIGHT: f64 = 22.0;
const ROW_GAP: f64 = 8.0;
/// Wide enough for the longest caption, "This Mac's public key", which at 490
/// against 126pt rendered as "This Mac's public ke".
const LABEL_WIDTH: f64 = 150.0;
const CONTROL_X: f64 = LABEL_WIDTH + 8.0;
/// A single line of label text, so a caption can be centred against its control
/// rather than sitting at the top of a 24pt frame.
const LABEL_HEIGHT: f64 = 17.0;
/// Between a section's heading and its first row: tighter than [`ROW_GAP`], so
/// the heading reads as belonging to what follows rather than floating between
/// two sections.
const HEADING_GAP: f64 = 3.0;
/// Between the last row of one section and the next heading. Wider than
/// [`ROW_GAP`] by enough to group without needing a rule drawn between them.
const SECTION_GAP: f64 = 14.0;
/// How many rows [`config`] lays out, headings and their explanations included.
const ROWS: usize = 13;
/// Copy belongs to the public value on its row. Import and Regenerate belong to
/// the private identity on the next row, where the text says why no private value
/// is visible. Keeping those two action groups separate is more important than
/// saving one row.
const COPY_WIDTH: f64 = 78.0;
const IMPORT_WIDTH: f64 = 84.0;
const REGENERATE_WIDTH: f64 = 144.0;
const BUTTON_GAP: f64 = 8.0;

/// How much of the read-only fill is let through, over the panel behind it.
///
/// The fill is a semantic grey thinned with alpha rather than a lighter named
/// colour, and the alpha is the reason: it is one number to turn when the shade
/// is wrong, and it stays correct in dark mode — where the panel behind is dark
/// and a hardcoded light grey would be a bright band across it. At `1.0` the grey
/// read heavier than the row needed.
const READ_ONLY_FILL_ALPHA: f64 = 0.4;

/// The fill behind a value that can be read and copied but not typed into.
fn read_only_fill() -> Retained<NSColor> {
    NSColor::unemphasizedSelectedContentBackgroundColor()
        .colorWithAlphaComponent(READ_ONLY_FILL_ALPHA)
}

/// What a bezel insets its text by, and a bare border does not.
///
/// Measured against the bezeled gateway field on the row below, which is the
/// alignment this one has to match.
const BEZEL_INSET: f64 = 3.0;

/// The line that distinguishes the agent-made display from a physical one.
///
/// Its suffix is the detail shared with the viewer's Display menu. Matching the
/// whole label and separator keeps a capture error mentioning a virtual display
/// from being mistaken for the display itself.
const VIRTUAL_DISPLAY_PREFIX: &str = "Virtual display — ";
const PENDING_VIRTUAL_DISPLAY: &str = "Virtual display — added after Save";

fn is_virtual_display(line: &str) -> bool {
    line.starts_with(VIRTUAL_DISPLAY_PREFIX)
}

/// What the viewer's Display menu will contain after this draft is saved.
fn display_list_after_save(displays: &[String], virtual_display: bool) -> String {
    let has_virtual = displays.iter().any(|line| is_virtual_display(line));
    let mut saved: Vec<String> = displays
        .iter()
        .filter(|line| virtual_display || !is_virtual_display(line))
        .cloned()
        .collect();
    if virtual_display && !has_virtual {
        saved.push(PENDING_VIRTUAL_DISPLAY.to_owned());
    }
    saved.join("\n")
}

/// Reserve the largest number of lines the saved list can need while the checkbox
/// is toggled, so the panel and everything below the list stay still.
fn display_list_rows(displays: &[String]) -> usize {
    displays.len() + usize::from(!displays.iter().any(|line| is_virtual_display(line)))
}

/// Every setting the agent has, as the dialog reads and writes it.
///
/// A plain struct rather than [`crate::config::Config`] because a draft is
/// allowed to be nonsense — it is whatever the user has typed, and the caller
/// validates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub listen: String,
    /// This Mac's private key. Carried through the dialog but never shown in it
    /// and never typed into a persistent field: only **Import…** and
    /// **Regenerate identity** change it. What the dialog *displays* is the
    /// public half derived from it, which is not a secret.
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
    let (network_copy, network_copy_height) = section_copy(
        mtm,
        "Connections arrive at this address. Saving any setting restarts the agent \
         and drops the current connection; the gateway reconnects on its own.",
    );
    let (displays_copy, displays_copy_height) = section_copy(
        mtm,
        "The viewer's Display menu chooses which screen a session shows. The list \
         below shows that menu after Save: physical displays always remain; \
         clearing Add a virtual display removes Virtual display.",
    );
    let mut pairing_copy = String::from(
        "Pairing uses two public keys, one in each direction; neither is secret. \
         Copy this Mac's key to `agent_public_key` on the gateway, then paste the \
         gateway key printed by `remotex rxa-pubkey` below.",
    );
    if current.gateway_public_key.trim().is_empty() {
        pairing_copy
            .push_str("\n\n⚠︎ Unpaired: no gateway key is set, so every connection is refused.");
    }
    // Said out loud rather than left in a tooltip, because the difference
    // between the saved pairing and the running one is the difference between a
    // gateway that connects and one that is refused. Saving re-execs into the
    // new values, so they normally agree; they differ only when that did not
    // happen — a re-exec that failed, or a file edited by hand.
    if !in_force.matches(current) {
        pairing_copy.push_str(
            "\n\n⚠︎ The keys below are the ones in the config file, and they never took \
             effect: this agent is still using the previous pairing. Quit remotex-agent \
             and open it again to start using these.",
        );
    }
    let (pairing_copy, pairing_copy_height) = section_copy(mtm, &pairing_copy);

    // Three sections, because the settings answer three unrelated questions —
    // where to listen, what to share, and who may connect — and read as one
    // undifferentiated column of eight rows otherwise. Each heading is followed
    // by enough plain-language copy to explain the decision its controls make;
    // the tooltips retain the details for individual fields.
    //
    // Every row's height and the gap that follows it, in order. Only the display
    // list is taller than one row, and only because it grows with the number of
    // screens attached.
    let list_height = (display_list_rows(displays).max(1) as f64) * LABEL_HEIGHT;
    let rows: [(f64, f64); ROWS] = [
        // Network
        (LABEL_HEIGHT, HEADING_GAP),
        (network_copy_height, ROW_GAP),
        (ROW_HEIGHT, SECTION_GAP), // listen address
        // Displays
        (LABEL_HEIGHT, HEADING_GAP),
        (displays_copy_height, ROW_GAP),
        (ROW_HEIGHT, ROW_GAP),     // add a virtual display
        (ROW_HEIGHT, ROW_GAP),     // its initial size
        (list_height, SECTION_GAP), // resulting viewer menu
        // Pairing
        (LABEL_HEIGHT, HEADING_GAP),
        (pairing_copy_height, ROW_GAP),
        (ROW_HEIGHT, ROW_GAP), // this Mac's public key and Copy
        (ROW_HEIGHT, ROW_GAP), // its private key actions
        (ROW_HEIGHT, 0.0),     // the gateway's public key
    ];
    let height: f64 = rows.iter().map(|(row, gap)| row + gap).sum();
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(WIDTH, height)),
    );
    // AppKit's origin is bottom-left, so rows are laid out upwards and named
    // downwards: the order above is the order on screen, top to bottom.
    let mut tops = [0.0; ROWS];
    let mut cursor = height;
    for (top, (row_height, gap)) in tops.iter_mut().zip(rows) {
        cursor -= row_height;
        *top = cursor;
        cursor -= gap;
    }
    let row = |n: usize| tops[n];

    view.addSubview(&heading(mtm, "Network", row(0)));
    network_copy.setFrame(NSRect::new(
        NSPoint::new(0.0, row(1)),
        NSSize::new(WIDTH, network_copy_height),
    ));
    view.addSubview(&network_copy);
    view.addSubview(&label(mtm, "Listen address", row(2)));
    let listen = field(mtm, &current.listen, row(2), WIDTH - CONTROL_X, false);
    view.addSubview(&listen);

    view.addSubview(&heading(mtm, "Displays", row(3)));
    displays_copy.setFrame(NSRect::new(
        NSPoint::new(0.0, row(4)),
        NSSize::new(WIDTH, displays_copy_height),
    ));
    view.addSubview(&displays_copy);
    // "Add a virtual display", not "a private 2x display": the density is not a
    // property of the setting — a client reports the screen it is on and the
    // display matches it, 1x or 2x — so naming a number here promised something
    // this checkbox does not decide. "Virtual" is also the word the list below,
    // both clients' display menus and this Mac's own System Settings use for it.
    let virtual_display = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Add a virtual display"),
            None,
            None,
            mtm,
        )
    };
    virtual_display.setFrame(NSRect::new(
        NSPoint::new(CONTROL_X, row(5)),
        NSSize::new(WIDTH - CONTROL_X, ROW_HEIGHT),
    ));
    virtual_display.setState(if current.virtual_display {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    virtual_display.setToolTip(Some(&NSString::from_str(
        "Give this Mac an extra display that nobody is sitting in front of. It \
         joins the list below — the Mac's own screens stay shareable — and it is \
         the only display a client can ask to resize.",
    )));
    view.addSubview(&virtual_display);

    // Greyed while the box above is clear, and enabled the moment it is ticked —
    // which reverses an earlier call that a field changing state under the cursor
    // was more startling than one whose value simply did not apply. It reads as a
    // second, unrelated setting when it is live with no display to be the size
    // of, and "Initial size" gave nothing away about which display it meant.
    //
    // "Its" ties it to the checkbox rather than repeating "virtual display" in a
    // 150pt label, and "initial" stays because it is load-bearing: this is the
    // size the display is *created* at, and macOS remembers whatever it is
    // changed to afterwards.
    view.addSubview(&label(mtm, "Its initial size", row(6)));
    let virtual_size = field(mtm, &current.virtual_size, row(6), WIDTH - CONTROL_X, false);
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

    // Read-only, and that is the design rather than a shortcut: which display a
    // session shares is picked in the viewer or the browser, by whoever is
    // looking at it, and can change several times while this dialog is closed.
    // This final row shows the choices after the two virtual-display controls
    // above are saved.
    //
    // The Mac's own screens are numbered "Display 1", "Display 2"; the one this
    // agent made is named "Virtual display" — which is the distinction the
    // checkbox decides, and which this list used to hide by numbering it in with
    // the rest (see `menubar::display_summary`).
    view.addSubview(&label(mtm, "Displays after Save", row(7)));
    let list = NSTextField::labelWithString(
        &NSString::from_str(&display_list_after_save(displays, current.virtual_display)),
        mtm,
    );
    list.setFrame(NSRect::new(
        NSPoint::new(CONTROL_X, row(7)),
        NSSize::new(WIDTH - CONTROL_X, list_height),
    ));
    list.setToolTip(Some(&NSString::from_str(
        "The choices the viewer's Display menu will show after Save. Clearing \
         Add a virtual display removes its line immediately; physical displays \
         remain available.",
    )));
    view.addSubview(&list);

    // Wired after both exist: the checkbox's target is this object, so it cannot
    // be handed to the checkbox at construction. Sets the field's state to match
    // the box as it stands, so the dialog opens consistent rather than becoming
    // so on the first click.
    let virtual_controls = VirtualDisplayControls::new(
        mtm,
        virtual_size.clone(),
        list.clone(),
        displays.to_vec(),
    );
    unsafe {
        virtual_display.setTarget(Some(&virtual_controls.as_object()));
        virtual_display.setAction(Some(sel!(virtualDisplayToggled:)));
    }
    virtual_controls.arm(virtual_display.clone());

    view.addSubview(&heading(mtm, "Pairing", row(8)));
    pairing_copy.setFrame(NSRect::new(
        NSPoint::new(0.0, row(9)),
        NSSize::new(WIDTH, pairing_copy_height),
    ));
    view.addSubview(&pairing_copy);
    // Shown whole in a read-only field: this is derived from the private key and
    // there is nothing to type into it. Whole because it is not a secret — which
    // is the entire reason the protocol stopped using a shared one — and because
    // reading it against the gateway's copy is the common visit to this dialog.
    // One click selects all 50 characters, and Copy is on this same row as the
    // exact one-step route to the pasteboard.
    view.addSubview(&label(mtm, "This Mac's public key", row(10)));
    let copy_x = WIDTH - COPY_WIDTH;
    let public_key = SelectAllField::new(
        mtm,
        &current.public_key(),
        NSRect::new(
            NSPoint::new(CONTROL_X, row(10)),
            NSSize::new(copy_x - BUTTON_GAP - CONTROL_X, ROW_HEIGHT),
        ),
    );
    public_key.setToolTip(Some(&NSString::from_str(
        "Paste this as `agent_public_key` on the matching [[targets]] entry in the \
         gateway's remotex.toml. Not a secret — the private key it comes from never \
         leaves this Mac. Click once to select the entire key.",
    )));
    view.addSubview(&public_key);

    // An ordinary editable field: this one is pasted in, and it is not a secret
    // either, so nothing about it wants hiding or locking. It follows both of
    // this Mac's rows, so neither row's buttons can read as actions on it.
    view.addSubview(&label(mtm, "Gateway public key", row(12)));
    let gateway = field(
        mtm,
        &current.gateway_public_key,
        row(12),
        WIDTH - CONTROL_X,
        true,
    );
    gateway.setToolTip(Some(&NSString::from_str(
        "The one gateway this Mac answers, from `remotex rxa-pubkey` on that server. \
         Leave it empty to unpair, which makes this agent refuse every connection.",
    )));
    view.addSubview(&gateway);

    // Owns all three buttons' actions and the private key behind the fields.
    // `buttonWithTitle:target:action:` holds its target weakly, so this has to
    // outlive `runModalForWindow` below — which is why it is a named local and
    // not a temporary.
    let actions = KeyActions::new(mtm, public_key.clone(), &current.private_key);
    // Copy sits beside the public value. The two actions that consume or replace
    // private key material get their own explicitly labelled row.
    let regenerate_x = WIDTH - REGENERATE_WIDTH;
    let import_x = regenerate_x - BUTTON_GAP - IMPORT_WIDTH;

    let copy = button(mtm, "Copy", &actions, sel!(copyKey:), copy_x, row(10), COPY_WIDTH);
    copy.setToolTip(Some(&NSString::from_str(
        "Put this Mac's public key on the clipboard, to paste as `agent_public_key` on \
         the gateway's rxa target.",
    )));
    view.addSubview(&copy);

    view.addSubview(&label(mtm, "This Mac's private key", row(11)));
    let private_key_status =
        NSTextField::labelWithString(&NSString::from_str("Stored on this Mac; never shown"), mtm);
    private_key_status.setFrame(NSRect::new(
        NSPoint::new(CONTROL_X, row(11) + (ROW_HEIGHT - LABEL_HEIGHT) / 2.0),
        NSSize::new(import_x - BUTTON_GAP - CONTROL_X, LABEL_HEIGHT),
    ));
    private_key_status.setToolTip(Some(&NSString::from_str(
        "The private half of this Mac's identity stays on this Mac. Import replaces \
         it with an existing private key; Regenerate identity creates a new one.",
    )));
    view.addSubview(&private_key_status);

    let import = button(
        mtm,
        "Import…",
        &actions,
        sel!(importIdentity:),
        import_x,
        row(11),
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
        row(11),
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

    // Settings owns a normal panel instead of borrowing the rigid layout of an
    // alert. In particular there is no application-icon view, hidden or
    // otherwise, and therefore no empty icon slot above this wide form.
    let button_y = PANEL_BOTTOM_MARGIN;
    let form_y = button_y + PANEL_BUTTON_HEIGHT + PANEL_CONTENT_GAP;
    let panel_height = form_y + height + PANEL_TOP_MARGIN;
    let content = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(PANEL_WIDTH, panel_height)),
    );
    view.setFrameOrigin(NSPoint::new(PANEL_MARGIN, form_y));
    content.addSubview(&view);

    let modal = SettingsModal::new(mtm);
    let save_x = PANEL_WIDTH - PANEL_MARGIN - PANEL_BUTTON_WIDTH;
    let cancel_x = save_x - BUTTON_GAP - PANEL_BUTTON_WIDTH;
    let save = modal_button(
        mtm,
        "Save",
        &modal,
        sel!(saveSettings:),
        save_x,
        button_y,
        "\r",
    );
    let cancel = modal_button(
        mtm,
        "Cancel",
        &modal,
        sel!(cancelSettings:),
        cancel_x,
        button_y,
        "\u{1b}",
    );
    content.addSubview(&save);
    content.addSubview(&cancel);

    activate(mtm);
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(PANEL_WIDTH, panel_height)),
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setTitle(&NSString::from_str("remotex-agent Settings"));
    panel.setContentView(Some(&content));
    // A close is a cancellation, including while the panel is inside AppKit's
    // modal loop. Replacing the standard close button's action avoids a closed
    // window leaving that loop alive with nothing on screen.
    if let Some(close) = panel.standardWindowButton(NSWindowButton::CloseButton) {
        unsafe {
            close.setTarget(Some(&modal.as_object()));
            close.setAction(Some(sel!(cancelSettings:)));
        }
    }
    // Without this nothing is focused and the first keystroke goes nowhere, which
    // reads as a dialog that has ignored you. The gateway field when there is no
    // pairing yet: on a fresh Mac that is the one thing left to do.
    let first = if current.gateway_public_key.trim().is_empty() {
        &gateway
    } else {
        &listen
    };
    panel.setInitialFirstResponder(Some(first));
    panel.center();
    panel.makeKeyAndOrderFront(None);

    let response = NSApplication::sharedApplication(mtm).runModalForWindow(&panel);
    panel.orderOut(None);
    if response != NSModalResponseOK {
        return None;
    }
    Some(Draft {
        listen: listen.stringValue().to_string().trim().to_owned(),
        // From the actions, not the field: the field shows the *public* half,
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
    ///
    /// Trimmed on both sides. The draft's is whatever was typed, and the running
    /// side is the config file as parsed — which `Config::validate` accepts with
    /// spaces inside the quotes, because `key::parse_private` trims before it
    /// looks. Comparing a padded key in force against a trimmed draft of the same
    /// key reported a pairing that "never took effect" while the agent was using
    /// exactly it.
    fn matches(&self, draft: &Draft) -> bool {
        self.private_key.trim() == draft.private_key.trim()
            && self.gateway_public_key.trim() == draft.gateway_public_key.trim()
    }
}

/// Report a failure while the menu bar remains in its degraded state.
///
/// Same panel as [`error`], with the activation policy kept explicit so this
/// helper remains safe for any startup call site. [`crate::menubar::Starting`]
/// has already created the status item and set the ordinary GUI launch policy.
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

/// Ends the settings panel's modal loop with the action the user chose.
///
/// Save, Cancel, Return, Escape and the title-bar close button all meet here, so
/// there is one answer from every normal way to dismiss a window.
struct SettingsModalIvars;

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `SettingsModal` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RxaSettingsModal"]
    #[ivars = SettingsModalIvars]
    struct SettingsModal;

    unsafe impl NSObjectProtocol for SettingsModal {}

    impl SettingsModal {
        #[unsafe(method(saveSettings:))]
        fn save_settings(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(MainThreadMarker::from(self))
                .stopModalWithCode(NSModalResponseOK);
        }

        #[unsafe(method(cancelSettings:))]
        fn cancel_settings(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(MainThreadMarker::from(self))
                .stopModalWithCode(NSModalResponseCancel);
        }
    }
);

impl SettingsModal {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SettingsModalIvars);
        unsafe { msg_send![super(this), init] }
    }

    fn as_object(&self) -> Retained<AnyObject> {
        let this: Retained<Self> = self.retain();
        // Safety: upcasting a subclass of NSObject to AnyObject.
        unsafe { Retained::cast_unchecked(this) }
    }
}

/// A public key that looks and behaves like the read-only value it is.
///
/// A selectable label makes copying possible but makes the common operation a
/// careful drag across 50 characters. A click here gives the field focus and
/// selects the whole value, ready for Command-C; typing cannot change it.
struct SelectAllFieldIvars;

define_class!(
    // SAFETY:
    // - NSTextField supports subclassing.
    // - `SelectAllField` does not implement `Drop`.
    #[unsafe(super(NSTextField))]
    #[thread_kind = MainThreadOnly]
    #[name = "RxaSelectAllField"]
    #[ivars = SelectAllFieldIvars]
    struct SelectAllField;

    unsafe impl NSObjectProtocol for SelectAllField {}

    impl SelectAllField {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &AnyObject) {
            // Do not let NSTextField perform its ordinary word selection first:
            // the hyphens in a key are word boundaries. Start its field editor,
            // then pin that editor's selection to the entire NSString range.
            // SAFETY: `self` is the control being selected, and AppKit accepts a
            // nil sender for this standard action.
            unsafe { self.selectText(None) };
            if let Some(editor) = self.currentEditor() {
                let whole_value = NSRange::new(0, self.stringValue().length());
                editor.setSelectedRange(whole_value);
                editor.scrollRangeToVisible(whole_value);
            }
        }
    }
);

impl SelectAllField {
    fn new(mtm: MainThreadMarker, value: &str, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SelectAllFieldIvars);
        // Inset on the horizontal only. A bezel's own inset is what puts its text
        // where the neighbouring rows have theirs; a bordered field has none, so
        // without this the key sits hard against its border and ~3pt left of the
        // gateway key below.
        let frame = NSRect::new(
            NSPoint::new(frame.origin.x + BEZEL_INSET, frame.origin.y),
            NSSize::new(frame.size.width - BEZEL_INSET, frame.size.height),
        );
        let this: Retained<Self> =
            unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setStringValue(&NSString::from_str(value));
        this.setFont(NSFont::userFixedPitchFontOfSize(12.0).as_deref());
        this.setEditable(false);
        this.setSelectable(true);
        // The fill is the whole of how this reads as a value rather than a field.
        // `initWithFrame:` gives an NSTextField a bezel and `textBackgroundColor` —
        // white, the colour of somewhere to type — so it was indistinguishable
        // from the gateway field below, which *is* typed into.
        //
        // A bezeled NSTextField ignores `backgroundColor` — measured both ways on
        // the test Mac: bezel on, an obvious grey drew white; bezel off, the colour
        // appears. So a custom fill means no bezel, and the bezel's two other jobs
        // have to be done by hand:
        //
        // - **the border**, or the value has nothing to sit in. `setBordered` draws
        //   a plain rectangle where the bezel drew a rounded, shaded one.
        // - **the horizontal inset**. A bezel insets its text; a bare border does
        //   not, so the key started left of the gateway key below it — the
        //   misalignment. The frame is inset by [`BEZEL_INSET`] to put the text back
        //   in the column the bezeled rows have it in. Vertically the two agree
        //   without help, so nothing is done to the height.
        this.setBezeled(false);
        this.setBordered(true);
        this.setDrawsBackground(true);
        this.setBackgroundColor(Some(&read_only_fill()));
        this
    }
}

/// How long Copy says it copied.
const COPIED_FOR: f64 = 1.2;

/// The key buttons' target: copy this Mac's public key, or replace the identity
/// behind it — and the keeper of the private key, which nothing on screen shows.
///
/// A whole class for three buttons, because an `NSButton` action has to be a
/// selector on an Objective-C object — there is nowhere to hang a Rust closure.
struct KeyActionsIvars {
    /// The read-only field showing the public key, updated by a regenerate.
    public_key: Retained<SelectAllField>,
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
        /// This Mac's public key. The field shows it whole and selects it on a
        /// click; this button is the exact one-step path to the pasteboard.
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
        public_key: Retained<SelectAllField>,
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

    /// What the read-only field is showing, which is the public half of the above.
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

/// Keeps both consequences of the virtual-display checkbox visible.
///
/// A whole class for one checkbox, for the same reason [`KeyActions`] is one: an
/// `NSButton` action has to be a selector on an Objective-C object.
struct VirtualDisplayControlsIvars {
    size: Retained<NSTextField>,
    /// The display list after Save.
    list: Retained<NSTextField>,
    /// The displays present when the panel opened. The checkbox filters or adds
    /// the virtual line without inventing new details for the physical ones.
    displays: Vec<String>,
    /// The checkbox, so its state is read through `NSButton::state` rather than a
    /// `msg_send` at the sender. Filled in by [`VirtualDisplayControls::arm`]
    /// rather than at construction: the checkbox takes *this* object as its
    /// target, so it cannot exist before this does — the same shape as
    /// [`KeyActions::arm`].
    checkbox: RefCell<Option<Retained<NSButton>>>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `VirtualDisplayControls` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    // AppKit sends the action on the main thread, and the UI ivars are
    // main-thread-only objects.
    #[thread_kind = MainThreadOnly]
    #[name = "RxaVirtualDisplayControls"]
    #[ivars = VirtualDisplayControlsIvars]
    struct VirtualDisplayControls;

    unsafe impl NSObjectProtocol for VirtualDisplayControls {}

    impl VirtualDisplayControls {
        /// The checkbox moved; both the size field and display list follow it.
        #[unsafe(method(virtualDisplayToggled:))]
        fn virtual_display_toggled(&self, _sender: Option<&AnyObject>) {
            self.follow();
        }
    }
);

impl VirtualDisplayControls {
    fn new(
        mtm: MainThreadMarker,
        size: Retained<NSTextField>,
        list: Retained<NSTextField>,
        displays: Vec<String>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(VirtualDisplayControlsIvars {
            size,
            list,
            displays,
            checkbox: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Hand over the checkbox, and match both dependent controls to it as it
    /// stands — so the dialog opens consistent rather than becoming so on the
    /// first click.
    fn arm(&self, checkbox: Retained<NSButton>) {
        *self.ivars().checkbox.borrow_mut() = Some(checkbox);
        self.follow();
    }

    /// Match the size field and saved display list to the checkbox.
    fn follow(&self) {
        let on = self
            .ivars()
            .checkbox
            .borrow()
            .as_ref()
            .is_some_and(|checkbox| checkbox.state() == NSControlStateValueOn);
        self.enable(on);
        let ivars = self.ivars();
        ivars
            .list
            .setStringValue(&NSString::from_str(&display_list_after_save(&ivars.displays, on)));
    }

    /// Editable *and* selectable, or a disabled field's contents cannot even be
    /// read — and the size it is showing is worth reading while deciding whether
    /// to switch the display on.
    fn enable(&self, on: bool) {
        let size = &self.ivars().size;
        size.setEnabled(on);
        size.setSelectable(true);
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
    // Exactly the row, like every other control here: nothing hangs outside the
    // settings form's bounds.
    button.setFrame(NSRect::new(
        NSPoint::new(x, y),
        NSSize::new(width, ROW_HEIGHT),
    ));
    button
}

/// Save or Cancel at the bottom of the settings panel.
fn modal_button(
    mtm: MainThreadMarker,
    title: &str,
    target: &SettingsModal,
    action: objc2::runtime::Sel,
    x: f64,
    y: f64,
    key_equivalent: &str,
) -> Retained<NSButton> {
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(&target.as_object()),
            Some(action),
            mtm,
        )
    };
    button.setFrame(NSRect::new(
        NSPoint::new(x, y),
        NSSize::new(PANEL_BUTTON_WIDTH, PANEL_BUTTON_HEIGHT),
    ));
    button.setKeyEquivalent(&NSString::from_str(key_equivalent));
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

/// Explanatory copy directly below one section heading.
fn section_copy(mtm: MainThreadMarker, text: &str) -> (Retained<NSTextField>, f64) {
    let copy = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    copy.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    copy.setPreferredMaxLayoutWidth(WIDTH);
    let height = copy
        .cell()
        .map(|cell| {
            cell.cellSizeForBounds(NSRect::new(
                NSPoint::ZERO,
                NSSize::new(WIDTH, 10_000.0),
            ))
            .height
            .ceil()
        })
        .unwrap_or(LABEL_HEIGHT);
    (copy, height)
}

/// A section heading: bold, and starting at the left edge rather than in the
/// label column.
///
/// Outdented deliberately. A heading right-aligned into [`LABEL_WIDTH`] with the
/// captions would read as one more caption belonging to the row beside it; at
/// x = 0 it spans the whole width and the rows under it are visibly indented from
/// it.
fn heading(mtm: MainThreadMarker, text: &str, y: f64) -> Retained<NSTextField> {
    let heading = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    heading.setFont(Some(&NSFont::boldSystemFontOfSize(12.0)));
    heading.setFrame(NSRect::new(
        NSPoint::new(0.0, y),
        NSSize::new(WIDTH, LABEL_HEIGHT),
    ));
    heading
}

/// A settings row's caption: right-aligned against its control, and centred on
/// it.
///
/// Its frame is one line tall and offset into the row, not the row's full height:
/// a label draws its text at the top of whatever frame it is given, so a 24pt one
/// would sit visibly above the field it names.
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

    /// The list shows the saved setting, not a stale report of what exists behind
    /// the checkbox. Physical displays never move; the virtual line follows the
    /// box in both directions.
    #[test]
    fn the_virtual_display_checkbox_updates_the_saved_display_list() {
        let physical = "Display 1 — 1440×900 at 2x".to_owned();
        let virtual_display = "Virtual display — 1280×800 at 2x".to_owned();
        let existing = vec![physical.clone(), virtual_display.clone()];

        assert_eq!(
            display_list_after_save(&existing, true),
            format!("{physical}\n{virtual_display}")
        );
        assert_eq!(display_list_after_save(&existing, false), physical);
        assert_eq!(display_list_rows(&existing), 2);

        let physical_only = vec!["Display 1 — 1440×900 at 2x".to_owned()];
        assert_eq!(
            display_list_after_save(&physical_only, true),
            format!("{}\n{PENDING_VIRTUAL_DISPLAY}", physical_only[0])
        );
        assert_eq!(display_list_after_save(&physical_only, false), physical_only[0]);
        assert_eq!(display_list_rows(&physical_only), 2);

        // Only an actual menu row is filtered, not an error that happens to
        // mention the same kind of display.
        let error = vec!["Cannot list displays — Virtual display is unavailable".to_owned()];
        assert_eq!(display_list_after_save(&error, false), error[0]);
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

        // And the same on the *running* side, which is not written by the GUI:
        // it is the config file as parsed, and `Config::validate` accepts a
        // padded key because `key::parse_private` trims before it looks. A
        // hand-edited file with spaces inside the quotes is therefore in force,
        // and warning that it "never took effect" would be a lie about the one
        // thing this warning exists to report.
        let padded = InForce {
            private_key: format!("  {private_key}\n"),
            gateway_public_key: format!(" {gateway} "),
        };
        assert!(padded.matches(&draft(private_key.clone(), gateway.clone())));

        // A regenerated identity the gateway has not been told about.
        assert!(!running.matches(&draft(key::generate_private(Role::Agent), gateway)));
        // A newly pasted gateway key.
        assert!(!running.matches(&draft(private_key, gateway_public_key())));
    }
}
