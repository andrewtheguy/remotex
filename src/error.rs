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

    /// The caller is logged in but is not the holder of what it asked for —
    /// rendered as `403 Forbidden`. Distinct from [`Self::Unauthorized`] on
    /// purpose: the login is fine and the browser must not be sent back to the
    /// login screen over it. What it means today is a session token that is not
    /// the current claim, which `/ws` answers with its own close code.
    #[error("forbidden")]
    Forbidden,

    /// The resource exists but has nothing to serve yet — rendered as
    /// `503 Service Unavailable`, naming what was unavailable. A retry is
    /// meaningful, which is what separates this from a 404.
    #[error("{0} is unavailable")]
    Unavailable(&'static str),

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

impl From<crate::session::AudioError> for AppError {
    /// The two refusals mean different things to a client and get different
    /// codes: a token that is not the current claim will never work again (403),
    /// while a session that has no audio *yet* — the picker, or an audio channel
    /// still coming up — is worth asking again for (503).
    fn from(err: crate::session::AudioError) -> Self {
        match err {
            crate::session::AudioError::InvalidToken(_) => AppError::Forbidden,
            crate::session::AudioError::NoSource => AppError::Unavailable("remote audio"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            AppError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
            }
            AppError::Forbidden => {
                (StatusCode::FORBIDDEN, "not this session's holder").into_response()
            }
            AppError::Unavailable(what) => (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("{what} is unavailable"),
            )
                .into_response(),
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
