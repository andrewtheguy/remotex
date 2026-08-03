//! `remotex://app/…` — the client, served out of the app's own bundle.
//!
//! **Why a scheme and not `file://`.** The page needs an origin that holds still,
//! because the client's three remembered preferences live in `localStorage` and
//! `localStorage` is keyed by origin. An ephemeral loopback port is a new origin
//! at every launch, and a `file://` document has no origin at all — it serializes
//! to `null`, which is also what every sandboxed frame on the web sends. A scheme
//! registered here is neither: `remotex://app` is one origin, the same one every
//! launch, and nobody else's.
//!
//! It is registered **standard, secure, CORS-enabled and fetch-enabled**, and all
//! four are load bearing. Standard makes it an origin at all rather than an opaque
//! blob. Secure makes it a secure context, which is what WebCodecs requires — and
//! the client's video and audio decode through WebCodecs. CORS-enabled and
//! fetch-enabled are what let the page reach the gateway on loopback, which is a
//! different origin and answers it by name (`SHELL_ORIGIN` in `src/server.rs`).
//!
//! The registration must happen in **every** process, which is why
//! [`register_schemes`] is called from the browser process's app and from the
//! helper's alike: a renderer that has not been told `remotex` is standard would
//! disagree with the browser about what the page's origin even is.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use cef::rc::Rc as _;
use cef::*;

/// The scheme, and the one host under it.
pub const SCHEME: &str = "remotex";
pub const HOST: &str = "app";

/// The page's URL. One document, always this one.
pub fn index_url() -> String {
    format!("{SCHEME}://{HOST}/index.html")
}

/// The origin the gateway answers cross-origin. Must equal `SHELL_ORIGIN` in
/// `src/server.rs`; the test below is what keeps the two from drifting.
pub fn origin() -> String {
    format!("{SCHEME}://{HOST}")
}

/// Tell this process that `remotex` is a real scheme. Both processes, always.
pub fn register_schemes(registrar: Option<&mut SchemeRegistrar>) {
    let Some(registrar) = registrar else {
        return;
    };
    let options = sys::cef_scheme_options_t::CEF_SCHEME_OPTION_STANDARD as i32
        | sys::cef_scheme_options_t::CEF_SCHEME_OPTION_SECURE as i32
        | sys::cef_scheme_options_t::CEF_SCHEME_OPTION_CORS_ENABLED as i32
        | sys::cef_scheme_options_t::CEF_SCHEME_OPTION_FETCH_ENABLED as i32;
    let registered = registrar.add_custom_scheme(Some(&CefString::from(SCHEME)), options);
    crate::trace!(
        "register {SCHEME} = {registered} in {}",
        std::env::args()
            .find(|arg| arg.starts_with("--type="))
            .unwrap_or_else(|| "--type=browser".to_owned())
    );
}

/// What a request under the scheme resolves to, and what to answer with.
struct Asset {
    body: Vec<u8>,
    mime: &'static str,
}

/// Read the file a `remotex://app/…` path names, refusing anything outside the
/// web root.
///
/// The traversal check is not decoration. The page is the only thing that should
/// be able to ask for a file, but a remote desktop is a stream of somebody else's
/// pixels and text, and this handler answers whatever URL the document ends up
/// asking for — so `../../..` is resolved and compared rather than trusted.
fn load(web_root: &Path, path: &str, gateway_origin: &str) -> Option<Asset> {
    let relative = path.trim_start_matches('/');
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    let candidate = web_root.join(relative);
    let resolved = candidate.canonicalize().ok()?;
    let root = web_root.canonicalize().ok()?;
    if !resolved.starts_with(&root) {
        return None;
    }
    let mime = mime_for(&resolved);
    let mut body = std::fs::read(&resolved).ok()?;
    if resolved.file_name().is_some_and(|name| name == "index.html") {
        body = inject_gateway(&body, gateway_origin);
    }
    Some(Asset { body, mime })
}

