//! One in-process Codex app-server per account (`CODEX_HOME`).
//!
//! This is the same embedding path the interactive CLI (`codex-tui`) and
//! `codex exec` use: `codex_app_server_client::InProcessAppServerClient` runs
//! the real app-server + core inside our process, so every upstream request is
//! built by Codex itself (prompts, tools, headers, auth, prompt-cache key).
//!
//! The proxy speaks to it with the public JSON-RPC surface (`thread/start`,
//! `turn/start`, `item/tool/call`, …) so behaviour matches an external client.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::anyhow;
use codex_app_server_client::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use codex_app_server_client::EnvironmentManager;
use codex_app_server_client::ExecServerRuntimePaths;
use codex_app_server_client::InProcessAppServerClient;
use codex_app_server_client::InProcessAppServerRequestHandle;
use codex_app_server_client::InProcessClientStartArgs;
use codex_app_server_client::InProcessServerEvent;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_arg0::Arg0DispatchPaths;
use codex_cloud_config::cloud_config_bundle_loader_for_storage;
use codex_config::CloudConfigBundleLoader;
use codex_config::ConfigLoadOptions;
use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::bootstrap_auth_config;
use codex_core::config::load_config_toml_with_layer_stack;
use codex_feedback::CodexFeedback;
use codex_login::default_client::set_default_client_residency_requirement;
use codex_protocol::protocol::SessionSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::config::AccountConfig;
use crate::config::ApprovalAnswer;
use crate::config::ProxyConfig;

/// Thread-scoped event delivered to whoever subscribed to that thread.
#[derive(Debug)]
pub enum ThreadEvent {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
}

enum RuntimeCmd {
    Resolve {
        id: RequestId,
        result: Value,
        tx: oneshot::Sender<std::io::Result<()>>,
    },
    Reject {
        id: RequestId,
        message: String,
        tx: oneshot::Sender<std::io::Result<()>>,
    },
    Shutdown {
        tx: oneshot::Sender<()>,
    },
}

pub struct CodexRuntime {
    pub id: String,
    pub codex_home: PathBuf,
    pub max_concurrent_turns: usize,
    pub active_turns: AtomicUsize,
    pub sessions: AtomicUsize,
    pub default_model: String,
    handle: InProcessAppServerRequestHandle,
    next_id: AtomicI64,
    cmd_tx: mpsc::Sender<RuntimeCmd>,
    subs: Mutex<HashMap<String, mpsc::UnboundedSender<ThreadEvent>>>,
    approvals: ApprovalAnswer,
}

