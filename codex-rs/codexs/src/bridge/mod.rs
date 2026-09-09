//! Orchestrates API requests onto Codex sessions.

pub mod canon;
pub mod session;
pub mod turn;
pub mod types;

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use serde_json::Value;
use serde_json::json;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::info;
use tracing::warn;

use self::canon::history_preamble;
use self::canon::split_turn;
use self::canon::tools_key;
use self::canon::transcript_key;
use self::session::Session;
use self::turn::TurnRun;
use self::turn::TurnStatus;
use self::types::BridgeEvent;
use self::types::CanonMessage;
use self::types::ConversationRequest;
use self::types::ToolCallRecord;
use self::types::ToolOutput;
use self::types::ToolSpec;
use self::types::UserPart;
use crate::codex::pool::AccountPool;
use crate::codex::runtime::CodexRuntime;
use crate::config::CodexToolsMode;
use crate::config::HistorySeeding;
use crate::config::ProxyConfig;
use crate::config::SystemPromptMode;

#[derive(Debug)]
pub enum BridgeError {
    BadRequest(String),
    NotFound(String),
    Unavailable(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for BridgeError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(m) | Self::NotFound(m) | Self::Unavailable(m) => f.write_str(m),
            Self::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

pub struct Bridge {
    cfg: Arc<ProxyConfig>,
    pool: Arc<AccountPool>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    by_response: Mutex<HashMap<String, String>>,
    by_key: Mutex<HashMap<String, Vec<String>>>,
}

/// One HTTP response worth of Codex output.
pub struct RunHandle {
    pub session: Arc<Session>,
    pub turn: Arc<TurnRun>,
    pub response_id: String,
    pub model: String,
    pub events: mpsc::UnboundedReceiver<BridgeEvent>,
    bridge: Arc<Bridge>,
    history: Vec<CanonMessage>,
    finished: bool,
    _guard: OwnedMutexGuard<()>,
}

impl RunHandle {
    /// Call after the terminal `Done` event was consumed: records the new
    /// conversation state so the client's next request can find this session.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let summary = self.turn.last_summary();
        let mut history = self.history.clone();
        history.push(CanonMessage::Assistant {
            text: summary.text.clone(),
            tool_calls: summary.tool_calls.clone(),
        });
        let scoped = format!("{}\u{5}{}", self.session.scope, self.session.instructions);
        let key = transcript_key(&scoped, &self.session.tools_key, &history);
        self.bridge
            .index_session(&self.session, key, &self.response_id);
        self.session.touch();
        if self.turn.status() == TurnStatus::Finished {
            self.session.set_turn(None);
            self.session
                .runtime
                .active_turns
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(1))
                })
                .ok();
        }
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Client went away mid-segment: stop the turn so the account is not
        // left running an orphaned agent loop.
        let turn = Arc::clone(&self.turn);
        let session = Arc::clone(&self.session);
        warn!(session = %session.id, "client disconnected mid-turn; interrupting");
        tokio::spawn(async move {
            turn.fail_pending("The client disconnected before providing a tool result.")
                .await;
            turn.interrupt().await;
            turn.wait_finished(Duration::from_secs(15)).await;
            session.set_turn(None);
            session
                .runtime
                .active_turns
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(1))
                })
                .ok();
        });
    }
}

