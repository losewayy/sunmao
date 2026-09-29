//! Kernel context: the `ctx.*` seams assembled in one place.
//!
//! v0.1 ships concrete seams only — llm, sessions, tools, audit. No speculative
//! interfaces: each field earns its indirection when a second implementation
//! needs it.

use std::path::PathBuf;
use std::sync::Arc;

use crate::audit::AuditLog;
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
    /// Working directory tools resolve paths against.
    pub cwd: PathBuf,
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
            cwd,
        }
    }
}
