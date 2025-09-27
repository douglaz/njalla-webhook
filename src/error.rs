use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Njalla API error: {0}")]
    NjallaApi(String),

    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Domain not allowed: {0}")]
    DomainNotAllowed(String),

    #[error("Record not found: {0}")]
    RecordNotFound(String),

    #[error("Configuration error: {0}")]
    Configuration(String),

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            Error::NjallaApi(msg) => (StatusCode::BAD_GATEWAY, msg),
            Error::InvalidRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Error::DomainNotAllowed(msg) => (StatusCode::FORBIDDEN, msg),
            Error::RecordNotFound(msg) => (StatusCode::NOT_FOUND, msg),
            Error::Configuration(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            Error::Network(e) => (StatusCode::BAD_GATEWAY, e.to_string()),
            Error::Json(e) => (StatusCode::BAD_REQUEST, e.to_string()),
            Error::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            Error::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let body = Json(json!({
            "error": error_message,
            "status": status.as_u16(),
        }));

        (status, body).into_response()
    }
}
