// The seam between `remotex.app`'s Swift shell and the Chromium it shows the
// client in.
//
// Deliberately narrow, and narrow in the same shape the WKWebView it replaces
// was: put the launch token in the cookie jar, show one page, evaluate one
// string against it, and hand back whatever the page posts. Nothing about the
// session crosses here — no claim, no socket, no wire format — because that is
// all the client's, and the client is the same build a browser loads.
//
// Everything below must be called on the main thread. CEF's browser process
// puts its UI thread there and Swift's AppKit is already there, so "the main
// thread" is one thread and not two that agree.

#ifndef REMOTEX_CEF_H
#define REMOTEX_CEF_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/// One browser, showing the client. Opaque: everything about it is Rust's.
typedef struct RemotexCefBrowser RemotexCefBrowser;

/// Called with one `NativeEvent` as JSON, exactly as the page posted it.
///
/// Delivered on the main thread. The string belongs to the caller and is not
/// valid after this returns, so copy anything kept.
typedef void (*RemotexCefOnMessage)(const char *json, void *context);

/// Called when CEF wants its message pump run in `delay_ms` milliseconds.
///
/// The implementation must schedule `remotex_cef_do_work` asynchronously — a
/// GCD `asyncAfter`, not a direct call. Re-entering the pump from inside this
/// callback is the documented way to deadlock it.
typedef void (*RemotexCefScheduleWork)(int64_t delay_ms, void *context);

/// Called once the cookie is in the jar. See `remotex_cef_set_cookie`.
typedef void (*RemotexCefDone)(void *context);

/// Bring Chromium up. Once per process, before anything else here.
///
/// `web_root` is the directory holding the built SPA — `Contents/Resources/web`
/// — which is served as `remotex://app/`. `cache_dir` is where Chromium keeps
/// this instance's profile, including the `localStorage` the client's three
/// remembered preferences live in; it belongs under `--instance-dir` so that
/// isolating an instance isolates those too.
///
/// Returns false if CEF refused to start, which is terminal: there is no second
/// engine to fall back to.
bool remotex_cef_initialize(const char *web_root,
                            const char *cache_dir,
                            RemotexCefScheduleWork schedule,
                            void *schedule_context);

/// Run one slice of CEF's message pump. Only ever from the scheduler above.
void remotex_cef_do_work(void);

/// Put the gateway's launch token in the cookie jar, then call `done`.
///
/// The wait is not optional and the order is the whole of it: a page loaded
/// before the cookie is in the jar arrives unauthenticated, which the client
/// reports as a gateway that will not have it.
///
/// A cookie rather than a header because the requests that matter are not the
/// app's: the page issues its own `fetch` calls and opens its own `ws://`
/// sockets, and neither can be given a header from outside the document.
///
/// The cookie is written `SameSite=None; Secure`, and both halves are load
/// bearing. `remotex://app` and `http://127.0.0.1:<port>` are different sites,
/// so a `Lax` cookie would simply not be sent; `Secure` is what `None` requires,
/// and Chromium allows it here because a loopback address is a trustworthy
/// origin even over plain HTTP.
void remotex_cef_set_cookie(const char *gateway_origin,
                            const char *token,
                            RemotexCefDone done,
                            void *done_context);

/// Show the client in `parent_view` (an `NSView *`).
///
/// `gateway_origin` is the loopback origin the page should talk to, with no
/// trailing slash. It reaches the page as `window.__remotexGateway`, injected
/// into `index.html` as it is served — before any of the client's own script,
/// which is what `gateway.ts` requires and what a `WKUserScript` used to do.
///
/// Returns NULL if Chromium refused to create the browser.
RemotexCefBrowser *remotex_cef_create(void *parent_view,
                                      const char *gateway_origin,
                                      RemotexCefOnMessage on_message,
                                      void *on_message_context);

/// Evaluate `script` in the page. Dropped if the browser is not up yet, which
/// is a menu item pressed while the window is still empty.
void remotex_cef_execute(RemotexCefBrowser *browser, const char *script);

// There is deliberately nothing here about size. The browser is an `NSView` under
// the one passed to `remotex_cef_create`, with an autoresizing mask, so AppKit
// keeps it filling its parent and the shell has nothing to say about it.

/// Open Chromium's inspector on the page.
void remotex_cef_show_dev_tools(RemotexCefBrowser *browser);

/// Close the browser and forget it. The pointer is invalid afterwards.
void remotex_cef_close(RemotexCefBrowser *browser);

/// Take Chromium down. Once, on the way out.
void remotex_cef_shutdown(void);

#ifdef __cplusplus
}
#endif

#endif  // REMOTEX_CEF_H
