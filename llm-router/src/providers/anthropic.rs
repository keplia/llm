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
// Request: OpenAI format -> Anthropic /v1/messages format
// ============================================================

fn push_merged(messages: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if let Some(last) = messages.last_mut() {
        if last["role"].as_str() == Some(role) {
            if let Some(arr) = last["content"].as_array_mut() {
                for c in content {
                    arr.push(c);
                }
                return;
            }
        }
    }
    messages.push(json!({"role": role, "content": content}));
}

fn image_block(url: &str) -> Value {
    // data URI -> base64 source, URL biasa -> url source
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(";base64,") {
            return json!({
                "type": "image",
                "source": {"type": "base64", "media_type": meta, "data": data}
            });
        }
    }
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

fn convert_parts(content: &Value, is_assistant: bool) -> Vec<Value> {
    match content {
        Value::String(s) => vec![json!({"type": "text", "text": s})],
        Value::Array(arr) => {
            let mut out = Vec::new();
            for p in arr {
                match p["type"].as_str() {
                    Some("text") => out.push(json!({"type": "text", "text": p["text"]})),
                    Some("image_url") if !is_assistant => {
                        let url = p["image_url"]["url"].as_str().unwrap_or("");
                        out.push(image_block(url));
                    }
                    _ => {}
                }
            }
            out
        }
        _ => vec![],
    }
}

pub fn transform_request(openai: &Value, model: &str) -> Result<Value, ApiError> {
    let mut system = String::new();
    let mut messages: Vec<Value> = Vec::new();

    let msgs = openai["messages"].as_array().cloned().unwrap_or_default();
    for m in &msgs {
        let role = m["role"].as_str().unwrap_or("user");
        match role {
            "system" | "developer" => {
                system.push_str(&util::content_to_text(&m["content"]));
                system.push('\n');
            }
            "user" => push_merged(&mut messages, "user", convert_parts(&m["content"], false)),
            "assistant" => {
                let mut blocks = convert_parts(&m["content"], true);
                for tc in m["tool_calls"].as_array().unwrap_or(&Vec::new()) {
                    let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
                    let input: Value =
                        serde_json::from_str(args).unwrap_or(json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc["id"].as_str().unwrap_or("toolu_unknown"),
                        "name": tc["function"]["name"],
                        "input": input
                    }));
                }
                push_merged(&mut messages, "assistant", blocks);
            }
            "tool" => {
                // OpenAI: role "tool" -> Anthropic: tool_result di dalam user message
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m["tool_call_id"].as_str().unwrap_or(""),
                    "content": [{"type": "text", "text": util::content_to_text(&m["content"])}]
                });
                push_merged(&mut messages, "user", vec![block]);
            }
            _ => {}
        }
    }

    let mut obj = Map::new();
    obj.insert("model".into(), json!(model));
    obj.insert("messages".into(), Value::Array(messages));
    // Anthropic wajib max_tokens
    obj.insert(
        "max_tokens".into(),
        json!(openai["max_tokens"].as_u64().unwrap_or(4096)),
    );
    if !system.trim().is_empty() {
        obj.insert("system".into(), json!(system.trim()));
    }
    if let Some(t) = openai["temperature"].as_f64() {
        obj.insert("temperature".into(), json!(t));
    }
    if let Some(t) = openai["top_p"].as_f64() {
        obj.insert("top_p".into(), json!(t));
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
            obj.insert("stop_sequences".into(), json!(seqs));
        }
    }
    if let Some(tools) = openai["tools"].as_array() {
        let converted: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let f = &t["function"];
                if !f["name"].is_string() {
                    return None;
                }
                let schema = if f["parameters"].is_object() {
                    f["parameters"].clone()
                } else {
                    json!({"type": "object", "properties": {}})
                };
                Some(json!({
                    "name": f["name"],
                    "description": f["description"],
                    "input_schema": schema
                }))
            })
            .collect();
        if !converted.is_empty() {
            obj.insert("tools".into(), Value::Array(converted));
            match openai["tool_choice"].as_str() {
                Some("required") => {
                    obj.insert("tool_choice".into(), json!({"type": "any"}));
                }
                Some("auto") => {
                    obj.insert("tool_choice".into(), json!({"type": "auto"}));
                }
                _ => {
                    if openai["tool_choice"]["function"]["name"].is_string() {
                        obj.insert(
                            "tool_choice".into(),
                            json!({"type": "tool", "name": openai["tool_choice"]["function"]["name"]}),
                        );
                    }
                }
            }
        }
    }

    Ok(Value::Object(obj))
}

// ============================================================
// Response: Anthropic -> OpenAI format (non-streaming)
// ============================================================

