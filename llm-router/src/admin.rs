use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::auth::constant_eq;
use crate::error::ApiError;

/// Router endpoint admin (dilindungi admin_key).
pub fn admin_routes(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/stats", get(stats))
        .route("/admin/spend", get(spend))
        .route("/admin/usage", get(recent_usage))
        .route("/admin/keys", get(list_keys).post(create_key))
        .route("/admin/keys/:key", delete(revoke_key))
        .route("/admin/providers", get(provider_status))
        .route("/admin/providers/:name/reset", post(reset_provider))
        .route("/admin/cache", get(cache_status).delete(cache_clear))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_auth,
        ))
}

async fn admin_auth(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let admin_key = &state.config.admin_key;
    if admin_key.is_empty() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": {
                    "message": "Admin API dinonaktifkan. Isi 'admin_key' di config.toml untuk mengaktifkan.",
                    "type": "admin_disabled",
                }
            })),
        )
            .into_response();
    }

    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|k| constant_eq(k, admin_key))
        .unwrap_or(false);

    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {"message": "Invalid admin key", "type": "invalid_request_error"}
            })),
        )
            .into_response();
    }

    next.run(req).await
}

/// GET /admin/stats?days=7
/// Agregasi usage per hari x model x provider.
async fn stats(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let days = params
        .get("days")
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(7)
        .min(90);
    let data = state.db.stats(days).map_err(ApiError::Internal)?;
    Ok(Json(data))
}

/// GET /admin/spend?days=30
/// Total pemakaian & biaya per API key.
async fn spend(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let days = params
        .get("days")
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(30)
        .min(365);
    let data = state.db.spend(days).map_err(ApiError::Internal)?;
    Ok(Json(data))
}

/// GET /admin/usage?limit=100
/// N request terakhir.
async fn recent_usage(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, 1000);
    let data = state.db.recent(limit).map_err(ApiError::Internal)?;
    Ok(Json(data))
}

/// GET /admin/keys
async fn list_keys(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let rows = state.db.keys().map_err(ApiError::Internal)?;
    let data: Vec<Value> = rows
        .iter()
        .map(|k| {
            json!({
                "key": k.key,
                "name": k.name,
                "created_at": k.created_at,
                "enabled": k.enabled,
            })
        })
        .collect();
    Ok(Json(json!({"keys": data})))
}

/// POST /admin/keys  body: {"name": "budi"}
/// Generate key baru (sk-...) tanpa restart.
async fn create_key(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let name = body["name"].as_str().unwrap_or("").to_string();
    let key = format!("sk-{}", random_hex(16));
    state.db.upsert_key(&key, &name).map_err(ApiError::Internal)?;
    state.add_runtime_key(key.clone());
    tracing::info!(name = %name, "client key baru dibuat via admin API");
    Ok(Json(json!({"key": key, "name": name})))
}

/// DELETE /admin/keys/:key
async fn revoke_key(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let ok = state.revoke_key(&key).map_err(ApiError::Internal)?;
    if ok {
        tracing::info!(%key, "client key di-revoke via admin API");
    }
    Ok(Json(json!({"key": key, "revoked": ok})))
}

/// GET /admin/providers
/// Status semua provider + kondisi circuit breaker.
async fn provider_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let circuits = state.circuit_snapshot();
    let mut out = Vec::new();
    for (name, pc) in &state.config.providers {
        let (failures, open) = circuits.get(name).copied().unwrap_or((0, false));
        out.push(json!({
            "provider": name,
            "kind": pc.kind.as_str(),
            "enabled": pc.enabled,
            "base_url": pc.base_url,
            "api_keys": pc.api_keys.len(),
            "circuit_open": open,
            "consecutive_failures": failures,
        }));
    }
    Json(json!({"providers": out}))
}

/// POST /admin/providers/:name/reset — tutup paksa circuit breaker.
async fn reset_provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    state.reset_circuit(&name);
    Json(json!({"provider": name, "reset": true}))
}

/// GET /admin/cache — jumlah entri cache.
async fn cache_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "enabled": state.config.cache.enabled,
        "entries": state.cache.len(),
        "ttl_secs": state.config.cache.ttl_secs,
        "max_entries": state.config.cache.max_entries,
    }))
}

/// DELETE /admin/cache — kosongkan cache.
async fn cache_clear(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let n = state.cache_clear();
    Json(json!({"cleared": n}))
}

fn random_hex(bytes: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..bytes)
        .map(|_| format!("{:02x}", rng.gen::<u8>()))
        .collect()
}
