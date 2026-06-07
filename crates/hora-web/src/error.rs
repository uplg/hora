//! Handler error type: a 404 for missing resources, otherwise a logged 500.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub enum AppError {
    /// The requested resource does not exist (e.g. an unknown monitor id).
    NotFound(&'static str),
    /// Any other failure; logged and surfaced as a 500.
    Internal(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound(what) => (StatusCode::NOT_FOUND, what).into_response(),
            Self::Internal(err) => {
                tracing::error!("request failed: {err:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
            }
        }
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self::Internal(err.into())
    }
}
