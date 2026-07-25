//! The menu bar item: the only part of the agent a user ever sees.
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
//! It also carries the two things setup needs and a terminal would otherwise be
//! required for: copying the pre-shared key, and jumping straight to the two
//! Privacy panes the agent cannot open its own way into.
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
//! AppKit is main-thread-only and so is `NSCursor`, which the agent already
//! polled from `main` (see [`crate::cursor`]). Running an `NSApplication` needs
//! that same thread, so the poll moves onto an `NSTimer` in the run loop and the
//! run loop takes over. The timer is added in `NSRunLoopCommonModes` so the
//! pointer keeps updating while the menu is open — in the default mode alone it
//! would stall for as long as the user held the menu down.

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

use crate::{capture, cursor, input, loginitem, state};

/// How often the run loop re-reads the system cursor and refreshes the icon.
///
/// The same 100ms the plain polling loop used before the menu bar existed; the
/// pointer shape has to keep up with the mouse crossing a window edge.
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
    /// Copied to the clipboard on demand. Already on disk in a 0600 file, and
    /// putting it on the clipboard is the whole point of the menu item.
    psk: String,
    listen: String,
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
            self.refresh_icon();
        }

        #[unsafe(method(copyPsk:))]
        fn copy_psk(&self, _sender: Option<&AnyObject>) {
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            let copied = unsafe {
                pasteboard.setString_forType(
                    &NSString::from_str(&self.ivars().psk),
                    NSPasteboardTypeString,
                )
            };
            if copied {
                info!("menu: pre-shared key copied to the clipboard");
            } else {
                warn!("menu: the clipboard refused the pre-shared key");
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
                Err(e) => warn!("menu: could not change the login item: {e:#}"),
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
        menu.removeAllItems();

        let connection = ivars.state.current();
        menu.addItem(&self.info(&state::describe(connection.as_ref(), Instant::now()), mtm));
        menu.addItem(&self.info(
            &format!("Listening on {} · v{}", ivars.listen, env!("CARGO_PKG_VERSION")),
            mtm,
        ));

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&self.action("Copy Pre-Shared Key", sel!(copyPsk:), mtm));
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
            "Stops sharing until you log in again, or open remotex-agent yourself.",
        )));
        menu.addItem(&item);
    }

    /// A line of text, not a control. With `autoenablesItems` on (the default) a
    /// menu item without an action is drawn greyed out, which is exactly how a
    /// heading should look.
    fn info(&self, title: &str, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                None,
                &NSString::from_str(""),
            )
        }
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
    psk: String,
    listen: String,
    log_path: Option<PathBuf>,
) -> ! {
    let mtm = MainThreadMarker::new().expect("menubar::run must be called on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Accessory: a menu bar item, no Dock tile, no menu of our own in the menu
    // bar, and the agent never steals focus. The bundle's `LSUIElement` already
    // says this, but a hand-run binary has no Info.plist to read it from.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let controller = Controller::new(
        mtm,
        Ivars {
            state,
            tracker,
            psk,
            listen,
            log_path,
            status_item: OnceCell::new(),
            icon_connected: Cell::new(None),
        },
    );

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("remotex-agent"));
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
}
