//! Lowering of OpenAI wire formats (Responses API, Chat Completions) into the
//! provider-neutral [`ConversationRequest`].

use serde_json::Value;

use super::server::ApiError;
use crate::bridge::types::CanonMessage;
use crate::bridge::types::ConversationRequest;
use crate::bridge::types::ToolCallRecord;
use crate::bridge::types::ToolOutput;
use crate::bridge::types::ToolSpec;
use crate::bridge::types::UserPart;

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Accepts both the Responses flat tool shape and the Chat nested shape.
pub fn parse_tools(v: Option<&Value>) -> Result<Vec<ToolSpec>, ApiError> {
    let mut out = Vec::new();
    let Some(arr) = v.and_then(Value::as_array) else {
        return Ok(out);
    };
    for t in arr {
        let ty = str_of(t, "type").unwrap_or("function");
        if ty != "function" {
            // web_search / file_search / code_interpreter etc. are not bridged.
            continue;
        }
        let nested = t.get("function");
        let f = nested.unwrap_or(t);
        let Some(name) = non_empty(str_of(f, "name")) else {
            return Err(ApiError::bad_request("tool is missing a name"));
        };
        if nested.is_some() {
            // Chat nested shape: flatten into the Responses tool object.
            out.push(ToolSpec::from_parts(name, f, ty));
        } else {
            // Responses flat shape: keep the object verbatim.
            let mut spec = ToolSpec::from_parts(name, f, ty);
            spec.raw = t.clone();
            out.push(spec);
        }
    }
    Ok(out)
}

fn push_assistant(
    history: &mut Vec<CanonMessage>,
    text: Option<String>,
    call: Option<ToolCallRecord>,
) {
    if let Some(CanonMessage::Assistant {
        text: t,
        tool_calls,
    }) = history.last_mut()
    {
        if let Some(new_text) = text {
            if !t.is_empty() && !new_text.is_empty() {
                t.push_str("\n\n");
            }
            t.push_str(&new_text);
        }
        if let Some(c) = call {
            tool_calls.push(c);
        }
        return;
    }
    history.push(CanonMessage::Assistant {
        text: text.unwrap_or_default(),
        tool_calls: call.into_iter().collect(),
    });
}

fn content_parts(content: &Value) -> Vec<UserPart> {
    match content {
        Value::String(s) => vec![UserPart::Text(s.clone())],
        Value::Array(items) => {
            let mut parts = Vec::new();
            for it in items {
                match str_of(it, "type").unwrap_or("text") {
                    "input_text" | "text" | "output_text" => {
                        if let Some(t) = str_of(it, "text") {
                            parts.push(UserPart::Text(t.to_string()));
                        }
                    }
                    "input_image" | "image_url" | "image" => {
                        let url = match it.get("image_url") {
                            Some(Value::String(s)) => Some(s.clone()),
                            Some(obj) => str_of(obj, "url").map(str::to_string),
                            None => str_of(it, "url").map(str::to_string),
                        };
                        let detail = str_of(it, "detail")
                            .or_else(|| it.get("image_url").and_then(|o| str_of(o, "detail")))
                            .map(str::to_string);
                        if let Some(url) = url {
                            parts.push(UserPart::Image { url, detail });
                        }
                    }
                    _ => {}
                }
            }
            parts
        }
        _ => Vec::new(),
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|it| str_of(it, "text"))
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn output_to_string(output: &Value) -> (String, Vec<String>) {
    match output {
        Value::String(s) => (s.clone(), Vec::new()),
        Value::Array(items) => {
            let mut text = String::new();
            let mut images = Vec::new();
            for it in items {
                match str_of(it, "type").unwrap_or("text") {
                    "input_image" | "image_url" | "image" => {
                        let url = match it.get("image_url") {
                            Some(Value::String(s)) => Some(s.clone()),
                            Some(obj) => str_of(obj, "url").map(str::to_string),
                            None => str_of(it, "url").map(str::to_string),
                        };
                        if let Some(u) = url {
                            images.push(u);
                        }
                    }
                    _ => {
                        if let Some(t) = str_of(it, "text") {
                            text.push_str(t);
                        }
                    }
                }
            }
            (text, images)
        }
        Value::Null => (String::new(), Vec::new()),
        other => (other.to_string(), Vec::new()),
    }
}

pub struct ResponsesMeta {
    pub stream: bool,
    pub tools_echo: Value,
    pub reasoning_echo: Value,
    pub instructions_echo: Option<String>,
    pub metadata: Value,
    pub store: bool,
}

/// Responses API request -> ConversationRequest.
pub fn parse_responses(body: &Value) -> Result<(ConversationRequest, ResponsesMeta), ApiError> {
    let mut req = ConversationRequest {
        model: non_empty(str_of(body, "model")),
        previous_response_id: non_empty(str_of(body, "previous_response_id")),
        service_tier: non_empty(str_of(body, "service_tier")).filter(|t| t != "auto"),
        tools: parse_tools(body.get("tools"))?,
        ..ConversationRequest::default()
    };
    let mut instructions: Vec<String> = Vec::new();
    if let Some(i) = non_empty(str_of(body, "instructions")) {
        instructions.push(i);
    }
    if let Some(r) = body.get("reasoning") {
        req.reasoning_effort = non_empty(str_of(r, "effort"));
        req.reasoning_summary = non_empty(str_of(r, "summary"));
    }
    if let Some(fmt) = body.get("text").and_then(|t| t.get("format"))
        && str_of(fmt, "type") == Some("json_schema")
    {
        req.output_schema = fmt.get("schema").cloned();
    }

    let mut history: Vec<CanonMessage> = Vec::new();
    match body.get("input") {
        Some(Value::String(s)) => history.push(CanonMessage::User(vec![UserPart::Text(s.clone())])),
        Some(Value::Array(items)) => {
            for it in items {
                let ty = str_of(it, "type");
                let role = str_of(it, "role");
                match (ty, role) {
                    (Some("function_call"), _) => {
                        let call = ToolCallRecord {
                            call_id: str_of(it, "call_id").unwrap_or_default().to_string(),
                            name: str_of(it, "name").unwrap_or_default().to_string(),
                            arguments: match it.get("arguments") {
                                Some(Value::String(s)) => s.clone(),
                                Some(v) => v.to_string(),
                                None => "{}".to_string(),
                            },
                        };
                        push_assistant(&mut history, None, Some(call));
                    }
                    (Some("function_call_output"), _) => {
                        let (output, images) =
                            output_to_string(it.get("output").unwrap_or(&Value::Null));
                        history.push(CanonMessage::ToolResult(ToolOutput {
                            call_id: str_of(it, "call_id").unwrap_or_default().to_string(),
                            output,
                            success: true,
                            images,
                        }));
                    }
                    (Some("message") | None, Some(role)) => {
                        let content = it.get("content").unwrap_or(&Value::Null);
                        match role {
                            "user" => history.push(CanonMessage::User(content_parts(content))),
                            "assistant" => {
                                push_assistant(&mut history, Some(content_text(content)), None)
                            }
                            "system" | "developer" => {
                                let t = content_text(content);
                                if !t.trim().is_empty() {
                                    instructions.push(t);
                                }
                            }
                            _ => {}
                        }
                    }
                    // reasoning, item references, web_search_call, … are not replayable.
                    _ => {}
                }
            }
        }
        _ => {}
    }
    req.history = history;
    if !instructions.is_empty() {
        req.instructions = Some(instructions.join("\n\n"));
    }

    let meta = ResponsesMeta {
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        tools_echo: body
            .get("tools")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
        reasoning_echo: body.get("reasoning").cloned().unwrap_or(Value::Null),
        instructions_echo: non_empty(str_of(body, "instructions")),
        metadata: body
            .get("metadata")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default())),
        store: body.get("store").and_then(Value::as_bool).unwrap_or(true),
    };
    Ok((req, meta))
}

