//! The menu bar item: the only interface the agent has.
//!
//! Without it the agent is completely invisible. Nothing says whether it is
//! running, nothing says when somebody is looking at your screen, and stopping
//! it means finding the process from a terminal — which is a poor deal for
//! software whose entire job is to let a remote machine watch and drive this
//! one. So the status item answers three questions at a glance:
//!
//! 1. **Is it running, and can it work?** The icon is there or it is not, and it
//!    warns when a permission it cannot do without is missing.
//! 2. **Is anyone connected?** The icon changes, and the first menu line names
//!    the peer.
//! 3. **How do I stop it?** Quit, which really quits — see below.
//!
//! ## Everything is here, because there is nowhere else
//!
//! This menu is the agent's whole interface — the CLI is three launch flags and
//! no operations at all. It copies the pre-shared key, opens the one settings
//! dialog, reveals the config, opens the log, offers the Privacy panes when a
//! grant is missing, toggles the login item, and quits. The panels live in
//! [`crate::panels`]; what a saved change means lives in [`crate::settings`].
//!
//! ## Permissions are health, not settings
//!
//! Screen Recording and Accessibility are not options with a checkbox each: the
//! agent is useless without either. So they are reported by the icon and given a
//! menu row *only* while one is missing — see [`Permissions`] and [`Health`]. The
//! native permission requests happen at startup in [`crate::report_permissions`].
//!
//! They are also read on different schedules, because they *behave* differently:
//! Accessibility applies the instant it is granted, so it is polled until it is.
//! Screen Recording only reaches a fresh launch, so its effective state is fixed
//! at startup; a non-polling recheck when the menu opens can offer the required
//! quit and reopen without falsely reporting the current process healthy.
//!
//! ## Quit has to defeat launchd
//!
//! The embedded LaunchAgent plist sets `KeepAlive` to `SuccessfulExit: false`
//! rather than a plain `true`, precisely so this menu can work: a clean exit
//! stays exited, while a crash is still restarted. With `KeepAlive: true` the
//! Quit item would be a lie — launchd would bring the agent straight back.
//!
//! ## Why the main thread ends up here
//!
//! AppKit is main-thread-only, and so is the `NSCursor` the agent reads the
//! pointer shape from (see [`crate::cursor`]). Running an `NSApplication` needs
//! that same thread, so the run loop owns it and the cursor poll is an `NSTimer`
//! on that loop. The timer is added in `NSRunLoopCommonModes` so the pointer
//! keeps updating while the menu is open — in the default mode alone it would
//! stall for as long as the user held the menu down.

use std::cell::{Cell, OnceCell};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use log::{info, warn};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, Message as _, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSControlStateValue, NSControlStateValueOff,
    NSControlStateValueOn, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength, NSWorkspace,
};
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer, NSURL};

use crate::{capture, config, cursor, input, loginitem, panels, pasteboard, settings, state};

/// How often the run loop re-reads the system cursor and refreshes the icon.
///
/// 100ms, because the pointer shape has to keep up with the mouse crossing a
/// window edge.
const TICK: f64 = 0.1;

/// Re-read Accessibility every tenth tick, so once a second, until it is granted.
///
/// It has to be polled at all because it is granted in System Settings, outside
/// this process, with no notification to subscribe to. It does not have to be
/// polled ten times a second, and it does not have to be polled once it is on —
/// see [`Controller::refresh_accessibility`].
const PERMISSION_EVERY: u32 = 10;

/// The status item: blocked, idle, and with a gateway attached.
///
/// Three different symbols rather than one symbol in three colours: menu bar
/// icons are template images that follow the menu bar's own tint, so colour is
/// not a channel that survives. Shape is.
const ICON_BLOCKED: &str = "exclamationmark.triangle.fill";
const ICON_IDLE: &str = "display";
const ICON_CONNECTED: &str = "eye.fill";

/// If SF Symbols ever fails us, the item still has to be clickable — an empty
/// button is an invisible one, and then Quit is unreachable again.
const ICON_FALLBACK_BLOCKED: &str = "rxa!";
const ICON_FALLBACK_IDLE: &str = "rxa";
const ICON_FALLBACK_CONNECTED: &str = "rxa*";

