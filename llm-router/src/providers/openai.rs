use std::sync::Arc;
use std::time::{Duration, Instant};

use super::util;
use crate::error::ApiError;
use crate::state::{AppState, Route};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

/// Provider ber-format OpenAI: request di-forward apa adanya
/// (hanya nama model yang diganti ke nama upstream).
pub async fn chat(
    state: Arc<AppState>,
    route: &Route,
    body: &Value,
    stream: bool,
    requested_model: &str,
    client_key: Option<String>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let pc = state
        .config
        .providers
        .get(&route.provider)
        .ok_or_else(|| ApiError::Internal(format!("provider '{}' tidak ditemukan", route.provider)))?;

    let url = format!("{}/chat/completions", pc.base_url.trim_end_matches('/'));

    let mut payload = body.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".into(), json!(route.upstream));
    }

    let mut req = state.http.post(&url).json(&payload);
    // Provider tanpa key (mis. Ollama) -> kirim tanpa Authorization.
    if let Some(key) = state.next_key(&route.provider) {
        req = req.bearer_auth(key);
    }
    if !stream {
        req = req.timeout(Duration::from_secs(state.config.request_timeout_secs));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("upstream request gagal: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let msg = resp.text().await.unwrap_or_else(|_| "upstream error".into());
        return Err(ApiError::Upstream {
            status: status.as_u16(),
            message: util::truncate(&msg),
        });
    }

    if stream {
        // Streaming: pass-through SSE apa adanya (formatnya memang OpenAI).
        // Token usage untuk stream pass-through tidak bisa dihitung
        // tanpa mem-parsing chunk, jadi dicatat 0.
        util::log_usage(
            &state,
            client_key.as_deref(),
            requested_model,
            route,
            started,
            true,
            0,
            0,
        );
        let byte_stream = resp.bytes_stream();
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(axum::body::Body::from_stream(byte_stream))
            .map_err(|e| ApiError::Internal(e.to_string()));
    }

    let mut v: Value = resp
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("response upstream tidak valid: {e}")))?;

    let prompt_tokens = v["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
    let completion_tokens = v["usage"]["completion_tokens"].as_u64().unwrap_or(0);
    util::log_usage(
        &state,
        client_key.as_deref(),
        requested_model,
        route,
        started,
        false,
        prompt_tokens,
        completion_tokens,
    );

    if let Some(obj) = v.as_object_mut() {
        obj.insert("model".into(), json!(requested_model));
    }
    Ok(axum::Json(v).into_response())
}

pub async fn embeddings(
    state: Arc<AppState>,
    route: &Route,
    body: &Value,
    requested_model: &str,
    client_key: Option<String>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let pc = state
        .config
        .providers
        .get(&route.provider)
        .ok_or_else(|| ApiError::Internal(format!("provider '{}' tidak ditemukan", route.provider)))?;

    let url = format!("{}/embeddings", pc.base_url.trim_end_matches('/'));

    let mut payload = body.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".into(), json!(route.upstream));
    }

    let mut req = state.http.post(&url).json(&payload);
    if let Some(key) = state.next_key(&route.provider) {
        req = req.bearer_auth(key);
    }
    req = req.timeout(Duration::from_secs(state.config.request_timeout_secs));

    let resp = req
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("upstream request gagal: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let msg = resp.text().await.unwrap_or_else(|_| "upstream error".into());
        return Err(ApiError::Upstream {
            status: status.as_u16(),
            message: util::truncate(&msg),
        });
    }

    let mut v: Value = resp
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("response upstream tidak valid: {e}")))?;
    util::log_usage(&state, client_key.as_deref(), requested_model, route, started, false, 0, 0);

    if let Some(obj) = v.as_object_mut() {
        obj.insert("model".into(), json!(requested_model));
    }
    Ok(axum::Json(v).into_response())
}
