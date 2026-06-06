use actix_web::{http::StatusCode, HttpResponse};
use serde_json::json;

/// Errors raised while talking to PetroApp.
#[derive(thiserror::Error, Debug)]
pub enum PetroError {
    /// Token/cookie rejected (401/403). This is the signal to mark credentials
    /// expired and alert — it must never be retried or silently swallowed.
    #[error("petroapp authentication failed (expired token/cookie)")]
    Auth,

    #[error("petroapp request failed: {0}")]
    Request(String),

    #[error("petroapp returned unexpected payload: {0}")]
    Payload(String),
}

/// Errors surfaced by the HTTP API layer.
#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,

    #[error("not found")]
    NotFound,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("no PetroApp credentials configured")]
    NoCredentials,

    #[error(transparent)]
    Petro(#[from] PetroError),

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl actix_web::ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        match self {
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::NoCredentials => StatusCode::FAILED_DEPENDENCY,
            ApiError::Petro(PetroError::Auth) => StatusCode::FAILED_DEPENDENCY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        // Log full detail server-side, return a clean shape to the caller.
        if self.status_code().is_server_error() {
            tracing::error!(error = %self, "api error");
        }
        HttpResponse::build(self.status_code())
            .json(json!({ "error": self.to_string() }))
    }
}
