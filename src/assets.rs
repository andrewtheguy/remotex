//! The browser client, compiled into the gateway.
//!
//! `build.rs` writes the bundle to Cargo's `OUT_DIR` and every file in it becomes
//! bytes in the binary, so `remotex` is one file wherever it runs: no web root to
//! install beside it, no `[server]` key to point at one, and no launcher argument
//! for a managed worker. The build refuses to continue without the bundle, which
//! is where "the web UI will 404" used to be a warning at start-up.
//!
//! Vite names every asset by its content hash and only `index.html` keeps a stable
//! name, so each embedded file's hash is also its `ETag`: a browser that already
//! holds an asset revalidates it for a 304 instead of downloading it again, and a
//! redeployed gateway with a changed index answers with a fresh document.
//!
//! Every file goes out with the two headers that make the page cross-origin
//! isolated (COOP `same-origin`, COEP `require-corp`), the document for itself and
//! each worker's script for its worker: isolation is what gives the page
//! `SharedArrayBuffer`, which the graphics compositor's threads share their memory
//! through (`frontend/src/egfxCompositor.ts`), as the software HEVC decoder's do.
//! It costs the page nothing, since all it loads is this origin's.
//!
//! BETA: a gateway that has the archive also serves the software
//! HEVC decoder ([`crate::hevc_wasm`]), which it read at start-up, at `/hevc/` under
//! the names it was built with, which the page's decode worker and the decoder's
//! threads import the glue by. Without it `/hevc/` is a 404 and the
//! page, which asks for the decoder before choosing it, decodes as it did before.

use std::fmt::Write as _;

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::{EmbeddedFile, RustEmbed};

use crate::hevc_wasm::HevcDecoder;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/frontend-dist"]
struct Frontend;

const INDEX: &str = "index.html";

/// The document. Its presence is `build.rs`'s promise: the build fails without
/// `index.html`, so there is no gateway in which this is `None`.
fn index() -> EmbeddedFile {
    Frontend::get(INDEX).expect("build.rs verified the frontend index exists")
}

/// Serve the SPA: a real file as itself, and any other path as `index.html` with a
/// 200 so the page's own routes resolve. This is the router's fallback service,
/// so only paths no route claimed arrive here — `/api/*` has its own 404.
///
/// `decoder` is the software HEVC decoder a gateway that has it loaded, whose
/// files are served under `/hevc/`.
pub fn serve(decoder: Option<&HevcDecoder>, request: &Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = request.uri().path().trim_start_matches('/');
    let (body, content_type, etag) = if let Some(name) = path.strip_prefix("hevc/") {
        // Not the page: the decoder is looked for here, and a 200 with the
        // document would read as having found it.
        let Some((file, mime)) = decoder.and_then(|decoder| decoder.file(name)) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        (
            Body::from(file.data.clone()),
            HeaderValue::from_static(mime),
            quoted(&file.sha256),
        )
    } else {
        let file = match Frontend::get(path) {
            Some(file) if !path.is_empty() => file,
            _ => index(),
        };
        let (content_type, etag) = (content_type(&file), etag(&file));
        (Body::from(file.data), content_type, etag)
    };

    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .is_some_and(|held| *held == etag)
    {
        (ISOLATED, [(header::ETAG, etag)], StatusCode::NOT_MODIFIED).into_response()
    } else {
        (ISOLATED, [(header::CONTENT_TYPE, content_type), (header::ETAG, etag)], body).into_response()
    }
}

/// The headers that make the page cross-origin isolated, for its threads.
const ISOLATED: [(header::HeaderName, HeaderValue); 2] = [
    (
        header::HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    ),
    (
        header::HeaderName::from_static("cross-origin-embedder-policy"),
        HeaderValue::from_static("require-corp"),
    ),
];

/// A strong validator from the file's content hash, quoted as the header wants.
fn etag(file: &EmbeddedFile) -> HeaderValue {
    let mut hex = String::with_capacity(64);
    for byte in file.metadata.sha256_hash() {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    quoted(&hex)
}

fn quoted(hex: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{hex}\""))
        .expect("hex digits and quotes are a valid header value")
}

