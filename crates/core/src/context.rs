//! Kernel context: the `ctx.*` seams assembled in one place.
//!
//! v0.1 ships concrete seams only — llm, sessions, tools, audit. No speculative
//! interfaces: each field earns its indirection when a second implementation
//! needs it.

use std::path::PathBuf;
use std::sync::Arc;

use crate::approval::AllowAll;
use crate::approval::Approver;
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
    /// Declarative permission rules (.sunmao/permissions.json + .claude settings).
    pub permissions: crate::permissions::Permissions,
    /// Approval gate — risky tool calls pause here for a verdict.
    pub approval: Arc<dyn Approver>,
    /// Subagent nesting depth — Task tool refuses past MAX_DEPTH.
    pub depth: u8,
    /// Cooperative cancellation — `session/cancel` sets it; the loop checks
    /// between iterations and before each tool call.
    pub cancelled: std::sync::atomic::AtomicBool,
    /// Files read this session — the Read-before-Write gate's ledger.
    /// (crate-visible so sub-agent contexts can construct one)
    pub(crate) read_paths: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    /// Session-scoped approval grants — `"tool\tspecifier"` keys the user
    /// approved with `Approval::Session`. Exact-match only: a grant covers
    /// the identical call, nothing broader. `Arc` so `Task` sub-agents share
    /// the session's grants (they share the same interactive session).
    pub session_grants: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl Context {
    pub fn new(
        llm: Arc<dyn ProviderAdapter>,
        sessions: SessionLog,
        tools: ToolRegistry,
        cwd: PathBuf,
    ) -> Self {
        let permissions = crate::permissions::Permissions::load(&cwd);
        Self {
            llm,
            sessions: tokio::sync::Mutex::new(sessions),
            tools,
            audit: AuditLog::new(),
            hooks: HookEngine::load(&cwd, "session"),
            cwd,
            permissions,
            approval: Arc::new(AllowAll),
            depth: 0,
            cancelled: std::sync::atomic::AtomicBool::new(false),
            read_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
            session_grants: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
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

    /// A prior `Approval::Session` covers this exact call?
    pub fn session_granted(&self, tool: &str, specifier: &str) -> bool {
        self.session_grants
            .lock()
            .unwrap()
            .contains(&format!("{tool}\t{specifier}"))
    }

    /// Record a session-scoped grant.
    pub fn grant_session(&self, tool: &str, specifier: &str) {
        self.session_grants
            .lock()
            .unwrap()
            .insert(format!("{tool}\t{specifier}"));
    }
}