/// Deep links into the two Privacy panes. There is no API to grant these, and
/// finding them by hand is four levels down a settings tree.
const URL_SCREEN_RECORDING: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const URL_ACCESSIBILITY: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const URL_LOGIN_ITEMS: &str = "x-apple.systempreferences:com.apple.LoginItems-Settings.extension";

/// Whether Screen Recording is effective for this process launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenRecording {
    Missing,
    /// TCC is enabled, but this process was launched before the grant.
    RelaunchRequired,
    Granted,
}

impl ScreenRecording {
    fn at_launch(granted: bool) -> Self {
        if granted { Self::Granted } else { Self::Missing }
    }

    /// Observe TCC without treating a post-launch grant as effective.
    fn observe(self, granted: bool) -> Self {
        match (self, granted) {
            (Self::Granted, true) => Self::Granted,
            (Self::Missing | Self::RelaunchRequired, true) => Self::RelaunchRequired,
            (_, false) => Self::Missing,
        }
    }

    fn effective(self) -> bool {
        self == Self::Granted
    }
}

/// The two TCC grants, as they stand for *this* run of the agent.
///
/// Neither is optional — without Screen Recording the screen never paints, and
/// without Accessibility every click and keystroke is silently dropped — so they
/// are not settings with a checkbox each. They are a health state, which the icon
/// reports and a panel offers to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Permissions {
    screen: ScreenRecording,
    accessibility: bool,
}

impl Permissions {
    fn read(screen_recording_at_launch: bool) -> Self {
        Self {
            screen: ScreenRecording::at_launch(screen_recording_at_launch),
            accessibility: input::accessibility_granted(),
        }
    }

    fn complete(self) -> bool {
        self.screen.effective() && self.accessibility
    }
}

/// What the status item is saying, which is the only thing its icon has to
/// distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    /// A permission is missing: nothing will work, whoever connects.
    Blocked,
    Connected,
    Idle,
}

