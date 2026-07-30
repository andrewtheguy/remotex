//! Main-thread menu-bar UI for status, permissions, settings, login-item
//! control, and Quit.
//!
//! Accessibility is polled because it becomes effective immediately; Screen
//! Recording requires relaunch. The cursor timer runs in common run-loop modes
//! so menu tracking does not pause updates. Quit stays quit: the LaunchAgent has
//! no `KeepAlive`, so nothing restarts the process until the next login.
//!
//! Quit is also reachable *before* any of that: [`Starting`] puts the item up with a
//! loading icon and a working Quit, and pumps AppKit while startup runs on another
//! thread, so a launch that wedges on a permission prompt or a hung `launchctl` can
//! still be ended from the menu bar.

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
    NSControlStateValueOn, NSEventMask, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength, NSWorkspace,
};
use objc2_foundation::{
    NSDate, NSDefaultRunLoopMode, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer, NSURL,
};

use crate::{capture, config, cursor, input, loginitem, panels, pasteboard, settings, state};

/// How often the run loop re-reads the system cursor and refreshes the icon.
///
/// 100ms, because the pointer shape has to keep up with the mouse crossing a
/// window edge.
const TICK: f64 = 0.1;

/// Poll Accessibility once a second until granted; macOS sends no notification.
const PERMISSION_EVERY: u32 = 10;

/// The status item: blocked, idle, and with a gateway attached.
///
/// Three different symbols rather than one symbol in three colours: menu bar
/// icons are template images that follow the menu bar's own tint, so colour is
/// not a channel that survives. Shape is.
const ICON_BLOCKED: &str = "exclamationmark.triangle.fill";
const ICON_IDLE: &str = "display";
const ICON_CONNECTED: &str = "eye.fill";
/// Startup, before anything is known about health. Its own icon rather than
/// borrowing the warning triangle, which is a claim — that something needs
/// attention — that nothing has checked yet.
const ICON_STARTING: &str = "ellipsis.circle";

/// If SF Symbols ever fails us, the item still has to be clickable — an empty
/// button is an invisible one, and then Quit is unreachable again.
const ICON_FALLBACK_BLOCKED: &str = "rxa!";
const ICON_FALLBACK_IDLE: &str = "rxa";
const ICON_FALLBACK_CONNECTED: &str = "rxa*";
const ICON_FALLBACK_STARTING: &str = "rxa…";

