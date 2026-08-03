//! Chromium for `remotex.app`, behind a C ABI the Swift shell calls.
//!
//! The shell keeps everything a web page has no business owning — the window, the
//! menu bar, the `NSEvent` monitor, `NSPasteboard`, **Resize to Display** — and
//! this crate keeps everything about the engine. Between them is
//! `include/remotex_cef.h`, and it is deliberately the same shape the `WKWebView`
//! seam was: seed a cookie, show one page, evaluate one string, hand back what the
//! page posts.
//!
//! Nothing about the session is here. No claim, no socket, no wire format, no
//! protocol version — that is all the client's, and the client is the same build a
//! browser loads. A protocol change is a change to the SPA and to nothing in this
//! crate.
//!
//! **Threading.** Everything below runs on the main thread. CEF's browser process
//! puts its UI thread there and AppKit is already there, so there is one thread
//! rather than two that have to agree — which is why the state here is
//! `thread_local!` and nothing is a `Mutex`.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;
use std::rc::Rc as StdRc;

use cef::*;

pub mod app;
pub mod client;
pub mod scheme;

/// How long a cookie the shell seeds is good for. Long, because it is replaced at
/// every launch by the token that launch minted: the gateway that would accept an
/// older one no longer exists.
const COOKIE_LIFETIME_DAYS: i64 = 365;

// ---------------------------------------------------------------------------
// Callbacks out to the shell
// ---------------------------------------------------------------------------

/// The shell's "run the pump in N milliseconds" hook.
#[derive(Clone)]
pub struct PumpScheduler {
    schedule: extern "C" fn(i64, *mut c_void),
    context: *mut c_void,
}

impl PumpScheduler {
    pub fn schedule(&self, delay_ms: i64) {
        (self.schedule)(delay_ms, self.context);
    }
}

/// The shell's "here is what the page posted" hook.
#[derive(Clone)]
pub struct MessageSink {
    deliver: extern "C" fn(*const c_char, *mut c_void),
    context: *mut c_void,
}

// The message router requires its handlers to be `Send + Sync`, because a router
// may in general be shared across CEF's threads. This one is not: CEF delivers
// query callbacks on the UI thread, which in this process is the main thread and
// the only thread that ever holds a `MessageSink`. The pointer inside is the
// shell's `NativeBridge`, which lives on that same thread.
//
// Asserted rather than assumed — `deliver` checks it in a debug build, so a future
// CEF that moved this callback would fail loudly here instead of handing an AppKit
// object to the wrong thread.
unsafe impl Send for MessageSink {}
unsafe impl Sync for MessageSink {}

impl MessageSink {
    pub fn deliver(&self, json: &str) {
        debug_assert_ne!(
            currently_on(ThreadId::UI),
            0,
            "the shell's bridge is main-thread only"
        );
        let Ok(encoded) = std::ffi::CString::new(json) else {
            // A NUL inside the JSON cannot have come from `JSON.stringify`, so
            // this is not a case to recover from — it is one to not crash on.
            return;
        };
        (self.deliver)(encoded.as_ptr(), self.context);
    }
}

// ---------------------------------------------------------------------------
// Process-wide state
// ---------------------------------------------------------------------------

thread_local! {
    /// True once `on_context_initialized` has run, which is the first moment a
    /// browser may be created.
    static CONTEXT_READY: Cell<bool> = const { Cell::new(false) };
    /// A create that arrived before the context was ready, waiting for it.
    static PENDING_CREATE: RefCell<Option<PendingCreate>> = const { RefCell::new(None) };
    /// The gateway origin the scheme handler injects into `index.html`.
    static GATEWAY_ORIGIN: RefCell<String> = const { RefCell::new(String::new()) };
}

struct PendingCreate {
    parent_view: *mut c_void,
    handle: *mut RemotexCefBrowser,
}

pub(crate) fn mark_context_ready() {
    CONTEXT_READY.with(|ready| ready.set(true));
    let pending = PENDING_CREATE.with(|slot| slot.borrow_mut().take());
    if let Some(pending) = pending {
        // SAFETY: the handle was leaked by `remotex_cef_create` and nothing has
        // freed it — `remotex_cef_close` is the only thing that can, and the shell
        // has not been given a chance to call it yet.
        unsafe { spawn_browser(pending.parent_view, pending.handle) };
    }
}

/// One browser. The handle the shell holds is a pointer to this.
pub struct RemotexCefBrowser {
    browser: client::BrowserSlot,
    client: RefCell<Option<Client>>,
    router: RefCell<Option<std::sync::Arc<cef::wrapper::message_router::BrowserSideRouter>>>,
    deliver: MessageSink,
}

// ---------------------------------------------------------------------------
// The C ABI
// ---------------------------------------------------------------------------

