use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("HTTP request failed: {0}")]
    HttpRequest(#[from] reqwest::Error),

    #[error("JSON serialization failed: {0}")]
    JsonSerialization(#[from] serde_json::Error),

    #[error("Configuration error: {0}")]
    Configuration(#[from] anyhow::Error),

    #[error("Resource not found: {0}")]
    NotFound(String),

    #[error("Internal server error: {0}")]
    Internal(String),

    #[error("Database error: {0}")]
    DbError(String),

    #[error("Service not ready: {0}")]
    NotReady(String),

    #[error("Rate limit exceeded")]
    RateLimit,

    #[error("Invalid request: {0}")]
    BadRequest(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AppError::DbError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", msg),
            ),
            AppError::NotReady(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            AppError::RateLimit => (
                StatusCode::TOO_MANY_REQUESTS,
                "Rate limit exceeded".to_string(),
            ),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::HttpRequest(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("HTTP request failed: {}", e),
            ),
            AppError::JsonSerialization(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("JSON error: {}", e),
            ),
            AppError::Configuration(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Configuration error: {}", e),
            ),
        };

        let body = Json(json!({
            "error": error_message,
            "status": status.as_u16()
        }));

        (status, body).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
