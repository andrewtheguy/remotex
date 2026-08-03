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
//! **Threading.** Almost everything below runs on the main thread. CEF's browser
//! process puts its UI thread there and AppKit is already there, so there is one
//! thread rather than two that have to agree — which is why the state here is
//! `thread_local!`. The one exception is [`GATEWAY_ORIGIN`], which a scheme
//! handler reads on the IO thread and which is therefore shared; see the note on
//! it, and do not reach for a lock for anything else.

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
}

/// The gateway origin the scheme handler injects into `index.html`.
///
/// Shared rather than `thread_local!`, and read at the moment a request is
/// answered rather than copied when the handler is made — because the two happen
/// on different threads and in the wrong order. CEF calls a scheme handler
/// factory on the **IO thread**, while the shell supplies this on the main thread;
/// and the factory is built during `on_context_initialized`, which is before
/// `remotex_cef_set_cookie` has said what the origin is. A copy taken then is
/// always the empty string, and an empty `window.__remotexGateway` is a client
/// that loads, paints its background, and then cannot reach anything.
static GATEWAY_ORIGIN: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

/// Whether to narrate what the engine is doing, on this process's stderr.
///
/// Off unless `REMOTEX_CEF_TRACE` is set, and worth having at all because the
/// interesting failures here are silent ones: a browser that is never created, a
/// scheme handler that is never asked, a navigation refused by our own policy. All
/// three look identical from the outside — an empty window — and none of them
/// reaches Chromium's log.
pub(crate) fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REMOTEX_CEF_TRACE").is_some())
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::tracing() {
            eprintln!("remotex-cef: {}", format_args!($($arg)*));
        }
    };
}
pub(crate) use trace;

/// What the page should be told its gateway is.
pub(crate) fn gateway_origin() -> String {
    GATEWAY_ORIGIN
        .read()
        .map(|origin| origin.clone())
        .unwrap_or_default()
}

fn set_gateway_origin(origin: &str) {
    if let Ok(mut slot) = GATEWAY_ORIGIN.write() {
        origin.clone_into(&mut slot);
    }
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
    /// Kept for as long as the browser is, because everything the page can reach
    /// this process through — the message router included — hangs off it.
    client: RefCell<Option<Client>>,
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
    trace!("initialize: web root {web_root:?}, profile {cache_dir:?}");
    let mut app = app::browser_app(web_root, scheduler);

    // Four fields, and every one of them is here because this app needs it rather
    // than because CEF wants telling.
    //
    // Nothing about *paths* is set. CEF derives the framework and the five helper
    // bundles from the main executable's own location and name, and it is right
    // about both — which is why the bundles are `remotex-viewer Helper (…)`, after
    // this binary, and not `remotex Helper (…)` after the product. Naming them here
    // instead was tried and made it worse: `browser_subprocess_path` covers only
    // the plain helper, the four role-suffixed ones are derived regardless, and a
    // `main_bundle_path` one directory out fails as every child refusing to launch
    // rather than as a bad path. See `build-viewer-app.sh`, which makes them.
    let settings = Settings {
        // The shell owns the run loop — it is an AppKit app — so CEF is pumped
        // rather than given `[NSApp run]`. `on_schedule_message_pump_work` is the
        // other half of this one flag.
        external_message_pump: 1,
        // This instance's profile, and the reason it is not the default location:
        // the client's three remembered preferences live in `localStorage` here,
        // so `--instance-dir` isolating an instance has to isolate these too.
        cache_path: CefString::from(cache_dir.as_str()),
        root_cache_path: CefString::from(cache_dir.as_str()),
        // Deliberately no `persist_session_cookies`. It governs cookies with no
        // expiry, and the launch token is written with one — and would not want
        // persisting anyway, since the gateway that minted it is gone by the next
        // launch and the shell sets a fresh one before the first page load.
        ..Default::default()
    };

    // This process's own argv, and nothing added to it. Chromium's switches are
    // appended in `on_before_command_line_processing` instead, which is where the
    // engine asks for them and what the shell's `ArgumentCheck` is then free to go
    // on refusing an unknown `--option` at the front door.
    //
    // `Args` owns the C strings CEF will read for the life of the process, so it
    // is leaked rather than dropped at the end of this function.
    let args = Box::leak(Box::new(args::Args::new()));
    let started = initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) == 1;
    trace!("initialize returned {started}");
    started
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
    set_gateway_origin(&origin);

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
    let queued = manager.as_mut().map(|manager| {
        manager.set_cookie(
            Some(&CefString::from(origin.as_str())),
            Some(&cookie),
            None,
        )
    });
    trace!("cookie for {origin} on host {host:?}: queued = {queued:?}");
    // Answered synchronously. `set_cookie` without a callback still queues the
    // write on the IO thread, but every read of it goes through the same queue —
    // so the load below cannot overtake it, which is the ordering the callback
    // existed to guarantee.
    if let Some(done) = done {
        done(done_context);
    }
}

