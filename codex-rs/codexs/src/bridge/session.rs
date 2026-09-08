//! A proxy session == one Codex thread on one account.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use super::turn::TurnRun;
use crate::codex::runtime::CodexRuntime;

pub struct Session {
    pub id: String,
    pub runtime: Arc<CodexRuntime>,
    pub thread_id: String,
    pub model: String,
    pub cwd: PathBuf,
    pub tools_key: String,
    pub instructions: String,
    /// Identity/account scope the session belongs to (part of the transcript key).
    pub scope: String,
    /// sanitized (Codex-side) tool name -> client tool name.
    pub tool_names: HashMap<String, String>,
    /// Serialises HTTP requests targeting this session.
    pub lock: Arc<tokio::sync::Mutex<()>>,
    pub turn: Mutex<Option<Arc<TurnRun>>>,
    /// Transcript key of the conversation state this session currently holds.
    pub transcript_key: Mutex<Option<String>>,
    pub last_used: Mutex<Instant>,
    pub created_at: Instant,
}

impl Session {
    pub fn current_turn(&self) -> Option<Arc<TurnRun>> {
        self.turn.lock().ok().and_then(|t| t.clone())
    }

    pub fn set_turn(&self, turn: Option<Arc<TurnRun>>) {
        if let Ok(mut t) = self.turn.lock() {
            *t = turn;
        }
    }

    pub fn touch(&self) {
        if let Ok(mut t) = self.last_used.lock() {
            *t = Instant::now();
        }
    }

    pub fn idle_for(&self) -> std::time::Duration {
        self.last_used
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }
}
