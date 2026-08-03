//! The two `CefApp`s: the browser process's and the helper's.
//!
//! They exist mostly to say the same thing twice. `on_register_custom_schemes`
//! runs in **every** process and must agree everywhere, or a renderer and the
//! browser disagree about what `remotex://app` even is. What differs is the half
//! each one implements: the browser process gets a `BrowserProcessHandler` (the
//! message pump and the moment the scheme handler can be registered), and the
//! helper gets a `RenderProcessHandler` (the message router's renderer side,
//! which is what puts `window.remotexNative` on the page).

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use cef::rc::Rc as _;
use cef::wrapper::message_router::{
    MessageRouterConfig, MessageRouterRendererSide, MessageRouterRendererSideHandlerCallbacks,
    RendererSideRouter,
};
use cef::*;

use crate::scheme;

/// The function the page calls to reach the app, and the name `nativeHost.ts`
/// tests for. `NATIVE_HOST` is `typeof window.remotexNative === "function"`, so
/// this string is half of that check and the client is the other half.
pub const QUERY_FUNCTION: &str = "remotexNative";

/// Chromium switches this app runs with, and each is here for a named reason.
///
/// Spelled the way they would be typed at a Chrome command line, so a line from
/// here can be pasted into [`SWITCHES_ENV`] and back again without translation.
pub const DEFAULT_SWITCHES: &[&str] = &[
    // **The keychain prompt.** Chromium encrypts its cookie store at rest with a
    // key it keeps in the login keychain as "Chromium Safe Storage", and reaching
    // for it puts a password dialog in front of the app on first launch — one the
    // user has to answer before anything works, on a window that has not painted
    // yet. A mock keychain replaces that key with an in-process one.
    //
    // It is worse than once, too: the key is bound to the binary's identity, so
    // every rebuild during development asks again.
    //
    // Nothing is lost. The only cookie here is the launch token, which this
    // gateway mints per launch and forgets on exit; there is no credential at rest
    // worth a key, and the gateway it opens is bound to loopback and gone with the
    // process. Encrypting it would protect nothing and cost a modal dialog.
    //
    // Paired with `password-store` below, and both are wanted: this one replaces
    // the key, that one keeps the password manager from reaching for a store at
    // all.
    "--use-mock-keychain",
    // The other half of `use-mock-keychain`. The client keeps no passwords —
    // credentials for a target live in the gateway's config, on the far side of a
    // socket — so Chromium's password manager has nothing to store and no reason
    // to ask the OS for somewhere to store it.
    "--password-store=basic",
    // No gesture requirement, unlike a browser tab. This is what lets **Remote ›
    // Enable Audio** start sound from a menu item: a menu press is not a user
    // activation as far as the engine is concerned, and without this the page's
    // `AudioContext` would come up suspended with nothing on screen to say so.
    "--autoplay-policy=no-user-gesture-required",
    // Pointer clients always show desktops at 100% — see the display-geometry
    // rules. A pinch that quietly scaled the canvas would make the pixel the
    // client reports and the pixel the user touched two different things.
    "--disable-pinch",
];

/// The environment variable that replaces [`DEFAULT_SWITCHES`] wholesale.
///
/// Answering "is this switch the thing?" should cost a relaunch, not a rebuild —
/// and a rebuild here is Chromium's static library, the Swift relink, and a
/// re-sign of a 350 MB bundle. So the list is read at startup rather than compiled
/// in, and every process inherits the variable from the browser process that was
/// launched with it.
///
/// Set it to run with exactly the switches named and none of the defaults; set it
/// empty to run with none at all; leave it unset for the defaults, which is what
/// every real launch does. It is only ever a diagnostic: a switch worth keeping
/// belongs in the list above, with the reason.
///
/// ```sh
/// # what the keychain switches are actually buying
/// REMOTEX_CHROMIUM_SWITCHES="--disable-pinch" \
///   dist/remotex.app/Contents/MacOS/remotex-viewer --instance-dir "$PWD/tmp/app-qa"
/// ```
pub const SWITCHES_ENV: &str = "REMOTEX_CHROMIUM_SWITCHES";

/// The switches this process should run with, as CEF wants them: a bare name and
/// an optional value.
pub fn switches() -> Vec<(String, Option<String>)> {
    match std::env::var(SWITCHES_ENV) {
        Ok(spelled) => spelled.split_whitespace().map(parse_switch).collect(),
        Err(_) => DEFAULT_SWITCHES.iter().copied().map(parse_switch).collect(),
    }
}

/// Split `--name=value` into the pair CEF's two append calls need.
///
/// The leading dashes go: `CefCommandLine` names a switch without them, and
/// keeping them would put `-use-mock-keychain` in the switch map — a switch
/// Chromium has never heard of and will not complain about.
fn parse_switch(spelled: &str) -> (String, Option<String>) {
    let name = spelled.trim_start_matches('-');
    match name.split_once('=') {
        Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
        None => (name.to_owned(), None),
    }
}

/// The router's configuration, spelled once because both sides must be given
/// exactly the same one.
pub fn router_config() -> MessageRouterConfig {
    MessageRouterConfig {
        js_query_function: QUERY_FUNCTION.to_owned(),
        js_cancel_function: format!("{QUERY_FUNCTION}Cancel"),
        ..Default::default()
    }
}

