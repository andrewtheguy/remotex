//! The menu bar item: the only interface the agent has.
//!
//! Without it the agent is completely invisible. Nothing says whether it is
//! running, nothing says when somebody is looking at your screen, and stopping
//! it means finding the process from a terminal — which is a poor deal for
//! software whose entire job is to let a remote machine watch and drive this
//! one. So the status item answers three questions at a glance:
//!
//! 1. **Is it running?** The icon is there, or it is not.
//! 2. **Is anyone connected?** The icon changes, and the first menu line names
//!    the peer.
//! 3. **How do I stop it?** Quit, which really quits — see below.
//!
//! ## Everything is here, because there is nowhere else
//!
//! This menu is the agent's whole interface — the CLI is three launch flags and
//! no operations at all. So it reads the pre-shared key, mints a new one, changes
//! the listen address, picks the display, reveals the config, opens the log,
//! links to the two Privacy panes, toggles the login item and quits. The panels
//! some of that needs live in [`crate::panels`]; what a change *means* lives in
//! [`crate::settings`].
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
    NSControlStateValueOn, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSPasteboard,
    NSPasteboardTypeString, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWorkspace,
};
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer, NSURL};

use crate::{capture, cursor, input, loginitem, panels, settings, state};

/// How often the run loop re-reads the system cursor and refreshes the icon.
///
/// 100ms, because the pointer shape has to keep up with the mouse crossing a
/// window edge.
const TICK: f64 = 0.1;

/// The status item, idle and with a gateway attached.
///
/// Two different symbols rather than one symbol in two colours: menu bar icons
/// are template images that follow the menu bar's own tint, so colour is not a
/// channel that survives. Shape is.
const ICON_IDLE: &str = "display";
const ICON_CONNECTED: &str = "eye.fill";

/// If SF Symbols ever fails us, the item still has to be clickable — an empty
/// button is an invisible one, and then Quit is unreachable again.
const ICON_FALLBACK_IDLE: &str = "rxa";
const ICON_FALLBACK_CONNECTED: &str = "rxa*";

/// Title of the display submenu's `NSMenu`, which is how `menuNeedsUpdate:`
/// tells the two menus apart — the delegate for both is this one controller, and
/// AppKit hands it only the menu that needs filling in.
///
/// It is a title nothing displays: a submenu shows its *item's* title, not its
/// own.
const DISPLAY_MENU: &str = "rxa-displays";

