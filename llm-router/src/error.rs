use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug)]
pub enum ApiError {
    Auth(String),
    BadRequest(String),
    NotFound(String),
    Upstream { status: u16, message: String },
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            ApiError::Auth(_) => (StatusCode::UNAUTHORIZED, "invalid_api_key"),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request_error"),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "model_not_found"),
            ApiError::Upstream { status, .. } => (
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY),
                "upstream_error",
            ),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };

        let message = match &self {
            ApiError::Auth(m)
            | ApiError::BadRequest(m)
            | ApiError::NotFound(m)
            | ApiError::Internal(m) => m.clone(),
            ApiError::Upstream { message, .. } => message.clone(),
        };

        let body = axum::Json(json!({
            "error": { "message": message, "type": code, "code": code }
        }));
        (status, body).into_response()
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Auth(m)
            | ApiError::BadRequest(m)
            | ApiError::NotFound(m)
            | ApiError::Internal(m) => write!(f, "{m}"),
            ApiError::Upstream { status, message } => write!(f, "upstream {status}: {message}"),
        }
    }
}
