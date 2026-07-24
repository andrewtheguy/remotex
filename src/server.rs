use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use serde::{Deserialize, Serialize};
use tower::service_fn;
use tower_http::services::ServeDir;

use crate::{
    auth::{self, AuthSessions},
    config::AppConfig,
    error::{ApiResult, AppError},
    session::SessionManager,
    ws,
};

/// Shared application state handed to route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    /// The single session slot: claim here, attach over `/ws`.
    pub sessions: Arc<SessionManager>,
    /// Live auth sessions behind the login cookie.
    pub auth: Arc<AuthSessions>,
}

/// Build the axum router.
///
/// - `/api/auth/*` + `/api/health` — public: the login flow itself and the
///   liveness probe.
/// - the rest of `/api/*` and `/ws` — refuse requests without a valid login
///   cookie; unknown `/api/*` paths return 404 rather than the SPA,
///   so API clients get an honest error.
/// - `/ws`    — binary WebSocket carrying the remote-desktop session
/// - fallback — the built SPA, served from `config.static_dir` on disk. Real
///   files are served by [`ServeDir`]; any unknown path returns `index.html`
///   with a 200 so client-side routes resolve (matching an SPA's expectations).
///   The static shell stays public — it renders the login screen and holds no
///   secrets; everything it talks to is behind the cookie.
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
    let state = AppState {
        config,
        sessions,
        auth: Arc::new(AuthSessions::default()),
    };
    let require_auth = middleware::from_fn_with_state(state.clone(), require_auth);

    // Nested so unmatched `/api/*` paths hit this router's 404 fallback instead
    // of falling through to the SPA index.
    let api = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/auth/login", post(login_handler))
        .route("/auth/logout", post(logout_handler))
        .route("/auth/status", get(status_handler))
        .merge(
            Router::new()
                .route("/config", get(config_handler))
                .route("/session", post(claim_handler))
                .route_layer(require_auth.clone()),
        )
        .fallback(|| async { AppError::NotFound });

    Router::new()
        .nest("/api", api)
        // The cookie check runs before the upgrade, so an unauthenticated
        // WebSocket attempt fails its handshake with a plain 401. (A sub-router
        // because route_layer must come after a route to apply to it.)
        .merge(
            Router::new()
                .route("/ws", any(ws::handler))
                .route_layer(require_auth),
        )
        .fallback_service(spa)
        .with_state(state)
}

/// Middleware guarding everything session-related: no valid login cookie, no
/// service. Validation also refreshes the session's sliding expiry.
async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let authenticated = auth::token_from_headers(req.headers())
        .is_some_and(|token| state.auth.validate(&token));
    if !authenticated {
        return AppError::Unauthorized.into_response();
    }
    next.run(req).await
}

/// `Set-Cookie` attributes for the session cookie. `Secure` cookies set over
/// plain HTTP are silently dropped by Safari (even on localhost, unlike
/// Chrome), so the flag is only added when the request actually arrived over
/// HTTPS — which, since this server only speaks HTTP, means via a
/// TLS-terminating proxy setting `x-forwarded-proto`.
fn cookie_flags(headers: &HeaderMap) -> &'static str {
    let https = headers
        .get("x-forwarded-proto")
        .is_some_and(|proto| proto.as_bytes() == b"https");
    if https {
        "HttpOnly; SameSite=Strict; Path=/; Secure"
    } else {
        "HttpOnly; SameSite=Strict; Path=/"
    }
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct OkResponse {
    ok: bool,
}

/// Verify the credentials against `[server].site_passwd` and set the session
/// cookie. 401 on a mismatch, with no hint which of the two fields was wrong.
async fn login_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> ApiResult<impl IntoResponse> {
    let site_passwd = state.config.site_passwd.clone();
    // bcrypt verification burns tens of milliseconds by design — keep it off
    // the async workers.
    let ok = tokio::task::spawn_blocking(move || {
        site_passwd.verify(&req.username, &req.password)
    })
    .await
    .map_err(anyhow::Error::from)?;
    if !ok {
        return Err(AppError::Unauthorized);
    }
    let token = state.auth.create();
    let cookie = format!("{}={token}; {}", auth::COOKIE_NAME, cookie_flags(&headers));
    Ok(([(header::SET_COOKIE, cookie)], Json(OkResponse { ok: true })))
}

/// Invalidate the caller's session (if any) and clear the cookie. Public: it
/// only ever drops the caller's own token.
async fn logout_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = auth::token_from_headers(&headers) {
        state.auth.invalidate(&token);
    }
    let cookie = format!(
        "{}=; {}; Max-Age=0",
        auth::COOKIE_NAME,
        cookie_flags(&headers)
    );
    ([(header::SET_COOKIE, cookie)], Json(OkResponse { ok: true }))
}

#[derive(Serialize)]
struct StatusResponse {
    authenticated: bool,
}

/// Whether the caller holds a live session — the SPA asks on load to decide
/// between the login screen and the desktop.
async fn status_handler(State(state): State<AppState>, headers: HeaderMap) -> Json<StatusResponse> {
    let authenticated = auth::token_from_headers(&headers)
        .is_some_and(|token| state.auth.validate(&token));
    Json(StatusResponse { authenticated })
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

/// Claim the single session slot. Returns the token the WebSocket
/// must present as `/ws?session=<token>`; 409 while another browser is
/// attached (retry with `force` to take over).
async fn claim_handler(
    State(state): State<AppState>,
    Json(req): Json<ClaimRequest>,
) -> ApiResult<Json<ClaimResponse>> {
    let session_id = state.sessions.claim(req.force, req.session_id.as_deref())?;
    Ok(Json(ClaimResponse { session_id }))
}
