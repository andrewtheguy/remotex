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
        gateway_origin: RefCell<String>,
        schedule: RefCell<Option<crate::PumpScheduler>>,
    }

    impl App {
        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            scheme::register_schemes(registrar);
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(RemotexBrowserProcessHandler::new(
                self.web_root.clone(),
                self.gateway_origin.borrow().clone(),
                RefCell::new(self.schedule.borrow().clone()),
            ))
        }
    }
}

wrap_browser_process_handler! {
    pub struct RemotexBrowserProcessHandler {
        web_root: PathBuf,
        gateway_origin: String,
        schedule: RefCell<Option<crate::PumpScheduler>>,
    }

    impl BrowserProcessHandler {
        /// The scheme handler can only be registered once there is a context to
        /// register it against, which is here and not in `initialize`.
        fn on_context_initialized(&self) {
            let mut factory =
                scheme::asset_factory(self.web_root.clone(), self.gateway_origin.clone());
            register_scheme_handler_factory(
                Some(&CefString::from(scheme::SCHEME)),
                Some(&CefString::from(scheme::HOST)),
                Some(&mut factory),
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
pub fn browser_app(
    web_root: PathBuf,
    gateway_origin: String,
    schedule: Option<crate::PumpScheduler>,
) -> App {
    BrowserApp::new(
        web_root,
        RefCell::new(gateway_origin),
        RefCell::new(schedule),
    )
}
