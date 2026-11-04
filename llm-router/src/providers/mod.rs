pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod util;

use crate::config::ProviderKind;
use crate::error::ApiError;
use crate::state::{AppState, Route};
use axum::response::Response;
use serde_json::Value;
use std::sync::Arc;

pub async fn chat(
    state: Arc<AppState>,
    route: &Route,
    body: &Value,
    stream: bool,
    requested_model: &str,
    client_key: Option<String>,
) -> Result<Response, ApiError> {
    let pc = state
        .config
        .providers
        .get(&route.provider)
        .ok_or_else(|| ApiError::Internal(format!("provider '{}' tidak ditemukan", route.provider)))?;

    match pc.kind {
        ProviderKind::Openai => {
            openai::chat(state, route, body, stream, requested_model, client_key).await
        }
        ProviderKind::Anthropic => {
            anthropic::chat(state, route, body, stream, requested_model, client_key).await
        }
        ProviderKind::Gemini => {
            gemini::chat(state, route, body, stream, requested_model, client_key).await
        }
    }
}

pub async fn embeddings(
    state: Arc<AppState>,
    route: &Route,
    body: &Value,
    requested_model: &str,
    client_key: Option<String>,
) -> Result<Response, ApiError> {
    let pc = state
        .config
        .providers
        .get(&route.provider)
        .ok_or_else(|| ApiError::Internal(format!("provider '{}' tidak ditemukan", route.provider)))?;

    match pc.kind {
        ProviderKind::Openai => {
            openai::embeddings(state, route, body, requested_model, client_key).await
        }
        _ => Err(ApiError::BadRequest(
            "provider ini belum mendukung endpoint embeddings".into(),
        )),
    }
}
