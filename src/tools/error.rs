use actix_web::{HttpResponse, ResponseError};
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

    /// 404 carrying a machine-readable discriminator (FEED_NOT_FOUND, DEPOT_NOT_FOUND, ...) so a
    /// caller can tell an unknown feed from an unknown depot; a plain NotFound collapses both.
    #[error("{1}")]
    NotFoundCoded(&'static str, String),
}

/// Discriminators emitted alongside a 404 in the `errorCode` field.
pub mod error_codes {
    pub const FEED_NOT_FOUND: &str = "FEED_NOT_FOUND";
    pub const DEPOT_NOT_FOUND: &str = "DEPOT_NOT_FOUND";
    pub const FLEET_NOT_FOUND: &str = "FLEET_NOT_FOUND";
}

impl ResponseError for AppError {
    fn error_response(&self) -> HttpResponse {
        let error_message = match self {
            AppError::NotFound(msg) => msg.clone(),
            AppError::NotFoundCoded(_, msg) => msg.clone(),
            AppError::Internal(msg) => msg.clone(),
            AppError::DbError(msg) => format!("Database error: {}", msg),
            AppError::NotReady(msg) => msg.clone(),
            AppError::RateLimit => "Rate limit exceeded".to_string(),
            AppError::BadRequest(msg) => msg.clone(),
            AppError::HttpRequest(err) => format!("HTTP request failed: {}", err),
            AppError::JsonSerialization(err) => format!("JSON serialization failed: {}", err),
            AppError::Configuration(err) => format!("Configuration error: {}", err),
        };

        let status_code = match self {
            AppError::NotFound(_) => actix_web::http::StatusCode::NOT_FOUND,
            AppError::NotFoundCoded(..) => actix_web::http::StatusCode::NOT_FOUND,
            AppError::Internal(_) => actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            AppError::DbError(_) => actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            AppError::NotReady(_) => actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
            AppError::RateLimit => actix_web::http::StatusCode::TOO_MANY_REQUESTS,
            AppError::BadRequest(_) => actix_web::http::StatusCode::BAD_REQUEST,
            AppError::HttpRequest(_) => actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            AppError::JsonSerialization(_) => actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Configuration(_) => actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
        };

        let mut body = json!({
            "error": error_message,
            "code": status_code.as_u16()
        });
        if let AppError::NotFoundCoded(code, _) = self {
            body["errorCode"] = json!(code);
        }
        HttpResponse::build(status_code).json(body)
    }
}

pub type AppResult<T> = Result<T, AppError>;
