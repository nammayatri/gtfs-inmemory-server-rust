//! The editor's error shape: `{"error": {"code", "message", "details"}}`.

use actix_web::{http::StatusCode, HttpResponse, ResponseError};
use serde_json::{json, Value};

#[derive(Debug)]
pub struct EditorError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

pub type EditorResult<T> = Result<T, EditorError>;

impl EditorError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: Value::Null,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn unauthorized(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code, message)
    }

    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message)
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }
}

impl std::fmt::Display for EditorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.code, self.status, self.message)
    }
}

impl ResponseError for EditorError {
    fn status_code(&self) -> StatusCode {
        self.status
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status).json(json!({
            "error": {"code": self.code, "message": self.message, "details": self.details}
        }))
    }
}

impl From<sqlx::Error> for EditorError {
    fn from(e: sqlx::Error) -> Self {
        // A deadlock or serialization failure is nobody's fault: 503 try_again,
        // and the transaction is retried (feed_lock.rs).
        if super::feed_lock::is_transient(&e) {
            return super::feed_lock::try_again(&e);
        }
        // The message is logged, never returned: it can carry SQL and values.
        tracing::error!(tag = "[GTFS EDITOR DB]", error = %e);
        EditorError::internal("database error")
    }
}