// The browser process's app.
wrap_app! {
    pub struct BrowserApp {
        web_root: PathBuf,
        schedule: RefCell<Option<crate::PumpScheduler>>,
    }

    impl App {
        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            scheme::register_schemes(registrar);
        }

        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else {
                return;
            };
            for (switch, value) in switches() {
                match value {
                    Some(value) => command_line.append_switch_with_value(
                        Some(&CefString::from(switch.as_str())),
                        Some(&CefString::from(value.as_str())),
                    ),
                    None => command_line.append_switch(Some(&CefString::from(switch.as_str()))),
                }
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(RemotexBrowserProcessHandler::new(
                self.web_root.clone(),
                RefCell::new(self.schedule.borrow().clone()),
            ))
        }
    }
}

wrap_browser_process_handler! {
    pub struct RemotexBrowserProcessHandler {
        web_root: PathBuf,
        schedule: RefCell<Option<crate::PumpScheduler>>,
    }

    impl BrowserProcessHandler {
        /// The scheme handler can only be registered once there is a context to
        /// register it against, which is here and not in `initialize`.
        fn on_context_initialized(&self) {
            let mut factory = scheme::asset_factory(self.web_root.clone());
            let registered = register_scheme_handler_factory(
                Some(&CefString::from(scheme::SCHEME)),
                Some(&CefString::from(scheme::HOST)),
                Some(&mut factory),
            );
            crate::trace!(
                "context ready; {}://{} factory registered = {registered}, web root {:?}",
                scheme::SCHEME,
                scheme::HOST,
                self.web_root
            );
            crate::mark_context_ready();
        }

        /// CEF asking for its pump to be run.
        ///
        /// Handed straight back out to the shell, which schedules it on the main
        /// queue. Deliberately not run here: calling `do_message_loop_work` from
        /// inside this callback re-enters the pump, which is the documented way to
        /// wedge it.
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            crate::note_pump_scheduled(delay_ms);
            if let Some(scheduler) = self.schedule.borrow().as_ref() {
                scheduler.schedule(delay_ms);
            }
        }
    }
}

// The helper's app. One `RenderProcessHandler`, and the scheme.
wrap_app! {
    pub struct HelperApp {
        router: Arc<RendererSideRouter>,
    }

    impl App {
        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            scheme::register_schemes(registrar);
        }

        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(RemotexRenderProcessHandler::new(self.router.clone()))
        }
    }
}

// Everything here is the message router's; there is nothing of the client's
// in the renderer. `window.remotexNative` appears as each frame's V8 context
// is created, which is why `NATIVE_HOST` can be read once at module load and
// never be wrong.
wrap_render_process_handler! {
    pub struct RemotexRenderProcessHandler {
        router: Arc<RendererSideRouter>,
    }

    impl RenderProcessHandler {
        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            self.router.on_context_created(
                browser.map(|b| b.clone()),
                frame.map(|f| f.clone()),
                context.map(|c| c.clone()),
            );
        }

        fn on_context_released(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            self.router.on_context_released(
                browser.map(|b| b.clone()),
                frame.map(|f| f.clone()),
                context.map(|c| c.clone()),
            );
        }

        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            i32::from(self.router.on_process_message_received(
                browser.map(|b| b.clone()),
                frame.map(|f| f.clone()),
                Some(source_process),
                message.map(|m| m.clone()),
            ))
        }
    }
}

/// Build the helper's app, router and all. Called by `remotex-cef-helper`.
pub fn helper_app() -> App {
    let router = RendererSideRouter::new(router_config());
    HelperApp::new(router)
}

/// Build the browser process's app.
pub fn browser_app(web_root: PathBuf, schedule: Option<crate::PumpScheduler>) -> App {
    BrowserApp::new(web_root, RefCell::new(schedule))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_is_named_without_its_dashes() {
        assert_eq!(
            parse_switch("--use-mock-keychain"),
            ("use-mock-keychain".to_owned(), None)
        );
        assert_eq!(
            parse_switch("--password-store=basic"),
            ("password-store".to_owned(), Some("basic".to_owned()))
        );
        // Written either way, because the environment variable is typed by hand.
        assert_eq!(parse_switch("disable-pinch"), parse_switch("--disable-pinch"));
    }

    /// A value may contain the separator; only the first one splits.
    #[test]
    fn only_the_first_equals_separates() {
        assert_eq!(
            parse_switch("--host-rules=MAP * 127.0.0.1:8080"),
            (
                "host-rules".to_owned(),
                Some("MAP * 127.0.0.1:8080".to_owned())
            )
        );
    }

    /// The defaults must survive the same parse the environment variable gets, or
    /// the override would be running a different command line from the one it
    /// replaced.
    #[test]
    fn every_default_switch_parses() {
        for spelled in DEFAULT_SWITCHES {
            let (name, _) = parse_switch(spelled);
            assert!(!name.is_empty(), "{spelled} names nothing");
            assert!(!name.starts_with('-'), "{spelled} kept its dashes");
        }
    }
}
