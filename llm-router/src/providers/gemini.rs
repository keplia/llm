use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::util;
use crate::error::ApiError;
use crate::state::Route;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{json, Map, Value};

// ============================================================
// Request: OpenAI format -> Gemini generateContent format
// ============================================================

fn image_part(url: &str) -> Value {
    // data URI -> inline_data (base64), URL biasa -> file_data
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(";base64,") {
            return json!({"inline_data": {"mime_type": meta, "data": data}});
        }
    }
    json!({"file_data": {"file_uri": url}})
}

fn user_parts(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) => vec![json!({"text": s})],
        Value::Array(arr) => arr
            .iter()
            .map(|p| match p["type"].as_str() {
                Some("text") => json!({"text": p["text"]}),
                Some("image_url") => image_part(p["image_url"]["url"].as_str().unwrap_or("")),
                _ => json!({"text": ""}),
            })
            .collect(),
        _ => vec![],
    }
}

pub fn transform_request(openai: &Value, model: &str) -> Result<Value, ApiError> {
    let mut contents: Vec<Value> = Vec::new();
    let mut system_parts: Vec<Value> = Vec::new();
    // tool_call_id -> nama function, dipakai untuk role "tool"
    let mut tool_names: HashMap<String, String> = HashMap::new();

    let msgs = openai["messages"].as_array().cloned().unwrap_or_default();

    // Pass 1: kumpulkan pemetaan tool_call_id -> nama function
    for m in &msgs {
        if m["role"].as_str() == Some("assistant") {
            for tc in m["tool_calls"].as_array().unwrap_or(&Vec::new()) {
                if let (Some(id), Some(name)) = (
                    tc["id"].as_str(),
                    tc["function"]["name"].as_str(),
                ) {
                    tool_names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    // Pass 2: konversi isi pesan
    for m in &msgs {
        let role = m["role"].as_str().unwrap_or("user");
        match role {
            "system" | "developer" => {
                system_parts.push(json!({"text": util::content_to_text(&m["content"])}));
            }
            "user" => {
                contents.push(json!({"role": "user", "parts": user_parts(&m["content"])}));
            }
            "assistant" => {
                let mut parts = user_parts(&m["content"]);
                for tc in m["tool_calls"].as_array().unwrap_or(&Vec::new()) {
                    let args: Value = serde_json::from_str(
                        tc["function"]["arguments"].as_str().unwrap_or("{}"),
                    )
                    .unwrap_or(json!({}));
                    parts.push(json!({
                        "function_call": {"name": tc["function"]["name"], "args": args}
                    }));
                }
                contents.push(json!({"role": "model", "parts": parts}));
            }
            "tool" => {
                let id = m["tool_call_id"].as_str().unwrap_or("");
                let name = tool_names.get(id).cloned().unwrap_or_else(|| id.to_string());
                contents.push(json!({
                    "role": "user",
                    "parts": [{"function_response": {
                        "name": name,
                        "response": {"result": util::content_to_text(&m["content"])}
                    }}]
                }));
            }
            _ => {}
        }
    }

    let mut obj = Map::new();
    obj.insert("contents".into(), Value::Array(contents));
    if !system_parts.is_empty() {
        obj.insert("system_instruction".into(), json!({"parts": system_parts}));
    }

    let mut gen = Map::new();
    if let Some(t) = openai["temperature"].as_f64() {
        gen.insert("temperature".into(), json!(t));
    }
    if let Some(t) = openai["top_p"].as_f64() {
        gen.insert("topP".into(), json!(t));
    }
    if let Some(mt) = openai["max_tokens"].as_u64() {
        gen.insert("maxOutputTokens".into(), json!(mt));
    }
    if let Some(stop) = openai.get("stop") {
        let seqs: Vec<String> = match stop {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect(),
            _ => vec![],
        };
        if !seqs.is_empty() {
            gen.insert("stopSequences".into(), json!(seqs));
        }
    }
    if !gen.is_empty() {
        obj.insert("generationConfig".into(), Value::Object(gen));
    }

    if let Some(tools) = openai["tools"].as_array() {
        let decls: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let f = &t["function"];
                if !f["name"].is_string() {
                    return None;
                }
                Some(json!({
                    "name": f["name"],
                    "description": f["description"],
                    "parameters": if f["parameters"].is_object() {
                        f["parameters"].clone()
                    } else {
                        json!({"type": "object", "properties": {}})
                    }
                }))
            })
            .collect();
        if !decls.is_empty() {
            obj.insert("tools".into(), json!({"function_declarations": decls}));
        }
    }

    let _ = model; // model dipakai di URL, bukan di body
    Ok(Value::Object(obj))
}

// ============================================================
// Response: Gemini -> OpenAI format (non-streaming)
// ============================================================

fn map_finish(reason: &str) -> &'static str {
    match reason {
        "MAX_TOKENS" => "length",
        "SAFETY" => "content_filter",
        _ => "stop",
    }
}

pub fn transform_response(v: &Value, requested_model: &str) -> Value {
    let cand = &v["candidates"][0];

    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut ti: usize = 0;

    for p in cand["content"]["parts"].as_array().unwrap_or(&Vec::new()) {
        if let Some(t) = p["text"].as_str() {
            text.push_str(t);
        }
        if p["function_call"].is_object() {
            tool_calls.push(json!({
                "id": format!("call_{ti}"),
                "type": "function",
                "function": {
                    "name": p["function_call"]["name"],
                    "arguments": p["function_call"]["args"].to_string()
                }
            }));
            ti += 1;
        }
    }

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    if !text.is_empty() {
        message.insert("content".into(), json!(text));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    let input = v["usageMetadata"]["promptTokenCount"].as_u64().unwrap_or(0);
    let output = v["usageMetadata"]["candidatesTokenCount"]
        .as_u64()
        .unwrap_or(0);

    json!({
        "id": format!("chatcmpl-gemini-{}", util::now_secs()),
        "object": "chat.completion",
        "created": util::now_secs(),
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": map_finish(cand["finishReason"].as_str().unwrap_or("STOP"))
        }],
        "usage": {
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_tokens": input + output
        }
    })
}

