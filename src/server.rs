use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::{any, get, post},
};
use serde::{Deserialize, Serialize};
use tower::service_fn;
use tower_http::services::ServeDir;

use crate::{config::AppConfig, error::{ApiResult, AppError}, session::SessionManager, ws};

/// Shared application state handed to route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    /// The single session slot (phase 6): claim here, attach over `/ws`.
    pub sessions: Arc<SessionManager>,
}

/// Build the axum router.
///
/// - `/api/*` — JSON API (health, config, session claim); unknown `/api/*`
///   paths return 404 rather than the SPA, so API clients get an honest error.
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

    let sessions = Arc::new(SessionManager::new(config.target.clone()));
    let state = AppState { config, sessions };

    // Nested so unmatched `/api/*` paths hit this router's 404 fallback instead
    // of falling through to the SPA index.
    let api = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/config", get(config_handler))
        .route("/session", post(claim_handler))
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

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ClaimRequest {
    /// Take the slot even if another browser's WebSocket holds it (takeover).
    force: bool,
    /// The caller's previous token; matching the current claim lets the same
    /// browser reclaim (reconnect) without the takeover prompt.
    session_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimResponse {
    session_id: String,
}

/// Claim the single session slot (phase 6). Returns the token the WebSocket
/// must present as `/ws?session=<token>`; 409 while another browser is
/// attached (retry with `force` to take over).
async fn claim_handler(
    State(state): State<AppState>,
    Json(req): Json<ClaimRequest>,
) -> ApiResult<Json<ClaimResponse>> {
    let session_id = state.sessions.claim(req.force, req.session_id.as_deref())?;
    Ok(Json(ClaimResponse { session_id }))
}
