//! Proxy configuration (`codexs.toml`).

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ListenConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8790,
        }
    }
}

/// One Codex login == one `CODEX_HOME` (holding `auth.json` + optional `config.toml`).
/// Every account runs its own in-process app-server, so upstream traffic carries
/// exactly the identity/headers a normal Codex session for that login would send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: String,
    pub codex_home: PathBuf,
    #[serde(default = "default_max_concurrent_turns")]
    pub max_concurrent_turns: usize,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_max_concurrent_turns() -> usize {
    4
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAnswer {
    Accept,
    Decline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    /// `clientInfo.name` sent on `initialize`; becomes the upstream `originator`
    /// header, so keep it equal to what the interactive CLI reports.
    pub client_name: String,
    /// `clientInfo.version`; defaults to this crate's version (same as the CLI in-tree build).
    pub client_version: Option<String>,
    /// Path to a real `codex` binary used for helper re-execs (sandbox helpers etc.).
    /// Defaults to this executable.
    pub codex_self_exe: Option<PathBuf>,
    /// Auto-answer for approval prompts a proxy thread cannot forward to a human.
    pub approvals: ApprovalAnswer,
    /// Session source recorded in thread metadata (`cli`, `vscode`, `exec`).
    pub session_source: String,
    /// Outbound proxy for the embedded Codex's upstream traffic
    /// (`http://`, `socks5://`, `socks5h://`). Applied process-wide via
    /// `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`, so it also reaches helper
    /// processes Codex spawns. `codexs server --proxy` overrides it.
    pub proxy: Option<String>,
    /// Upstream base for raw-forwarded Responses requests.
    pub chatgpt_base_url: String,
    /// IANA zone stamped into raw-forwarded `<environment_context>`; empty =
    /// detect from the egress IP (through the proxy) and refresh periodically.
    pub timezone: Option<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            client_name: "codex-tui".to_string(),
            client_version: None,
            codex_self_exe: None,
            approvals: ApprovalAnswer::Accept,
            session_source: "cli".to_string(),
            proxy: None,
            chatgpt_base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            timezone: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexToolsMode {
    /// Keep Codex's own tools (shell, apply_patch, update_plan, web search …).
    /// The wire request is exactly what a normal Codex turn sends.
    Full,
    /// Disable Codex's built-in tools so only the client's tools are offered
    /// (still shaped by Codex: sanitized schemas, code-mode wrapping when the
    /// model uses it).
    None,
    /// The client's tools replace Codex's tools and go to the model exactly as
    /// received (raw schema, `strict`, order); nothing else changes, so the
    /// request keeps Codex's headers, instructions and telemetry. Default in
    /// `codexs server`.
    Passthrough,
}

impl CodexToolsMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "full" => Some(Self::Full),
            "none" => Some(Self::None),
            "passthrough" => Some(Self::Passthrough),
            _ => None,
        }
    }
}

/// When a `/v1/responses` body is forwarded upstream as the client built it
/// (see `api::raw`) instead of being bridged through the in-process Codex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawForwardMode {
    /// Bodies that Codex itself assembled (`client_metadata`, Responses Lite
    /// `additional_tools`, Codex's base prompt as a developer message) go raw;
    /// everything else is bridged. Default in `codexs server`.
    Auto,
    /// Every Responses request goes raw.
    Always,
    /// Never (config-file mode default).
    Never,
}