impl Bridge {
    pub fn new(cfg: Arc<ProxyConfig>, pool: Arc<AccountPool>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            pool,
            sessions: Mutex::new(HashMap::new()),
            by_response: Mutex::new(HashMap::new()),
            by_key: Mutex::new(HashMap::new()),
        })
    }

    pub fn pool(&self) -> &AccountPool {
        &self.pool
    }

    pub fn config(&self) -> &ProxyConfig {
        &self.cfg
    }

    fn index_session(&self, session: &Arc<Session>, key: String, response_id: &str) {
        if let Ok(mut t) = session.transcript_key.lock() {
            *t = Some(key.clone());
        }
        if let Ok(mut by_key) = self.by_key.lock() {
            let list = by_key.entry(key).or_default();
            if !list.contains(&session.id) {
                list.push(session.id.clone());
            }
        }
        if let Ok(mut by_response) = self.by_response.lock() {
            by_response.insert(response_id.to_string(), session.id.clone());
        }
    }

    fn session_by_id(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().ok().and_then(|s| s.get(id).cloned())
    }

    fn session_by_response(&self, response_id: &str) -> Option<Arc<Session>> {
        let id = self
            .by_response
            .lock()
            .ok()
            .and_then(|m| m.get(response_id).cloned())?;
        self.session_by_id(&id)
    }

    fn session_by_key(&self, key: &str) -> Option<Arc<Session>> {
        let ids = self
            .by_key
            .lock()
            .ok()
            .and_then(|m| m.get(key).cloned())
            .unwrap_or_default();
        let mut best: Option<Arc<Session>> = None;
        for id in ids {
            if let Some(s) = self.session_by_id(&id) {
                // Only sessions whose *current* state is this key qualify.
                let current = s
                    .transcript_key
                    .lock()
                    .ok()
                    .and_then(|k| k.clone())
                    .is_some_and(|k| k == key);
                if !current {
                    continue;
                }
                let newer = best
                    .as_ref()
                    .map(|b| s.idle_for() < b.idle_for())
                    .unwrap_or(true);
                if newer {
                    best = Some(s);
                }
            }
        }
        best
    }

    /// Resolve the request onto a session and start/continue a Codex turn.
    pub async fn run(self: &Arc<Self>, req: ConversationRequest) -> Result<RunHandle, BridgeError> {
        let instructions = match self.cfg.defaults.system_prompt_mode {
            SystemPromptMode::Developer => req.instructions.clone().unwrap_or_default(),
            SystemPromptMode::Ignore => String::new(),
        };
        let tkey = tools_key(&req.tools);
        // Sessions are scoped to the identity/account that owns the Codex thread.
        let scope = match &req.identity {
            Some(id) => format!("id:{}", id.key),
            None => req.account_id.clone().unwrap_or_default(),
        };
        let scoped_instructions = format!("{scope}\u{5}{instructions}");
        let (prefix, tail): (Vec<CanonMessage>, Vec<CanonMessage>) =
            if req.previous_response_id.is_some() {
                (Vec::new(), req.history.clone())
            } else {
                let (p, t) = split_turn(&req.history);
                (p.to_vec(), t.to_vec())
            };
        if tail.is_empty() {
            return Err(BridgeError::BadRequest(
                "request contains no new user message or tool output".to_string(),
            ));
        }

        // ---- locate session -------------------------------------------------------
        let mut session: Option<Arc<Session>> = None;
        if let Some(prev) = &req.previous_response_id {
            session = Some(self.session_by_response(prev).ok_or_else(|| {
                BridgeError::NotFound(format!(
                    "previous_response_id {prev} is not a live response"
                ))
            })?);
        } else if let Some(sid) = &req.session_id {
            session = Some(
                self.session_by_id(sid)
                    .ok_or_else(|| BridgeError::NotFound(format!("session {sid} not found")))?,
            );
        } else if !prefix.is_empty() {
            let key = transcript_key(&scoped_instructions, &tkey, &prefix);
            session = self.session_by_key(&key);
            if session.is_none() {
                debug!(key = %&key[..12], "no live session for history prefix");
            }
        }
        if let Some(s) = &session
            && let Some(id) = &req.identity
            && s.runtime.id != format!("id:{}", id.key)
        {
            return Err(BridgeError::NotFound(
                "session belongs to a different identity".to_string(),
            ));
        }
        if let Some(s) = &session
            && (s.tools_key != tkey || s.instructions != instructions)
        {
            if req.previous_response_id.is_some() || req.session_id.is_some() {
                // Stateful continuation: the Codex thread already carries its
                // developer instructions and dynamic tools; a client that omits
                // or changes them cannot alter the thread mid-flight.
                if !req.tools.is_empty() && s.tools_key != tkey {
                    warn!(session = %s.id, "tool set changed on a stateful continuation; thread keeps its original tools");
                }
                if !instructions.is_empty() && s.instructions != instructions {
                    warn!(session = %s.id, "instructions changed on a stateful continuation; thread keeps its original developer instructions");
                }
            } else {
                session = None;
            }
        }

        let mut history = req.history.clone();
        if let Some(prev) = &req.previous_response_id
            && let Some(s) = &session
        {
            // Stateful continuation: rebuild the canonical history from what we know.
            debug!(session = %s.id, prev, "continuing via previous_response_id");
            history = tail.clone();
        }

        // ---- continuation of a turn awaiting tool results -------------------------
        let tool_outputs: Vec<ToolOutput> = tail
            .iter()
            .filter_map(|m| match m {
                CanonMessage::ToolResult(o) => Some(o.clone()),
                _ => None,
            })
            .collect();
        let user_parts: Vec<UserPart> = tail
            .iter()
            .filter_map(|m| match m {
                CanonMessage::User(p) => Some(p.clone()),
                _ => None,
            })
            .flatten()
            .collect();

        if let Some(s) = session.clone() {
            let guard = Arc::clone(&s.lock).lock_owned().await;
            s.touch();
            let turn = s.current_turn();
            if let Some(turn) = turn.clone()
                && turn.status() == TurnStatus::AwaitingTools
                && !tool_outputs.is_empty()
            {
                let unmatched = turn.provide_outputs(tool_outputs).await;
                if !unmatched.is_empty() {
                    warn!(session = %s.id, ?unmatched, "tool outputs for unknown call ids");
                }
                let still_pending = turn.pending_call_ids();
                if !still_pending.is_empty() {
                    turn.fail_pending("No result was provided for this tool call.")
                        .await;
                }
                let events = turn.attach();
                if !user_parts.is_empty() {
                    let input = user_input_json(&user_parts, None);
                    if let Err(e) = turn.steer(input).await {
                        warn!(session = %s.id, "turn/steer failed: {e}");
                    }
                }
                let response_id = new_response_id();
                return Ok(RunHandle {
                    model: s.model.clone(),
                    session: s,
                    turn,
                    response_id,
                    events,
                    bridge: Arc::clone(self),
                    history,
                    finished: false,
                    _guard: guard,
                });
            }
            if let Some(turn) = turn
                && turn.status() != TurnStatus::Finished
            {
                info!(session = %s.id, "abandoning in-flight turn for new input");
                turn.fail_pending("The client abandoned this tool call.")
                    .await;
                turn.interrupt().await;
                if !turn.wait_finished(Duration::from_secs(20)).await {
                    return Err(BridgeError::Unavailable(
                        "previous turn did not stop in time".to_string(),
                    ));
                }
                s.set_turn(None);
                s.runtime
                    .active_turns
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                        Some(v.saturating_sub(1))
                    })
                    .ok();
            }
            if user_parts.is_empty() {
                return Err(BridgeError::BadRequest(
                    "tool outputs were provided but this session has no pending tool call"
                        .to_string(),
                ));
            }
            let (turn, events) = self.start_turn(&s, &req, &user_parts, None).await?;
            let response_id = new_response_id();
            return Ok(RunHandle {
                model: s.model.clone(),
                session: s,
                turn,
                response_id,
                events,
                bridge: Arc::clone(self),
                history,
                finished: false,
                _guard: guard,
            });
        }

        // ---- brand new session ------------------------------------------------------
        if user_parts.is_empty() {
            return Err(BridgeError::BadRequest(
                "tool outputs were provided but no live session has a matching pending tool call"
                    .to_string(),
            ));
        }
        let mut preamble: Option<String> = None;
        let mut initial_history: Option<Vec<Value>> = None;
        if !prefix.is_empty() {
            match self.cfg.sessions.history_seeding {
                HistorySeeding::Reject => {
                    return Err(BridgeError::BadRequest(
                        "conversation history does not match a live session (history_seeding = reject)"
                            .to_string(),
                    ));
                }
                HistorySeeding::Preamble => preamble = Some(history_preamble(&prefix)),
                HistorySeeding::Patch => initial_history = Some(history_to_response_items(&prefix)),
            }
        }
        let runtime = match &req.identity {
            Some(identity) => self
                .pool
                .identity_runtime(identity, req.credentials_changed)
                .await
                .map_err(|e| {
                    BridgeError::Unavailable(format!(
                        "starting Codex for identity {}: {e:#}",
                        identity.key
                    ))
                })?,
            None => self
                .pool
                .acquire(req.account_id.as_deref())
                .ok_or_else(|| {
                    BridgeError::Unavailable(
                        "no Codex account available; supply codex credentials".to_string(),
                    )
                })?,
        };
        let session = self
            .create_session(runtime, &req, instructions, tkey, scope, initial_history)
            .await?;
        let guard = Arc::clone(&session.lock).lock_owned().await;
        let (turn, events) = self
            .start_turn(&session, &req, &user_parts, preamble.as_deref())
            .await?;
        let response_id = new_response_id();
        Ok(RunHandle {
            model: session.model.clone(),
            session,
            turn,
            response_id,
            events,
            bridge: Arc::clone(self),
            history,
            finished: false,
            _guard: guard,
        })
    }

    async fn create_session(
        &self,
        runtime: Arc<CodexRuntime>,
        req: &ConversationRequest,
        instructions: String,
        tkey: String,
        scope: String,
        initial_history: Option<Vec<Value>>,
    ) -> Result<Arc<Session>, BridgeError> {
        let id = format!("sess_{}", uuid::Uuid::new_v4().simple());
        let cwd = match &req.thread.cwd {
            Some(c) if !c.is_empty() => PathBuf::from(c),
            _ => self.cfg.workspace_root.join(&id),
        };
        tokio::fs::create_dir_all(&cwd)
            .await
            .map_err(|e| anyhow!("creating workspace {}: {e}", cwd.display()))?;

        let defaults = &self.cfg.defaults;
        let codex_tools = req.codex_tools.unwrap_or(defaults.codex_tools);
        let (dynamic_tools, tool_names) = if codex_tools == CodexToolsMode::Passthrough {
            verbatim_tools_json(&req.tools)
        } else {
            dynamic_tools_json(&req.tools)
        };
        let model = req
            .model
            .clone()
            .filter(|m| !m.is_empty())
            .or_else(|| (!defaults.model.is_empty()).then(|| defaults.model.clone()))
            .or_else(|| (!runtime.default_model.is_empty()).then(|| runtime.default_model.clone()));

        let mut config_overrides: serde_json::Map<String, Value> = serde_json::Map::new();
        match codex_tools {
            CodexToolsMode::Full => {}
            CodexToolsMode::None => {
                config_overrides.insert("features.shell_tool".into(), json!(false));
                config_overrides.insert("features.view_image".into(), json!(false));
                config_overrides.insert("web_search".into(), json!("disabled"));
                config_overrides.insert("tools.update_plan.enabled".into(), json!(false));
                config_overrides.insert("mcp_servers".into(), json!({}));
            }
            CodexToolsMode::Passthrough => {
                // Patched Codex: the dynamic tools are the whole tool surface,
                // sent verbatim, never wrapped in code mode.
                config_overrides.insert("tools.client_only".into(), json!(true));
                config_overrides.insert("mcp_servers".into(), json!({}));
            }
        }
        for (k, v) in &req.thread.config {
            config_overrides.insert(k.clone(), v.clone());
        }

        let t = &req.thread;
        // Passthrough: nothing runs on this host (Codex's tools are off), and the
        // client executes its own tools on its own machine — a read-only sandbox
        // in the environment context would only make the model refuse to write.
        let sandbox = t.sandbox.clone().unwrap_or_else(|| {
            if codex_tools == CodexToolsMode::Passthrough {
                "danger-full-access".to_string()
            } else {
                defaults.sandbox.clone()
            }
        });
        let mut params = json!({
            "cwd": cwd.to_string_lossy(),
            "approvalPolicy": t.approval_policy.clone().unwrap_or_else(|| defaults.approval_policy.clone()),
            "sandbox": sandbox,
            "ephemeral": t.ephemeral.unwrap_or(defaults.ephemeral),
            "experimentalRawEvents": true,
            "dynamicTools": dynamic_tools,
        });
        let obj = params
            .as_object_mut()
            .ok_or_else(|| anyhow!("params must be an object"))?;
        if let Some(m) = &model {
            obj.insert("model".into(), json!(m));
        }
        let mut developer = instructions.clone();
        if let Some(extra) = &t.developer_instructions
            && !extra.is_empty()
        {
            if !developer.is_empty() {
                developer.push_str("\n\n");
            }
            developer.push_str(extra);
        }
        if !developer.is_empty() {
            obj.insert("developerInstructions".into(), json!(developer));
        }
        if let Some(base) = &t.base_instructions
            && !base.is_empty()
        {
            obj.insert("baseInstructions".into(), json!(base));
        }
        let personality =
            t.personality.clone().filter(|p| !p.is_empty()).or_else(|| {
                (!defaults.personality.is_empty()).then(|| defaults.personality.clone())
            });
        if let Some(p) = personality {
            obj.insert("personality".into(), json!(p));
        }
        let service_tier = req
            .service_tier
            .clone()
            .or_else(|| (!defaults.service_tier.is_empty()).then(|| defaults.service_tier.clone()));
        if let Some(t) = service_tier {
            obj.insert("serviceTier".into(), json!(t));
        }
        if !config_overrides.is_empty() {
            obj.insert("config".into(), Value::Object(config_overrides));
        }
        if let Some(h) = initial_history {
            obj.insert("initialHistory".into(), Value::Array(h));
        }

        let res = runtime.request("thread/start", params).await.map_err(|e| {
            let msg = e.to_string();
            if msg.contains("initialHistory") {
                BridgeError::BadRequest(format!(
                    "this Codex build does not support initialHistory (set sessions.history_seeding = \"preamble\"): {msg}"
                ))
            } else if msg.contains("dynamic tool") {
                // Codex validated the client's tools (name pattern, duplicates…).
                BridgeError::BadRequest(format!("invalid tools: {msg}"))
            } else if msg.contains("client_only") || msg.contains("verbatim") {
                BridgeError::BadRequest(format!(
                    "this Codex build does not support codex_tools = \"passthrough\" (needs the ASXS patch): {msg}"
                ))
            } else {
                BridgeError::Internal(e)
            }
        })?;
        let thread_id = res
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("thread/start returned no thread id"))?
            .to_string();
        let model = res
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(model)
            .unwrap_or_default();
        info!(
            session = %id, thread = %thread_id, account = %runtime.id, model = %model,
            tools = req.tools.len(), tool_names = ?req.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            mode = ?codex_tools, "session created"
        );
        runtime.sessions.fetch_add(1, Ordering::Relaxed);

        let session = Arc::new(Session {
            id: id.clone(),
            runtime,
            thread_id,
            model,
            cwd,
            tools_key: tkey,
            instructions,
            scope,
            tool_names,
            lock: Arc::new(tokio::sync::Mutex::new(())),
            turn: Mutex::new(None),
            transcript_key: Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
            created_at: Instant::now(),
        });
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(id, Arc::clone(&session));
        }
        Ok(session)
    }

    async fn start_turn(
        &self,
        session: &Arc<Session>,
        req: &ConversationRequest,
        user_parts: &[UserPart],
        preamble: Option<&str>,
    ) -> Result<(Arc<TurnRun>, mpsc::UnboundedReceiver<BridgeEvent>), BridgeError> {
        let defaults = &self.cfg.defaults;
        let input = user_input_json(user_parts, preamble);
        let mut params = json!({ "threadId": session.thread_id, "input": input });
        let obj = params
            .as_object_mut()
            .ok_or_else(|| anyhow!("params must be an object"))?;
        let effort = req.reasoning_effort.clone().or_else(|| {
            (!defaults.reasoning_effort.is_empty()).then(|| defaults.reasoning_effort.clone())
        });
        if let Some(e) = effort {
            obj.insert("effort".into(), json!(e));
        }
        let summary = req.reasoning_summary.clone().or_else(|| {
            (!defaults.reasoning_summary.is_empty()).then(|| defaults.reasoning_summary.clone())
        });
        if let Some(s) = summary {
            obj.insert("summary".into(), json!(s));
        }
        if let Some(schema) = &req.output_schema {
            obj.insert("outputSchema".into(), schema.clone());
        }

        // Subscribe before starting so no notification is lost.
        let rx = session.runtime.subscribe(&session.thread_id);
        let res = match session.runtime.request("turn/start", params).await {
            Ok(r) => r,
            Err(e) => {
                session.runtime.unsubscribe(&session.thread_id);
                return Err(BridgeError::Internal(e));
            }
        };
        let turn_id = res
            .get("turn")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("turn/start returned no turn id"))?
            .to_string();
        let turn = TurnRun::new(
            Arc::clone(&session.runtime),
            session.thread_id.clone(),
            turn_id,
            session.tool_names.clone(),
            self.cfg.api.expose_activity,
        );
        let events = turn.attach();
        turn.spawn_pump(rx);
        session.set_turn(Some(Arc::clone(&turn)));
        session.runtime.active_turns.fetch_add(1, Ordering::Relaxed);
        Ok((turn, events))
    }

    /// Periodic maintenance: time out abandoned tool calls, drop idle sessions.
    pub fn spawn_reaper(self: &Arc<Self>) {
        let bridge = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                bridge.reap().await;
                bridge.pool.evict_idle_identities().await;
            }
        });
    }

    async fn reap(&self) {
        let tool_timeout = Duration::from_secs(self.cfg.sessions.tool_result_timeout_secs);
        let idle_ttl = Duration::from_secs(self.cfg.sessions.idle_ttl_secs);
        let sessions: Vec<Arc<Session>> = self
            .sessions
            .lock()
            .map(|s| s.values().cloned().collect())
            .unwrap_or_default();
        for s in sessions {
            if let Some(turn) = s.current_turn()
                && turn.status() == TurnStatus::AwaitingTools
                && turn
                    .awaiting_since()
                    .is_some_and(|t| t.elapsed() > tool_timeout)
            {
                warn!(session = %s.id, "tool result timeout; interrupting turn");
                turn.fail_pending("The client did not return a tool result in time.")
                    .await;
                turn.interrupt().await;
                turn.wait_finished(Duration::from_secs(15)).await;
                s.set_turn(None);
                s.runtime
                    .active_turns
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                        Some(v.saturating_sub(1))
                    })
                    .ok();
            }
            if s.current_turn().is_none() && s.idle_for() > idle_ttl {
                info!(session = %s.id, "dropping idle session");
                self.drop_session(&s).await;
            }
        }
    }

    async fn drop_session(&self, s: &Arc<Session>) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(&s.id);
        }
        if let Ok(mut by_key) = self.by_key.lock() {
            for list in by_key.values_mut() {
                list.retain(|id| id != &s.id);
            }
            by_key.retain(|_, v| !v.is_empty());
        }
        if let Ok(mut by_response) = self.by_response.lock() {
            by_response.retain(|_, v| v != &s.id);
        }
        s.runtime
            .sessions
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
        if let Err(e) = s
            .runtime
            .request("thread/unsubscribe", json!({ "threadId": s.thread_id }))
            .await
        {
            debug!(session = %s.id, "thread/unsubscribe: {e}");
        }
        let _ = tokio::fs::remove_dir_all(&s.cwd).await;
    }

    pub fn session_snapshot(&self) -> Vec<Value> {
        self.sessions
            .lock()
            .map(|m| {
                m.values()
                    .map(|s| {
                        json!({
                            "id": s.id,
                            "account": s.runtime.id,
                            "thread_id": s.thread_id,
                            "model": s.model,
                            "turn_status": s.current_turn().map(|t| format!("{:?}", t.status())),
                            "idle_secs": s.idle_for().as_secs(),
                            "age_secs": s.created_at.elapsed().as_secs(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn new_response_id() -> String {
    format!("resp_{}", uuid::Uuid::new_v4().simple())
}

/// Build `turn/start` input items from canonical user parts.
pub fn user_input_json(parts: &[UserPart], preamble: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut text = String::new();
    if let Some(p) = preamble {
        text.push_str(p);
    }
    let mut pending_text = text;
    for p in parts {
        match p {
            UserPart::Text(t) => {
                if !pending_text.is_empty() && !pending_text.ends_with('\n') {
                    pending_text.push('\n');
                }
                pending_text.push_str(t);
            }
            UserPart::Image { url, detail } => {
                if !pending_text.is_empty() {
                    out.push(json!({ "type": "text", "text": pending_text, "text_elements": [] }));
                    pending_text = String::new();
                }
                let mut img = json!({ "type": "image", "url": url });
                if let Some(d) = detail
                    && let Some(o) = img.as_object_mut()
                {
                    o.insert("detail".into(), json!(d));
                }
                out.push(img);
            }
        }
    }
    if !pending_text.is_empty() || out.is_empty() {
        out.push(json!({ "type": "text", "text": pending_text, "text_elements": [] }));
    }
    out
}

fn sanitize_tool_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s = "tool".to_string();
    }
    if s.len() > 128 {
        s.truncate(128);
    }
    s
}

/// Convert client tools into Codex `dynamicTools` (top-level function specs).
fn dynamic_tools_json(tools: &[ToolSpec]) -> (Vec<Value>, HashMap<String, String>) {
    let mut out = Vec::new();
    let mut names = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for t in tools {
        if !t.is_function() {
            // Only passthrough forwards hosted / non-function tools.
            continue;
        }
        let mut name = sanitize_tool_name(&t.name);
        let base = name.clone();
        let mut n = 1;
        while !used.insert(name.clone()) {
            n += 1;
            name = format!("{base}_{n}");
        }
        names.insert(name.clone(), t.name.clone());
        let mut schema = t.parameters.clone();
        if !schema.is_object() {
            schema = json!({ "type": "object", "properties": {} });
        }
        out.push(json!({
            "type": "function",
            "name": name,
            "description": t.description,
            "inputSchema": schema,
        }));
    }
    (out, names)
}

/// Passthrough: every client tool becomes a `verbatim` dynamic tool (patched
/// Codex) — the object goes to the model untouched, names are not rewritten.
fn verbatim_tools_json(tools: &[ToolSpec]) -> (Vec<Value>, HashMap<String, String>) {
    let mut out = Vec::with_capacity(tools.len());
    let mut names = HashMap::new();
    for t in tools {
        names.insert(t.name.clone(), t.name.clone());
        out.push(json!({ "type": "verbatim", "tool": t.raw }));
    }
    (out, names)
}

/// Lower canonical history to Responses API items for the `initialHistory`
/// extension (patched Codex).
fn history_to_response_items(prefix: &[CanonMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in prefix {
        match m {
            CanonMessage::User(parts) => {
                let content: Vec<Value> = parts
                    .iter()
                    .map(|p| match p {
                        UserPart::Text(t) => json!({ "type": "input_text", "text": t }),
                        UserPart::Image { url, detail } => {
                            let mut v = json!({ "type": "input_image", "image_url": url });
                            if let Some(d) = detail
                                && let Some(o) = v.as_object_mut()
                            {
                                o.insert("detail".into(), json!(d));
                            }
                            v
                        }
                    })
                    .collect();
                out.push(json!({ "type": "message", "role": "user", "content": content }));
            }
            CanonMessage::Assistant { text, tool_calls } => {
                if !text.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for c in tool_calls {
                    out.push(json!({
                        "type": "function_call",
                        "name": c.name,
                        "arguments": c.arguments,
                        "call_id": c.call_id,
                    }));
                }
            }
            CanonMessage::ToolResult(r) => {
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": r.call_id,
                    "output": r.output,
                }));
            }
        }
    }
    out
}

#[allow(dead_code)]
pub fn tool_call_record(call_id: &str, name: &str, arguments: &str) -> ToolCallRecord {
    ToolCallRecord {
        call_id: call_id.to_string(),
        name: name.to_string(),
        arguments: arguments.to_string(),
    }
}
