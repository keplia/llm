use crate::db::UsageRecord;
use crate::state::{AppState, Route};
use serde_json::Value;
use std::time::Instant;

/// Ambil semua bagian teks dari field `content` OpenAI
/// (bisa string atau array of parts).
pub fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                if p["type"].as_str() == Some("text") {
                    p["text"].as_str().map(str::to_string)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Batasi panjang pesan error upstream supaya log & response tidak meledak.
pub fn truncate(s: &str) -> String {
    if s.chars().count() <= 2000 {
        s.to_string()
    } else {
        s.chars().take(2000).collect()
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Catat satu pemakaian sukses ke database usage.
/// Biaya dihitung dari harga per model di config (USD per 1M token).
pub fn log_usage(
    state: &AppState,
    client_key: Option<&str>,
    model: &str,
    route: &Route,
    started: Instant,
    stream: bool,
    prompt_tokens: u64,
    completion_tokens: u64,
) {
    let (in_price, out_price) = state
        .config
        .models
        .get(model)
        .map(|m| (m.input_price_per_mtok, m.output_price_per_mtok))
        .unwrap_or((0.0, 0.0));
    let cost_usd = prompt_tokens as f64 / 1_000_000.0 * in_price
        + completion_tokens as f64 / 1_000_000.0 * out_price;

    state.record_usage(UsageRecord {
        api_key: client_key.map(String::from),
        model: model.to_string(),
        provider: route.provider.clone(),
        upstream: route.upstream.clone(),
        status: "ok".into(),
        duration_ms: started.elapsed().as_millis() as u64,
        stream,
        prompt_tokens,
        completion_tokens,
        cost_usd,
    });
}
