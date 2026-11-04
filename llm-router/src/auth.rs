use crate::state::AppState;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use std::sync::Arc;

/// Identitas client key, disisipkan ke request extensions
/// setelah auth sukses, dipakai handler untuk usage tracking.
#[derive(Clone)]
pub struct ClientKey(pub String);

/// Middleware auth untuk endpoint /v1/*:
/// 1. Validasi `Authorization: Bearer <key>` terhadap daftar key runtime.
/// 2. Cek rate limit per key.
/// 3. Sisipkan identitas key ke extensions.
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let provided: Option<String> = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(String::from));

    let open_mode = state.key_count() == 0;

    let identity = match provided {
        Some(k) => {
            if open_mode || state.valid_key(&k) {
                k
            } else {
                return unauthorized();
            }
        }
        None if open_mode => "anonymous".to_string(),
        None => return unauthorized(),
    };

    if !state.check_rate_limit(&identity) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": {
                    "message": "Rate limit exceeded, slow down",
                    "type": "rate_limit_error",
                    "code": "rate_limit_exceeded"
                }
            })),
        )
            .into_response();
    }

    req.extensions_mut().insert(ClientKey(identity));
    next.run(req).await
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": {
                "message": "Invalid API key",
                "type": "invalid_request_error",
                "code": "invalid_api_key"
            }
        })),
    )
        .into_response()
}

/// Perbandingan constant-time biar tidak rawan timing attack.
pub fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}
