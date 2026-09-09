//! One Codex turn as seen by the HTTP API.
//!
//! A turn may span several HTTP requests: the model calls a client tool, the
//! proxy ends the current HTTP response with `tool_calls`, and the client's
//! next request (carrying the tool result) resumes the same Codex turn. The
//! state machine here maps thread notifications onto "segments" (one per HTTP
//! response) and parks `item/tool/call` server requests until the client
//! answers them.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_app_server_protocol::RequestId;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::debug;
use tracing::warn;

use super::types::BridgeEvent;
use super::types::DoneReason;
use super::types::ToolCallRecord;
use super::types::ToolOutput;
use super::types::Usage;
use crate::codex::runtime::CodexRuntime;
use crate::codex::runtime::ThreadEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Running,
    AwaitingTools,
    Finished,
}

struct PendingCall {
    request_id: RequestId,
}

#[derive(Debug, Clone, Default)]
pub struct SegmentSummary {
    pub text: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub usage: Usage,
    pub context_window: Option<i64>,
    pub reason: Option<DoneReason>,
}

struct TurnState {
    status: TurnStatus,
    segment: Option<mpsc::UnboundedSender<BridgeEvent>>,
    backlog: Vec<BridgeEvent>,
    /// call_id -> pending `item/tool/call` (insertion ordered).
    pending: Vec<(String, PendingCall)>,
    expected_dynamic_calls: usize,
    response_completed: bool,
    usage: Usage,
    usage_from_raw: bool,
    thread_usage_last: Option<Usage>,
    context_window: Option<i64>,
    last_error: Option<String>,
    segment_text: Vec<String>,
    segment_calls: Vec<ToolCallRecord>,
    last_summary: SegmentSummary,
    debounce_gen: u64,
    awaiting_since: Option<std::time::Instant>,
    /// Raw response events are flowing for this turn (so no debounce is needed).
    raw_events_seen: bool,
}

pub struct TurnRun {
    pub thread_id: String,
    pub turn_id: String,
    runtime: Arc<CodexRuntime>,
    /// Sanitized dynamic tool names (as registered with Codex).
    dynamic_tools: HashSet<String>,
    /// sanitized name -> client's original tool name.
    tool_names: HashMap<String, String>,
    expose_activity: bool,
    state: Mutex<TurnState>,
    status_tx: watch::Sender<TurnStatus>,
}

impl TurnRun {
    pub fn new(
        runtime: Arc<CodexRuntime>,
        thread_id: String,
        turn_id: String,
        tool_names: HashMap<String, String>,
        expose_activity: bool,
    ) -> Arc<Self> {
        let (status_tx, _rx) = watch::channel(TurnStatus::Running);
        Arc::new(Self {
            thread_id,
            turn_id,
            runtime,
            dynamic_tools: tool_names.keys().cloned().collect(),
            tool_names,
            expose_activity,
            state: Mutex::new(TurnState {
                status: TurnStatus::Running,
                segment: None,
                backlog: Vec::new(),
                pending: Vec::new(),
                expected_dynamic_calls: 0,
                response_completed: false,
                usage: Usage::default(),
                usage_from_raw: false,
                thread_usage_last: None,
                context_window: None,
                last_error: None,
                segment_text: Vec::new(),
                segment_calls: Vec::new(),
                last_summary: SegmentSummary::default(),
                debounce_gen: 0,
                awaiting_since: None,
                raw_events_seen: false,
            }),
            status_tx,
        })
    }

