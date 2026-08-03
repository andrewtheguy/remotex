//! The browser's client: what happens to the page, and what the page says back.
//!
//! Three jobs, and nothing else belongs here. It holds the browser once Chromium
//! makes one, it refuses to navigate anywhere but the client, and it forwards the
//! message router's four hooks. Everything the *session* does is the page's.

use std::cell::RefCell;
use std::rc::Rc as StdRc;
use std::sync::Arc;

use cef::rc::Rc as _;
use cef::wrapper::message_router::{
    BrowserSideCallback, BrowserSideHandler, BrowserSideRouter, MessageRouterBrowserSide,
    MessageRouterBrowserSideHandlerCallbacks,
};
use cef::*;

use crate::scheme;

/// The browser, once there is one. Shared between the client (which is handed it
/// by `on_after_created`) and the C ABI (which evaluates script against it).
pub type BrowserSlot = StdRc<RefCell<Option<Browser>>>;

/// Whether `url` is the client itself.
///
/// The client is `remotex://app/…`, so this is a scheme-and-host test again — the
/// thing a `file://` document could not have, since a file URL has no host and no
/// port and every one of them therefore matched every other.
///
/// It matters because a remote desktop is a stream of somebody else's pixels and
/// text, and the clipboard bridge carries their strings into this process. None of
/// it should be able to send this window somewhere. There is no tab and no address
/// bar to notice with, so the refusal is here.
pub fn permits(url: &str) -> bool {
    let Some((left, rest)) = url.split_once("://") else {
        return false;
    };
    if left != scheme::SCHEME {
        return false;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host == scheme::HOST
}

/// Make Chromium's view fill the container the shell gave it, and keep filling it.
///
/// The browser is created at a token 1×1, because at that moment the container may
/// not have been laid out yet — SwiftUI sizes a view after it makes it, and the
/// browser may be created later still if Chromium's context was not ready. So the
/// size is taken here instead, when there is certainly a view to take it from.
///
/// The autoresizing mask is the half that lasts. Without it the view would be
/// correct once and then stay that size, because AppKit does not resize a subview
/// it did not create — which is the difference between a window that shows the
/// client and a window that shows a 1×1 corner of it.
///
/// # Safety
/// `handle` must be an `NSView *` and this must run on the main thread.
unsafe fn fill_parent(handle: sys::cef_window_handle_t) {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSAutoresizingMaskOptions, NSView};

    if handle.is_null() {
        return;
    }
    let view: Retained<NSView> = unsafe { Retained::retain(handle.cast()) }.unwrap();
    let Some(parent) = (unsafe { view.superview() }) else {
        return;
    };
    let bounds = parent.bounds();
    view.setFrame(bounds);
    view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
}

/// Receives what the page posts and hands it to the shell.
struct PageMessages {
    deliver: crate::MessageSink,
}

impl BrowserSideHandler for PageMessages {
    fn on_query_str(
        &self,
        _browser: Option<Browser>,
        _frame: Option<Frame>,
        _query_id: i64,
        request: &str,
        _persistent: bool,
        callback: Arc<std::sync::Mutex<dyn BrowserSideCallback>>,
    ) -> bool {
        self.deliver.deliver(request);
        // Answered at once and always successfully. The page does not read the
        // reply — `postToHost` supplies no `onSuccess` — but a query left
        // unanswered is a leak on both sides of the IPC, so this is a receipt
        // rather than a result.
        if let Ok(callback) = callback.lock() {
            callback.success_str("");
        }
        true
    }
}

wrap_client! {
    pub struct RemotexClient {
        browser: BrowserSlot,
        router: Arc<BrowserSideRouter>,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(RemotexLifeSpanHandler::new(
                self.browser.clone(),
                self.router.clone(),
            ))
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(RemotexRequestHandler::new(self.router.clone()))
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(RemotexDisplayHandler::new())
        }

        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(RemotexContextMenuHandler::new())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(RemotexLoadHandler::new())
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
                source_process,
                message.map(|m| m.clone()),
            ))
        }
    }
}

wrap_life_span_handler! {
    pub struct RemotexLifeSpanHandler {
        browser: BrowserSlot,
        router: Arc<BrowserSideRouter>,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            crate::trace!("browser created");
            let browser = browser.map(|browser| browser.clone());
            if let Some(host) = browser.as_ref().and_then(|browser| browser.host()) {
                // SAFETY: on macOS a windowed browser's window handle is the
                // `NSView` CEF added under the parent, and this runs on the UI
                // thread, which is the main thread.
                unsafe { fill_parent(host.window_handle()) };
            }
            *self.browser.borrow_mut() = browser;
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            self.router.on_before_close(browser.map(|b| b.clone()));
            *self.browser.borrow_mut() = None;
        }

        /// No popups. Every URL this client could produce is refused by
        /// `on_before_browse` anyway; this is the same rule for the window a
        /// `target="_blank"` would otherwise open beside it.
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            _target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            1
        }
    }
}

