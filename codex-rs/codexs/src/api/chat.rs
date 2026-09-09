//! `POST /v1/chat/completions` — Chat Completions front-end.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::Event;
use axum::response::sse::KeepAlive;
use axum::response::sse::Sse;
use serde_json::Value;
use serde_json::json;

use super::openai::ChatMeta;
use super::openai::parse_chat;
use super::server::ApiError;
use super::server::AppState;
use super::server::apply_request_context;
use super::server::now_secs;
use crate::bridge::RunHandle;
use crate::bridge::types::BridgeEvent;
use crate::bridge::types::DoneReason;
use crate::bridge::types::Usage;
use crate::config::ApiConfig;

pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    super::server::dump_incoming("/v1/chat/completions", &headers, &body);
    let (mut req, meta) = parse_chat(&body)?;
    apply_request_context(&mut req, &headers, &body, &state.cfg)?;

    let run = state.bridge.run(req).await?;
    let writer = ChatWriter::new(&run, &meta, &state.cfg.api);
    if meta.stream {
        Ok(stream_response(run, writer).into_response())
    } else {
        let mut run = run;
        let mut writer = writer;
        while let Some(ev) = run.events.recv().await {
            let done = matches!(ev, BridgeEvent::Done { .. });
            writer.handle(ev);
            if done {
                break;
            }
        }
        run.finish();
        if let Some(err) = writer.error.clone() {
            return Err(ApiError::turn_failed(err));
        }
        Ok(Json(writer.final_completion()).into_response())
    }
}

fn stream_response(mut run: RunHandle, mut writer: ChatWriter) -> impl IntoResponse {
    let stream = async_stream::stream! {
        loop {
            let Some(ev) = run.events.recv().await else {
                for frame in writer.handle(BridgeEvent::Done {
                    reason: DoneReason::Error,
                    error: Some("codex turn ended without completion".to_string()),
                }) {
                    yield Ok::<Event, Infallible>(frame);
                }
                break;
            };
            let done = matches!(ev, BridgeEvent::Done { .. });
            for frame in writer.handle(ev) {
                yield Ok(frame);
            }
            if done {
                run.finish();
                break;
            }
        }
        yield Ok(Event::default().data("[DONE]"));
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

struct ChatWriter {
    id: String,
    created: u64,
    model: String,
    include_usage: bool,
    reasoning_content: bool,
    activity_in_reasoning: bool,
    sent_role: bool,
    message_start: usize,
    content: String,
    reasoning: String,
    tool_calls: Vec<Value>,
    usage: Option<Usage>,
    finish_reason: Option<&'static str>,
    error: Option<String>,
    session_id: String,
}

impl ChatWriter {
    fn new(run: &RunHandle, meta: &ChatMeta, api: &ApiConfig) -> Self {
        Self {
            id: format!("chatcmpl-{}", run.response_id.trim_start_matches("resp_")),
            created: now_secs(),
            model: run.model.clone(),
            include_usage: meta.include_usage,
            reasoning_content: api.chat_reasoning_content,
            activity_in_reasoning: api.chat_activity_in_reasoning,
            sent_role: false,
            message_start: 0,
            content: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            usage: None,
            finish_reason: None,
            error: None,
            session_id: run.session.id.clone(),
        }
    }

    fn chunk(&mut self, delta: Value, finish_reason: Option<&str>) -> Event {
        let mut delta = delta;
        if !self.sent_role
            && let Some(o) = delta.as_object_mut()
        {
            o.insert("role".into(), json!("assistant"));
            self.sent_role = true;
        }
        let body = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "system_fingerprint": format!("asxs_{}", self.session_id),
            "choices": [{ "index": 0, "delta": delta, "logprobs": null, "finish_reason": finish_reason }],
        });
        Event::default().data(body.to_string())
    }

    fn reasoning_delta(&mut self, text: &str) -> Option<Event> {
        if text.is_empty() {
            return None;
        }
        self.reasoning.push_str(text);
        Some(self.chunk(json!({ "reasoning_content": text }), None))
    }

    fn handle(&mut self, ev: BridgeEvent) -> Vec<Event> {
        let mut frames = Vec::new();
        match ev {
            BridgeEvent::MessageStart { .. } => {
                // Consecutive agent messages are joined with a blank line, matching
                // the transcript text the bridge records for session matching.
                if !self.content.is_empty() {
                    self.content.push_str("\n\n");
                    frames.push(self.chunk(json!({ "content": "\n\n" }), None));
                } else if !self.sent_role {
                    frames.push(self.chunk(json!({ "content": "" }), None));
                }
                self.message_start = self.content.len();
            }
            BridgeEvent::TextDelta { delta, .. } => {
                if !delta.is_empty() {
                    self.content.push_str(&delta);
                    frames.push(self.chunk(json!({ "content": delta }), None));
                }
            }
            BridgeEvent::MessageEnd { text, .. } => {
                // Deltas may be absent (non-streamed model output); reconcile with
                // the completed item text.
                let streamed = self.content.get(self.message_start..).unwrap_or_default();
                if streamed.is_empty() && !text.is_empty() {
                    self.content.push_str(&text);
                    frames.push(self.chunk(json!({ "content": text }), None));
                }
            }
            BridgeEvent::ReasoningStart { .. } => {}
            BridgeEvent::ReasoningPart { index, .. } => {
                if self.reasoning_content && index > 0 {
                    if let Some(f) = self.reasoning_delta("\n\n") {
                        frames.push(f);
                    }
                }
            }
            BridgeEvent::ReasoningDelta { delta, .. } => {
                if self.reasoning_content
                    && let Some(f) = self.reasoning_delta(&delta)
                {
                    frames.push(f);
                }
            }
            BridgeEvent::ReasoningEnd { .. } => {
                if self.reasoning_content
                    && !self.reasoning.is_empty()
                    && !self.reasoning.ends_with('\n')
                    && let Some(f) = self.reasoning_delta("\n")
                {
                    frames.push(f);
                }
            }
            BridgeEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                let index = self.tool_calls.len();
                self.tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                }));
                frames.push(self.chunk(
                    json!({
                        "tool_calls": [{
                            "index": index,
                            "id": call_id,
                            "type": "function",
                            "function": { "name": name, "arguments": arguments },
                        }]
                    }),
                    None,
                ));
            }
            BridgeEvent::ActivityStart { item } => {
                if self.activity_in_reasoning
                    && let Some(line) = activity_line(&item, false)
                    && let Some(f) = self.reasoning_delta(&line)
                {
                    frames.push(f);
                }
            }
            BridgeEvent::ActivityDelta { .. } => {}
            BridgeEvent::ActivityEnd { item } => {
                if self.activity_in_reasoning
                    && let Some(line) = activity_line(&item, true)
                    && let Some(f) = self.reasoning_delta(&line)
                {
                    frames.push(f);
                }
            }
            BridgeEvent::Usage { usage, .. } => self.usage = Some(usage),
            BridgeEvent::Done { reason, error } => {
                let finish = match reason {
                    DoneReason::Stop => "stop",
                    DoneReason::ToolCalls => "tool_calls",
                    DoneReason::Interrupted => "stop",
                    DoneReason::Error => "stop",
                };
                self.finish_reason = Some(finish);
                if reason == DoneReason::Error {
                    let message = error.unwrap_or_else(|| "turn failed".to_string());
                    self.error = Some(message.clone());
                    frames.push(Event::default().data(
                        json!({ "error": { "message": message, "type": "server_error", "code": "codex_turn_failed" } })
                            .to_string(),
                    ));
                }
                frames.push(self.chunk(json!({}), Some(finish)));
                if self.include_usage {
                    let usage = self.usage.clone().unwrap_or_default();
                    let body = json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "created": self.created,
                        "model": self.model,
                        "choices": [],
                        "usage": chat_usage(&usage),
                    });
                    frames.push(Event::default().data(body.to_string()));
                }
            }
        }
        frames
    }

    fn final_completion(&self) -> Value {
        let mut message = json!({ "role": "assistant", "content": self.content, "refusal": null });
        if let Some(o) = message.as_object_mut() {
            if self.content.is_empty() && !self.tool_calls.is_empty() {
                o.insert("content".into(), Value::Null);
            }
            if !self.tool_calls.is_empty() {
                o.insert("tool_calls".into(), Value::Array(self.tool_calls.clone()));
            }
            if self.reasoning_content && !self.reasoning.is_empty() {
                o.insert("reasoning_content".into(), json!(self.reasoning));
            }
        }
        let usage = self.usage.clone().unwrap_or_default();
        json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "system_fingerprint": format!("asxs_{}", self.session_id),
            "choices": [{
                "index": 0,
                "message": message,
                "logprobs": null,
                "finish_reason": self.finish_reason.unwrap_or("stop"),
            }],
            "usage": chat_usage(&usage),
        })
    }
}