struct Ivars {
    state: Arc<state::AgentState>,
    tracker: Arc<cursor::Tracker>,
    /// The config, and the only thing allowed to change it.
    settings: Arc<settings::Settings>,
    log_path: Option<PathBuf>,
    /// Set once, immediately after the status item exists — it cannot go in the
    /// initial ivars because the item's menu delegate is this very object.
    status_item: OnceCell<Retained<NSStatusItem>>,
    /// What the icon currently shows, so a 10Hz tick is not 10 image swaps a
    /// second. `None` until the first refresh paints it.
    icon: Cell<Option<Health>>,
    /// Ticks since launch, for [`PERMISSION_EVERY`].
    ticks: Cell<u32>,
    /// The last permissions read, shared between the icon and the menu.
    permissions: Cell<Permissions>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `Controller` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    // AppKit calls every one of these methods on the main thread, and the ivars
    // hold main-thread-only objects.
    #[thread_kind = MainThreadOnly]
    #[name = "RxaMenuBarController"]
    #[ivars = Ivars]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSMenuDelegate for Controller {
        // The menu is rebuilt on open rather than kept in sync, so everything in
        // it is read at the moment it is displayed. Permission state and login
        // item registration both change *outside* this process — in System
        // Settings — with no notification we could subscribe to, so anything
        // cached would be stale exactly when the user went to look at it.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.rebuild(menu);
        }
    }

    impl Controller {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            self.ivars().tracker.poll();

            let ticks = self.ivars().ticks.get();
            self.ivars().ticks.set(ticks.wrapping_add(1));
            if ticks.is_multiple_of(PERMISSION_EVERY) {
                self.refresh_accessibility();
            }

            self.refresh_icon();
        }

        /// The settings dialog: every setting the agent has, in one panel.
        ///
        /// Loops on a rejected draft, re-opening the dialog on exactly what was
        /// typed — a mistyped port must not cost the user the key they pasted in
        /// the same visit.
        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let settings = &self.ivars().settings;
            let saved = settings.saved();
            let choices = display_choices(saved.display);
            let mut draft = panels::Draft {
                listen: saved.listen,
                psk: saved.psk,
                display: saved.display,
            };

            loop {
                let Some(edited) = panels::config(mtm, &draft, &choices) else {
                    return;
                };
                let next = config::Config {
                    listen: edited.listen.clone(),
                    psk: edited.psk.clone(),
                    display: edited.display,
                };
                match settings.apply(next) {
                    // Nothing changed, so there is nothing to restart into and no
                    // reason to interrupt a session that is running.
                    Ok(false) => return,
                    // Never returns unless the exec itself failed.
                    Ok(true) => {
                        let status_item = self.ivars().status_item.get();
                        if let Some(status_item) = status_item {
                            status_item.setVisible(false);
                        }
                        let e = crate::restart();
                        if let Some(status_item) = status_item {
                            status_item.setVisible(true);
                        }
                        warn!("menu: {e:#}");
                        panels::error(
                            mtm,
                            "Saved, but could not restart",
                            &format!(
                                "{e:#}\n\nThe settings are in the config file. Quit \
                                 remotex-agent and open it again to start using them."
                            ),
                        );
                        return;
                    }
                    Err(e) => {
                        warn!("menu: rejected settings: {e:#}");
                        panels::error(mtm, "Those settings were not saved", &format!("{e:#}"));
                        draft = edited;
                    }
                }
            }
        }

        #[unsafe(method(copyPsk:))]
        fn copy_psk(&self, _sender: Option<&AnyObject>) {
            // The key this process is authenticating with, not the one in the
            // file — the same reason the "Listening on" line reads `running()`.
            // Saving a key normally re-execs straight into it, so the two agree;
            // when they do not (a re-exec that failed, a hand-edited file) the
            // file's key is the one nothing will accept, and pasting it into the
            // gateway would look like the agent had gone deaf.
            self.copy_to_clipboard(&self.ivars().settings.running().psk);
        }

        /// Show the config file in the Finder.
        ///
        /// Not "open" it: the file holds the key, and handing it to whatever has
        /// claimed `.toml` is a surprise. Revealing it is the useful half anyway
        /// — it answers "where is this thing?" without putting the credential in
        /// front of an editor.
        #[unsafe(method(revealConfig:))]
        fn reveal_config(&self, _sender: Option<&AnyObject>) {
            let path = self.ivars().settings.path().to_owned();
            let shown = NSWorkspace::sharedWorkspace().selectFile_inFileViewerRootedAtPath(
                Some(&NSString::from_str(&path.to_string_lossy())),
                &NSString::from_str(""),
            );
            if !shown {
                warn!("menu: could not reveal {}", path.display());
            }
        }

        #[unsafe(method(openLog:))]
        fn open_log(&self, _sender: Option<&AnyObject>) {
            let Some(path) = &self.ivars().log_path else {
                return;
            };
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            if !NSWorkspace::sharedWorkspace().openURL(&url) {
                warn!("menu: could not open {}", path.display());
            }
        }

        #[unsafe(method(openScreenRecordingSettings:))]
        fn open_screen_recording_settings(&self, _sender: Option<&AnyObject>) {
            open_pane(URL_SCREEN_RECORDING);
        }

        #[unsafe(method(openAccessibilitySettings:))]
        fn open_accessibility_settings(&self, _sender: Option<&AnyObject>) {
            open_pane(URL_ACCESSIBILITY);
        }

        #[unsafe(method(toggleLoginItem:))]
        fn toggle_login_item(&self, _sender: Option<&AnyObject>) {
            let outcome = match loginitem::status() {
                loginitem::Status::Enabled => loginitem::unregister(),
                // Only the user can undo this one, in System Settings — no
                // amount of re-registering moves it, so send them there.
                loginitem::Status::RequiresApproval => {
                    open_pane(URL_LOGIN_ITEMS);
                    return;
                }
                _ => loginitem::register(),
            };
            match outcome {
                Ok(()) => info!("menu: login item is now {}", loginitem::status()),
                Err(e) => {
                    warn!("menu: could not change the login item: {e:#}");
                    panels::error(
                        MainThreadMarker::from(self),
                        "Could not change Start at Login",
                        &format!(
                            "{e:#}\n\nSMAppService refuses an improperly signed bundle. Running \
                             the binary outside remotex-agent.app cannot register at all."
                        ),
                    );
                }
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            // Exit 0 on purpose: the plist's `SuccessfulExit: false` KeepAlive
            // is what makes that stick. `NSApplication::terminate` would run a
            // shutdown sequence that the capture stream and tokio runtime have
            // no part in, for no benefit — there is nothing to save.
            info!("menu: quitting at the user's request");
            std::process::exit(0);
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker, ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    /// Point the status item's icon at what the agent is currently able to do.
    fn refresh_icon(&self) {
        let health = self.health();
        if self.ivars().icon.get() == Some(health) {
            return;
        }
        let Some(item) = self.ivars().status_item.get() else {
            return;
        };
        let Some(button) = item.button(MainThreadMarker::from(self)) else {
            return;
        };

        let (symbol, fallback, description) = match health {
            // Ahead of "connected" on purpose: a gateway attached to an agent
            // that cannot capture or inject is the case most worth warning about,
            // not the one to reassure about.
            Health::Blocked => (
                ICON_BLOCKED,
                ICON_FALLBACK_BLOCKED,
                "remotex agent, missing permissions",
            ),
            Health::Connected => (
                ICON_CONNECTED,
                ICON_FALLBACK_CONNECTED,
                "remotex agent, connected",
            ),
            Health::Idle => (ICON_IDLE, ICON_FALLBACK_IDLE, "remotex agent"),
        };
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(symbol),
            Some(&NSString::from_str(description)),
        );
        match image {
            Some(image) => {
                // Template: the menu bar tints it, so it stays legible in light
                // mode, dark mode and under a wallpaper-tinted bar alike.
                image.setTemplate(true);
                button.setImage(Some(&image));
            }
            None => button.setTitle(&NSString::from_str(fallback)),
        }
        self.ivars().icon.set(Some(health));
    }

    /// Re-read Accessibility while it is missing.
    ///
    /// Accessibility takes effect immediately, so polling once a second while it
    /// is absent lets the status icon recover without a restart. Once granted it
    /// is rechecked only when the menu opens, where revocation matters to the UI.
    fn refresh_accessibility(&self) {
        let mut permissions = self.ivars().permissions.get();
        if !permissions.accessibility {
            permissions.accessibility = input::accessibility_granted();
            self.ivars().permissions.set(permissions);
        }
    }

    /// Refresh both TCC values for a user-driven menu update.
    ///
    /// Screen Recording is deliberately not polled. A post-launch grant becomes
    /// `RelaunchRequired`, never `Granted`; opening the menu is enough to discover
    /// it and explain the required quit and reopen. This also notices revocation
    /// without a permanent background IPC loop.
    fn refresh_permissions_for_menu(&self) -> Permissions {
        let mut permissions = self.ivars().permissions.get();
        permissions.screen = permissions
            .screen
            .observe(capture::screen_recording_granted());
        permissions.accessibility = input::accessibility_granted();
        self.ivars().permissions.set(permissions);
        self.refresh_icon();
        permissions
    }

    fn health(&self) -> Health {
        if !self.ivars().permissions.get().complete() {
            Health::Blocked
        } else if self.ivars().state.is_connected() {
            Health::Connected
        } else {
            Health::Idle
        }
    }

    /// Fill the menu in from scratch. See `menuNeedsUpdate:` for why nothing is
    /// cached between openings.
    fn rebuild(&self, menu: &NSMenu) {
        let mtm = MainThreadMarker::from(self);
        let ivars = self.ivars();
        let settings = &ivars.settings;
        let saved = settings.saved();
        menu.removeAllItems();

        let connection = ivars.state.current();
        menu.addItem(&self.info(&state::describe(connection.as_ref(), Instant::now()), mtm));
        // The address the agent is *serving*, not the one in the file — they
        // differ until a pending change has been restarted into, and this line is
        // the one place that has to be true about right now.
        menu.addItem(&self.info(
            &format!(
                "Listening on {} · v{}",
                settings.running().listen,
                env!("CARGO_PKG_VERSION")
            ),
            mtm,
        ));
        if settings.restart_pending() {
            menu.addItem(&self.info("⚠︎ Saved changes apply after a restart", mtm));
        }

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        // Outside the dialog, because it is not an edit: copying the key onto the
        // gateway is the one thing anybody does with it, and it should not need a
        // dialog and a Cancel every time.
        let item = self.action("Copy Pre-Shared Key", sel!(copyPsk:), mtm);
        // Not a state a save can leave behind: saving a key re-execs into it, so
        // the two agree. The keys differ only when that did not happen — a
        // re-exec that failed, or a config edited by hand — and then which key
        // this copies is the difference between a gateway that connects and one
        // that is refused.
        if saved.psk != settings.running().psk {
            item.setToolTip(Some(&NSString::from_str(
                "Copies the key in force right now. The config file holds a different one that \
                 never took effect — quit remotex-agent and open it again to switch to it.",
            )));
        }
        menu.addItem(&item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let item = self.action("Settings…", sel!(openSettings:), mtm);
        item.setToolTip(Some(&NSString::from_str(&format!(
            "Listen address ({}), display and pre-shared key. Saving a change restarts \
             the agent.",
            saved.listen
        ))));
        menu.addItem(&item);
        menu.addItem(&self.action("Reveal Config in Finder", sel!(revealConfig:), mtm));
        if ivars.log_path.is_some() {
            menu.addItem(&self.action("Open Log", sel!(openLog:), mtm));
        }

        // Re-read on this user-driven event so revocations are visible and a
        // post-launch Screen Recording grant can require an explicit relaunch.
        let permissions = self.refresh_permissions_for_menu();
        if !permissions.complete() {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let status = if permissions.screen == ScreenRecording::RelaunchRequired {
                "⚠︎ Quit and reopen to enable Screen Recording"
            } else {
                "⚠︎ Not usable until permissions are granted"
            };
            menu.addItem(&self.info(status, mtm));
            match permissions.screen {
                ScreenRecording::Missing => {
                    let item = self.action(
                        "Enable Screen Recording…",
                        sel!(openScreenRecordingSettings:),
                        mtm,
                    );
                    item.setToolTip(Some(&NSString::from_str(
                        "Not granted — the screen never paints. Click to open System Settings.",
                    )));
                    menu.addItem(&item);
                }
                ScreenRecording::RelaunchRequired => {
                    let item = self.action(
                        "Quit Agent to Apply Screen Recording",
                        sel!(quit:),
                        mtm,
                    );
                    item.setToolTip(Some(&NSString::from_str(
                        "Screen Recording is enabled in System Settings. Quit, then reopen \
                         remotex-agent from Applications to apply it to a new process.",
                    )));
                    menu.addItem(&item);
                }
                ScreenRecording::Granted => {}
            }
            if !permissions.accessibility {
                let item = self.action("Enable Accessibility…", sel!(openAccessibilitySettings:), mtm);
                item.setToolTip(Some(&NSString::from_str(
                    "Not granted — every click and keystroke is silently ignored. Click to \
                     open System Settings.",
                )));
                menu.addItem(&item);
            }
        }

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let login = loginitem::status();
        let item = self.action("Start at Login", sel!(toggleLoginItem:), mtm);
        item.setState(checkmark(login == loginitem::Status::Enabled));
        item.setToolTip(Some(&NSString::from_str(&login.to_string())));
        menu.addItem(&item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let item = self.action("Quit remotex-agent", sel!(quit:), mtm);
        item.setToolTip(Some(&NSString::from_str(
            "Stops sharing until you log in again, or open remotex-agent yourself. Opening \
             it again is also what applies a saved change.",
        )));
        menu.addItem(&item);
    }

    fn copy_to_clipboard(&self, psk: &str) {
        if pasteboard::write(psk) {
            info!("menu: pre-shared key copied to the clipboard");
        } else {
            warn!("menu: the clipboard refused the pre-shared key");
        }
    }

    /// A line of text, not a control: disabled, which is how AppKit draws a
    /// heading — greyed out and unclickable.
    ///
    /// Explicitly disabled rather than left to `autoenablesItems`, which both
    /// menus here switch off. Automatic enabling decides a submenu's parent from
    /// whether the submenu has any enabled items, and the display submenu is
    /// deliberately empty until it is opened — so it would be greyed out and
    /// could never be opened to fill itself in.
    fn info(&self, title: &str, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                None,
                &NSString::from_str(""),
            )
        };
        item.setEnabled(false);
        item
    }

    fn action(&self, title: &str, action: Sel, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(""),
            )
        };
        // Safety: the target is this controller, which outlives the menu — both
        // are owned by `run`, which never returns. NSMenuItem holds its target
        // weakly, so a shorter-lived controller would be a dangling send.
        unsafe { item.setTarget(Some(&*self.as_object())) };
        item
    }

    fn as_object(&self) -> Retained<AnyObject> {
        let this: Retained<Self> = self.retain();
        // Safety: upcasting a subclass of NSObject to AnyObject.
        unsafe { Retained::cast_unchecked(this) }
    }
}

