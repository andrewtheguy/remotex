//! Application error type usable as an axum response.
//!
//! Unused by the skeleton's handlers so far; provided as the shared error seam
//! for Phase 1 handlers that can fail (RDP connect, config load, etc.).

#![allow(dead_code)]

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Internal(e) => {
                log::error!("internal error: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
            }
        }
    }
}
