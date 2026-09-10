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
use crate::bridge::types::ConversationRequest;
use crate::codex::identity;
use crate::config::ClientCredentialsMode;
use crate::config::ProxyConfig;

pub struct AppState {
    pub cfg: Arc<ProxyConfig>,
    pub bridge: Arc<Bridge>,
    pub raw: Arc<super::raw::RawForwarder>,
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

/// Codex turn failures that mean the caller's credentials are unusable.
pub fn is_auth_failure(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("could not be refreshed")
        || m.contains("sign in again")
        || m.contains("unauthorized")
        || m.contains("401")
        || m.contains("not logged in")
        || m.contains("authentication")
}

impl ApiError {
    pub fn turn_failed(message: String) -> Self {
        if is_auth_failure(&message) {
            Self {
                status: StatusCode::UNAUTHORIZED,
                kind: "authentication_error",
                message,
            }
        } else {
            Self::internal(message)
        }
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
        .route("/v1/asxs/auth", get(asxs_auth))
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
    let mut ok = presented.is_some_and(|k| state.cfg.api_keys.iter().any(|a| a == k));
    if !ok
        && state.cfg.auth.client_credentials != ClientCredentialsMode::Disabled
        && (presented.is_some_and(identity::looks_like_jwt)
            || req.headers().contains_key("x-codex-access-token"))
    {
        // Server mode: a Codex JWT stands in for the proxy API key; Codex
        // itself validates it against the backend.
        ok = true;
    }
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
        if let Some(rest) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        {
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
    Ok(Json(
        json!({ "object": "list", "data": seen.into_values().collect::<Vec<_>>() }),
    ))
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
                "identity": rt.is_identity,
                "idle_secs": rt.idle_for().as_secs(),
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

/// Debug aid: with `CODEXS_DUMP_REQUESTS_DIR` set, every incoming request body
/// is written there as `in-<ms>-<endpoint>.json` (before any lowering).
pub fn dump_incoming(endpoint: &str, headers: &HeaderMap, body: &Value) {
    let Some(dir) = std::env::var_os("CODEXS_DUMP_REQUESTS_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    if dir.as_os_str().is_empty() {
        return;
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let hdrs: serde_json::Map<String, Value> = headers
        .iter()
        .filter(|(k, _)| k.as_str() != "authorization" && !k.as_str().contains("token"))
        .map(|(k, v)| {
            (
                k.to_string(),
                Value::String(String::from_utf8_lossy(v.as_bytes()).into_owned()),
            )
        })
        .collect();
    let doc = serde_json::json!({ "endpoint": endpoint, "headers": hdrs, "body": body });
    let path = dir.join(format!(
        "in-{ms}-{}.json",
        endpoint.trim_start_matches('/').replace('/', "_")
    ));
    if let Err(e) = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap_or_default()))
    {
        tracing::warn!(path = %path.display(), "CODEXS_DUMP_REQUESTS_DIR: {e}");
    }
}

/// Apply per-request context: `asxs` body object, `x-asxs-*` headers and
/// client-supplied Codex credentials.
pub fn apply_request_context(
    req: &mut ConversationRequest,
    headers: &HeaderMap,
    body: &Value,
    cfg: &ProxyConfig,
) -> Result<(), ApiError> {
    let ov = overrides_from_headers(headers);
    req.session_id = ov.session_id;
    req.account_id = ov.account_id;
    req.codex_tools = ov.codex_tools;
    let h = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    req.thread.sandbox = h("x-asxs-sandbox");
    req.thread.approval_policy = h("x-asxs-approval-policy");
    req.thread.personality = h("x-asxs-personality");
    req.thread.cwd = h("x-asxs-cwd");

    if let Some(a) = body.get("asxs").and_then(Value::as_object) {
        let s = |k: &str| {
            a.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        if let Some(v) = s("session_id") {
            req.session_id = Some(v);
        }
        if let Some(v) = s("account_id") {
            req.account_id = Some(v);
        }
        if let Some(v) = s("model") {
            req.model = Some(v);
        }
        if let Some(v) = s("reasoning_effort") {
            req.reasoning_effort = Some(v);
        }
        if let Some(v) = s("reasoning_summary") {
            req.reasoning_summary = Some(v);
        }
        if let Some(v) = s("service_tier") {
            req.service_tier = Some(v);
        }
        if let Some(v) = s("codex_tools") {
            req.codex_tools = match crate::config::CodexToolsMode::parse(&v) {
                Some(mode) => Some(mode),
                None => {
                    return Err(ApiError::bad_request(format!(
                        "asxs.codex_tools must be \"passthrough\", \"none\" or \"full\", got {v:?}"
                    )));
                }
            };
        }
        if let Some(v) = s("sandbox") {
            req.thread.sandbox = Some(v);
        }
        if let Some(v) = s("approval_policy") {
            req.thread.approval_policy = Some(v);
        }
        if let Some(v) = s("personality") {
            req.thread.personality = Some(v);
        }
        if let Some(v) = s("cwd") {
            req.thread.cwd = Some(v);
        }
        if let Some(v) = s("base_instructions") {
            req.thread.base_instructions = Some(v);
        }
        if let Some(v) = s("developer_instructions") {
            req.thread.developer_instructions = Some(v);
        }
        if let Some(v) = a.get("ephemeral").and_then(Value::as_bool) {
            req.thread.ephemeral = Some(v);
        }
        if let Some(c) = a.get("config").and_then(Value::as_object) {
            req.thread.config = c.clone();
        }
    }

    match cfg.auth.client_credentials {
        ClientCredentialsMode::Disabled => {}
        mode => {
            let creds = identity::extract_credentials(headers, body)
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            match creds {
                Some(creds) => {
                    let (id, changed) = identity::materialize(
                        &cfg.auth.identity_root,
                        cfg.auth.identity_config_template.as_deref(),
                        &creds,
                    )
                    .map_err(|e| ApiError::internal(format!("storing credentials: {e:#}")))?;
                    req.identity = Some(id);
                    req.credentials_changed = changed;
                }
                None if mode == ClientCredentialsMode::Required => {
                    return Err(ApiError {
                        status: StatusCode::UNAUTHORIZED,
                        kind: "authentication_error",
                        message: "codex credentials required: send asxs.auth.access_token, x-codex-access-token or a JWT bearer token".to_string(),
                    });
                }
                None => {}
            }
        }
    }
    Ok(())
}

/// Stored (possibly Codex-refreshed) tokens for the presented identity.
async fn asxs_auth(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    if state.cfg.auth.client_credentials == ClientCredentialsMode::Disabled {
        return Err(ApiError::bad_request("client credentials are disabled"));
    }
    let creds = identity::extract_credentials(&headers, &Value::Null)
        .map_err(|e| ApiError::bad_request(e.to_string()))?
        .ok_or_else(|| ApiError::bad_request("no codex credentials in request"))?;
    let (id, _) = identity::materialize(
        &state.cfg.auth.identity_root,
        state.cfg.auth.identity_config_template.as_deref(),
        &creds,
    )
    .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    let stored = identity::read_stored_auth(&id.codex_home).unwrap_or(Value::Null);
    let runtime = state.bridge.pool().get(&format!("id:{}", id.key));
    Ok(Json(json!({
        "identity": id.key,
        "account_id": id.account_id,
        "codex_home": id.codex_home.to_string_lossy(),
        "runtime_running": runtime.is_some(),
        "tokens": stored,
    })))
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
    let codex_tools =
        h("x-asxs-codex-tools").and_then(|v| crate::config::CodexToolsMode::parse(&v));
    RequestOverrides {
        session_id: h("x-asxs-session"),
        account_id: h("x-asxs-account"),
        codex_tools,
    }
}