/// Take over the main thread: status item, cursor timer, run loop. Never
/// returns.
pub fn run(
    state: Arc<state::AgentState>,
    tracker: Arc<cursor::Tracker>,
    settings: Arc<settings::Settings>,
    log_path: Option<PathBuf>,
    screen_recording_at_launch: bool,
) -> ! {
    let mtm = MainThreadMarker::new().expect("menubar::run must be called on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Accessory: a menu bar item, no Dock tile, no menu of our own in the menu
    // bar, and the agent never steals focus. The bundle's `LSUIElement` already
    // says this, but a hand-run binary has no Info.plist to read it from.
    //
    // It does still activate for a modal panel — see crate::panels.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let controller = Controller::new(
        mtm,
        Ivars {
            state,
            tracker,
            settings,
            log_path,
            status_item: OnceCell::new(),
            icon: Cell::new(None),
            ticks: Cell::new(0),
            // Read here rather than left blank: the first icon is painted before
            // the timer has ever fired, and it must not claim health it has not
            // checked.
            permissions: Cell::new(Permissions::read(screen_recording_at_launch)),
        },
    );

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("remotex-agent"));
    // Enablement is decided item by item (see `Controller::info`), not inferred.
    menu.setAutoenablesItems(false);
    menu.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    item.setMenu(Some(&menu));
    controller
        .ivars()
        .status_item
        .set(item)
        .expect("the status item is set exactly once");
    controller.refresh_icon();

    // Common modes, not the default one: a timer in the default mode stops
    // firing while a menu is open, and the pointer shape would freeze for as
    // long as the user held it down.
    //
    // Safety: the target is the controller, which lives as long as this process.
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
            TICK,
            &controller.as_object(),
            sel!(tick:),
            None,
            true,
        )
    };
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };

    info!("menu bar: status item ready");
    app.run();

    // `run` only returns if something terminated the app, and by then the
    // process should be going away anyway.
    std::process::exit(0);
}

