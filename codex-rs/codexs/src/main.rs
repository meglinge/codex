//! codexs: OpenAI-compatible HTTP API served by real in-process Codex sessions.
#![recursion_limit = "256"]

mod api;
mod bridge;
mod codex;
mod config;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use codex_arg0::Arg0DispatchPaths;
use codex_arg0::arg0_dispatch_or_else;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::codex::identity::ClientCredentials;
use crate::config::AccountConfig;
use crate::config::ClientCredentialsMode;
use crate::config::CodexToolsMode;
use crate::config::ProxyConfig;

#[derive(Parser, Debug)]
#[command(name = "codexs", version, about)]
struct Cli {
    /// Path to codexs.toml (default: $CODEXS_CONFIG, ./codexs.toml, then ~/.codexs/codexs.toml).
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    host: Option<String>,
    #[arg(long, global = true)]
    port: Option<u16>,
    /// Print the effective configuration and exit.
    #[arg(long, global = true)]
    print_config: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Serve exactly one Codex account whose credentials and outbound proxy are
    /// given at startup; downstream requests carry no Codex credentials.
    /// Run one `codexs server` per account.
    Server(ServerArgs),
}

#[derive(Args, Debug)]
#[command(group(
    clap::ArgGroup::new("credentials")
        .required(true)
        .args(["codex_home", "access_token", "auth_file"]),
))]
struct ServerArgs {
    /// Outbound proxy for upstream traffic: http://, socks5:// or socks5h://
    /// (process-wide: HTTPS_PROXY/HTTP_PROXY/ALL_PROXY).
    #[arg(long, env = "CODEXS_PROXY")]
    proxy: Option<String>,

    /// Use an existing CODEX_HOME (holding auth.json from `codex login`).
    #[arg(long, env = "CODEXS_CODEX_HOME", value_name = "DIR")]
    codex_home: Option<PathBuf>,
    /// ChatGPT OAuth access token (JWT). A private CODEX_HOME is created under --identity-root.
    #[arg(
        long,
        env = "CODEXS_ACCESS_TOKEN",
        value_name = "JWT",
        hide_env_values = true
    )]
    access_token: Option<String>,
    /// Refresh token (lets Codex renew the access token itself).
    #[arg(
        long,
        env = "CODEXS_REFRESH_TOKEN",
        hide_env_values = true,
        requires = "access_token"
    )]
    refresh_token: Option<String>,
    /// ID token (JWT); defaults to the access token.
    #[arg(
        long,
        env = "CODEXS_ID_TOKEN",
        hide_env_values = true,
        requires = "access_token"
    )]
    id_token: Option<String>,
    /// ChatGPT account id; defaults to the one in the JWT claims.
    #[arg(long, env = "CODEXS_ACCOUNT_ID")]
    account_id: Option<String>,
    /// An auth.json in Codex's format; its tokens are imported into a private CODEX_HOME.
    #[arg(long, env = "CODEXS_AUTH_FILE", value_name = "FILE")]
    auth_file: Option<PathBuf>,
    /// Where private CODEX_HOMEs are created (default: auth.identity_root from the
    /// config file, else ~/.codexs/identities).
    #[arg(long, env = "CODEXS_IDENTITY_ROOT", value_name = "DIR")]
    identity_root: Option<PathBuf>,

    /// API key(s) downstream must send as `Authorization: Bearer ...` (repeatable).
    /// Default: none (open, local use only).
    #[arg(long = "api-key", value_name = "KEY")]
    api_keys: Vec<String>,
    /// Max concurrent turns for this account.
    #[arg(long)]
    max_concurrent_turns: Option<usize>,
    /// How client tools meet Codex's tools: passthrough (client tools replace
    /// Codex's and go upstream verbatim; default), none, full.
    #[arg(
        long,
        env = "CODEXS_CODEX_TOOLS",
        value_name = "MODE",
        default_value = "passthrough"
    )]
    codex_tools: String,
}

fn main() -> anyhow::Result<()> {
    arg0_dispatch_or_else(|arg0_paths: Arg0DispatchPaths| async move { run(arg0_paths).await })
}

