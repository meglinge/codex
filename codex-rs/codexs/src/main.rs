//! codexs: OpenAI-compatible HTTP API served by real in-process Codex sessions.
#![recursion_limit = "256"]

mod api;
mod bridge;
mod codex;
mod config;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use codex_arg0::Arg0DispatchPaths;
use codex_arg0::arg0_dispatch_or_else;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "codexs", version, about)]
struct Cli {
    /// Path to codexs.toml (default: $CODEXS_CONFIG, ./codexs.toml, then ~/.codexs/codexs.toml).
    #[arg(short, long)]
    config: Option<PathBuf>,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    /// Print the effective configuration and exit.
    #[arg(long)]
    print_config: bool,
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