/// The settings dialog's display menu.
///
/// Two cases besides the ordinary one, and both have to leave the dialog usable —
/// it is the only way to reach the other two settings:
///
/// - **The list cannot be read**, which is almost always the missing Screen
///   Recording grant. The configured display is offered on its own, so the key
///   and the address are still editable.
/// - **The configured display is not attached** (a `display = 3` left over from an
///   unplugged monitor). It is appended, and stays selected, so opening the dialog
///   and pressing Save does not silently move the agent to another screen. The
///   agent falls back to the main display meanwhile — see [`capture::probe`].
fn display_choices(current: usize) -> Vec<panels::DisplayChoice> {
    let mut choices: Vec<panels::DisplayChoice> = match capture::displays() {
        Ok(displays) => displays
            .iter()
            .map(|display| panels::DisplayChoice {
                index: display.index,
                label: format!(
                    "Display {} · {}×{} at {}x",
                    display.index + 1,
                    display.geometry.width,
                    display.geometry.height,
                    display.geometry.scale
                ),
            })
            .collect(),
        Err(e) => {
            warn!("menu: cannot list displays: {e:#}");
            Vec::new()
        }
    };
    if !choices.iter().any(|choice| choice.index == current) {
        choices.push(panels::DisplayChoice {
            index: current,
            label: format!("Display {} · not attached", current + 1),
        });
    }
    choices
}