/// Read a C string, or an empty string when it is null or not UTF-8.
///
/// # Safety
/// `text` must be null or a valid NUL-terminated C string.
unsafe fn string_from(text: *const c_char) -> String {
    if text.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(text) }
        .to_str()
        .unwrap_or_default()
        .to_owned()
}

/// # Safety
/// `web_root` and `cache_dir` must be valid C strings; `schedule` must remain
/// callable for the life of the process.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remotex_cef_initialize(
    web_root: *const c_char,
    cache_dir: *const c_char,
    schedule: Option<extern "C" fn(i64, *mut c_void)>,
    schedule_context: *mut c_void,
) -> bool {
    let web_root = PathBuf::from(unsafe { string_from(web_root) });
    let cache_dir = unsafe { string_from(cache_dir) };

    // The framework is loaded out of the bundle beside this executable, and the
    // API version is pinned before anything else touches CEF.
    let loader = library_loader::LibraryLoader::new(
        &std::env::current_exe().unwrap_or_default(),
        false,
    );
    if !loader.load() {
        return false;
    }
    std::mem::forget(loader);
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let scheduler = schedule.map(|schedule| PumpScheduler {
        schedule,
        context: schedule_context,
    });
    let mut app = app::browser_app(web_root, String::new(), scheduler);

    let settings = Settings {
        // The shell owns the run loop — it is an AppKit app — so CEF is pumped
        // rather than given `[NSApp run]`. `on_schedule_message_pump_work` is the
        // other half of this one flag.
        external_message_pump: 1,
        // The browser process passes a null `sandbox_info` on macOS and merely
        // says the sandbox is on; entering it is the helper's job, which is why
        // only `remotex-cef-helper` links `cef_sandbox`.
        no_sandbox: 0,
        // This instance's profile, and the reason it is not a default location:
        // the client's three remembered preferences live in `localStorage` here,
        // so `--instance-dir` isolating an instance has to isolate these too.
        cache_path: CefString::from(cache_dir.as_str()),
        root_cache_path: CefString::from(cache_dir.as_str()),
        // The launch token goes in as a session cookie in every sense but this
        // one: it must outlive the process, or a relaunch is unauthenticated.
        persist_session_cookies: 1,
        ..Default::default()
    };

    let args = args::Args::new();
    initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) == 1
}

#[unsafe(no_mangle)]
pub extern "C" fn remotex_cef_do_work() {
    do_message_loop_work();
}

/// # Safety
/// `gateway_origin` and `token` must be valid C strings; `done` is called once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remotex_cef_set_cookie(
    gateway_origin: *const c_char,
    token: *const c_char,
    done: Option<extern "C" fn(*mut c_void)>,
    done_context: *mut c_void,
) {
    let origin = unsafe { string_from(gateway_origin) };
    let token = unsafe { string_from(token) };
    GATEWAY_ORIGIN.with(|slot| *slot.borrow_mut() = origin.clone());

    let host = origin
        .split_once("://")
        .map(|(_, rest)| rest.split(':').next().unwrap_or(rest).to_owned())
        .unwrap_or_default();

    let cookie = Cookie {
        name: CefString::from("remotex_session"),
        value: CefString::from(token.as_str()),
        domain: CefString::from(host.as_str()),
        path: CefString::from("/"),
        // Both of these, and neither is optional. `remotex://app` and
        // `http://127.0.0.1:<port>` are different sites, so a `Lax` cookie is
        // simply not sent on the page's own `fetch` and `ws://` calls — the
        // request then arrives *unauthenticated*, which surfaces as a mysterious
        // 401 rather than as anything about cookies. `None` requires `Secure`,
        // and Chromium allows a `Secure` cookie here because a loopback address
        // is a trustworthy origin even over plain HTTP.
        same_site: CookieSameSite::from(sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_NO_RESTRICTION),
        secure: 1,
        httponly: 0,
        has_expires: 1,
        expires: expiry_in_days(COOKIE_LIFETIME_DAYS),
        ..Default::default()
    };

    let mut manager = cookie_manager_get_global_manager(None);
    if let Some(manager) = manager.as_mut() {
        manager.set_cookie(
            Some(&CefString::from(origin.as_str())),
            Some(&cookie),
            None,
        );
    }
    // Answered synchronously. `set_cookie` without a callback still queues the
    // write on the IO thread, but every read of it goes through the same queue —
    // so the load below cannot overtake it, which is the ordering the callback
    // existed to guarantee.
    if let Some(done) = done {
        done(done_context);
    }
}

fn expiry_in_days(days: i64) -> Basetime {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
        + days * 24 * 60 * 60;
    Basetime {
        val: seconds * 1_000_000,
    }
}