/// Deep links into the two Privacy panes. There is no API to grant these, and
/// finding them by hand is four levels down a settings tree.
const URL_SCREEN_RECORDING: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const URL_ACCESSIBILITY: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

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
    /// Still starting: nothing has been read, bound, granted or refused yet.
    Starting,
    /// Startup failed or a permission is missing: the agent cannot serve.
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
    /// The display this agent made, if it made one.
    ///
    /// Held only so the settings dialog can name it. Without it every display is
    /// measured and numbered as one of the Mac's own, and the agent's own screen
    /// was listed as "Display 2" — indistinguishable from a panel somebody is
    /// sitting at, in the one dialog whose checkbox decides whether it exists.
    owned: Option<capture::Target>,
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
            let attached = display_summary(self.ivars().owned);
            let mut draft = panels::Draft {
                listen: saved.listen.clone(),
                private_key: saved.private_key.clone(),
                authorized: settings.saved_authorized(),
                virtual_display: saved.virtual_display,
                virtual_size: saved.virtual_display_initial_size.clone(),
            };
            // The pairing the *process* is using, so the dialog can say when the
            // file's is one that never took effect — Copy is in there, and
            // copying a key the agent is not actually using looks like an agent
            // gone deaf.
            let in_force = panels::InForce {
                private_key: settings.running().private_key.clone(),
                authorized: settings.running_authorized().clone(),
            };

            loop {
                let Some(edited) =
                    panels::config(mtm, &draft, &attached, &in_force, settings.authorized_path())
                else {
                    return;
                };
                let next = config::Config {
                    listen: edited.listen.clone(),
                    private_key: edited.private_key.clone(),
                    virtual_display: edited.virtual_display,
                    virtual_display_initial_size: edited.virtual_size.clone(),
                };
                match settings.apply(next, edited.authorized.clone()) {
                    // Nothing changed, so there is nothing to restart into and no
                    // reason to interrupt a session that is running.
                    Ok(false) => return,
                    // Never returns unless launchd refused the restart.
                    Ok(true) => {
                        // The status item is left alone on the way out, and that
                        // is the whole point. Hiding it here — to spare the user a
                        // stale icon while the agent restarted — persisted:
                        // `setVisible(false)` is recorded by Control Center in
                        // this app's preferences, `restart` does not come back to
                        // undo it, and every launch afterwards came up with no
                        // icon and therefore no way to quit. See [`run`], which
                        // now also insists on visibility at creation.
                        let e = crate::restart();
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
            // Only a login item naming *this* copy is something to switch off.
            // One naming another copy is the state worth repairing, so ticking it
            // there installs over it rather than removing it — which is the whole
            // point of the item being able to say which copy it names.
            let outcome = match loginitem::status() {
                loginitem::Status::Installed => loginitem::uninstall(),
                _ => loginitem::install(),
            };
            match outcome {
                Ok(()) => info!("menu: login item is now {}", loginitem::status()),
                Err(e) => {
                    warn!("menu: could not change the login item: {e:#}");
                    panels::error(
                        MainThreadMarker::from(self),
                        "Could not change Start at Login",
                        &format!(
                            "{e:#}\n\nThe login item names this copy of the app by its full \
                             path, so it cannot be set from a mounted disk image — copy \
                             remotex-agent.app to Applications and open it from there."
                        ),
                    );
                }
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            // Exit rather than `NSApplication::terminate`, which would run a
            // shutdown sequence that the capture stream and tokio runtime have
            // no part in, for no benefit — there is nothing to save. Nothing
            // brings the process back either: the job has no `KeepAlive`, so Quit
            // means quit until the next login.
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
        set_icon(item, health, MainThreadMarker::from(self));
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
        if self.ivars().state.failure().is_some()
            || !self.ivars().permissions.get().complete()
        {
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

        if let Some(failure) = ivars.state.failure() {
            menu.addItem(&self.info("⚠︎ Agent is not serving", mtm));
            menu.addItem(&self.info(&one_line(&failure), mtm));
            menu.addItem(&self.info(&format!("v{}", env!("CARGO_PKG_VERSION")), mtm));
        } else {
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
        }
        if settings.restart_pending() {
            menu.addItem(&self.info("⚠︎ Saved changes apply after a restart", mtm));
        }
        // Only ever non-empty once a pasteboard read has actually tripped an
        // alert, which needs the clipboard bridge to have run — so this stays
        // invisible for anyone not using it, and appears exactly when it
        // explains the prompts they are seeing.
        if let Some(warning) = pasteboard::access_warning() {
            let item = self.info(&format!("⚠︎ {warning}"), mtm);
            item.setToolTip(Some(&NSString::from_str(
                "System Settings › Privacy & Security › Paste from Other Apps. Clipboard sync \
                 reads the pasteboard once per copy while a gateway is connected.",
            )));
            menu.addItem(&item);
        }

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        // An agent that will answer nobody says so, because there is nothing else
        // on screen to explain a Mac that is plainly running and refusing every
        // connection. Above Settings, which is where it is fixed.
        //
        // The *running* list, not the file's — same rule as the "Listening on" line
        // above, and for the same reason: this claim is in the present tense, so it
        // has to be true about right now. Reading the file would both warn about an
        // agent that is serving happily and stay silent about one that is refusing
        // everything because a key was added and not yet restarted into. The
        // pending-restart line above says the file has moved on.
        if settings.running_authorized().is_empty() {
            let item = self.action(
                "No authorized gateways — open Settings",
                sel!(openSettings:),
                mtm,
            );
            item.setToolTip(Some(&NSString::from_str(
                "This Mac refuses every connection until a gateway's public key is on its \
                 authorized list. `remotex rxa-pubkey` prints that key; add it under \
                 Authorized gateways in Settings.",
            )));
            menu.addItem(&item);
        }
        // No key is out here. Copying this Mac's is one button inside the dialog,
        // beside the key it copies — one click further than a menu item was, and it
        // puts the copy next to the list it has to be exchanged with, which is the
        // thing being done.
        let item = self.action("Settings…", sel!(openSettings:), mtm);
        item.setToolTip(Some(&NSString::from_str(&format!(
            "Listen address ({}), display, this Mac's public key with a Copy button, and \
             the gateways allowed to reach it. Saving a change restarts the agent.",
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
        // Unchecked when the login item names a *different* copy, which is the
        // honest answer: this copy does not start at login, something else does.
        // The tooltip names it, and ticking the box takes it over.
        item.setState(checkmark(login == loginitem::Status::Installed));
        item.setToolTip(Some(&NSString::from_str(&login.to_string())));
        menu.addItem(&item);
        // Not a tooltip: a login item pointing at another copy is why a
        // `kickstart` starts the wrong binary, and it stayed invisible for three
        // releases. Given a row of its own, it is the first thing anyone opening
        // this menu to ask "why am I running an old build" sees.
        if let loginitem::Status::Elsewhere(other) = &login {
            let warning = self.info(&format!("⚠︎ Login starts {}", other.display()), mtm);
            warning.setToolTip(Some(&NSString::from_str(
                "launchd starts that copy, not this one — so `launchctl kickstart` \
                 runs it too. Tick Start at Login here to point it at this copy.",
            )));
            menu.addItem(&warning);
        }

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let item = self.action("Quit remotex-agent", sel!(quit:), mtm);
        item.setToolTip(Some(&NSString::from_str(
            "Stops sharing until you log in again, or open remotex-agent yourself. Opening \
             it again is also what applies a saved change.",
        )));
        menu.addItem(&item);
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
        info_item(title, mtm)
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

/// The visible AppKit shell, created before config I/O, login-item registration,
/// socket binding, permission probes or worker setup.
///
/// A menu-bar-only app without this object has no UI at all. Holding the item
/// from the first lines of a GUI launch means every later outcome can replace
/// its menu and icon in place instead of making the application disappear.
pub struct Starting {
    item: Retained<NSStatusItem>,
}

/// What a handled startup failure still has to work with.
///
/// Carried so the degraded menu can offer **Settings…**, which is the whole point of
/// having one: the failure a user actually meets is "that port is already in use",
/// and the port is a setting. Without this the menu said what was wrong, offered
/// Quit, and left editing `config.toml` by hand as the only way out.
///
/// `None` where there is nothing to edit *with* — a config file that would not parse
/// has no settings to show, and the dialog is built from a parsed one.
pub struct Degraded {
    pub settings: Arc<settings::Settings>,
    pub state: Arc<state::AgentState>,
    pub tracker: Arc<cursor::Tracker>,
    pub log_path: Option<PathBuf>,
}

impl Starting {
    pub fn new() -> Self {
        let mtm = MainThreadMarker::new().expect("the menu bar must start on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        // Accessory: a menu bar item, no Dock tile, no menu of our own in the
        // menu bar, and the agent never steals focus. The bundle's `LSUIElement`
        // already says this, but a hand-run binary has no Info.plist to read it
        // from. It does still activate for a modal panel — see crate::panels.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let item =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        item.setVisible(true);
        set_icon(&item, Health::Starting, mtm);

        let menu =
            NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("remotex-agent"));
        menu.setAutoenablesItems(false);
        menu.addItem(&info_item("Starting remotex-agent…", mtm));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        // Reachable from the first instant, which is the point of this menu: a launch
        // that wedges before `run` would otherwise leave an icon that does nothing and
        // a process only `launchctl` or Activity Monitor can end.
        menu.addItem(&quit_item(mtm));
        item.setMenu(Some(&menu));
        // Finish the AppKit launch now, so the status item is registered with Control
        // Center before any startup work begins and so `pump_until` — which the caller
        // enters immediately — has an application to dequeue events for. `run` itself
        // still comes only once startup has succeeded or settled into a degraded state.
        app.finishLaunching();

        Self { item }
    }

    /// Pump AppKit until `startup` answers, and hand back what it said.
    ///
    /// The whole reason the status item goes up before startup begins is so there is
    /// a way out of a launch that is stuck — an unanswered Screen Recording prompt, a
    /// `launchctl` that will not return, a display the WindowServer is thinking
    /// about. That only works if AppKit is *running*: an item whose run loop has not
    /// started is on screen and unclickable, which is the worst of both, so the menu
    /// it shows says "Starting…" over a Quit that works.
    ///
    /// `nextEventMatchingMask:` rather than `NSRunLoop::runMode:` — running the run
    /// loop turns its input sources, but it is AppKit dequeuing events that opens a
    /// menu. A click on the item starts menu tracking in its own nested loop from
    /// inside `sendEvent:`, so Quit is reached without this loop needing to know
    /// anything about it.
    ///
    /// The date is short rather than distant so the channel is checked promptly once
    /// startup finishes; a launch that goes well spends a fraction of a second here.
    pub fn pump_until<T>(&self, startup: &std::sync::mpsc::Receiver<T>) -> T {
        let mtm = MainThreadMarker::new().expect("the startup pump runs on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        loop {
            match startup.try_recv() {
                Ok(outcome) => return outcome,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                // The worker is gone without an answer, which is a panic in it. There
                // is nothing to serve and nothing to show; the panic already said why.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    std::process::exit(1);
                }
            }
            let until = NSDate::dateWithTimeIntervalSinceNow(TICK);
            let event = unsafe {
                app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&until),
                    NSDefaultRunLoopMode,
                    true,
                )
            };
            if let Some(event) = event {
                app.sendEvent(&event);
            }
        }
    }

    /// Keep the status item alive after a handled startup failure.
    pub fn fail(self, title: &str, body: &str, degraded: Option<Degraded>) -> ! {
        let mtm = MainThreadMarker::new().expect("startup failures run on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        // The icon has to stop saying "starting", because this is where it stopped:
        // the loading ellipsis over an alert about a port already taken reads as work
        // still in progress, and nothing else will ever repaint it — `run`, which is
        // what installs the controller that keeps the icon honest, is never reached
        // from here.
        set_icon(&self.item, Health::Blocked, mtm);
        let menu =
            NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("remotex-agent"));
        menu.setAutoenablesItems(false);
        menu.addItem(&info_item(&format!("⚠︎ {title}"), mtm));
        let detail = info_item(&one_line(body), mtm);
        detail.setToolTip(Some(&NSString::from_str(body)));
        menu.addItem(&detail);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        // Settings first, because on the failure people actually hit — a port already
        // in use — it is the way out, and Quit is only the way to stop looking at the
        // problem. Held in `controller` for as long as this menu can be opened: a
        // menu item does not retain its target, and `app.run()` below never returns,
        // so the binding outlives every click there can be.
        let controller = degraded.map(|degraded| {
            let controller = Controller::new(
                mtm,
                Ivars {
                    state: degraded.state,
                    tracker: degraded.tracker,
                    settings: degraded.settings,
                    log_path: degraded.log_path,
                    status_item: OnceCell::new(),
                    icon: Cell::new(Some(Health::Blocked)),
                    ticks: Cell::new(0),
                    permissions: Cell::new(Permissions::read(false)),
                    // Nothing was created: startup stopped before it could have been,
                    // or the display is exactly what stopped it.
                    owned: None,
                },
            );
            menu.addItem(&controller.action("Settings…", sel!(openSettings:), mtm));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            controller
        });
        menu.addItem(&quit_item(mtm));
        // Deliberately no delegate: `rebuild` would draw the ordinary menu, whose
        // permission and session lines describe a process that never got that far.
        self.item.setMenu(Some(&menu));

        panels::startup_failure(mtm, title, body);
        app.run();
        // Reached only if AppKit's loop is ever left. `controller` is alive until
        // here, which is what the binding is for.
        drop(controller);
        std::process::exit(0);
    }
}

/// Take over the main thread: status item, cursor timer, run loop. Never
/// returns.
pub fn run(
    starting: Starting,
    state: Arc<state::AgentState>,
    tracker: Arc<cursor::Tracker>,
    settings: Arc<settings::Settings>,
    log_path: Option<PathBuf>,
    screen_recording_at_launch: bool,
    owned: Option<capture::Target>,
) -> ! {
    let mtm = MainThreadMarker::new().expect("menubar::run must be called on the main thread");
    let app = NSApplication::sharedApplication(mtm);

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
            owned,
        },
    );

    let item = starting.item;
    // Visible on purpose, at every launch, because the item's visibility is not
    // this process's to remember. macOS persists it per item — as
    // `NSStatusItem VisibleCC Item-0` in this app's preferences, written by
    // Control Center, which owns third-party menu bar items — and on the test VM
    // it was written `0`, to the second, by a settings save's `exec` restart. The
    // icon vanished, stayed gone across a reboot, and came back for no launch
    // path, while the item object was created successfully every time and this
    // module logged that it was ready.
    //
    // Which is the worst state this app has, because the icon is its only
    // interface. An invisible agent cannot be quit — Quit is in the menu that is
    // not there — and while it runs it goes on holding the port, so the next copy
    // the user opens meets "already in use" from a copy they cannot see. It can at
    // least be killed: the job has no `KeepAlive`, so a signal is the end of it.
    item.setVisible(true);
    // And say so if it still is not, rather than reporting "ready" for an item
    // nobody can see. Being wrong about this cost an afternoon.
    if !item.isVisible() {
        warn!(
            "menu bar: the status item is hidden and would not come back — the agent has no \
             interface at all. Stop it with `launchctl bootout gui/$(id -u)/{}`.",
            loginitem::LABEL
        );
    }
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

/// "Quit remotex-agent", targeting the application itself.
///
/// Shared by the startup menu, the degraded failure menu and the ordinary one,
/// because it is the one item that has to be there in every state this app can be
/// in — including the states where nothing else in the menu means anything yet.
fn quit_item(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let app = NSApplication::sharedApplication(mtm);
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit remotex-agent"),
            Some(sel!(terminate:)),
            &NSString::from_str(""),
        )
    };
    unsafe { quit.setTarget(Some(&*app)) };
    quit
}

fn set_icon(item: &NSStatusItem, health: Health, mtm: MainThreadMarker) {
    let Some(button) = item.button(mtm) else {
        return;
    };
    let (symbol, fallback, description) = match health {
        // Ahead of "connected" on purpose: a gateway attached to an agent
        // that cannot capture or inject is the case most worth warning about,
        // not the one to reassure about.
        Health::Blocked => (
            ICON_BLOCKED,
            ICON_FALLBACK_BLOCKED,
            "remotex agent, needs attention",
        ),
        Health::Connected => (
            ICON_CONNECTED,
            ICON_FALLBACK_CONNECTED,
            "remotex agent, connected",
        ),
        Health::Idle => (ICON_IDLE, ICON_FALLBACK_IDLE, "remotex agent"),
        Health::Starting => (
            ICON_STARTING,
            ICON_FALLBACK_STARTING,
            "remotex agent, starting",
        ),
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
}

fn info_item(title: &str, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
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

fn one_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Unknown startup error")
        .trim()
        .to_owned()
}

/// The settings dialog's read-only list of what this Mac can share, one line per
/// display.
///
/// Informational only. Which display a session shares is picked in the viewer or
/// the browser, so there is nothing here to set — but "what is there to pick
/// from" is still worth being able to see from the Mac itself, not least to
/// confirm that a virtual display switched on in this same dialog actually
/// arrived.
///
/// A list that cannot be read is almost always the missing Screen Recording
/// grant, and it must still leave the dialog usable: this is the only way to
/// reach the address and the key. So the failure becomes a line of text rather
/// than an empty box or an error panel.
///
/// The agent's own display is not appended here. It is a real display to macOS
/// once created, so it comes back from [`capture::displays`] like any other —
/// and if it is missing from this list after being switched on, that is worth
/// seeing rather than papering over.
fn display_summary(owned: Option<capture::Target>) -> Vec<String> {
    // `owned` so the agent's own display is *named* rather than numbered in with
    // the Mac's screens: `capture::displays` labels it "Virtual display" only
    // when it is told which id is ours, and passing `None` here listed it as
    // "Display 2". Which of these a client can be asked to resize depends on
    // exactly that difference, so the dialog has to show it.
    match capture::displays(owned) {
        Ok(displays) => displays
            .iter()
            .map(capture::DisplayInfo::summary)
            .collect(),
        Err(e) => {
            warn!("menu: cannot list displays: {e:#}");
            vec![format!("Cannot list displays — {e}")]
        }
    }
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
        for url in [URL_SCREEN_RECORDING, URL_ACCESSIBILITY] {
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
        let looked_up: Vec<_> = [ICON_BLOCKED, ICON_IDLE, ICON_CONNECTED, ICON_STARTING]
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
