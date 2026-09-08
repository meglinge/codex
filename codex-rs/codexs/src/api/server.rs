//! axum router, auth and error plumbing.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use serde_json::Value;
use serde_json::json;

use crate::bridge::Bridge;
use crate::bridge::BridgeError;
use crate::config::ProxyConfig;

pub struct AppState {
    pub cfg: Arc<ProxyConfig>,
    pub bridge: Arc<Bridge>,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "invalid_request_error",
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            kind: "server_error",
            message: message.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "error": {
                "message": self.message,
                "type": self.kind,
                "param": null,
                "code": null,
            }
        })
    }
}

impl From<BridgeError> for ApiError {
    fn from(e: BridgeError) -> Self {
        match e {
            BridgeError::BadRequest(m) => Self::bad_request(m),
            BridgeError::NotFound(m) => Self {
                status: StatusCode::NOT_FOUND,
                kind: "invalid_request_error",
                message: m,
            },
            BridgeError::Unavailable(m) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                kind: "server_error",
                message: m,
            },
            BridgeError::Internal(err) => Self::internal(format!("{err:#}")),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.to_json())).into_response()
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/v1/responses", post(crate::api::responses::handle))
        .route("/responses", post(crate::api::responses::handle))
        .route("/v1/chat/completions", post(crate::api::chat::handle))
        .route("/chat/completions", post(crate::api::chat::handle))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/sessions", get(sessions))
        .route_layer(middleware::from_fn_with_state(Arc::clone(&state), auth));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api)
        .with_state(state)
}

async fn auth(State(state): State<Arc<AppState>>, req: Request<Body>, next: Next) -> Response {
    if state.cfg.api_keys.is_empty() {
        return next.run(req).await;
    }
    let presented = bearer(req.headers());
    let ok = presented.is_some_and(|k| state.cfg.api_keys.iter().any(|a| a == k));
    if !ok {
        return ApiError {
            status: StatusCode::UNAUTHORIZED,
            kind: "authentication_error",
            message: "invalid or missing API key".to_string(),
        }
        .into_response();
    }
    next.run(req).await
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if let Some(rest) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) {
            return Some(rest.trim());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

async fn models(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let mut seen = std::collections::BTreeMap::<String, Value>::new();
    let created = now_secs();
    for rt in state.bridge.pool().all() {
        match rt.model_list().await {
            Ok(list) => {
                for m in list {
                    let id = m
                        .get("model")
                        .or_else(|| m.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    let efforts: Vec<String> = m
                        .get("supportedReasoningEfforts")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|e| e.get("reasoningEffort").and_then(Value::as_str))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    seen.entry(id.clone()).or_insert(json!({
                        "id": id,
                        "object": "model",
                        "created": created,
                        "owned_by": "openai",
                        "display_name": m.get("displayName").cloned().unwrap_or(Value::Null),
                        "description": m.get("description").cloned().unwrap_or(Value::Null),
                        "supported_reasoning_efforts": efforts,
                        "default_reasoning_effort": m.get("defaultReasoningEffort").cloned().unwrap_or(Value::Null),
                        "input_modalities": m.get("inputModalities").cloned().unwrap_or(json!([])),
                        "is_default": m.get("isDefault").cloned().unwrap_or(json!(false)),
                    }));
                }
            }
            Err(e) => tracing::warn!(account = %rt.id, "model/list failed: {e}"),
        }
    }
    Ok(Json(json!({ "object": "list", "data": seen.into_values().collect::<Vec<_>>() })))
}

async fn sessions(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": state.bridge.session_snapshot(),
        "accounts": state
            .bridge
            .pool()
            .all()
            .iter()
            .map(|rt| json!({
                "id": rt.id,
                "codex_home": rt.codex_home.to_string_lossy(),
                "default_model": rt.default_model,
                "active_turns": rt.active_turns.load(std::sync::atomic::Ordering::Relaxed),
                "sessions": rt.sessions.load(std::sync::atomic::Ordering::Relaxed),
            }))
            .collect::<Vec<_>>(),
    }))
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Per-request overrides carried in headers.
pub struct RequestOverrides {
    pub session_id: Option<String>,
    pub account_id: Option<String>,
    pub codex_tools: Option<crate::config::CodexToolsMode>,
}

pub fn overrides_from_headers(headers: &HeaderMap) -> RequestOverrides {
    let h = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let codex_tools = h("x-asxs-codex-tools").and_then(|v| match v.as_str() {
        "none" => Some(crate::config::CodexToolsMode::None),
        "full" => Some(crate::config::CodexToolsMode::Full),
        _ => None,
    });
    RequestOverrides {
        session_id: h("x-asxs-session"),
        account_id: h("x-asxs-account"),
        codex_tools,
    }
}