/// # Safety
/// `parent_view` must be an `NSView *` that outlives the browser, and
/// `gateway_origin` a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remotex_cef_create(
    parent_view: *mut c_void,
    gateway_origin: *const c_char,
    on_message: Option<extern "C" fn(*const c_char, *mut c_void)>,
    on_message_context: *mut c_void,
) -> *mut RemotexCefBrowser {
    let origin = unsafe { string_from(gateway_origin) };
    if !origin.is_empty() {
        GATEWAY_ORIGIN.with(|slot| *slot.borrow_mut() = origin);
    }
    let Some(on_message) = on_message else {
        return std::ptr::null_mut();
    };

    let handle = Box::into_raw(Box::new(RemotexCefBrowser {
        browser: StdRc::new(RefCell::new(None)),
        client: RefCell::new(None),
        router: RefCell::new(None),
        deliver: MessageSink {
            deliver: on_message,
            context: on_message_context,
        },
    }));

    if CONTEXT_READY.with(|ready| ready.get()) {
        unsafe { spawn_browser(parent_view, handle) };
    } else {
        // The shell may put its view on screen before Chromium finishes coming
        // up. Held rather than refused, so the caller has one code path.
        PENDING_CREATE.with(|slot| {
            *slot.borrow_mut() = Some(PendingCreate {
                parent_view,
                handle,
            })
        });
    }
    handle
}

/// # Safety
/// `handle` must come from `remotex_cef_create` and still be live.
unsafe fn spawn_browser(parent_view: *mut c_void, handle: *mut RemotexCefBrowser) {
    let entry = unsafe { &*handle };
    let (mut cef_client, router) = client::client(entry.browser.clone(), entry.deliver.clone());
    *entry.router.borrow_mut() = Some(router);
    *entry.client.borrow_mut() = Some(cef_client.clone());

    let window_info = WindowInfo::default().set_as_child(
        parent_view.cast(),
        &Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
    );
    let url = CefString::from(scheme::index_url().as_str());
    let settings = BrowserSettings::default();
    browser_host_create_browser(
        Some(&window_info),
        Some(&mut cef_client),
        Some(&url),
        Some(&settings),
        None,
        None,
    );
}

/// # Safety
/// `browser` must come from `remotex_cef_create`, and `script` be a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remotex_cef_execute(
    browser: *mut RemotexCefBrowser,
    script: *const c_char,
) {
    if browser.is_null() {
        return;
    }
    let script = unsafe { string_from(script) };
    let entry = unsafe { &*browser };
    // Dropped silently before the first load, which is a menu item pressed while
    // the window is still empty.
    let Some(frame) = entry
        .browser
        .borrow()
        .as_ref()
        .and_then(|browser| browser.main_frame())
    else {
        return;
    };
    frame.execute_java_script(
        Some(&CefString::from(script.as_str())),
        Some(&CefString::from(scheme::index_url().as_str())),
        0,
    );
}

/// # Safety
/// `browser` must come from `remotex_cef_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remotex_cef_resize(
    browser: *mut RemotexCefBrowser,
    width: f64,
    height: f64,
) {
    if browser.is_null() {
        return;
    }
    let entry = unsafe { &*browser };
    let Some(host) = entry
        .browser
        .borrow()
        .as_ref()
        .and_then(|browser| browser.host())
    else {
        return;
    };
    let _ = (width, height);
    // The child view is laid out by AppKit; this only tells Chromium that its
    // window changed, which is what makes it re-read the size and repaint.
    host.notify_move_or_resize_started();
    host.was_resized();
}

/// # Safety
/// `browser` must come from `remotex_cef_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remotex_cef_show_dev_tools(browser: *mut RemotexCefBrowser) {
    #[cfg(debug_assertions)]
    {
        if browser.is_null() {
            return;
        }
        let entry = unsafe { &*browser };
        let Some(host) = entry
            .browser
            .borrow()
            .as_ref()
            .and_then(|browser| browser.host())
        else {
            return;
        };
        let window_info = WindowInfo::default();
        host.show_dev_tools(Some(&window_info), None, None, None);
    }
    #[cfg(not(debug_assertions))]
    let _ = browser;
}

/// # Safety
/// `browser` must come from `remotex_cef_create`; it is invalid afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remotex_cef_close(browser: *mut RemotexCefBrowser) {
    if browser.is_null() {
        return;
    }
    let entry = unsafe { Box::from_raw(browser) };
    if let Some(host) = entry
        .browser
        .borrow()
        .as_ref()
        .and_then(|browser| browser.host())
    {
        host.close_browser(1);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn remotex_cef_shutdown() {
    shutdown();
}

// The subprocess's entry point is deliberately **not** here, even though it would
// read better beside the rest. This crate is compiled as a `staticlib` and linked
// into the Swift app, and a staticlib keeps every public symbol — so a public
// `run_helper_process` would drag `cef::sandbox` into the app as well, and with it
// an undefined `cef_sandbox_initialize` that the browser process has no business
// resolving. The helper's `main.rs` therefore calls `app::helper_app` itself.
//
// What the two processes must agree about — the scheme and the message router's
// configuration — is still shared, which was the point of putting it here.
