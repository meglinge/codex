//! Account pool: static [`CodexRuntime`]s from the config plus dynamically
//! started runtimes for client-supplied identities.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_arg0::Arg0DispatchPaths;
use tracing::error;
use tracing::info;

use crate::codex::identity::Identity;
use crate::codex::runtime::CodexRuntime;
use crate::config::ClientCredentialsMode;
use crate::config::ProxyConfig;

pub struct AccountPool {
    cfg: Arc<ProxyConfig>,
    arg0: Arg0DispatchPaths,
    statics: Vec<Arc<CodexRuntime>>,
    identities: Mutex<HashMap<String, Arc<CodexRuntime>>>,
    /// Serialises identity runtime start/restart.
    identity_start: tokio::sync::Mutex<()>,
}

impl AccountPool {
    pub async fn start(cfg: Arc<ProxyConfig>, arg0: Arg0DispatchPaths) -> anyhow::Result<Self> {
        let mut statics = Vec::new();
        for account in &cfg.accounts {
            match CodexRuntime::start(
                &account.id,
                &account.codex_home,
                account.max_concurrent_turns,
                false,
                &cfg,
                &arg0,
            )
            .await
            {
                Ok(rt) => statics.push(rt),
                Err(e) => error!(account = %account.id, "failed to start: {e:#}"),
            }
        }
        if statics.is_empty() && cfg.auth.client_credentials == ClientCredentialsMode::Disabled {
            anyhow::bail!("no Codex account could be started and client credentials are disabled");
        }
        Ok(Self {
            cfg,
            arg0,
            statics,
            identities: Mutex::new(HashMap::new()),
            identity_start: tokio::sync::Mutex::new(()),
        })
    }

    /// Static accounts plus live identity runtimes.
    pub fn all(&self) -> Vec<Arc<CodexRuntime>> {
        let mut out = self.statics.clone();
        if let Ok(ids) = self.identities.lock() {
            out.extend(ids.values().cloned());
        }
        out
    }

    pub fn get(&self, id: &str) -> Option<Arc<CodexRuntime>> {
        self.statics
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .or_else(|| self.identities.lock().ok().and_then(|m| m.get(id).cloned()))
    }

    /// Least-loaded static account for a brand-new session.
    pub fn acquire(&self, preferred: Option<&str>) -> Option<Arc<CodexRuntime>> {
        if let Some(id) = preferred
            && let Some(rt) = self.statics.iter().find(|r| r.id == id)
        {
            return Some(rt.clone());
        }
        self.statics
            .iter()
            .min_by(|a, b| {
                let la = load(a);
                let lb = load(b);
                la.partial_cmp(&lb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned()
    }

    /// Runtime for a client-supplied identity; started on first use and
    /// restarted when `credentials_changed` (new tokens on disk).
    pub async fn identity_runtime(
        &self,
        identity: &Identity,
        credentials_changed: bool,
    ) -> anyhow::Result<Arc<CodexRuntime>> {
        let id = format!("id:{}", identity.key);
        let _guard = self.identity_start.lock().await;
        let existing = self
            .identities
            .lock()
            .ok()
            .and_then(|m| m.get(&id).cloned());
        if let Some(rt) = existing {
            if !credentials_changed {
                rt.touch();
                return Ok(rt);
            }
            info!(runtime = %id, "credentials changed; restarting identity runtime");
            rt.shutdown().await;
            if let Ok(mut m) = self.identities.lock() {
                m.remove(&id);
            }
        }
        let rt = CodexRuntime::start(
            &id,
            &identity.codex_home,
            self.cfg.auth.identity_max_concurrent_turns,
            true,
            &self.cfg,
            &self.arg0,
        )
        .await?;
        if let Ok(mut m) = self.identities.lock() {
            m.insert(id, Arc::clone(&rt));
        }
        Ok(rt)
    }

    /// Stop identity runtimes that have no sessions and have been idle for
    /// longer than `identity_idle_ttl_secs`.
    pub async fn evict_idle_identities(&self) {
        let ttl = Duration::from_secs(self.cfg.auth.identity_idle_ttl_secs);
        let victims: Vec<Arc<CodexRuntime>> = self
            .identities
            .lock()
            .map(|m| {
                m.values()
                    .filter(|rt| {
                        rt.sessions.load(Ordering::Relaxed) == 0
                            && rt.active_turns.load(Ordering::Relaxed) == 0
                            && rt.idle_for() > ttl
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for rt in victims {
            let _guard = self.identity_start.lock().await;
            info!(runtime = %rt.id, "evicting idle identity runtime");
            if let Ok(mut m) = self.identities.lock() {
                m.remove(&rt.id);
            }
            rt.shutdown().await;
        }
    }

    pub async fn shutdown(&self) {
        for rt in self.all() {
            rt.shutdown().await;
        }
    }
}

fn load(rt: &CodexRuntime) -> f64 {
    let turns = rt.active_turns.load(Ordering::Relaxed) as f64;
    let sessions = rt.sessions.load(Ordering::Relaxed) as f64;
    turns / rt.max_concurrent_turns as f64 + sessions * 0.01
}