fn chat_usage(u: &Usage) -> Value {
    json!({
        "prompt_tokens": u.input_tokens,
        "completion_tokens": u.output_tokens,
        "total_tokens": if u.total_tokens > 0 { u.total_tokens } else { u.input_tokens + u.output_tokens },
        "prompt_tokens_details": { "cached_tokens": u.cached_input_tokens },
        "completion_tokens_details": { "reasoning_tokens": u.reasoning_output_tokens },
    })
}

/// One-line rendering of a Codex activity item for `reasoning_content`.
fn activity_line(item: &Value, completed: bool) -> Option<String> {
    let ty = item.get("type").and_then(Value::as_str)?;
    let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default();
    let line = match (ty, completed) {
        ("commandExecution", false) => format!("[codex] $ {}\n", s("command")),
        ("commandExecution", true) => {
            let out = s("aggregatedOutput");
            let exit = item.get("exitCode").and_then(Value::as_i64).unwrap_or(-1);
            let mut o = out.trim_end().to_string();
            if o.len() > 4000 {
                let cut = o
                    .char_indices()
                    .nth(4000)
                    .map(|(i, _)| i)
                    .unwrap_or(o.len());
                o.truncate(cut);
                o.push_str("\n…");
            }
            if o.is_empty() {
                format!("[codex] exit {exit}\n")
            } else {
                format!("{o}\n[codex] exit {exit}\n")
            }
        }
        ("fileChange", true) => {
            let paths: Vec<String> = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.get("path").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            format!("[codex] edited {}\n", paths.join(", "))
        }
        ("webSearch", true) => format!("[codex] web search: {}\n", s("query")),
        ("mcpToolCall", true) => format!("[codex] mcp {}/{}\n", s("server"), s("tool")),
        ("plan", true) => format!("[codex] plan:\n{}\n", s("text")),
        _ => return None,
    };
    Some(line)
}
