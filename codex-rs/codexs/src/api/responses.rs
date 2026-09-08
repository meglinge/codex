//! `POST /v1/responses` — OpenAI Responses API front-end.

use std::collections::HashMap;
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

use super::openai::ResponsesMeta;
use super::openai::parse_responses;
use super::server::ApiError;
use super::server::AppState;
use super::server::now_secs;
use super::server::overrides_from_headers;
use crate::bridge::RunHandle;
use crate::bridge::types::BridgeEvent;
use crate::bridge::types::DoneReason;
use crate::bridge::types::Usage;

pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let (mut req, meta) = parse_responses(&body)?;
    let ov = overrides_from_headers(&headers);
    req.session_id = ov.session_id;
    req.account_id = ov.account_id;
    req.codex_tools = ov.codex_tools;

    let run = state.bridge.run(req).await?;
    let writer = ResponsesWriter::new(&run, &meta, state.cfg.api.expose_activity);

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
        let response = writer.final_response();
        let status = if writer.failed {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        } else {
            axum::http::StatusCode::OK
        };
        Ok((status, Json(response)).into_response())
    }
}

fn stream_response(mut run: RunHandle, mut writer: ResponsesWriter) -> impl IntoResponse {
    let stream = async_stream::stream! {
        for frame in writer.opening() {
            yield Ok::<Event, Infallible>(frame);
        }
        loop {
            let Some(ev) = run.events.recv().await else {
                for frame in writer.handle(BridgeEvent::Done {
                    reason: DoneReason::Error,
                    error: Some("codex turn ended without completion".to_string()),
                }) {
                    yield Ok(frame);
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
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

struct OpenItem {
    output_index: usize,
    id: String,
    /// Reasoning: summary parts seen so far.
    parts: Vec<String>,
    text: String,
}

pub struct ResponsesWriter {
    response: Value,
    seq: u64,
    output: Vec<Value>,
    open: HashMap<String, OpenItem>,
    activity_open: HashMap<String, OpenItem>,
    usage: Option<Usage>,
    expose_activity: bool,
    pub failed: bool,
}

impl ResponsesWriter {
    fn new(run: &RunHandle, meta: &ResponsesMeta, expose_activity: bool) -> Self {
        let response = json!({
            "id": run.response_id,
            "object": "response",
            "created_at": now_secs(),
            "status": "in_progress",
            "background": false,
            "error": null,
            "incomplete_details": null,
            "instructions": meta.instructions_echo,
            "max_output_tokens": null,
            "model": run.model,
            "output": [],
            "parallel_tool_calls": true,
            "previous_response_id": null,
            "reasoning": if meta.reasoning_echo.is_null() { json!({ "effort": null, "summary": null }) } else { meta.reasoning_echo.clone() },
            "store": meta.store,
            "temperature": null,
            "text": { "format": { "type": "text" } },
            "tool_choice": "auto",
            "tools": meta.tools_echo,
            "top_p": null,
            "truncation": "disabled",
            "usage": null,
            "user": null,
            "metadata": meta.metadata,
            "asxs": {
                "session_id": run.session.id,
                "account": run.session.runtime.id,
                "thread_id": run.session.thread_id,
                "turn_id": run.turn.turn_id,
            },
        });
        Self {
            response,
            seq: 0,
            output: Vec::new(),
            open: HashMap::new(),
            activity_open: HashMap::new(),
            usage: None,
            expose_activity,
            failed: false,
        }
    }

    fn event(&mut self, name: &str, mut data: Value) -> Event {
        if let Some(o) = data.as_object_mut() {
            o.insert("type".into(), json!(name));
            o.insert("sequence_number".into(), json!(self.seq));
        }
        self.seq += 1;
        Event::default().event(name).data(data.to_string())
    }

    fn opening(&mut self) -> Vec<Event> {
        vec![
            self.event("response.created", json!({ "response": self.response })),
            self.event("response.in_progress", json!({ "response": self.response })),
        ]
    }

    fn push_output(&mut self, item: Value) -> usize {
        self.output.push(item);
        self.output.len() - 1
    }

    fn handle(&mut self, ev: BridgeEvent) -> Vec<Event> {
        let mut frames = Vec::new();
        match ev {
            BridgeEvent::MessageStart { item_id, .. } => {
                let id = format!("msg_{}", short_id(&item_id));
                let item = json!({ "id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": [] });
                let idx = self.push_output(item.clone());
                self.open.insert(
                    item_id,
                    OpenItem {
                        output_index: idx,
                        id: id.clone(),
                        parts: Vec::new(),
                        text: String::new(),
                    },
                );
                frames.push(self.event(
                    "response.output_item.added",
                    json!({ "output_index": idx, "item": item }),
                ));
                frames.push(self.event(
                    "response.content_part.added",
                    json!({ "item_id": id, "output_index": idx, "content_index": 0, "part": { "type": "output_text", "text": "", "annotations": [] } }),
                ));
            }
            BridgeEvent::TextDelta { item_id, delta } => {
                if !self.open.contains_key(&item_id) {
                    frames.extend(self.handle(BridgeEvent::MessageStart {
                        item_id: item_id.clone(),
                        phase: None,
                    }));
                }
                let (idx, id) = match self.open.get_mut(&item_id) {
                    Some(o) => {
                        o.text.push_str(&delta);
                        (o.output_index, o.id.clone())
                    }
                    None => return frames,
                };
                frames.push(self.event(
                    "response.output_text.delta",
                    json!({ "item_id": id, "output_index": idx, "content_index": 0, "delta": delta, "logprobs": [] }),
                ));
            }
            BridgeEvent::MessageEnd { item_id, text, .. } => {
                if !self.open.contains_key(&item_id) {
                    frames.extend(self.handle(BridgeEvent::MessageStart {
                        item_id: item_id.clone(),
                        phase: None,
                    }));
                    if !text.is_empty() {
                        frames.extend(self.handle(BridgeEvent::TextDelta {
                            item_id: item_id.clone(),
                            delta: text.clone(),
                        }));
                    }
                }
                let Some(o) = self.open.remove(&item_id) else {
                    return frames;
                };
                let final_text = if text.is_empty() { o.text.clone() } else { text };
                let part = json!({ "type": "output_text", "text": final_text, "annotations": [], "logprobs": [] });
                let item = json!({ "id": o.id, "type": "message", "status": "completed", "role": "assistant", "content": [part.clone()] });
                if let Some(slot) = self.output.get_mut(o.output_index) {
                    *slot = item.clone();
                }
                frames.push(self.event(
                    "response.output_text.done",
                    json!({ "item_id": o.id, "output_index": o.output_index, "content_index": 0, "text": final_text, "logprobs": [] }),
                ));
                frames.push(self.event(
                    "response.content_part.done",
                    json!({ "item_id": o.id, "output_index": o.output_index, "content_index": 0, "part": part }),
                ));
                frames.push(self.event(
                    "response.output_item.done",
                    json!({ "output_index": o.output_index, "item": item }),
                ));
            }
            BridgeEvent::ReasoningStart { item_id } => {
                let id = format!("rs_{}", short_id(&item_id));
                let item = json!({ "id": id, "type": "reasoning", "summary": [], "status": "in_progress" });
                let idx = self.push_output(item.clone());
                self.open.insert(
                    item_id,
                    OpenItem {
                        output_index: idx,
                        id,
                        parts: Vec::new(),
                        text: String::new(),
                    },
                );
                frames.push(self.event(
                    "response.output_item.added",
                    json!({ "output_index": idx, "item": item }),
                ));
            }
            BridgeEvent::ReasoningPart { item_id, index } => {
                if !self.open.contains_key(&item_id) {
                    frames.extend(self.handle(BridgeEvent::ReasoningStart {
                        item_id: item_id.clone(),
                    }));
                }
                let Some(o) = self.open.get_mut(&item_id) else {
                    return frames;
                };
                let index = index.max(0) as usize;
                while o.parts.len() <= index {
                    o.parts.push(String::new());
                }
                let (id, idx) = (o.id.clone(), o.output_index);
                frames.push(self.event(
                    "response.reasoning_summary_part.added",
                    json!({ "item_id": id, "output_index": idx, "summary_index": index, "part": { "type": "summary_text", "text": "" } }),
                ));
            }
            BridgeEvent::ReasoningDelta {
                item_id,
                delta,
                index,
            } => {
                let index_u = index.max(0) as usize;
                let needs_part = self
                    .open
                    .get(&item_id)
                    .map(|o| o.parts.len() <= index_u)
                    .unwrap_or(true);
                if needs_part {
                    frames.extend(self.handle(BridgeEvent::ReasoningPart {
                        item_id: item_id.clone(),
                        index,
                    }));
                }
                let Some(o) = self.open.get_mut(&item_id) else {
                    return frames;
                };
                if let Some(p) = o.parts.get_mut(index_u) {
                    p.push_str(&delta);
                }
                let (id, idx) = (o.id.clone(), o.output_index);
                frames.push(self.event(
                    "response.reasoning_summary_text.delta",
                    json!({ "item_id": id, "output_index": idx, "summary_index": index_u, "delta": delta }),
                ));
            }
            BridgeEvent::ReasoningEnd {
                item_id, summary, ..
            } => {
                let Some(o) = self.open.remove(&item_id) else {
                    return frames;
                };
                let parts: Vec<String> = if summary.is_empty() { o.parts.clone() } else { summary };
                for (i, text) in parts.iter().enumerate() {
                    frames.push(self.event(
                        "response.reasoning_summary_text.done",
                        json!({ "item_id": o.id, "output_index": o.output_index, "summary_index": i, "text": text }),
                    ));
                    frames.push(self.event(
                        "response.reasoning_summary_part.done",
                        json!({ "item_id": o.id, "output_index": o.output_index, "summary_index": i, "part": { "type": "summary_text", "text": text } }),
                    ));
                }
                let item = json!({
                    "id": o.id,
                    "type": "reasoning",
                    "status": "completed",
                    "summary": parts.iter().map(|t| json!({ "type": "summary_text", "text": t })).collect::<Vec<_>>(),
                });
                if let Some(slot) = self.output.get_mut(o.output_index) {
                    *slot = item.clone();
                }
                frames.push(self.event(
                    "response.output_item.done",
                    json!({ "output_index": o.output_index, "item": item }),
                ));
            }
            BridgeEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                let id = format!("fc_{}", short_id(&call_id));
                let item = json!({ "id": id, "type": "function_call", "status": "in_progress", "call_id": call_id, "name": name, "arguments": "" });
                let idx = self.push_output(item.clone());
                frames.push(self.event(
                    "response.output_item.added",
                    json!({ "output_index": idx, "item": item }),
                ));
                frames.push(self.event(
                    "response.function_call_arguments.delta",
                    json!({ "item_id": id, "output_index": idx, "delta": arguments }),
                ));
                frames.push(self.event(
                    "response.function_call_arguments.done",
                    json!({ "item_id": id, "output_index": idx, "arguments": arguments }),
                ));
                let done = json!({ "id": id, "type": "function_call", "status": "completed", "call_id": call_id, "name": name, "arguments": arguments });
                if let Some(slot) = self.output.get_mut(idx) {
                    *slot = done.clone();
                }
                frames.push(self.event(
                    "response.output_item.done",
                    json!({ "output_index": idx, "item": done }),
                ));
            }
            BridgeEvent::ActivityStart { item } => {
                if !self.expose_activity {
                    return frames;
                }
                let item_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let id = format!("cx_{}", short_id(&item_id));
                let out_item = json!({ "id": id, "type": "codex_activity", "status": "in_progress", "activity": item });
                let idx = self.push_output(out_item.clone());
                self.activity_open.insert(
                    item_id,
                    OpenItem {
                        output_index: idx,
                        id,
                        parts: Vec::new(),
                        text: String::new(),
                    },
                );
                frames.push(self.event(
                    "response.output_item.added",
                    json!({ "output_index": idx, "item": out_item }),
                ));
            }
            BridgeEvent::ActivityDelta { item_id, delta } => {
                if let Some(o) = self.activity_open.get_mut(&item_id) {
                    o.text.push_str(&delta);
                    let (id, idx) = (o.id.clone(), o.output_index);
                    frames.push(self.event(
                        "response.codex_activity.output_delta",
                        json!({ "item_id": id, "output_index": idx, "delta": delta }),
                    ));
                }
            }
            BridgeEvent::ActivityEnd { item } => {
                if !self.expose_activity {
                    return frames;
                }
                let item_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let (id, idx) = match self.activity_open.remove(&item_id) {
                    Some(o) => (o.id, o.output_index),
                    None => {
                        let id = format!("cx_{}", short_id(&item_id));
                        let placeholder = json!({ "id": id, "type": "codex_activity", "status": "in_progress", "activity": Value::Null });
                        let idx = self.push_output(placeholder.clone());
                        frames.push(self.event(
                            "response.output_item.added",
                            json!({ "output_index": idx, "item": placeholder }),
                        ));
                        (id, idx)
                    }
                };
                let out_item = json!({ "id": id, "type": "codex_activity", "status": "completed", "activity": item });
                if let Some(slot) = self.output.get_mut(idx) {
                    *slot = out_item.clone();
                }
                frames.push(self.event(
                    "response.output_item.done",
                    json!({ "output_index": idx, "item": out_item }),
                ));
            }
            BridgeEvent::Usage { usage, .. } => {
                self.usage = Some(usage);
            }
            BridgeEvent::Done { reason, error } => {
                // Close anything still open (interrupted mid-item).
                let open: Vec<String> = self.open.keys().cloned().collect();
                for item_id in open {
                    let is_reasoning = self
                        .open
                        .get(&item_id)
                        .map(|o| o.id.starts_with("rs_"))
                        .unwrap_or(false);
                    if is_reasoning {
                        frames.extend(self.handle(BridgeEvent::ReasoningEnd {
                            item_id,
                            summary: Vec::new(),
                            content: Vec::new(),
                        }));
                    } else {
                        frames.extend(self.handle(BridgeEvent::MessageEnd {
                            item_id,
                            text: String::new(),
                            phase: None,
                        }));
                    }
                }
                let (status, event_name) = match reason {
                    DoneReason::Stop | DoneReason::ToolCalls => ("completed", "response.completed"),
                    DoneReason::Interrupted => ("incomplete", "response.incomplete"),
                    DoneReason::Error => ("failed", "response.failed"),
                };
                self.failed = reason == DoneReason::Error;
                self.finalize(status, reason, error);
                let response = self.response.clone();
                frames.push(self.event(event_name, json!({ "response": response })));
            }
        }
        frames
    }

    fn finalize(&mut self, status: &str, reason: DoneReason, error: Option<String>) {
        let usage = self.usage.clone().unwrap_or_default();
        if let Some(o) = self.response.as_object_mut() {
            o.insert("status".into(), json!(status));
            o.insert("output".into(), Value::Array(self.output.clone()));
            o.insert("usage".into(), usage_json(&usage));
            if reason == DoneReason::Interrupted {
                o.insert("incomplete_details".into(), json!({ "reason": "interrupted" }));
            }
            if reason == DoneReason::Error {
                o.insert(
                    "error".into(),
                    json!({ "code": "server_error", "message": error.unwrap_or_else(|| "turn failed".to_string()) }),
                );
            }
        }
    }

    fn final_response(&self) -> Value {
        self.response.clone()
    }
}

pub fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "input_tokens_details": { "cached_tokens": u.cached_input_tokens },
        "output_tokens": u.output_tokens,
        "output_tokens_details": { "reasoning_tokens": u.reasoning_output_tokens },
        "total_tokens": if u.total_tokens > 0 { u.total_tokens } else { u.input_tokens + u.output_tokens },
    })
}

fn short_id(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if cleaned.is_empty() {
        uuid::Uuid::new_v4().simple().to_string()
    } else {
        cleaned
    }
}