pub struct ChatMeta {
    pub stream: bool,
    pub include_usage: bool,
}

/// Chat Completions request -> ConversationRequest.
pub fn parse_chat(body: &Value) -> Result<(ConversationRequest, ChatMeta), ApiError> {
    let mut req = ConversationRequest {
        model: non_empty(str_of(body, "model")),
        reasoning_effort: non_empty(str_of(body, "reasoning_effort")),
        service_tier: non_empty(str_of(body, "service_tier")).filter(|t| t != "auto"),
        tools: parse_tools(body.get("tools"))?,
        ..ConversationRequest::default()
    };
    if req.tools.is_empty()
        && let Some(functions) = body.get("functions").and_then(Value::as_array)
    {
        // Legacy `functions` parameter.
        for f in functions {
            if let Some(name) = non_empty(str_of(f, "name")) {
                req.tools.push(ToolSpec::from_parts(name, f, "function"));
            }
        }
    }
    if let Some(rf) = body.get("response_format")
        && str_of(rf, "type") == Some("json_schema")
    {
        req.output_schema = rf.get("json_schema").and_then(|j| j.get("schema")).cloned();
    }

    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Err(ApiError::bad_request("`messages` must be an array"));
    };
    let mut instructions: Vec<String> = Vec::new();
    let mut history: Vec<CanonMessage> = Vec::new();
    for m in messages {
        let role = str_of(m, "role").unwrap_or("user");
        let content = m.get("content").unwrap_or(&Value::Null);
        match role {
            "system" | "developer" => {
                let t = content_text(content);
                if !t.trim().is_empty() {
                    instructions.push(t);
                }
            }
            "user" => history.push(CanonMessage::User(content_parts(content))),
            "assistant" => {
                let text = content_text(content);
                let calls: Vec<ToolCallRecord> = m
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|c| {
                                let f = c.get("function").unwrap_or(c);
                                ToolCallRecord {
                                    call_id: str_of(c, "id").unwrap_or_default().to_string(),
                                    name: str_of(f, "name").unwrap_or_default().to_string(),
                                    arguments: match f.get("arguments") {
                                        Some(Value::String(s)) => s.clone(),
                                        Some(v) => v.to_string(),
                                        None => "{}".to_string(),
                                    },
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // Legacy single function_call.
                let legacy = m.get("function_call").map(|f| ToolCallRecord {
                    call_id: str_of(m, "id").unwrap_or("call_legacy").to_string(),
                    name: str_of(f, "name").unwrap_or_default().to_string(),
                    arguments: str_of(f, "arguments").unwrap_or("{}").to_string(),
                });
                history.push(CanonMessage::Assistant {
                    text,
                    tool_calls: calls.into_iter().chain(legacy).collect(),
                });
            }
            "tool" | "function" => {
                let (output, images) = output_to_string(content);
                history.push(CanonMessage::ToolResult(ToolOutput {
                    call_id: str_of(m, "tool_call_id")
                        .or_else(|| str_of(m, "name"))
                        .unwrap_or_default()
                        .to_string(),
                    output,
                    success: true,
                    images,
                }));
            }
            _ => {}
        }
    }
    req.history = history;
    if !instructions.is_empty() {
        req.instructions = Some(instructions.join("\n\n"));
    }
    let include_usage = body
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((
        req,
        ChatMeta {
            stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
            include_usage,
        },
    ))
}