impl RawForwardMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemPromptMode {
    /// Map the client's system prompt to `developerInstructions`.
    Developer,
    /// Drop the client's system prompt.
    Ignore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadDefaults {
    /// Model passed to `thread/start`; empty = the account's `config.toml` default.
    pub model: String,
    /// Reasoning effort (`low`, `medium`, `high`, `xhigh`, …); empty = default.
    pub reasoning_effort: String,
    /// Reasoning summary mode (`auto`, `concise`, `detailed`, `none`); empty = default.
    pub reasoning_summary: String,
    /// `read-only` | `workspace-write` | `danger-full-access`.
    pub sandbox: String,
    /// `never` | `on-request` | `untrusted`.
    pub approval_policy: String,
    pub codex_tools: CodexToolsMode,
    /// Forward Codex-shaped Responses bodies upstream untouched (`x-asxs-raw: on|off` per request).
    pub raw_forward: RawForwardMode,
    pub system_prompt_mode: SystemPromptMode,
    /// Dotted `config.toml` overrides applied to every thread (`-c key=value` semantics).
    pub config_overrides: BTreeMap<String, toml::Value>,
    /// Do not persist rollouts for proxy threads.
    pub ephemeral: bool,
    /// `friendly` | `pragmatic` | `none`; empty = server default.
    pub personality: String,
    /// Service tier (`fast`, `default`); empty = account default.
    pub service_tier: String,
}

impl Default for ThreadDefaults {
    fn default() -> Self {
        Self {
            model: String::new(),
            reasoning_effort: String::new(),
            reasoning_summary: String::new(),
            sandbox: "read-only".to_string(),
            approval_policy: "never".to_string(),
            codex_tools: CodexToolsMode::Full,
            raw_forward: RawForwardMode::Never,
            system_prompt_mode: SystemPromptMode::Developer,
            config_overrides: BTreeMap::new(),
            ephemeral: false,
            personality: String::new(),
            service_tier: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySeeding {
    /// Use the ASXS `initialHistory` extension on `thread/start` (patched codex).
    Patch,
    /// Fold prior turns into the first user message (works with stock codex).
    Preamble,
    /// Return 400 when history cannot be matched to a live session.
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionsConfig {
    pub idle_ttl_secs: u64,
    /// Pending client tool calls are failed after this long without a follow-up request.
    pub tool_result_timeout_secs: u64,
    pub history_seeding: HistorySeeding,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            idle_ttl_secs: 6 * 60 * 60,
            tool_result_timeout_secs: 15 * 60,
            history_seeding: HistorySeeding::Preamble,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Emit Codex-internal activity (commands, file changes, web searches) as
    /// extra `codex_activity` output items on the Responses API.
    pub expose_activity: bool,
    /// Chat Completions: stream reasoning summaries as `reasoning_content`.
    pub chat_reasoning_content: bool,
    /// Chat Completions: also stream Codex activity lines into `reasoning_content`.
    pub chat_activity_in_reasoning: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            expose_activity: true,
            chat_reasoning_content: true,
            chat_activity_in_reasoning: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientCredentialsMode {
    /// Requests may carry their own Codex credentials; otherwise static accounts are used.
    Allowed,
    /// Every request must carry Codex credentials ("server mode").
    Required,
    /// Ignore client credentials; only static accounts are used.
    Disabled,
}

/// Server-mode authentication: clients bring their own ChatGPT OAuth tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub client_credentials: ClientCredentialsMode,
    /// Per-identity `CODEX_HOME`s are created under here.
    pub identity_root: PathBuf,
    /// Optional `config.toml` copied into a new identity's `CODEX_HOME`.
    pub identity_config_template: Option<PathBuf>,
    /// Stop an identity's in-process app-server after this much idle time with no sessions.
    pub identity_idle_ttl_secs: u64,
    pub identity_max_concurrent_turns: usize,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            client_credentials: ClientCredentialsMode::Allowed,
            identity_root: PathBuf::from("data/identities"),
            identity_config_template: None,
            identity_idle_ttl_secs: 30 * 60,
            identity_max_concurrent_turns: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub listen: ListenConfig,
    /// API keys accepted in `Authorization: Bearer …`. Empty = no auth (local use only).
    pub api_keys: Vec<String>,
    pub auth: AuthConfig,
    pub codex: CodexConfig,
    pub accounts: Vec<AccountConfig>,
    pub defaults: ThreadDefaults,
    /// Root directory for per-session working directories.
    pub workspace_root: PathBuf,
    pub sessions: SessionsConfig,
    pub api: ApiConfig,
    pub log_filter: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen: ListenConfig::default(),
            api_keys: Vec::new(),
            auth: AuthConfig::default(),
            codex: CodexConfig::default(),
            accounts: Vec::new(),
            defaults: ThreadDefaults::default(),
            workspace_root: PathBuf::from("data/workspaces"),
            sessions: SessionsConfig::default(),
            api: ApiConfig::default(),
            log_filter: "info,codexs=debug".to_string(),
        }
    }
}

pub fn default_codex_home() -> PathBuf {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home);
    }
    codex_core::config::find_codex_home()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| PathBuf::from(".codex"))
}

pub fn load(explicit: Option<PathBuf>) -> anyhow::Result<(ProxyConfig, PathBuf)> {
    let path = match explicit {
        Some(p) => p,
        None => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from);
            let candidates = [
                std::env::var_os("CODEXS_CONFIG").map(PathBuf::from),
                Some(PathBuf::from("codexs.toml")),
                home.map(|h| h.join(".codexs").join("codexs.toml")),
            ];
            candidates
                .into_iter()
                .flatten()
                .find(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("codexs.toml"))
        }
    };
    let path = std::path::absolute(&path).unwrap_or(path);
    let mut config = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str::<ProxyConfig>(&raw)
            .with_context(|| format!("parsing {}", path.display()))?
    } else {
        tracing::warn!("config file {} not found; using defaults", path.display());
        ProxyConfig::default()
    };
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    if let Ok(v) = std::env::var("CODEXS_PORT")
        && let Ok(port) = v.parse::<u16>()
    {
        config.listen.port = port;
    }
    if let Ok(v) = std::env::var("CODEXS_HOST")
        && !v.is_empty()
    {
        config.listen.host = v;
    }
    if let Ok(v) = std::env::var("CODEXS_API_KEYS") {
        config.api_keys = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }

    if config.accounts.is_empty()
        && config.auth.client_credentials != ClientCredentialsMode::Required
    {
        config.accounts.push(AccountConfig {
            id: "default".to_string(),
            codex_home: default_codex_home(),
            max_concurrent_turns: default_max_concurrent_turns(),
            enabled: true,
        });
    }
    config.accounts.retain(|a| a.enabled);
    for account in &mut config.accounts {
        account.codex_home = absolutize(&base_dir, &account.codex_home);
    }
    config.workspace_root = absolutize(&base_dir, &config.workspace_root);
    config.auth.identity_root = absolutize(&base_dir, &config.auth.identity_root);
    if let Some(t) = config.auth.identity_config_template.take() {
        config.auth.identity_config_template = Some(absolutize(&base_dir, &t));
    }
    if let Some(exe) = config.codex.codex_self_exe.take() {
        config.codex.codex_self_exe = Some(absolutize(&base_dir, &exe));
    }
    Ok((config, path))
}

fn absolutize(base: &Path, p: &Path) -> PathBuf {
    let expanded = expand_home(p);
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

fn expand_home(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\"))
        && let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    {
        return PathBuf::from(home).join(rest);
    }
    p.to_path_buf()
}