    /// Start pumping thread events into this turn.
    pub fn spawn_pump(self: &Arc<Self>, mut rx: mpsc::UnboundedReceiver<ThreadEvent>) {
        let run = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let finished = run.handle_event(ev).await;
                if finished {
                    break;
                }
            }
            // Turn ended (or the runtime went away): make sure consumers unblock.
            run.force_finish("codex turn ended unexpectedly");
        });
    }

    pub fn status(&self) -> TurnStatus {
        self.state
            .lock()
            .map(|s| s.status)
            .unwrap_or(TurnStatus::Finished)
    }

    pub fn awaiting_since(&self) -> Option<std::time::Instant> {
        self.state.lock().ok().and_then(|s| s.awaiting_since)
    }

    pub fn subscribe_status(&self) -> watch::Receiver<TurnStatus> {
        self.status_tx.subscribe()
    }

    pub fn pending_call_ids(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|s| s.pending.iter().map(|(id, _)| id.clone()).collect())
            .unwrap_or_default()
    }

    /// Attach a new HTTP segment: returns the event receiver and replays any
    /// events that arrived while no segment was attached.
    pub fn attach(&self) -> mpsc::UnboundedReceiver<BridgeEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(mut s) = self.state.lock() {
            s.segment_text.clear();
            s.segment_calls.clear();
            s.usage = Usage::default();
            s.usage_from_raw = false;
            s.awaiting_since = None;
            if s.status == TurnStatus::AwaitingTools {
                s.status = TurnStatus::Running;
            }
            for ev in s.backlog.drain(..) {
                let _ = tx.send(ev);
            }
            s.segment = Some(tx);
            if s.status == TurnStatus::Finished {
                // Already over: replay the terminal event.
                let summary = s.last_summary.clone();
                if let Some(seg) = &s.segment {
                    let _ = seg.send(BridgeEvent::Usage {
                        usage: summary.usage.clone(),
                        context_window: summary.context_window,
                    });
                    let _ = seg.send(BridgeEvent::Done {
                        reason: summary.reason.unwrap_or(DoneReason::Stop),
                        error: None,
                    });
                }
                s.segment = None;
            }
        }
        let _ = self.status_tx.send(self.status());
        rx
    }

    pub fn last_summary(&self) -> SegmentSummary {
        self.state
            .lock()
            .map(|s| s.last_summary.clone())
            .unwrap_or_default()
    }

    /// Deliver client tool results for pending `item/tool/call` requests.
    /// Returns the call ids that had no pending request.
    pub async fn provide_outputs(&self, outputs: Vec<ToolOutput>) -> Vec<String> {
        let mut unmatched = Vec::new();
        let mut to_resolve = Vec::new();
        if let Ok(mut s) = self.state.lock() {
            for out in outputs {
                if let Some(pos) = s.pending.iter().position(|(id, _)| *id == out.call_id) {
                    let (_, pending) = s.pending.remove(pos);
                    to_resolve.push((pending.request_id, out));
                } else {
                    unmatched.push(out.call_id);
                }
            }
        }
        for (request_id, out) in to_resolve {
            let mut items = vec![json!({ "type": "inputText", "text": out.output })];
            for url in &out.images {
                items.push(json!({ "type": "inputImage", "imageUrl": url }));
            }
            let result = json!({ "contentItems": items, "success": out.success });
            if let Err(e) = self.runtime.resolve(request_id, result).await {
                warn!(thread = %self.thread_id, "failed to deliver tool output: {e}");
            }
        }
        unmatched
    }

    /// Fail every pending client tool call (client abandoned the turn).
    pub async fn fail_pending(&self, reason: &str) {
        let pending: Vec<(String, PendingCall)> = match self.state.lock() {
            Ok(mut s) => s.pending.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        for (call_id, p) in pending {
            debug!(thread = %self.thread_id, call_id, "failing pending tool call: {reason}");
            let result = json!({
                "contentItems": [{ "type": "inputText", "text": reason }],
                "success": false,
            });
            if let Err(e) = self.runtime.resolve(p.request_id, result).await {
                warn!(thread = %self.thread_id, "failed to fail tool call: {e}");
            }
        }
    }

    pub async fn interrupt(&self) {
        if self.status() == TurnStatus::Finished {
            return;
        }
        if let Err(e) = self
            .runtime
            .request(
                "turn/interrupt",
                json!({ "threadId": self.thread_id, "turnId": self.turn_id }),
            )
            .await
        {
            warn!(thread = %self.thread_id, "turn/interrupt failed: {e}");
        }
    }

    pub async fn wait_finished(&self, timeout: Duration) -> bool {
        let mut rx = self.subscribe_status();
        if *rx.borrow() == TurnStatus::Finished {
            return true;
        }
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return self.status() == TurnStatus::Finished;
                    }
                    if *rx.borrow() == TurnStatus::Finished {
                        return true;
                    }
                }
                _ = &mut deadline => return false,
            }
        }
    }

    pub async fn steer(&self, input: Vec<Value>) -> anyhow::Result<()> {
        self.runtime
            .request(
                "turn/steer",
                json!({
                    "threadId": self.thread_id,
                    "expectedTurnId": self.turn_id,
                    "input": input,
                }),
            )
            .await
            .map(|_| ())
    }

    // ---- event handling ----------------------------------------------------------

    fn emit(state: &mut TurnState, ev: BridgeEvent) {
        if let Some(tx) = &state.segment {
            if tx.send(ev).is_ok() {
                return;
            }
            // Consumer is gone (client disconnected). Drop the segment; the
            // bridge interrupts the turn separately.
            state.segment = None;
        } else {
            state.backlog.push(ev);
        }
    }

    fn end_segment(&self, state: &mut TurnState, reason: DoneReason, error: Option<String>) {
        if !state.usage_from_raw
            && let Some(last) = &state.thread_usage_last
        {
            state.usage = last.clone();
        }
        let summary = SegmentSummary {
            text: state.segment_text.join("\n\n"),
            tool_calls: state.segment_calls.clone(),
            usage: state.usage.clone(),
            context_window: state.context_window,
            reason: Some(reason),
        };
        Self::emit(
            state,
            BridgeEvent::Usage {
                usage: summary.usage.clone(),
                context_window: summary.context_window,
            },
        );
        Self::emit(state, BridgeEvent::Done { reason, error });
        state.segment = None;
        state.backlog.clear();
        state.last_summary = summary;
        state.response_completed = false;
        state.expected_dynamic_calls = 0;
    }

    fn maybe_end_for_tools(&self, state: &mut TurnState) {
        if state.status != TurnStatus::Running || state.pending.is_empty() {
            return;
        }
        if state.response_completed && state.pending.len() >= state.expected_dynamic_calls {
            state.status = TurnStatus::AwaitingTools;
            state.awaiting_since = Some(std::time::Instant::now());
            self.end_segment(state, DoneReason::ToolCalls, None);
            let _ = self.status_tx.send(TurnStatus::AwaitingTools);
        }
    }

    fn force_finish(&self, error: &str) {
        let mut ended = false;
        if let Ok(mut s) = self.state.lock()
            && s.status != TurnStatus::Finished
        {
            s.status = TurnStatus::Finished;
            self.end_segment(&mut s, DoneReason::Error, Some(error.to_string()));
            ended = true;
        }
        if ended {
            let _ = self.status_tx.send(TurnStatus::Finished);
            self.runtime.unsubscribe(&self.thread_id);
        }
    }

    fn client_tool_name(&self, codex_name: &str) -> String {
        self.tool_names
            .get(codex_name)
            .cloned()
            .unwrap_or_else(|| codex_name.to_string())
    }

    /// Returns `true` once the turn is finished.
    async fn handle_event(self: &Arc<Self>, ev: ThreadEvent) -> bool {
        match ev {
            ThreadEvent::Notification { method, params } => {
                self.handle_notification(&method, params)
            }
            ThreadEvent::Request { id, method, params } => {
                if method == "item/tool/call" {
                    self.handle_tool_call(id, &params);
                } else {
                    let runtime = Arc::clone(&self.runtime);
                    tokio::spawn(async move {
                        runtime.auto_answer(id, &method, &params).await;
                    });
                }
                false
            }
        }
    }

    fn handle_tool_call(self: &Arc<Self>, id: RequestId, params: &Value) {
        let call_id = params
            .get("callId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let codex_name = params
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = self.client_tool_name(&codex_name);
        let arguments = match params.get("arguments") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => "{}".to_string(),
        };
        let generation = {
            let Ok(mut s) = self.state.lock() else {
                return;
            };
            s.pending
                .push((call_id.clone(), PendingCall { request_id: id }));
            s.segment_calls.push(ToolCallRecord {
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            });
            Self::emit(
                &mut s,
                BridgeEvent::ToolCall {
                    call_id,
                    name,
                    arguments,
                },
            );
            self.maybe_end_for_tools(&mut s);
            s.debounce_gen += 1;
            s.debounce_gen
        };
        // Fallback for Codex builds that do not emit raw response events: end
        // the segment once tool calls stop arriving.
        let run = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(3000)).await;
            if let Ok(mut s) = run.state.lock()
                && !s.raw_events_seen
                && s.debounce_gen == generation
                && s.status == TurnStatus::Running
                && !s.pending.is_empty()
            {
                s.response_completed = true;
                s.expected_dynamic_calls = s.pending.len();
                run.maybe_end_for_tools(&mut s);
            }
        });
    }

    fn handle_notification(&self, method: &str, params: Value) -> bool {
        let Ok(mut s) = self.state.lock() else {
            return true;
        };
        let item_id = || {
            params
                .get("itemId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match method {
            "item/started" => {
                let item = params.get("item").cloned().unwrap_or(Value::Null);
                let ty = item.get("type").and_then(Value::as_str).unwrap_or_default();
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                match ty {
                    "agentMessage" => Self::emit(
                        &mut s,
                        BridgeEvent::MessageStart {
                            item_id: id,
                            phase: item
                                .get("phase")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        },
                    ),
                    "reasoning" => Self::emit(&mut s, BridgeEvent::ReasoningStart { item_id: id }),
                    "dynamicToolCall" | "userMessage" => {}
                    _ => {
                        if self.expose_activity {
                            Self::emit(&mut s, BridgeEvent::ActivityStart { item });
                        }
                    }
                }
            }
            "item/agentMessage/delta" => {
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Self::emit(
                    &mut s,
                    BridgeEvent::TextDelta {
                        item_id: item_id(),
                        delta,
                    },
                );
            }
            "item/reasoning/summaryPartAdded" => {
                let index = params
                    .get("summaryIndex")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                Self::emit(
                    &mut s,
                    BridgeEvent::ReasoningPart {
                        item_id: item_id(),
                        index,
                    },
                );
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let index = params
                    .get("summaryIndex")
                    .or_else(|| params.get("contentIndex"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                Self::emit(
                    &mut s,
                    BridgeEvent::ReasoningDelta {
                        item_id: item_id(),
                        delta,
                        index,
                    },
                );
            }
            "item/commandExecution/outputDelta" => {
                if self.expose_activity {
                    let delta = params
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Self::emit(
                        &mut s,
                        BridgeEvent::ActivityDelta {
                            item_id: item_id(),
                            delta,
                        },
                    );
                }
            }
            "item/completed" => {
                let item = params.get("item").cloned().unwrap_or(Value::Null);
                let ty = item.get("type").and_then(Value::as_str).unwrap_or_default();
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                match ty {
                    "agentMessage" => {
                        let text = item
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if !text.is_empty() {
                            s.segment_text.push(text.clone());
                        }
                        Self::emit(
                            &mut s,
                            BridgeEvent::MessageEnd {
                                item_id: id,
                                text,
                                phase: item
                                    .get("phase")
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                            },
                        );
                    }
                    "reasoning" => {
                        let strings = |k: &str| -> Vec<String> {
                            item.get(k)
                                .and_then(Value::as_array)
                                .map(|a| {
                                    a.iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_string)
                                        .collect()
                                })
                                .unwrap_or_default()
                        };
                        Self::emit(
                            &mut s,
                            BridgeEvent::ReasoningEnd {
                                item_id: id,
                                summary: strings("summary"),
                                content: strings("content"),
                            },
                        );
                    }
                    "dynamicToolCall" | "userMessage" => {}
                    _ => {
                        if self.expose_activity {
                            Self::emit(&mut s, BridgeEvent::ActivityEnd { item });
                        }
                    }
                }
            }
            "thread/tokenUsage/updated" => {
                if let Some(tu) = params.get("tokenUsage") {
                    if let Some(last) = tu.get("last") {
                        s.thread_usage_last = Some(Usage::from_json(last));
                    }
                    s.context_window = tu.get("modelContextWindow").and_then(Value::as_i64);
                }
            }
            "rawResponseItem/completed" => {
                s.raw_events_seen = true;
                if let Some(item) = params.get("item")
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                    && let Some(name) = item.get("name").and_then(Value::as_str)
                    && self.dynamic_tools.contains(name)
                {
                    s.expected_dynamic_calls += 1;
                }
            }
            "rawResponse/completed" => {
                s.raw_events_seen = true;
                if let Some(usage) = params.get("usage")
                    && !usage.is_null()
                {
                    s.usage.add_json(usage);
                    s.usage_from_raw = true;
                }
                s.response_completed = true;
                self.maybe_end_for_tools(&mut s);
            }
            "error" => {
                let will_retry = params
                    .get("willRetry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let message = params
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string();
                if will_retry {
                    debug!(thread = %self.thread_id, "transient error (will retry): {message}");
                } else {
                    s.last_error = Some(message);
                }
            }
            "turn/completed" => {
                let turn = params.get("turn").cloned().unwrap_or(Value::Null);
                let status = turn
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed");
                let (reason, error) = match status {
                    "interrupted" => (DoneReason::Interrupted, None),
                    "failed" => (
                        DoneReason::Error,
                        Some(
                            turn.get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .or_else(|| s.last_error.clone())
                                .unwrap_or_else(|| "turn failed".to_string()),
                        ),
                    ),
                    _ => (DoneReason::Stop, None),
                };
                s.status = TurnStatus::Finished;
                self.end_segment(&mut s, reason, error);
                drop(s);
                let _ = self.status_tx.send(TurnStatus::Finished);
                self.runtime.unsubscribe(&self.thread_id);
                return true;
            }
            _ => {}
        }
        false
    }
}