/// Put `window.__remotexGateway` in the document before any of the client's own
/// script.
///
/// This is the one thing the page cannot work out for itself. A browser loads the
/// client *from* its gateway, so every path can be relative; this page was loaded
/// from the bundle, so it is told once, here, where the backend is —
/// `frontend/src/gateway.ts` reads the global at module load and never again.
///
/// Injected into the bytes as they are served rather than evaluated afterwards,
/// because "afterwards" is too late: the client's script would already have read
/// an unset global. This is what `WKUserScript` at `.atDocumentStart` used to do.
///
/// The origin goes through JSON rather than into the source text raw, for the same
/// reason every command to the page does — a string in a JavaScript source is data,
/// however sure we are of its shape.
fn inject_gateway(body: &[u8], gateway_origin: &str) -> Vec<u8> {
    let encoded = serde_json::to_string(gateway_origin).unwrap_or_else(|_| "\"\"".to_owned());
    // `<` as well as the quoting JSON already did, and the second is not the same
    // job as the first. JSON escaping makes the value a valid JavaScript string;
    // it says nothing about HTML, and the HTML parser reaches `</script>` before
    // any JavaScript parser sees the text at all — so a `<` left alone could close
    // the element the string is inside. `<` is the same character to
    // JavaScript and not a tag to the parser above it.
    let encoded = encoded.replace('<', "\\u003C");
    let script = format!("<script>window.__remotexGateway={encoded};</script>");
    let text = String::from_utf8_lossy(body);
    match text.find("</head>") {
        Some(at) => {
            let mut out = String::with_capacity(text.len() + script.len());
            out.push_str(&text[..at]);
            out.push_str(&script);
            out.push_str(&text[at..]);
            out.into_bytes()
        }
        // No `</head>` is not a document this build produces, so rather than
        // guess at where the script belongs, serve the page unchanged and let it
        // fail loudly at its first request — `gateway.ts` resolves to the string
        // "null" and every call fails at once, which is far easier to read than a
        // script silently placed after the one that needed it.
        None => body.to_vec(),
    }
}

/// The mime type alone, **without** a `; charset=` on the end.
///
/// `CefResponse` has a field for each, and putting them in one string is not a
/// harmless nicety: Chromium compares the mime type exactly, so
/// `text/html; charset=utf-8` is not `text/html` and the client is rendered as its
/// own source code. The charset goes through `set_charset`; see [`charset_for`].
fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html",
        Some("js") | Some("mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/vnd.microsoft.icon",
        _ => "application/octet-stream",
    }
}

/// The charset for a text format, and nothing for the rest. `bun run build` writes
/// UTF-8 and the injected script is UTF-8 by construction.
fn charset_for(mime: &str) -> Option<&'static str> {
    let text = mime.starts_with("text/")
        || mime == "application/json"
        || mime == "image/svg+xml";
    text.then_some("utf-8")
}

// Answers every `remotex://app/…` request out of `web_root`.
wrap_scheme_handler_factory! {
    pub struct AssetFactory {
        web_root: PathBuf,
    }

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let url = request
                .map(|request| CefString::from(&request.url()).to_string())
                .unwrap_or_default();
            let path = url
                .split_once("://")
                .and_then(|(_, rest)| rest.split_once('/'))
                .map(|(_, path)| path.split(['?', '#']).next().unwrap_or("").to_owned())
                .unwrap_or_default();
            // A path that resolves to nothing becomes an empty body, which
            // `response_headers` answers as a 404. Refusing to make a handler at
            // all would let CEF fall back to its own network stack, which for a
            // scheme only this process knows about means a blank window with
            // nothing said.
            let asset = load(&self.web_root, &path, &crate::gateway_origin());
            let missing = asset.is_none();
            crate::trace!(
                "serve {url} -> {}",
                match &asset {
                    Some(asset) => format!("{} bytes, {}", asset.body.len(), asset.mime),
                    None => format!("404 (no {path:?} under {:?})", self.web_root),
                }
            );
            let (body, mime) = asset
                .map(|asset| (asset.body, asset.mime))
                .unwrap_or_else(|| (Vec::new(), "text/plain"));
            Some(AssetHandler::new(
                RefCell::new(body),
                RefCell::new(0),
                mime.to_owned(),
                missing,
            ))
        }
    }
}

// One response, already in memory. The whole client is a few hundred KB and
// it is on local disk, so streaming would buy nothing but states to get wrong.
wrap_resource_handler! {
    pub struct AssetHandler {
        body: RefCell<Vec<u8>>,
        offset: RefCell<usize>,
        mime: String,
        missing: bool,
    }

    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            // Answered immediately rather than through the callback: the bytes are
            // already here.
            if let Some(handle_request) = handle_request {
                *handle_request = 1;
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            if let Some(response) = response {
                response.set_status(if self.missing { 404 } else { 200 });
                response.set_mime_type(Some(&CefString::from(self.mime.as_str())));
                if let Some(charset) = charset_for(&self.mime) {
                    response.set_charset(Some(&CefString::from(charset)));
                }
            }
            if let Some(response_length) = response_length {
                *response_length = self.body.borrow().len() as i64;
            }
            crate::trace!("headers: {} bytes of {}", self.body.borrow().len(), self.mime);
        }

        // The raw pointer is CEF's signature, not a choice: the macro implements
        // `ImplResourceHandler` and this is the shape it must have. Its one
        // dereference is bounded by `bytes_to_read` below.
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            let body = self.body.borrow();
            let mut offset = self.offset.borrow_mut();
            let remaining = body.len().saturating_sub(*offset);
            let count = remaining.min(bytes_to_read.max(0) as usize);
            if count > 0 {
                // SAFETY: CEF hands us a buffer of at least `bytes_to_read` bytes,
                // and `count` is bounded by it and by what is left of the body.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        body.as_ptr().add(*offset),
                        data_out,
                        count,
                    );
                }
                *offset += count;
            }
            if let Some(bytes_read) = bytes_read {
                *bytes_read = count as ::std::os::raw::c_int;
            }
            crate::trace!("read: asked {bytes_to_read}, gave {count}, at {}", *offset);
            // Zero read with nothing left is end of stream, which is a 0 return;
            // anything copied is a 1.
            i32::from(count > 0)
        }
    }
}

