use axum::{
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use rust_embed::Embed;

/// The built frontend, embedded into the binary at compile time.
///
/// `frontend/dist/` must exist for a release build — run `bun run build` in
/// `frontend/` first. `.gitignore`d, so a git checkout needs that step.
#[derive(Embed)]
#[folder = "frontend/dist/"]
struct StaticAssets;

/// Serve an embedded asset, falling back to `index.html` for SPA routes.
pub async fn static_handler(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if let Some(file) = StaticAssets::get(path) {
        let mime = new_mime_guess::from_path(path).first_or_octet_stream();
        ([(header::CONTENT_TYPE, mime.as_ref())], file.data).into_response()
    } else if let Some(index) = StaticAssets::get("index.html") {
        Html(index.data).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}