async fn run(arg0_paths: Arg0DispatchPaths) -> anyhow::Result<()> {
    let cli = Cli::parse();
    let (mut cfg, cfg_path) = config::load(cli.config.clone())?;
    if let Some(h) = cli.host {
        cfg.listen.host = h;
    }
    if let Some(p) = cli.port {
        cfg.listen.port = p;
    }
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.log_filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();

    if let Some(Command::Server(args)) = cli.command {
        apply_server_args(&mut cfg, cfg_path.exists(), args)?;
    }
    if let Some(proxy) = cfg.codex.proxy.clone() {
        set_outbound_proxy(&proxy);
    }

    if cli.print_config {
        println!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(());
    }
    info!(config = %cfg_path.display(), accounts = cfg.accounts.len(), "starting codexs");

    let cfg = Arc::new(cfg);
    let pool = Arc::new(codex::pool::AccountPool::start(Arc::clone(&cfg), arg0_paths).await?);
    let bridge = bridge::Bridge::new(Arc::clone(&cfg), Arc::clone(&pool));
    bridge.spawn_reaper();

    let state = Arc::new(api::server::AppState {
        cfg: Arc::clone(&cfg),
        bridge,
    });
    let app = api::server::router(state);
    let addr = format!("{}:{}", cfg.listen.host, cfg.listen.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(
        "listening on http://{addr}  (POST /v1/responses, POST /v1/chat/completions, GET /v1/models)"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutdown requested");
        })
        .await?;
    pool.shutdown().await;
    Ok(())
}

/// `codexs server`: exactly one static account from the CLI, downstream
/// credentials ignored, proxy from `--proxy` (falls back to `codex.proxy`).
fn apply_server_args(
    cfg: &mut ProxyConfig,
    config_file_exists: bool,
    args: ServerArgs,
) -> anyhow::Result<()> {
    if args.proxy.is_some() {
        cfg.codex.proxy = args.proxy;
    }
    if !args.api_keys.is_empty() {
        cfg.api_keys = args.api_keys;
    }
    let max_concurrent_turns = args
        .max_concurrent_turns
        .unwrap_or(cfg.auth.identity_max_concurrent_turns)
        .max(1);

    let identity_root = args.identity_root.unwrap_or_else(|| {
        if config_file_exists {
            cfg.auth.identity_root.clone()
        } else {
            home_dir().join(".codexs").join("identities")
        }
    });

    let (id, codex_home) = if let Some(home) = args.codex_home {
        anyhow::ensure!(
            home.join("auth.json").is_file(),
            "{} does not contain auth.json (run `codex login` with CODEX_HOME={0})",
            home.display()
        );
        (
            args.account_id.unwrap_or_else(|| "default".to_string()),
            home,
        )
    } else {
        let creds = if let Some(file) = args.auth_file {
            credentials_from_auth_file(&file, args.account_id)?
        } else {
            let access_token = args.access_token.context("--access-token is required")?;
            anyhow::ensure!(
                codex::identity::looks_like_jwt(&access_token),
                "--access-token must be a ChatGPT access token (JWT)"
            );
            ClientCredentials {
                access_token,
                id_token: args.id_token,
                refresh_token: args.refresh_token,
                account_id: args.account_id,
            }
        };
        let (identity, _) = codex::identity::materialize(
            &identity_root,
            cfg.auth.identity_config_template.as_deref(),
            &creds,
        )?;
        info!(
            identity = %identity.key,
            codex_home = %identity.codex_home.display(),
            "server mode: credentials imported"
        );
        (identity.key, identity.codex_home)
    };

    cfg.accounts = vec![AccountConfig {
        id,
        codex_home,
        max_concurrent_turns,
        enabled: true,
    }];
    cfg.defaults.codex_tools = CodexToolsMode::parse(&args.codex_tools).with_context(|| {
        format!(
            "--codex-tools must be passthrough, none or full (got {:?})",
            args.codex_tools
        )
    })?;
    // Downstream never supplies Codex credentials in this mode.
    cfg.auth.client_credentials = ClientCredentialsMode::Disabled;
    Ok(())
}

fn credentials_from_auth_file(
    path: &Path,
    account_id: Option<String>,
) -> anyhow::Result<ClientCredentials> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let tokens = v
        .get("tokens")
        .context("auth.json has no `tokens` object")?;
    let field = |k: &str| {
        tokens
            .get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let access_token = field("access_token").context("auth.json has no tokens.access_token")?;
    Ok(ClientCredentials {
        access_token,
        id_token: field("id_token"),
        refresh_token: field("refresh_token"),
        account_id: account_id.or_else(|| field("account_id")),
    })
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The embedded Codex uses reqwest's default proxy discovery, which reads
/// these variables; there is no per-client hook, so set them process-wide.
fn set_outbound_proxy(proxy: &str) {
    info!(proxy = %proxy, "outbound proxy");
    for key in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
        // SAFETY: called during startup before any HTTP client or helper
        // process exists; nothing else reads the environment concurrently.
        unsafe { std::env::set_var(key, proxy) };
    }
}