impl CodexRuntime {
    pub async fn start(
        account: &AccountConfig,
        cfg: &ProxyConfig,
        arg0: &Arg0DispatchPaths,
    ) -> anyhow::Result<Arc<Self>> {
        let codex_home = account.codex_home.clone();
        anyhow::ensure!(
            codex_home.is_dir(),
            "account {}: CODEX_HOME {} is not a directory",
            account.id,
            codex_home.display()
        );
        std::fs::create_dir_all(&cfg.workspace_root)
            .with_context(|| format!("creating {}", cfg.workspace_root.display()))?;
        let cwd = AbsolutePathBuf::from_absolute_path(&cfg.workspace_root)
            .with_context(|| format!("workspace_root {} must be absolute", cfg.workspace_root.display()))?;

        let mut arg0 = arg0.clone();
        if let Some(exe) = &cfg.codex.codex_self_exe {
            arg0.codex_self_exe = Some(exe.clone());
        }

        let cli_overrides: Vec<(String, toml::Value)> = cfg
            .defaults
            .config_overrides
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let loader_overrides = LoaderOverrides::default();

        let bootstrap = load_config_toml_with_layer_stack(
            &codex_home,
            Some(&cwd),
            cli_overrides.clone(),
            ConfigLoadOptions {
                loader_overrides: loader_overrides.clone(),
                strict_config: false,
                cloud_config_bundle: CloudConfigBundleLoader::default(),
            },
        )
        .await
        .with_context(|| format!("account {}: loading config.toml", account.id))?;
        let auth_config = bootstrap_auth_config(&codex_home, &bootstrap)?;
        let cloud_config_bundle = cloud_config_bundle_loader_for_storage(auth_config, false).await?;

        let overrides = ConfigOverrides {
            model: (!cfg.defaults.model.is_empty()).then(|| cfg.defaults.model.clone()),
            cwd: Some(cfg.workspace_root.clone()),
            codex_self_exe: arg0.codex_self_exe.clone(),
            codex_linux_sandbox_exe: arg0.codex_linux_sandbox_exe.clone(),
            main_execve_wrapper_exe: arg0.main_execve_wrapper_exe.clone(),
            ephemeral: cfg.defaults.ephemeral.then_some(true),
            ..ConfigOverrides::default()
        };
        let config = ConfigBuilder::default()
            .codex_home(codex_home.clone())
            .cli_overrides(cli_overrides.clone())
            .harness_overrides(overrides)
            .loader_overrides(loader_overrides.clone())
            .strict_config(false)
            .cloud_config_bundle(cloud_config_bundle.clone())
            .build()
            .await
            .with_context(|| format!("account {}: building Codex config", account.id))?;
        set_default_client_residency_requirement(config.enforce_residency.value());

        let state_db = codex_core::init_state_db(&config).await;
        let runtime_paths = ExecServerRuntimePaths::from_optional_paths(
            arg0.codex_self_exe.clone(),
            arg0.codex_linux_sandbox_exe.clone(),
        )?;
        let environment_manager = EnvironmentManager::from_codex_home(
            config.codex_home.clone(),
            Some(runtime_paths),
            config.http_client_factory(),
        )
        .await?;
        let default_model = config.model.clone().unwrap_or_default();

        let session_source = match cfg.codex.session_source.as_str() {
            "vscode" => SessionSource::VSCode,
            "exec" => SessionSource::Exec,
            "mcp" => SessionSource::Mcp,
            "cli" | "" => SessionSource::Cli,
            other => SessionSource::Custom(other.to_string()),
        };

        let args = InProcessClientStartArgs {
            arg0_paths: arg0,
            config: Arc::new(config),
            cli_overrides,
            loader_overrides,
            strict_config: false,
            cloud_config_bundle,
            feedback: CodexFeedback::new(),
            log_db: None,
            state_db,
            environment_manager: Arc::new(environment_manager),
            config_warnings: Vec::new(),
            session_source,
            enable_codex_api_key_env: false,
            client_name: cfg.codex.client_name.clone(),
            client_version: cfg
                .codex
                .client_version
                .clone()
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: vec![
                "fs/changed".to_string(),
                "turn/diff/updated".to_string(),
                "item/commandExecution/terminalInteraction".to_string(),
            ],
            channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
        };

        let mut client = InProcessAppServerClient::start(args)
            .await
            .map_err(|e| anyhow!("account {}: starting in-process app-server: {e}", account.id))?;
        let handle = client.request_handle();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<RuntimeCmd>(64);

        let runtime = Arc::new(Self {
            id: account.id.clone(),
            codex_home,
            max_concurrent_turns: account.max_concurrent_turns.max(1),
            active_turns: AtomicUsize::new(0),
            sessions: AtomicUsize::new(0),
            default_model,
            handle,
            next_id: AtomicI64::new(1),
            cmd_tx,
            subs: Mutex::new(HashMap::new()),
            approvals: cfg.codex.approvals,
        });

        let loop_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = client.next_event() => {
                        let Some(event) = event else {
                            warn!(account = %loop_runtime.id, "app-server event stream ended");
                            break;
                        };
                        loop_runtime.handle_event(&client, event).await;
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(RuntimeCmd::Resolve { id, result, tx }) => {
                                let _ = tx.send(client.resolve_server_request(id, result).await);
                            }
                            Some(RuntimeCmd::Reject { id, message, tx }) => {
                                let error = JSONRPCErrorError { code: -32000, message, data: None };
                                let _ = tx.send(client.reject_server_request(id, error).await);
                            }
                            Some(RuntimeCmd::Shutdown { tx }) => {
                                if let Err(e) = client.shutdown().await {
                                    warn!(account = %loop_runtime.id, "shutdown failed: {e}");
                                }
                                let _ = tx.send(());
                                return;
                            }
                            None => break,
                        }
                    }
                }
            }
            let _ = client.shutdown().await;
        });

        info!(account = %runtime.id, home = %runtime.codex_home.display(), model = %runtime.default_model, "codex runtime started");
        Ok(runtime)
    }

    fn next_request_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a JSON-RPC request built from raw JSON (method + params).
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_request_id();
        let request: ClientRequest =
            serde_json::from_value(json!({ "method": method, "id": id, "params": params }))
                .with_context(|| format!("building {method} request"))?;
        debug!(account = %self.id, method, id, "-> request");
        let result = self
            .handle
            .request(request)
            .await
            .map_err(|e| anyhow!("{method}: transport error: {e}"))?;
        match result {
            Ok(value) => Ok(value),
            Err(err) => Err(anyhow!(
                "{method} failed: {} (code {}){}",
                err.message,
                err.code,
                err.data
                    .as_ref()
                    .map(|d| format!(", data: {d}"))
                    .unwrap_or_default()
            )),
        }
    }

    pub async fn resolve(&self, id: RequestId, result: Value) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(RuntimeCmd::Resolve { id, result, tx })
            .await
            .map_err(|_| anyhow!("runtime {} is shut down", self.id))?;
        rx.await
            .map_err(|_| anyhow!("runtime {} dropped resolve reply", self.id))?
            .map_err(Into::into)
    }

    pub async fn reject(&self, id: RequestId, message: String) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(RuntimeCmd::Reject { id, message, tx })
            .await
            .map_err(|_| anyhow!("runtime {} is shut down", self.id))?;
        rx.await
            .map_err(|_| anyhow!("runtime {} dropped reject reply", self.id))?
            .map_err(Into::into)
    }

    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(RuntimeCmd::Shutdown { tx }).await.is_ok() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(60), rx).await;
        }
    }

    pub fn subscribe(&self, thread_id: &str) -> mpsc::UnboundedReceiver<ThreadEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(mut subs) = self.subs.lock() {
            subs.insert(thread_id.to_string(), tx);
        }
        rx
    }

    pub fn unsubscribe(&self, thread_id: &str) {
        if let Ok(mut subs) = self.subs.lock() {
            subs.remove(thread_id);
        }
    }

    pub async fn model_list(&self) -> anyhow::Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let res = self
                .request(
                    "model/list",
                    json!({ "cursor": cursor, "limit": 100, "includeHidden": false }),
                )
                .await?;
            if let Some(items) = res.get("data").and_then(Value::as_array) {
                out.extend(items.iter().cloned());
            }
            cursor = res
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Auto-answer a server request that no thread handler wants (or one a
    /// handler explicitly delegated back), so a proxy thread never blocks on a
    /// human prompt.
    pub async fn auto_answer(&self, id: RequestId, method: &str, params: &Value) {
        let accept = self.approvals == ApprovalAnswer::Accept;
        let result = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                Some(json!({ "decision": if accept { "accept" } else { "decline" } }))
            }
            "execCommandApproval" | "applyPatchApproval" => {
                Some(json!({ "decision": if accept { "approved" } else { "denied" } }))
            }
            "item/permissions/requestApproval" if accept => Some(json!({
                "scope": "turn",
                "permissions": params.get("permissions").cloned().unwrap_or(json!({})),
            })),
            "item/tool/call" => Some(json!({
                "contentItems": [{ "type": "inputText", "text": "Tool call could not be routed to a client." }],
                "success": false,
            })),
            _ => None,
        };
        let outcome = match result {
            Some(result) => self.resolve(id, result).await,
            None => {
                self.reject(id, format!("asxs-proxy: {method} is not supported for proxy threads"))
                    .await
            }
        };
        if let Err(e) = outcome {
            warn!(account = %self.id, method, "auto-answer failed: {e}");
        }
    }

    async fn handle_event(&self, client: &InProcessAppServerClient, event: InProcessServerEvent) {
        match event {
            InProcessServerEvent::Lagged { skipped } => {
                warn!(account = %self.id, skipped, "app-server event stream lagged");
            }
            InProcessServerEvent::ServerNotification(notification) => {
                let Ok(value) = serde_json::to_value(&*notification) else {
                    return;
                };
                let method = value
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                let thread_id = thread_id_of(&params);
                if let Some(thread_id) = thread_id {
                    let sender = self
                        .subs
                        .lock()
                        .ok()
                        .and_then(|subs| subs.get(&thread_id).cloned());
                    if let Some(sender) = sender {
                        if sender
                            .send(ThreadEvent::Notification { method, params })
                            .is_err()
                        {
                            self.unsubscribe(&thread_id);
                        }
                        return;
                    }
                }
                match method.as_str() {
                    "account/rateLimits/updated" | "account/updated" | "warning" | "configWarning"
                    | "deprecationNotice" => {
                        debug!(account = %self.id, method, params = %params, "notification");
                    }
                    _ => debug!(account = %self.id, method, "unrouted notification"),
                }
            }
            InProcessServerEvent::ServerRequest(request) => {
                let id = request.id().clone();
                let Ok(value) = serde_json::to_value(&*request) else {
                    return;
                };
                let method = value
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                if let Some(thread_id) = thread_id_of(&params) {
                    let sender = self
                        .subs
                        .lock()
                        .ok()
                        .and_then(|subs| subs.get(&thread_id).cloned());
                    if let Some(sender) = sender
                        && sender
                            .send(ThreadEvent::Request {
                                id: id.clone(),
                                method: method.clone(),
                                params: params.clone(),
                            })
                            .is_ok()
                    {
                        return;
                    }
                }
                // Nobody owns this thread: answer inline with the client we hold here.
                let accept = self.approvals == ApprovalAnswer::Accept;
                let result = match method.as_str() {
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                        Some(json!({ "decision": if accept { "accept" } else { "decline" } }))
                    }
                    "item/tool/call" => Some(json!({
                        "contentItems": [{ "type": "inputText", "text": "Tool call could not be routed to a client." }],
                        "success": false,
                    })),
                    _ => None,
                };
                let outcome = match result {
                    Some(result) => client.resolve_server_request(id, result).await,
                    None => {
                        client
                            .reject_server_request(
                                id,
                                JSONRPCErrorError {
                                    code: -32000,
                                    message: format!("asxs-proxy: {method} is not supported"),
                                    data: None,
                                },
                            )
                            .await
                    }
                };
                if let Err(e) = outcome {
                    warn!(account = %self.id, method, "failed to answer server request: {e}");
                }
            }
        }
    }
}

fn thread_id_of(params: &Value) -> Option<String> {
    if let Some(id) = params.get("threadId").and_then(Value::as_str) {
        return Some(id.to_string());
    }
    params
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}