/// A `Basetime` `days` from now.
///
/// Counted from CEF's own `basetime_now()` rather than from `SystemTime`, and that
/// is the whole point of the function. A `cef_basetime_t` is microseconds since
/// **base::Time's** epoch, not since the Unix one, and the two are 369 years apart —
/// so a Unix timestamp handed over as one is a date in the seventeenth century. A
/// cookie that expired in 1657 is not rejected loudly: it is accepted, dropped, and
/// the jar is simply empty afterwards, which reads exactly like a cookie that was
/// never set.
fn expiry_in_days(days: i64) -> Basetime {
    Basetime {
        val: basetime_now().val + days * 24 * 60 * 60 * 1_000_000,
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
        set_gateway_origin(&origin);
    }
    let Some(on_message) = on_message else {
        return std::ptr::null_mut();
    };

    let handle = Box::into_raw(Box::new(RemotexCefBrowser {
        browser: StdRc::new(RefCell::new(None)),
        client: RefCell::new(None),
        deliver: MessageSink {
            deliver: on_message,
            context: on_message_context,
        },
    }));

    if CONTEXT_READY.with(|ready| ready.get()) {
        unsafe { spawn_browser(parent_view, handle) };
    } else {
        trace!("create arrived before the context was ready; held");
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
    let startup = scheme::startup_url();
    trace!("creating a browser on {parent_view:?} for {startup}");
    let mut cef_client = client::client(entry.browser.clone(), entry.deliver.clone());
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
    let url = CefString::from(startup.as_str());
    let settings = BrowserSettings::default();
    let created = browser_host_create_browser(
        Some(&window_info),
        Some(&mut cef_client),
        Some(&url),
        Some(&settings),
        None,
        None,
    );
    trace!("browser_host_create_browser = {created}");
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

// There is deliberately no `remotex_cef_resize`. A windowed browser is an `NSView`
// under the shell's, and `client::fill_parent` gives it the autoresizing mask that
// makes AppKit keep it the size of its parent — so a resize is something the view
// system has already done by the time anyone could be told about it. The two calls
// that look like they belong here do not: `was_resized` and
// `notify_move_or_resize_started` are both *windowless* rendering, and CEF answers
// them on a windowed browser with "Incorrect usage" in its log and nothing else.

/// # Safety
/// `browser` must come from `remotex_cef_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remotex_cef_show_dev_tools(browser: *mut RemotexCefBrowser) {
    // Not gated on `debug_assertions`. A release build is the only build there is
    // to look at — the bundle QA runs and the bundle that ships are the same one —
    // and "why is the window blank" is answered by the inspector in a second and by
    // a rebuild in a minute. It shows nothing a browser pointed at the same gateway
    // would not; the client is a web page either way.
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
    // Its own window, which is what an empty `WindowInfo` means on macOS.
    let window_info = WindowInfo::default();
    host.show_dev_tools(Some(&window_info), None, None, None);
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
