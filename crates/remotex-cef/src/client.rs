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
            *self.browser.borrow_mut() = browser.map(|browser| browser.clone());
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
            // 1 cancels the navigation, 0 allows it.
            i32::from(!permits(&url))
        }

        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            _status: TerminationStatus,
            _error_code: ::std::os::raw::c_int,
            _error_string: Option<&CefString>,
        ) {
            self.router
                .on_render_process_terminated(browser.map(|b| b.clone()));
        }
    }
}

/// Build the client, its browser slot and the router that carries the page's
/// events out.
pub fn client(browser: BrowserSlot, deliver: crate::MessageSink) -> (Client, Arc<BrowserSideRouter>) {
    let router = BrowserSideRouter::new(crate::app::router_config());
    router.add_handler(Arc::new(PageMessages { deliver }), false);
    (
        RemotexClient::new(browser, router.clone()),
        router,
    )
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
