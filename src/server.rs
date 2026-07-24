use std::convert::Infallible;

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::{any, get},
};
use serde::Serialize;
use tower::service_fn;
use tower_http::services::ServeDir;

use crate::{config::AppConfig, error::AppError, ws};

/// Shared application state handed to route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
}

/// Build the axum router.
///
/// - `/api/*` — JSON API (health, config); unknown `/api/*` paths return 404
///   rather than the SPA, so API clients get an honest error.
/// - `/ws`    — binary WebSocket carrying the remote-desktop session
/// - fallback — the built SPA, served from `config.static_dir` on disk. Real
///   files are served by [`ServeDir`]; any unknown path returns `index.html`
///   with a 200 so client-side routes resolve (matching an SPA's expectations).
pub fn router(config: AppConfig) -> Router {
    // Use `.fallback` (returns the fallback response as-is) rather than
    // `.not_found_service` (which forces a 404 status), so SPA routes get 200.
    let index_path = config.static_dir.join("index.html");
    let spa_index = service_fn(move |_req| {
        let index_path = index_path.clone();
        async move {
            let response = match tokio::fs::read(&index_path).await {
                Ok(bytes) => {
                    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response()
                }
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            };
            Ok::<_, Infallible>(response)
        }
    });
    let spa = ServeDir::new(&config.static_dir).fallback(spa_index);

    let state = AppState { config };

    // Nested so unmatched `/api/*` paths hit this router's 404 fallback instead
    // of falling through to the SPA index.
    let api = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/config", get(config_handler))
        .fallback(|| async { AppError::NotFound });

    Router::new()
        .nest("/api", api)
        .route("/ws", any(ws::handler))
        .fallback_service(spa)
        .with_state(state)
}

#[derive(Serialize)]
struct ConfigResponse {
    target: String,
    protocol: &'static str,
    host: String,
    port: u16,
}

/// Non-secret info about the configured target. Never returns credentials.
async fn config_handler(State(state): State<AppState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        target: state.config.target.name.clone(),
        protocol: state.config.target.protocol.name(),
        host: state.config.target.host.clone(),
        port: state.config.target.port,
    })
}
