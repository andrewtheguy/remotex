use axum::{
    Json, Router,
    extract::State,
    routing::{any, get},
};
use serde::Serialize;

use crate::{assets, config::AppConfig, ws};

/// Shared application state handed to route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
}

/// Build the axum router.
///
/// - `/api/*` — JSON API (health, config)
/// - `/ws`    — binary WebSocket carrying the remote-desktop session
/// - fallback — the embedded SPA (see [`assets::static_handler`])
pub fn router(config: AppConfig) -> Router {
    let state = AppState { config };

    Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/config", get(config_handler))
        .route("/ws", any(ws::handler))
        .with_state(state)
        .fallback(assets::static_handler)
}

#[derive(Serialize)]
struct ConfigResponse {
    rdp_host: String,
    rdp_port: u16,
}

/// Non-secret info about the configured target. Never returns credentials.
async fn config_handler(State(state): State<AppState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        rdp_host: state.config.rdp_host.clone(),
        rdp_port: state.config.rdp_port,
    })
}