/// The content type from the file's extension. Vite writes UTF-8, and a text type
/// says so: a browser told `text/html` alone may guess a legacy encoding for the
/// login screen's non-ASCII branding.
fn content_type(file: &EmbeddedFile) -> HeaderValue {
    let mime = file.metadata.mimetype();
    let value = if mime.starts_with("text/") || mime == "application/javascript" {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_owned()
    };
    HeaderValue::from_str(&value).expect("mime_guess returns header-safe types")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str, if_none_match: Option<&HeaderValue>) -> Response {
        get_with(None, path, if_none_match)
    }

    fn get_with(
        decoder: Option<&HevcDecoder>,
        path: &str,
        if_none_match: Option<&HeaderValue>,
    ) -> Response {
        let mut request = Request::builder().uri(path);
        if let Some(held) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, held);
        }
        serve(decoder, &request.body(Body::empty()).unwrap())
    }

    async fn body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 24).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// The bundle Vite wrote is what is served: the document at `/`, and the
    /// hashed assets it references beside it.
    #[tokio::test]
    async fn the_document_and_its_assets_are_in_the_binary() {
        let response = get("/", None);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        let index = body(response).await;
        assert!(index.contains("<div id=\"root\">"), "{index}");

        let script = Frontend::iter()
            .find(|name| name.starts_with("assets/") && name.ends_with(".js"))
            .expect("the bundle has a script");
        let response = get(&format!("/{script}"), None);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        let stylesheet = Frontend::iter()
            .find(|name| name.ends_with(".css"))
            .expect("the bundle has a stylesheet");
        let response = get(&format!("/{stylesheet}"), None);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/css; charset=utf-8");
    }

    /// The document, a script a worker may start from, and a revalidation of
    /// either all say the page is isolated, with the decoder or without it.
    #[tokio::test]
    async fn every_answer_isolates_the_page() {
        let script = Frontend::iter()
            .find(|name| name.starts_with("assets/") && name.ends_with(".js"))
            .expect("the bundle has a script");
        let etag = get("/", None).headers()[header::ETAG].clone();
        for response in [get("/", None), get(&format!("/{script}"), None), get("/", Some(&etag))] {
            assert_eq!(response.headers()["cross-origin-opener-policy"], "same-origin");
            assert_eq!(response.headers()["cross-origin-embedder-policy"], "require-corp");
        }
    }

    /// With the decoder, its own files are served beside the page's, isolated as
    /// they are, revalidated or not.
    #[tokio::test]
    async fn the_decoder_is_served_beside_the_page() {
        let decoder = crate::hevc_wasm::tests::decoder();
        let decoder = Some(&decoder);
        let wasm = get_with(decoder, "/hevc/hevc.wasm", None);
        assert_eq!(wasm.status(), StatusCode::OK);
        assert_eq!(wasm.headers()[header::CONTENT_TYPE], "application/wasm");
        let wasm_etag = wasm.headers()[header::ETAG].clone();
        let script = get_with(decoder, "/hevc/hevc.js", None);
        assert_eq!(script.status(), StatusCode::OK);
        assert_eq!(
            script.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        let held = get_with(decoder, "/hevc/hevc.wasm", Some(&wasm_etag));
        assert_eq!(held.status(), StatusCode::NOT_MODIFIED);
        for response in [wasm, script, held] {
            assert_eq!(response.headers()["cross-origin-opener-policy"], "same-origin");
            assert_eq!(response.headers()["cross-origin-embedder-policy"], "require-corp");
        }
        assert_eq!(get_with(decoder, "/hevc/other", None).status(), StatusCode::NOT_FOUND);
        assert_eq!(body(get_with(decoder, "/hevc/hevc.js", None)).await, "export default 1;");
    }

    /// Without it, `/hevc/` is not found, rather than the document, so the page's
    /// question reads no.
    #[test]
    fn without_the_decoder_there_is_no_decoder() {
        for path in ["/hevc/hevc.js", "/hevc/hevc.wasm"] {
            assert_eq!(get(path, None).status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    /// A path that is not a file is the page, with a 200: the SPA's own routes have
    /// to load as the document, and a directory or a traversal is not a file either.
    #[tokio::test]
    async fn every_other_path_is_the_document() {
        for path in [
            "/login",
            "/assets/",
            "/assets/../index.html",
            "/no/such/thing",
        ] {
            let response = get(path, None);
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/html; charset=utf-8",
                "{path}"
            );
            assert!(body(response).await.contains("<div id=\"root\">"), "{path}");
        }
    }

    /// The `ETag` is the content hash, so a browser holding the file gets a 304 for
    /// it and a full answer once the file has changed.
    #[tokio::test]
    async fn a_held_file_revalidates_to_not_modified() {
        let first = get("/", None);
        let etag = first.headers()[header::ETAG].clone();
        assert!(etag.to_str().unwrap().starts_with('"'), "{etag:?}");

        let revalidated = get("/", Some(&etag));
        assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(revalidated.headers()[header::ETAG], etag);
        assert!(body(revalidated).await.is_empty());

        let stale = get("/", Some(&HeaderValue::from_static("\"something-else\"")));
        assert_eq!(stale.status(), StatusCode::OK);
    }

    /// Nothing here takes a body: the page is read, never written to.
    #[tokio::test]
    async fn only_reads_are_answered() {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        assert_eq!(serve(None, &request).status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