// ============================================================
// Streaming: Gemini SSE -> OpenAI chunk SSE
// ============================================================

struct GeminiStream {
    model: String,
    id: String,
    created: u64,
    started: bool,
    tool_counter: usize,
    finish: Option<String>,
    usage_prompt: u64,
    usage_completion: u64,
}

impl GeminiStream {
    fn new(model: String) -> Self {
        Self {
            model,
            id: format!("chatcmpl-gemini-{}", util::now_secs()),
            created: util::now_secs(),
            started: false,
            tool_counter: 0,
            finish: None,
            usage_prompt: 0,
            usage_completion: 0,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
    }

    /// Setiap event SSE Gemini = satu objek GenerateContentResponse utuh.
    fn handle(&mut self, data: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return out;
        };

        if !self.started {
            self.started = true;
            out.push(self.chunk(json!({"role": "assistant", "content": ""}), None));
        }

        if let Some(u) = v["usageMetadata"].as_object() {
            self.usage_prompt = u
                .get("promptTokenCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(self.usage_prompt);
            self.usage_completion = u
                .get("candidatesTokenCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(self.usage_completion);
        }

        if let Some(reason) = v["candidates"][0]["finishReason"].as_str() {
            self.finish = Some(map_finish(reason).to_string());
        }

        for p in v["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap_or(&Vec::new())
        {
            if let Some(t) = p["text"].as_str() {
                out.push(self.chunk(json!({"content": t}), None));
            }
            if let Some(fc) = p["function_call"].as_object() {
                let ti = self.tool_counter;
                self.tool_counter += 1;
                let name = fc.get("name").cloned().unwrap_or(json!(""));
                let args = fc.get("args").cloned().unwrap_or(json!({}));
                out.push(self.chunk(
                    json!({"tool_calls": [{
                        "index": ti,
                        "type": "function",
                        "id": format!("call_{ti}"),
                        "function": {"name": name, "arguments": args.to_string()}
                    }]}),
                    None,
                ));
            }
        }

        // Chunk terakhir: kirim finish_reason + usage
        if let Some(finish) = self.finish.take() {
            let mut final_chunk = self.chunk(json!({}), Some(finish.as_str()));
            final_chunk["usage"] = json!({
                "prompt_tokens": self.usage_prompt,
                "completion_tokens": self.usage_completion,
                "total_tokens": self.usage_prompt + self.usage_completion
            });
            out.push(final_chunk);
        }

        out
    }
}

// ============================================================
// Entry point
// ============================================================

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
    let key = state
        .next_key(&route.provider)
        .ok_or_else(|| ApiError::Internal(format!("provider '{}' butuh api key", route.provider)))?;

    let base = pc.base_url.trim_end_matches('/');
    let url = if stream {
        format!("{}/models/{}:streamGenerateContent?alt=sse", base, route.upstream)
    } else {
        format!("{}/models/{}:generateContent", base, route.upstream)
    };

    let payload = transform_request(body, &route.upstream)?;

    let mut req = state
        .http
        .post(&url)
        .header("x-goog-api-key", key)
        .json(&payload);
    if !stream {
        req = req.timeout(Duration::from_secs(state.config.request_timeout_secs));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("upstream request gagal: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let msg = resp.text().await.unwrap_or_default();
        return Err(ApiError::Upstream {
            status: status.as_u16(),
            message: util::truncate(&msg),
        });
    }

    if !stream {
        let v: Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Internal(format!("response upstream tidak valid: {e}")))?;
        let prompt = v["usageMetadata"]["promptTokenCount"].as_u64().unwrap_or(0);
        let completion = v["usageMetadata"]["candidatesTokenCount"]
            .as_u64()
            .unwrap_or(0);
        util::log_usage(
            &state,
            client_key.as_deref(),
            requested_model,
            route,
            started,
            false,
            prompt,
            completion,
        );
        return Ok(axum::Json(transform_response(&v, requested_model)).into_response());
    }

    // Streaming: tiap event SSE Gemini diubah jadi chunk OpenAI.
    use eventsource_stream::Eventsource;
    use futures::StreamExt;

    let mut events = Box::pin(resp.bytes_stream().eventsource());
    let mut st = GeminiStream::new(requested_model.to_string());

    // Clone buat logging di akhir stream (closure harus 'static).
    let log_state = state.clone();
    let log_model = requested_model.to_string();
    let log_route = route.clone();
    let log_key = client_key.clone();

    let out = async_stream::stream! {
        while let Some(ev) = events.next().await {
            match ev {
                Ok(event) => {
                    for chunk in st.handle(&event.data) {
                        let mut buf = String::from("data: ");
                        buf.push_str(&chunk.to_string());
                        buf.push_str("\n\n");
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(buf));
                    }
                }
                Err(e) => {
                    tracing::warn!("gemini stream error: {e}");
                    break;
                }
            }
        }
        util::log_usage(
            &log_state,
            log_key.as_deref(),
            &log_model,
            &log_route,
            started,
            true,
            st.usage_prompt,
            st.usage_completion,
        );
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };

    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from_stream(out))
        .map_err(|e| ApiError::Internal(e.to_string()))
}
