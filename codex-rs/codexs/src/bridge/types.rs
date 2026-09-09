//! Provider-neutral request/response model shared by both API front-ends.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// Client-supplied function tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// The tool exactly as the client sent it, normalized to the Responses
    /// flat shape (`{"type":"function","name",...}`) when it arrived in the
    /// Chat nested shape. Sent to the model untouched in `passthrough` mode.
    pub raw: Value,
}

impl ToolSpec {
    /// Build the flat Responses tool object from the fields a client provided,
    /// keeping unknown keys (e.g. `strict`) and omitting what was not given.
    pub fn from_parts(name: String, function: &Value, ty: &str) -> Self {
        let description = function
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let parameters = function
            .get("parameters")
            .or_else(|| function.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
        let mut raw = serde_json::Map::new();
        raw.insert("type".into(), Value::String(ty.to_string()));
        raw.insert("name".into(), Value::String(name.clone()));
        if let Some(obj) = function.as_object() {
            for (k, v) in obj {
                if k == "type" || k == "name" {
                    continue;
                }
                raw.insert(k.clone(), v.clone());
            }
        }
        Self {
            name,
            description,
            parameters,
            raw: Value::Object(raw),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum UserPart {
    Text(String),
    Image { url: String, detail: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRecord {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

/// Canonical conversation message. Both front-ends lower their wire formats
/// into this so session matching and Codex turn construction are shared.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonMessage {
    User(Vec<UserPart>),
    Assistant {
        text: String,
        tool_calls: Vec<ToolCallRecord>,
    },
    ToolResult(ToolOutput),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub call_id: String,
    pub output: String,
    pub success: bool,
    pub images: Vec<String>,
}

/// Per-request thread settings a client may pass (`asxs` body object or
/// `x-asxs-*` headers). Applied when a new Codex thread is created.
#[derive(Debug, Clone, Default)]
pub struct ThreadOverrides {
    pub sandbox: Option<String>,
    pub approval_policy: Option<String>,
    pub personality: Option<String>,
    pub cwd: Option<String>,
    pub base_instructions: Option<String>,
    /// Appended to the developer instructions derived from the system prompt.
    pub developer_instructions: Option<String>,
    pub ephemeral: Option<bool>,
    /// Dotted `config.toml` overrides (`-c key=value` semantics).
    pub config: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Default)]
pub struct ConversationRequest {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    /// Merged system/developer text.
    pub instructions: Option<String>,
    pub tools: Vec<ToolSpec>,
    /// Whole conversation, oldest first, including the new input (or only the
    /// new input when `previous_response_id` is set).
    pub history: Vec<CanonMessage>,
    pub previous_response_id: Option<String>,
    pub session_id: Option<String>,
    pub account_id: Option<String>,
    pub codex_tools: Option<crate::config::CodexToolsMode>,
    pub output_schema: Option<Value>,
    pub thread: ThreadOverrides,
    /// Client-supplied Codex identity (server mode); `None` = static account pool.
    pub identity: Option<crate::codex::identity::Identity>,
    pub credentials_changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneReason {
    Stop,
    ToolCalls,
    Interrupted,
    Error,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}

impl Usage {
    pub fn add_json(&mut self, v: &Value) {
        let g = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
        self.input_tokens += g("inputTokens");
        self.cached_input_tokens += g("cachedInputTokens");
        self.output_tokens += g("outputTokens");
        self.reasoning_output_tokens += g("reasoningOutputTokens");
        self.total_tokens += g("totalTokens");
    }

    pub fn from_json(v: &Value) -> Self {
        let mut u = Self::default();
        u.add_json(v);
        u
    }

    pub fn is_empty(&self) -> bool {
        self.total_tokens == 0 && self.input_tokens == 0 && self.output_tokens == 0
    }
}

#[derive(Debug, Clone)]
pub enum BridgeEvent {
    MessageStart {
        item_id: String,
        phase: Option<String>,
    },
    TextDelta {
        item_id: String,
        delta: String,
    },
    MessageEnd {
        item_id: String,
        text: String,
        phase: Option<String>,
    },
    ReasoningStart {
        item_id: String,
    },
    ReasoningPart {
        item_id: String,
        index: i64,
    },
    ReasoningDelta {
        item_id: String,
        delta: String,
        index: i64,
    },
    ReasoningEnd {
        item_id: String,
        summary: Vec<String>,
        content: Vec<String>,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    ActivityStart {
        item: Value,
    },
    ActivityDelta {
        item_id: String,
        delta: String,
    },
    ActivityEnd {
        item: Value,
    },
    Usage {
        usage: Usage,
        context_window: Option<i64>,
    },
    Done {
        reason: DoneReason,
        error: Option<String>,
    },
}
