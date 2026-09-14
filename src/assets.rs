//! The browser client, compiled into the gateway.
//!
//! `frontend/dist` is read when this crate is built and every file in it becomes
//! bytes in the binary, so `remotex` is one file wherever it runs: no web root to
//! install beside it, no `[server]` key to point at one, and no launcher argument
//! for a managed worker. `build.rs` refuses to build without the bundle, which is
//! where "the web UI will 404" used to be a warning at start-up.
//!
//! Vite names every asset by its content hash and only `index.html` keeps a stable
//! name, so each embedded file's hash is also its `ETag`: a browser that already
//! holds an asset revalidates it for a 304 instead of downloading it again, and a
//! redeployed gateway with a changed index answers with a fresh document.

use std::fmt::Write as _;

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::{EmbeddedFile, RustEmbed};

#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct Frontend;

const INDEX: &str = "index.html";

/// The document. Its presence is `build.rs`'s promise: the build fails without
/// `frontend/dist/index.html`, so there is no gateway in which this is `None`.
fn index() -> EmbeddedFile {
    Frontend::get(INDEX).expect("build.rs verified frontend/dist/index.html exists")
}

/// Serve the SPA: a real file as itself, and any other path as `index.html` with a
/// 200 so the page's own routes resolve. This is the router's fallback service,
/// so only paths no route claimed arrive here — `/api/*` has its own 404.
pub async fn serve(request: Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = request.uri().path().trim_start_matches('/');
    let file = match Frontend::get(path) {
        Some(file) if !path.is_empty() => file,
        _ => index(),
    };

    let etag = etag(&file);
    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .is_some_and(|held| *held == etag)
    {
        return ([(header::ETAG, etag)], StatusCode::NOT_MODIFIED).into_response();
    }
    (
        [
            (header::CONTENT_TYPE, content_type(&file)),
            (header::ETAG, etag),
        ],
        Body::from(file.data),
    )
        .into_response()
}

/// A strong validator from the file's content hash, quoted as the header wants.
fn etag(file: &EmbeddedFile) -> HeaderValue {
    let mut tag = String::with_capacity(66);
    tag.push('"');
    for byte in file.metadata.sha256_hash() {
        write!(tag, "{byte:02x}").expect("writing to a String cannot fail");
    }
    tag.push('"');
    HeaderValue::from_str(&tag).expect("hex digits and quotes are a valid header value")
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

    async fn get(path: &str, if_none_match: Option<&HeaderValue>) -> Response {
        let mut request = Request::builder().uri(path);
        if let Some(held) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, held);
        }
        serve(request.body(Body::empty()).unwrap()).await
    }

    async fn body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 24).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// The bundle Vite wrote is what is served: the document at `/`, and the
    /// hashed assets it references beside it.
    #[tokio::test]
    async fn the_document_and_its_assets_are_in_the_binary() {
        let response = get("/", None).await;
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
        let response = get(&format!("/{script}"), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        let stylesheet = Frontend::iter()
            .find(|name| name.ends_with(".css"))
            .expect("the bundle has a stylesheet");
        let response = get(&format!("/{stylesheet}"), None).await;
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/css; charset=utf-8");
    }

    /// A path that is not a file is the page, with a 200: the SPA's own routes have
    /// to load as the document, and a directory or a traversal is not a file either.
    #[tokio::test]
    async fn every_other_path_is_the_document() {
        for path in ["/login", "/assets/", "/assets/../index.html", "/no/such/thing"] {
            let response = get(path, None).await;
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
        let first = get("/", None).await;
        let etag = first.headers()[header::ETAG].clone();
        assert!(etag.to_str().unwrap().starts_with('"'), "{etag:?}");

        let revalidated = get("/", Some(&etag)).await;
        assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(revalidated.headers()[header::ETAG], etag);
        assert!(body(revalidated).await.is_empty());

        let stale = get("/", Some(&HeaderValue::from_static("\"something-else\""))).await;
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
        assert_eq!(serve(request).await.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