fn map_stop(reason: &str) -> &'static str {
    match reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        _ => "stop", // end_turn, stop_sequence, dll
    }
}

pub fn transform_response(v: &Value, requested_model: &str) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in v["content"].as_array().unwrap_or(&Vec::new()) {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or("")),
            Some("tool_use") => tool_calls.push(json!({
                "id": block["id"].as_str().unwrap_or("call_0"),
                "type": "function",
                "function": {
                    "name": block["name"],
                    "arguments": serde_json::to_string(&block["input"]).unwrap_or_else(|_| "{}".into())
                }
            })),
            _ => {}
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

    let input = v["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let output = v["usage"]["output_tokens"].as_u64().unwrap_or(0);

    json!({
        "id": v["id"].as_str().unwrap_or("chatcmpl-anthropic"),
        "object": "chat.completion",
        "created": util::now_secs(),
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": map_stop(v["stop_reason"].as_str().unwrap_or("end_turn"))
        }],
        "usage": {
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_tokens": input + output
        }
    })
}

// ============================================================
// Streaming: Anthropic SSE -> OpenAI chunk SSE
// ============================================================

struct AnthropicStream {
    model: String,
    id: String,
    created: u64,
    /// anthropic block index -> openai tool_calls index
    tool_map: HashMap<u64, usize>,
    tool_counter: usize,
    finish: Option<String>,
    usage_prompt: u64,
    usage_completion: u64,
}

impl AnthropicStream {
    fn new(model: String) -> Self {
        Self {
            model,
            id: format!("chatcmpl-anthropic-{}", util::now_secs()),
            created: util::now_secs(),
            tool_map: HashMap::new(),
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

    fn handle(&mut self, event: &str, data: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return out;
        };

        match event {
            "message_start" => {
                self.usage_prompt = v["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap_or(self.usage_prompt);
                out.push(self.chunk(json!({"role": "assistant", "content": ""}), None));
            }
            "content_block_start" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                if v["content_block"]["type"].as_str() == Some("tool_use") {
                    let ti = self.tool_counter;
                    self.tool_counter += 1;
                    self.tool_map.insert(idx, ti);
                    out.push(self.chunk(
                        json!({"tool_calls": [{
                            "index": ti,
                            "id": v["content_block"]["id"],
                            "type": "function",
                            "function": {"name": v["content_block"]["name"], "arguments": ""}
                        }]}),
                        None,
                    ));
                }
            }
            "content_block_delta" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                match v["delta"]["type"].as_str() {
                    Some("text_delta") => {
                        out.push(self.chunk(json!({"content": v["delta"]["text"]}), None));
                    }
                    Some("input_json_delta") => {
                        if let Some(&ti) = self.tool_map.get(&idx) {
                            out.push(self.chunk(
                                json!({"tool_calls": [{
                                    "index": ti,
                                    "function": {"arguments": v["delta"]["partial_json"]}
                                }]}),
                                None,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                self.usage_completion = v["usage"]["output_tokens"]
                    .as_u64()
                    .unwrap_or(self.usage_completion);
                let stop = v["delta"]["stop_reason"].as_str().unwrap_or("end_turn");
                self.finish = Some(map_stop(stop).to_string());
            }
            "message_stop" => {
                out.push(self.chunk(json!({}), Some(self.finish.as_deref().unwrap_or("stop"))));
            }
            _ => {}
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

    let url = format!("{}/messages", pc.base_url.trim_end_matches('/'));
    let payload = transform_request(body, &route.upstream)?;

    let mut req = state
        .http
        .post(&url)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
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
        let prompt = v["usage"]["input_tokens"].as_u64().unwrap_or(0);
        let completion = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
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

    // Streaming: konversi event Anthropic -> chunk OpenAI secara real-time.
    use eventsource_stream::Eventsource;
    use futures::StreamExt;

    let mut events = Box::pin(resp.bytes_stream().eventsource());
    let mut st = AnthropicStream::new(requested_model.to_string());

    // Clone buat logging di akhir stream (closure harus 'static).
    let log_state = state.clone();
    let log_model = requested_model.to_string();
    let log_route = route.clone();
    let log_key = client_key.clone();

    let out = async_stream::stream! {
        while let Some(ev) = events.next().await {
            match ev {
                Ok(event) => {
                    for chunk in st.handle(&event.event, &event.data) {
                        let mut buf = String::from("data: ");
                        buf.push_str(&chunk.to_string());
                        buf.push_str("\n\n");
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(buf));
                    }
                }
                Err(e) => {
                    tracing::warn!("anthropic stream error: {e}");
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
