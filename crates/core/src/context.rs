//! Kernel context: the `ctx.*` seams assembled in one place.
//!
//! v0.1 ships concrete seams only — llm, sessions, tools, audit. No speculative
//! interfaces: each field earns its indirection when a second implementation
//! needs it.

use std::path::PathBuf;
use std::sync::Arc;

use crate::audit::AuditLog;
use crate::hooks::HookEngine;
use crate::session::SessionLog;
use crate::tool::ToolRegistry;
use sunmao_llm::ProviderAdapter;

pub struct Context {
    /// Provider adapter (chat-completions dialect for v0.1).
    pub llm: Arc<dyn ProviderAdapter>,
    /// Active session's event log.
    pub sessions: tokio::sync::Mutex<SessionLog>,
    /// Tool registry (native + managed + shell).
    pub tools: ToolRegistry,
    /// Audit ledger — permission checks and notable facts.
    pub audit: AuditLog,
    /// Hook dispatcher — lifecycle events fire through dialect-compatible
    /// external commands.
    pub hooks: HookEngine,
    /// Working directory tools resolve paths against.
    pub cwd: PathBuf,
    /// Subagent nesting depth — Task tool refuses past MAX_DEPTH.
    pub depth: u8,
    /// Cooperative cancellation — `session/cancel` sets it; the loop checks
    /// between iterations and before each tool call.
    pub cancelled: std::sync::atomic::AtomicBool,
    /// Files read this session — the Read-before-Write gate's ledger.
    /// (crate-visible so sub-agent contexts can construct one)
    pub(crate) read_paths: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
}

impl Context {
    pub fn new(
        llm: Arc<dyn ProviderAdapter>,
        sessions: SessionLog,
        tools: ToolRegistry,
        cwd: PathBuf,
    ) -> Self {
        Self {
            llm,
            sessions: tokio::sync::Mutex::new(sessions),
            tools,
            audit: AuditLog::new(),
            hooks: HookEngine::load(&cwd, "session"),
            cwd,
            depth: 0,
            cancelled: std::sync::atomic::AtomicBool::new(false),
            read_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub fn mark_read(&self, path: &std::path::Path) {
        if let Ok(canon) = path.canonicalize() {
            self.read_paths.lock().unwrap().insert(canon);
        }
        self.read_paths.lock().unwrap().insert(path.to_path_buf());
    }

    pub fn has_read(&self, path: &std::path::Path) -> bool {
        let set = self.read_paths.lock().unwrap();
        if set.contains(path) {
            return true;
        }
        path.canonicalize()
            .map(|c| set.contains(&c))
            .unwrap_or(false)
    }
}