/// Build the factory that serves `web_root`, and the handle that lets the origin
/// be set once the gateway's port is known.
pub fn asset_factory(web_root: PathBuf) -> SchemeHandlerFactory {
    AssetFactory::new(web_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mime type with the charset stuck on the end is not the mime type
    /// Chromium is comparing against, and the symptom is not an error: the client
    /// is served, loads, and renders as its own source code in a window that looks
    /// exactly like a working one.
    #[test]
    fn a_mime_type_never_carries_its_charset() {
        for name in ["index.html", "app.js", "app.css", "config.json", "logo.svg"] {
            let mime = mime_for(Path::new(name));
            assert!(!mime.contains(';'), "{name} -> {mime}");
            assert_eq!(charset_for(mime), Some("utf-8"), "{name} -> {mime}");
        }
        assert_eq!(mime_for(Path::new("index.html")), "text/html");
        // Binary formats say nothing about a charset, because they have none.
        assert_eq!(charset_for(mime_for(Path::new("icon.png"))), None);
        assert_eq!(charset_for(mime_for(Path::new("body.woff2"))), None);
    }

    /// The gateway answers exactly one origin cross-origin, and this is where that
    /// string is spelled on the other side. They are two constants in two languages
    /// and nothing but this checks that they agree.
    #[test]
    fn the_origin_matches_what_the_gateway_answers() {
        assert_eq!(origin(), "remotex://app");
        assert_eq!(index_url(), "remotex://app/index.html");
    }

    #[test]
    fn the_gateway_origin_reaches_the_document_before_its_own_script() {
        let html = b"<html><head><title>t</title></head><body><script src=\"a.js\" defer></script></body></html>";
        let out = String::from_utf8(inject_gateway(html, "http://127.0.0.1:49213")).unwrap();
        let injected = out.find("__remotexGateway").expect("global was not injected");
        let client = out.find("a.js").expect("client script vanished");
        assert!(
            injected < client,
            "gateway.ts reads the global at module load, so it must be set first"
        );
        assert!(out.contains(r#"window.__remotexGateway="http://127.0.0.1:49213";"#));
    }

    /// The origin is data, not source text: a quote in it must not be able to end
    /// the string it is inside.
    #[test]
    fn a_quote_in_the_origin_cannot_end_the_string() {
        let html = b"<html><head></head><body></body></html>";
        let out = String::from_utf8(inject_gateway(html, "http://x\";alert(1);//")).unwrap();
        assert!(
            out.contains(r#"window.__remotexGateway="http://x\";alert(1);//";"#),
            "the quote must arrive escaped, inside the string: {out}"
        );
    }

    /// And the layer above JavaScript, which is the one JSON knows nothing about.
    /// The HTML parser finds `</script>` before any script is parsed, so a `<` left
    /// alone closes the element the string is sitting in — escaping the quotes does
    /// not help at all.
    #[test]
    fn a_closing_tag_in_the_origin_cannot_end_the_script_element() {
        let html = b"<html><head></head><body></body></html>";
        let hostile = "http://x</script><script>alert(1)</script>";
        let out = String::from_utf8(inject_gateway(html, hostile)).unwrap();
        assert_eq!(
            out.matches("</script>").count(),
            1,
            "only the injected element's own closing tag may appear: {out}"
        );
        assert!(out.contains(r"</script>"), "{out}");
    }

    #[test]
    fn a_path_outside_the_web_root_is_refused() {
        let root = std::env::temp_dir().join("remotex-cef-scheme-test");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.html"), "<html><head></head></html>").unwrap();
        assert!(load(&root, "/index.html", "http://127.0.0.1:1").is_some());
        assert!(
            load(&root, "/../../../etc/passwd", "http://127.0.0.1:1").is_none(),
            "a document must not be able to read outside the bundle"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
