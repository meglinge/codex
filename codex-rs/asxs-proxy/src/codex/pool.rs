//! Account pool: one [`CodexRuntime`] per configured Codex login.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use codex_arg0::Arg0DispatchPaths;
use tracing::error;

use crate::codex::runtime::CodexRuntime;
use crate::config::ProxyConfig;

pub struct AccountPool {
    runtimes: Vec<Arc<CodexRuntime>>,
}

impl AccountPool {
    pub async fn start(cfg: &ProxyConfig, arg0: &Arg0DispatchPaths) -> anyhow::Result<Self> {
        let mut runtimes = Vec::new();
        for account in &cfg.accounts {
            match CodexRuntime::start(account, cfg, arg0).await {
                Ok(rt) => runtimes.push(rt),
                Err(e) => error!(account = %account.id, "failed to start: {e:#}"),
            }
        }
        anyhow::ensure!(!runtimes.is_empty(), "no Codex account could be started");
        Ok(Self { runtimes })
    }

    pub fn all(&self) -> &[Arc<CodexRuntime>] {
        &self.runtimes
    }

    pub fn get(&self, id: &str) -> Option<Arc<CodexRuntime>> {
        self.runtimes.iter().find(|r| r.id == id).cloned()
    }

    /// Least-loaded account for a brand-new session.
    pub fn acquire(&self, preferred: Option<&str>) -> Option<Arc<CodexRuntime>> {
        if let Some(id) = preferred
            && let Some(rt) = self.get(id)
        {
            return Some(rt);
        }
        self.runtimes
            .iter()
            .min_by(|a, b| {
                let la = load(a);
                let lb = load(b);
                la.partial_cmp(&lb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned()
    }

    pub async fn shutdown(&self) {
        for rt in &self.runtimes {
            rt.shutdown().await;
        }
    }
}

fn load(rt: &CodexRuntime) -> f64 {
    let turns = rt.active_turns.load(Ordering::Relaxed) as f64;
    let sessions = rt.sessions.load(Ordering::Relaxed) as f64;
    turns / rt.max_concurrent_turns as f64 + sessions * 0.01
}