// No context menu, anywhere in this window.
//
// Chromium's is a *browser's* menu — Back, Reload, View Page Source, Inspect — and
// none of it has a meaning here: there is no history, no address bar, and the page
// is the app. Worse, a right-click is something the guest wants. The desktop
// surface already calls `preventDefault` on `contextmenu`, but that only covers the
// canvas, and even there the menu is Chromium's answer to the *native* event rather
// than to the DOM one. Clearing the model is what covers the whole window.
//
// Cleared rather than replaced: what a right-click ought to offer — Copy, Paste,
// the display controls — is on the menu bar, which is the shell's and is already
// there.
wrap_context_menu_handler! {
    pub struct RemotexContextMenuHandler {}

    impl ContextMenuHandler {
        fn on_before_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
        ) {
            // An empty model is not shown at all, which is the documented way to
            // suppress the menu without also suppressing the click.
            if let Some(model) = model {
                model.clear();
            }
            crate::trace!("context menu suppressed");
        }
    }
}

// What the page says about itself. Nothing acts on it: it is `REMOTEX_CEF_TRACE`
// and nothing else, because the failures worth catching here — a subresource that
// never loads, a script that throws before it can report anything — are exactly
// the ones that look like an empty window from outside.
wrap_display_handler! {
    pub struct RemotexDisplayHandler {}

    impl DisplayHandler {
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            _level: LogSeverity,
            message: Option<&CefString>,
            source: Option<&CefString>,
            line: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            crate::trace!(
                "console: {} ({}:{line})",
                message.map(|text| text.to_string()).unwrap_or_default(),
                source.map(|text| text.to_string()).unwrap_or_default()
            );
            // 0 so it still reaches the inspector and Chromium's own log.
            0
        }
    }
}

wrap_load_handler! {
    pub struct RemotexLoadHandler {}

    impl LoadHandler {
        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            crate::trace!(
                "load failed: {} ({error_code:?}) {}",
                failed_url.map(|text| text.to_string()).unwrap_or_default(),
                error_text.map(|text| text.to_string()).unwrap_or_default()
            );
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            status_code: ::std::os::raw::c_int,
        ) {
            crate::trace!("load ended: {status_code}");
        }
    }
}

wrap_request_handler! {
    pub struct RemotexRequestHandler {
        router: Arc<BrowserSideRouter>,
    }

    impl RequestHandler {
        fn on_before_browse(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let url = request
                .map(|request| CefString::from(&request.url()).to_string())
                .unwrap_or_default();
            // The router is told about every browse, whatever the verdict: it
            // cancels the queries the old document had in flight.
            self.router
                .on_before_browse(browser.map(|b| b.clone()), frame.map(|f| f.clone()));
            let allowed = permits(&url);
            crate::trace!("browse {url} -> {}", if allowed { "allowed" } else { "REFUSED" });
            // 1 cancels the navigation, 0 allows it.
            i32::from(!allowed)
        }

        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            _status: TerminationStatus,
            error_code: ::std::os::raw::c_int,
            error_string: Option<&CefString>,
        ) {
            crate::trace!(
                "render process gone: {error_code} {}",
                error_string.map(|text| text.to_string()).unwrap_or_default()
            );
            self.router
                .on_render_process_terminated(browser.map(|b| b.clone()));
        }
    }
}

/// Build the client, and with it the router that carries the page's events out.
///
/// The router is not handed back. Every hook it needs is one of the client's own —
/// the life span, request and process-message handlers above all forward to it —
/// and the client holds the only reference there is any use for.
pub fn client(browser: BrowserSlot, deliver: crate::MessageSink) -> Client {
    let router = BrowserSideRouter::new(crate::app::router_config());
    router.add_handler(Arc::new(PageMessages { deliver }), false);
    RemotexClient::new(browser, router)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These are the cases `NativeBridgeTests` used to make against a `file://`
    /// document, moved down to where the rule now lives.
    #[test]
    fn only_the_client_may_be_navigated_to() {
        assert!(permits("remotex://app/index.html"));
        assert!(permits("remotex://app/assets/index-abc.js"));
        assert!(permits("remotex://app/index.html?x=1#y"));
        assert!(permits("remotex://app"));
    }

    #[test]
    fn anywhere_else_is_refused() {
        for url in [
            "https://evil.example/",
            "http://127.0.0.1:52888/",
            "file:///etc/passwd",
            // The host is the whole point of the test: a second host under our own
            // scheme is not our page.
            "remotex://elsewhere/index.html",
            "remotex://app.evil.example/",
            "",
            "javascript:alert(1)",
        ] {
            assert!(!permits(url), "{url} must not be navigable");
        }
    }
}
