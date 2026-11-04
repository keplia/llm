use std::sync::Arc;
use std::time::Instant;

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::HeaderValue;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use serde_json::{json, Value};

use crate::admin;
use crate::auth::{auth_middleware, ClientKey};
use crate::db::UsageRecord;
use crate::error::ApiError;
use crate::providers;
use crate::state::AppState;

pub fn build_router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(embeddings))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let admin = admin::admin_routes(&state);

    Router::new()
        .route("/health", get(health))
        // Halaman dashboard (static HTML, datanya tetap dilindungi admin_key)
        .route("/admin", get(admin_dashboard))
        .merge(protected)
        .merge(admin)
        .layer(DefaultBodyLimit::max(20 * 1024 * 1024))
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn admin_dashboard() -> impl IntoResponse {
    Html(include_str!("../assets/dashboard.html"))
}

/// Daftar model = semua alias di config, format OpenAI /v1/models.
async fn list_models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut data = Vec::new();
    for (alias, route) in &state.config.models {
        let owned = route
            .routes
            .first()
            .map(|r| r.provider.clone())
            .unwrap_or_default();
        data.push(json!({
            "id": alias,
            "object": "model",
            "created": 0,
            "owned_by": owned,
        }));
    }
    Json(json!({"object": "list", "data": data}))
}

/// Endpoint utama, OpenAI-compatible.
/// Alur: cek cache -> resolve model -> skip provider yang circuit-open ->
/// coba provider primary -> kalau gagal (5xx/429/timeout) ->
/// failover ke fallback berikutnya -> simpan ke cache (kalau aktif).
async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Extension(client): Extension<ClientKey>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let model = body["model"].as_str().unwrap_or("").to_string();
    if model.is_empty() {
        return Err(ApiError::BadRequest("field 'model' wajib ada".into()));
    }

    let routes = state.resolve_model(&model);
    if routes.is_empty() {
        return Err(ApiError::NotFound(format!(
            "model '{model}' tidak dikenal"
        )));
    }

    let stream = body["stream"].as_bool().unwrap_or(false);
    let api_key = if client.0 == "anonymous" {
        None
    } else {
        Some(client.0.clone())
    };

    // Cache hanya untuk request non-streaming.
    let cache_key = if state.config.cache.enabled && !stream {
        Some(state.cache_hash(&body, &model))
    } else {
        None
    };

    if let Some(ck) = &cache_key {
        if let Some(cached) = state.cache_get(ck) {
            let mut resp = axum::Json(cached).into_response();
            resp.headers_mut()
                .insert("x-cache", HeaderValue::from_static("HIT"));
            return Ok(resp);
        }
    }

    let mut last_err: Option<ApiError> = None;
    for route in &routes {
        // Skip provider yang lagi di-open circuit breaker-nya
        // (kecuali dia satu-satunya kandidat, tetap dicoba sebagai upaya terakhir).
        if routes.len() > 1 && state.circuit_open(&route.provider) {
            tracing::warn!(provider = %route.provider, "circuit breaker open, provider di-skip");
            continue;
        }

        let started = Instant::now();
        match providers::chat(state.clone(), route, &body, stream, &model, api_key.clone()).await {
            Ok(resp) => {
                state.record_provider_result(&route.provider, true);
                if let Some(ck) = cache_key {
                    return Ok(cache_response(&state, ck, resp).await);
                }
                return Ok(resp);
            }
            Err(err) => {
                tracing::warn!(
                    provider = %route.provider,
                    model = %model,
                    error = %err,
                    "upstream gagal"
                );

                // Hanya retry ke fallback untuk error yang "layak retry":
                // network error / timeout / 5xx / 429 / 408.
                // 4xx lain (misal request invalid) langsung dilempar balik.
                let retryable = match &err {
                    ApiError::Internal(_) => true,
                    ApiError::Upstream { status, .. } => {
                        *status >= 500 || *status == 429 || *status == 408
                    }
                    _ => false,
                };

                if retryable {
                    state.record_provider_result(&route.provider, false);
                    state.record_usage(UsageRecord {
                        api_key: api_key.clone(),
                        model: model.clone(),
                        provider: route.provider.clone(),
                        upstream: route.upstream.clone(),
                        status: "error".into(),
                        duration_ms: started.elapsed().as_millis() as u64,
                        stream,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        cost_usd: 0.0,
                    });
                    tracing::info!(provider = %route.provider, "failover ke provider berikutnya");
                    last_err = Some(err);
                    continue;
                }

                // 4xx bukan salah provider, tidak perlu hit circuit breaker.
                state.record_provider_result(&route.provider, true);
                return Err(err);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        ApiError::Internal("tidak ada provider tersedia (semua circuit breaker open)".into())
    }))
}

/// Simpan response sukses ke cache lalu kembalikan response-nya
/// dengan header `x-cache: MISS`.
async fn cache_response(state: &Arc<AppState>, key: String, resp: Response) -> Response {
    let (parts, body) = resp.into_parts();
    match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(bytes) => {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                state.cache_put(&key, v);
            }
            let mut resp = Response::from_parts(parts, axum::body::Body::from(bytes));
            resp.headers_mut()
                .insert("x-cache", HeaderValue::from_static("MISS"));
            resp
        }
        Err(_) => Response::from_parts(parts, axum::body::Body::empty()),
    }
}

/// Endpoint embeddings — hanya untuk provider ber-format OpenAI.
async fn embeddings(
    State(state): State<Arc<AppState>>,
    Extension(client): Extension<ClientKey>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let model = body["model"].as_str().unwrap_or("").to_string();
    if model.is_empty() {
        return Err(ApiError::BadRequest("field 'model' wajib ada".into()));
    }

    let routes = state.resolve_model(&model);
    let route = routes
        .first()
        .ok_or_else(|| ApiError::NotFound(format!("model '{model}' tidak dikenal")))?;

    let api_key = if client.0 == "anonymous" {
        None
    } else {
        Some(client.0.clone())
    };

    providers::embeddings(state.clone(), route, &body, &model, api_key).await
}
