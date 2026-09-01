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
        let status = self.status_code();

        // Log full detail server-side, return a clean shape to the caller.
        //
        // Db and Other are #[error(transparent)], so `to_string()` is the raw
        // sqlx or anyhow message — which names tables, columns, the host and
        // the role. That is detail for the operator, not for whoever called
        // the endpoint, so a 5xx answers with its shape and nothing else. The
        // 4xx messages are ours and are written to be read by the dashboard.
        let detail = if status.is_server_error() {
            // This is the line that reaches Sentry, via sentry-tracing. Db and
            // Other are #[error(transparent)], so it is the raw sqlx or anyhow
            // message; the scrubber redacts by key and cannot clean free text,
            // so it is sanitized on the way out. Compliance control — do not
            // log `%self` here raw.
            tracing::error!(
                error = %crate::observability::sanitize_message(&self.to_string()),
                "api error"
            );
            "internal server error".to_string()
        } else {
            self.to_string()
        };

        HttpResponse::build(status).json(json!({ "error": detail }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::ResponseError;

    /// The client gets the shape; the operator log gets the detail. sqlx
    /// messages name the host, the role and the columns, and this response
    /// goes to anyone who can reach the endpoint.
    #[actix_web::test]
    async fn server_errors_do_not_describe_themselves_to_the_caller() {
        let err = ApiError::Other(anyhow::anyhow!("relation \"petroapp_bills\" does not exist"));
        assert_eq!(err.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(err.to_string().contains("petroapp_bills"), "precondition");

        let resp = err.error_response();
        let bytes = actix_web::body::to_bytes(resp.into_body()).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(!body.contains("petroapp_bills"), "leaked schema to the caller: {body}");
        assert!(body.contains("internal server error"), "unhelpful shape: {body}");
    }

    /// Client errors keep their messages: the dashboard reads them.
    #[test]
    fn client_errors_keep_their_message() {
        assert_eq!(ApiError::NotFound.status_code(), StatusCode::NOT_FOUND);
        assert_eq!(
            ApiError::NoCredentials.status_code(),
            StatusCode::FAILED_DEPENDENCY,
        );
        assert_eq!(
            ApiError::Petro(PetroError::Auth).status_code(),
            StatusCode::FAILED_DEPENDENCY,
        );
    }
}
