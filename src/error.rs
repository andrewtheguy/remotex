//! Typed error type for the HTTP API boundary.
//!
//! Per the project's error-handling convention (see CLAUDE.md): application and
//! internal code uses `anyhow`; the HTTP API surfaces a typed `thiserror` error.
//! Application errors bubbled up with `?` land in [`AppError::Internal`] (500)
//! via the `#[from] anyhow::Error` conversion, while handlers can also return
//! typed variants such as [`AppError::NotFound`] directly.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// An error returned from an HTTP handler, rendered into an HTTP response.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The requested resource does not exist — rendered as `404 Not Found`.
    #[error("not found")]
    NotFound,

    /// No valid auth session: a bad login, a missing/expired
    /// `remotex_session` cookie on a guarded route — rendered as
    /// `401 Unauthorized`. The browser reacts by showing the login screen.
    #[error("unauthorized")]
    Unauthorized,

    /// Another browser's WebSocket holds the single session slot — rendered
    /// as `409 Conflict`. The client may retry with `force` (takeover).
    #[error("session busy")]
    SessionBusy(#[from] crate::session::SessionBusy),

    /// An unexpected application error, bubbled up from `anyhow`. Rendered as
    /// `500 Internal Server Error`; the detail is logged, never sent to clients.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Result alias for fallible handlers, e.g. `async fn h() -> ApiResult<Json<T>>`.
pub type ApiResult<T> = Result<T, AppError>;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            AppError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
            }
            AppError::SessionBusy(_) => {
                (StatusCode::CONFLICT, "another browser holds the session").into_response()
            }
            AppError::Internal(e) => {
                // Log the full `source()` chain; return an opaque message.
                log::error!("internal error: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
            }
        }
    }
}