/// Deep links into the two Privacy panes. There is no API to grant these, and
/// finding them by hand is four levels down a settings tree.
const URL_SCREEN_RECORDING: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const URL_ACCESSIBILITY: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const URL_LOGIN_ITEMS: &str = "x-apple.systempreferences:com.apple.LoginItems-Settings.extension";

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
    icon_connected: Cell<Option<bool>>,
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
        // Both menus are rebuilt on open rather than kept in sync, so everything
        // in them is read at the moment it is displayed. Permission state, login
        // item registration and the set of attached displays all change *outside*
        // this process — in System Settings, or by plugging in a monitor — with no
        // notification we could subscribe to, so anything cached would be stale
        // exactly when the user went to look at it.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            if menu.title().to_string() == DISPLAY_MENU {
                self.rebuild_displays(menu);
            } else {
                self.rebuild(menu);
            }
        }
    }

    impl Controller {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            self.ivars().tracker.poll();
            self.refresh_icon();
        }

        /// Show the key, and offer the two things one can do with it.
        ///
        /// Loops so that regenerating lands back on the panel showing the *new*
        /// key: it has to be copied onto the gateway before anything can connect
        /// again, and the moment it is minted is the one moment the user is
        /// certainly thinking about that.
        #[unsafe(method(showPsk:))]
        fn show_psk(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let settings = &self.ivars().settings;
            loop {
                let psk = settings.saved().psk;
                let mut body = "This is the entire credential for reaching this Mac. Put it on \
                                the matching [[targets]] entry in the gateway's remotex.toml, \
                                as `psk`."
                    .to_owned();
                // Saying this is the whole reason the running key is tracked
                // separately: a key that has been regenerated but is not in force
                // yet gets acted on immediately, and the gateway would then fail
                // to connect for a reason nothing on screen explained.
                if psk != settings.running().psk {
                    body.push_str(
                        "\n\nThe agent is still authenticating with the previous key. Restart \
                         it to start using this one.",
                    );
                }
                match panels::secret(mtm, "Pre-Shared Key", &body, &psk) {
                    panels::Secret::Copy => {
                        self.copy_to_clipboard(&psk);
                        return;
                    }
                    panels::Secret::Close => return,
                    panels::Secret::Regenerate => {
                        if !self.regenerate() {
                            return;
                        }
                    }
                }
            }
        }

        #[unsafe(method(copyPsk:))]
        fn copy_psk(&self, _sender: Option<&AnyObject>) {
            self.copy_to_clipboard(&self.ivars().settings.saved().psk);
        }

        #[unsafe(method(editListen:))]
        fn edit_listen(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let settings = &self.ivars().settings;
            let current = settings.saved().listen;
            let Some(listen) = panels::prompt(
                mtm,
                "Listen Address",
                "Where the agent waits for the gateway, as address:port. 0.0.0.0 is every \
                 interface; narrow it to one if you prefer.",
                &current,
                "Change",
            ) else {
                return;
            };
            if listen == current {
                return;
            }
            match settings.set_listen(&listen) {
                // Whether the address can actually be *bound* is settled at the
                // next launch, not here — a port already in use is an error in
                // the log then, not a panel now.
                Ok(()) => self.note_restart("The new listen address"),
                Err(e) => panels::error(mtm, "That is not an address", &format!("{e:#}")),
            }
        }

        /// Share a different display. The index rides on the menu item's tag.
        #[unsafe(method(chooseDisplay:))]
        fn choose_display(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            // The sender is the NSMenuItem that was clicked; `tag` is the only
            // thing wanted from it, so it is read with a plain message send
            // rather than a downcast.
            //
            // Safety: every item wired to this action is an NSMenuItem, which
            // responds to `tag`.
            let index: isize = unsafe { msg_send![sender, tag] };
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            match self.ivars().settings.set_display(index) {
                Ok(()) => self.note_restart(&format!("Sharing display {}", index + 1)),
                Err(e) => panels::error(
                    MainThreadMarker::from(self),
                    "Could not change the display",
                    &format!("{e:#}"),
                ),
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
            open_settings(URL_SCREEN_RECORDING);
        }

        #[unsafe(method(openAccessibilitySettings:))]
        fn open_accessibility_settings(&self, _sender: Option<&AnyObject>) {
            open_settings(URL_ACCESSIBILITY);
        }

        #[unsafe(method(toggleLoginItem:))]
        fn toggle_login_item(&self, _sender: Option<&AnyObject>) {
            let outcome = match loginitem::status() {
                loginitem::Status::Enabled => loginitem::unregister(),
                // Only the user can undo this one, in System Settings — no
                // amount of re-registering moves it, so send them there.
                loginitem::Status::RequiresApproval => {
                    open_settings(URL_LOGIN_ITEMS);
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

    /// Point the status item's icon at the current connection state.
    fn refresh_icon(&self) {
        let connected = self.ivars().state.is_connected();
        if self.ivars().icon_connected.get() == Some(connected) {
            return;
        }
        let Some(item) = self.ivars().status_item.get() else {
            return;
        };
        let Some(button) = item.button(MainThreadMarker::from(self)) else {
            return;
        };

        let (symbol, fallback, description) = if connected {
            (ICON_CONNECTED, ICON_FALLBACK_CONNECTED, "remotex agent, connected")
        } else {
            (ICON_IDLE, ICON_FALLBACK_IDLE, "remotex agent")
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
        self.ivars().icon_connected.set(Some(connected));
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
        menu.addItem(&self.action("Pre-Shared Key…", sel!(showPsk:), mtm));
        // Alongside the panel, not inside it: copying the key onto the gateway is
        // the one thing anybody does with it, and it should not need two clicks
        // and a dialog every time.
        menu.addItem(&self.action("Copy Pre-Shared Key", sel!(copyPsk:), mtm));

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let item = self.action("Listen Address…", sel!(editListen:), mtm);
        item.setToolTip(Some(&NSString::from_str(&format!(
            "Currently {}. The gateway's target must name the same port.",
            saved.listen
        ))));
        menu.addItem(&item);
        menu.addItem(&self.display_item(saved.display, mtm));
        menu.addItem(&self.action("Reveal Config in Finder", sel!(revealConfig:), mtm));
        if ivars.log_path.is_some() {
            menu.addItem(&self.action("Open Log", sel!(openLog:), mtm));
        }

        // Both permissions are read live: the user may well have just granted
        // one in System Settings and come straight back here to check.
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let screen = capture::screen_recording_granted();
        let item = self.action(
            "Screen Recording",
            sel!(openScreenRecordingSettings:),
            mtm,
        );
        item.setState(checkmark(screen));
        item.setToolTip(Some(&NSString::from_str(if screen {
            "Granted. Click to open System Settings."
        } else {
            "Not granted — the screen will never paint. Click to open System Settings."
        })));
        menu.addItem(&item);

        let accessibility = input::accessibility_granted();
        let item = self.action("Accessibility", sel!(openAccessibilitySettings:), mtm);
        item.setState(checkmark(accessibility));
        item.setToolTip(Some(&NSString::from_str(if accessibility {
            "Granted. Click to open System Settings."
        } else {
            "Not granted — keyboard and mouse input is silently ignored. Click to \
             open System Settings."
        })));
        menu.addItem(&item);

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

    /// The display picker: a parent item naming the current choice, with the
    /// list itself built only if the user opens it.
    ///
    /// Listing displays means asking ScreenCaptureKit, which is a synchronous
    /// call into another process — cheap, but not free, and the main menu opens
    /// far more often than anyone changes their display.
    fn display_item(&self, current: usize, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
        let item = self.info(&format!("Display: {}", current + 1), mtm);
        let submenu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(DISPLAY_MENU));
        submenu.setAutoenablesItems(false);
        submenu.setDelegate(Some(ProtocolObject::from_ref(self)));
        item.setSubmenu(Some(&submenu));
        // It has no action of its own — opening the submenu is the whole job —
        // but `info` disabled it, and a disabled parent never opens.
        item.setEnabled(true);
        item
    }

    /// Fill in the display submenu, ticking the one that is chosen.
    fn rebuild_displays(&self, menu: &NSMenu) {
        let mtm = MainThreadMarker::from(self);
        let current = self.ivars().settings.saved().display;
        menu.removeAllItems();

        let displays = match capture::displays() {
            Ok(displays) => displays,
            Err(e) => {
                // Almost always the missing Screen Recording grant, which the
                // menu above already reports — so this says what it could not do
                // and leaves the fix where it belongs.
                warn!("menu: cannot list displays: {e:#}");
                menu.addItem(&self.info("Cannot list displays — grant Screen Recording", mtm));
                return;
            }
        };

        for display in &displays {
            let geometry = display.geometry;
            let item = self.action(
                &format!(
                    "Display {} · {}×{} at {}x",
                    display.index + 1,
                    geometry.width,
                    geometry.height,
                    geometry.scale
                ),
                sel!(chooseDisplay:),
                mtm,
            );
            item.setTag(display.index as isize);
            item.setState(checkmark(display.index == current));
            item.setToolTip(Some(&NSString::from_str(&format!(
                "CoreGraphics display {}. Applies when the agent restarts.",
                display.id
            ))));
            menu.addItem(&item);
        }

        // A `display = 3` left over from a monitor that has since been unplugged
        // still captures — the agent falls back to the main display — but nothing
        // above would show it, and "why is Display: 4 not ticked" deserves an
        // answer in the menu rather than in the log.
        if current >= displays.len() {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&self.info(
                &format!(
                    "Display {} is not attached — sharing Display 1",
                    current + 1
                ),
                mtm,
            ));
        }
    }

    fn copy_to_clipboard(&self, psk: &str) {
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        let copied = unsafe {
            pasteboard.setString_forType(&NSString::from_str(psk), NSPasteboardTypeString)
        };
        if copied {
            info!("menu: pre-shared key copied to the clipboard");
        } else {
            warn!("menu: the clipboard refused the pre-shared key");
        }
    }

    /// Confirm, then mint a new key. `false` if the user backed out or it failed.
    fn regenerate(&self) -> bool {
        let mtm = MainThreadMarker::from(self);
        if !panels::confirm(
            mtm,
            "Regenerate the pre-shared key?",
            "The new key has to go into the gateway's remotex.toml, and the agent only \
             starts using it once it restarts — so the gateway cannot connect between \
             those two steps.",
            "Regenerate",
        ) {
            return false;
        }
        match self.ivars().settings.regenerate_psk() {
            Ok(_) => {
                info!("menu: pre-shared key regenerated at the user's request");
                true
            }
            Err(e) => {
                warn!("menu: could not regenerate the pre-shared key: {e:#}");
                panels::error(mtm, "Could not save the new key", &format!("{e:#}"));
                false
            }
        }
    }

    /// Say that a saved change is not in force yet.
    ///
    /// Shown after every successful edit — a setting that appears to have taken
    /// hold and has not is worse than an extra click, and the menu's warning line
    /// is easy to miss when you have just come from a panel. Skipped when the
    /// edit happened to put the value back to what the agent is already running,
    /// where there is nothing to restart for.
    fn note_restart(&self, what: &str) {
        if !self.ivars().settings.restart_pending() {
            return;
        }
        panels::message(
            MainThreadMarker::from(self),
            "Saved — restart to apply",
            &format!(
                "{what} takes effect the next time remotex-agent starts. Quit it from this \
                 menu, then open remotex-agent again."
            ),
        );
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
            icon_connected: Cell::new(None),
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

fn checkmark(on: bool) -> NSControlStateValue {
    if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    }
}

fn open_settings(url: &str) {
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
    fn both_status_icons_exist_in_sf_symbols() {
        let looked_up: Vec<_> = [ICON_IDLE, ICON_CONNECTED]
            .into_iter()
            .map(|symbol| {
                let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(symbol),
                    None,
                );
                (symbol, image)
            })
            .collect();
        // *Both* nil is a session with no window server — over SSH, or in CI —
        // where AppKit answers nothing at all, not two simultaneous typos. Skip
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

    // The submenu is told apart from the main menu by its title, and a title
    // that ever collided with a real menu's would send AppKit's fill-in request
    // to the wrong builder — an empty display list, or a display list where the
    // whole menu should be.
    #[test]
    fn the_display_submenus_title_is_not_a_title_anything_displays() {
        assert!(DISPLAY_MENU.starts_with("rxa-"));
        assert_ne!(DISPLAY_MENU, "remotex-agent");
    }
}