fn checkmark(on: bool) -> NSControlStateValue {
    if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    }
}

fn open_pane(url: &str) {
    let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) else {
        warn!("menu: {url} is not a valid URL");
        return;
    };
    if !NSWorkspace::sharedWorkspace().openURL(&url) {
        warn!("menu: System Settings would not open");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkmarks_map_to_appkits_own_constants() {
        assert_eq!(checkmark(true), NSControlStateValueOn);
        assert_eq!(checkmark(false), NSControlStateValueOff);
    }

    // These are typed by hand and only fail at runtime, where the symptom is a
    // menu item that silently does nothing.
    #[test]
    fn the_settings_deep_links_are_parseable_urls() {
        for url in [URL_SCREEN_RECORDING, URL_ACCESSIBILITY, URL_LOGIN_ITEMS] {
            assert!(
                NSURL::URLWithString(&NSString::from_str(url)).is_some(),
                "{url} is not a URL"
            );
            assert!(url.starts_with("x-apple.systempreferences:"), "{url}");
        }
    }

    // A missing SF Symbol falls back to a text title, but silently — so pin the
    // names here rather than discovering it on a user's menu bar.
    #[test]
    fn every_status_icon_exists_in_sf_symbols() {
        let looked_up: Vec<_> = [ICON_BLOCKED, ICON_IDLE, ICON_CONNECTED]
            .into_iter()
            .map(|symbol| {
                let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(symbol),
                    None,
                );
                (symbol, image)
            })
            .collect();
        // *All* nil is a session with no window server — over SSH, or in CI —
        // where AppKit answers nothing at all, not three simultaneous typos. Skip
        // there, the same way the cursor test does; one nil still fails, which
        // is the typo this test exists to catch.
        if looked_up.iter().all(|(_, image)| image.is_none()) {
            eprintln!("no window server available (headless session); skipping");
            return;
        }
        for (symbol, image) in looked_up {
            assert!(image.is_some(), "SF Symbols has no {symbol:?}");
        }
    }

    // The icon is the only signal that a required permission is missing, so
    // "blocked" has to outrank the other two — an attached gateway on an agent
    // that cannot capture is precisely the case to warn about.
    #[test]
    fn a_missing_permission_outranks_being_connected() {
        let blocked = Permissions {
            screen: ScreenRecording::Missing,
            accessibility: true,
        };
        assert!(!blocked.complete());
        assert!(
            !Permissions {
                screen: ScreenRecording::Granted,
                accessibility: false,
            }
            .complete()
        );
        assert!(
            Permissions {
                screen: ScreenRecording::Granted,
                accessibility: true,
            }
            .complete()
        );
    }

    #[test]
    fn screen_recording_grants_after_launch_require_relaunch() {
        assert_eq!(
            ScreenRecording::at_launch(false).observe(true),
            ScreenRecording::RelaunchRequired
        );
        assert!(!ScreenRecording::RelaunchRequired.effective());
        assert!(ScreenRecording::at_launch(true).effective());
    }

    #[test]
    fn screen_recording_revocation_cannot_be_regranted_in_place() {
        let revoked = ScreenRecording::Granted.observe(false);
        assert_eq!(revoked, ScreenRecording::Missing);
        assert_eq!(revoked.observe(true), ScreenRecording::RelaunchRequired);
    }
}
