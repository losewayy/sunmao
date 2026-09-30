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
    /// Provider adapter (chat-completions dialect for v0.1). The session's
    /// baseline — `llm_override` wins when a mid-session switch landed.
    pub llm: Arc<dyn ProviderAdapter>,
    /// Mid-session model switch — `/model` and future role consumers install
    /// a resolved adapter here; the loop reads through `active_llm()` so the
    /// swap takes effect on the next request, never mid-stream.
    pub llm_override: std::sync::RwLock<Option<Arc<dyn ProviderAdapter>>>,
    /// Active session's event log. `Arc` because detached Task sub-agents
    /// (`run_in_background`) outlive their spawn call — they append their
    /// `TaskDone` result straight into the parent's log when they finish.
    /// Note: they hold the log that was active *at spawn time* — a /resume
    /// mid-flight keeps results in the session that launched them.
    pub sessions: Arc<tokio::sync::Mutex<SessionLog>>,
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
    /// Which concurrent lane this context occupies — 0 is the interactive
    /// agent; each Task spawn claims a fresh lane so parallel sub-agents'
    /// tool events stay attributable (a plain `depth` tag collides when two
    /// children run the same tool at once).
    pub lane: u8,
    /// Shared lane allocator — sub-contexts clone the same counter so lanes
    /// are unique across the whole spawn tree, not just siblings.
    pub lane_counter: std::sync::Arc<std::sync::atomic::AtomicU8>,
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
    /// Live-event sink for nested work — `Task` sub-agents relay their tool
    /// lifecycle here (marked with `depth`) so the frontend can show a
    /// sub-agent working instead of a silently-spinning `Task` block.
    /// `OnceLock` because the context is `Arc`-shared by the time a frontend
    /// exists; first install wins, `None` = discard (the historical
    /// behaviour for `-p` and tests).
    pub live_sink: std::sync::OnceLock<Arc<dyn crate::agent::Observer>>,
    /// Model routing seam — `models.json` + agent `model:` selectors resolve
    /// through it. `None` = single-model session (the `llm` field is the
    /// only adapter); sub-agent spawns then always inherit.
    pub models: Option<Arc<crate::models::ModelResolver>>,
    /// Name of the agent def this context belongs to — `None` for the
    /// interactive agent. Task spawns set it so the child's own Task calls
    /// can be gated by that def's `spawns:` whitelist (and self-recursion
    /// blocked).
    pub agent_name: Option<String>,
    /// Plugin roots layered on top of the convention dirs — resolved
    /// `--preset` dirs. Install via `with_extra_plugin_roots` so the derived
    /// seams (hooks, permissions) reload; consumers append these last,
    /// meaning presets win where layering implies precedence and fill gaps
    /// where lookup is first-match.
    pub extra_plugin_roots: Vec<PathBuf>,
}

impl Context {
    pub fn new(
        llm: Arc<dyn ProviderAdapter>,
        sessions: SessionLog,
        tools: ToolRegistry,
        cwd: PathBuf,
    ) -> Self {
        let permissions = crate::permissions::Permissions::load(&cwd, &[]);
        Self {
            llm,
            llm_override: std::sync::RwLock::new(None),
            sessions: Arc::new(tokio::sync::Mutex::new(sessions)),
            tools,
            audit: AuditLog::new(),
            hooks: HookEngine::load(&cwd, "session", &[]),
            cwd,
            permissions,
            approval: Arc::new(AllowAll),
            depth: 0,
            lane: 0,
            lane_counter: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            cancelled: std::sync::atomic::AtomicBool::new(false),
            read_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
            session_grants: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            live_sink: std::sync::OnceLock::new(),
            models: None,
            agent_name: None,
            extra_plugin_roots: Vec::new(),
        }
    }

    /// Layer preset dirs onto this context. Hooks and permissions reload
    /// because they were already folded into the seams above; everything
    /// else (agents, skills, commands, mcp) scans `extra_plugin_roots` at
    /// use time and picks the roots up directly.
    pub fn with_extra_plugin_roots(mut self, roots: Vec<PathBuf>) -> Self {
        if roots.is_empty() {
            return self;
        }
        self.extra_plugin_roots = roots;
        self.permissions =
            crate::permissions::Permissions::load(&self.cwd, &self.extra_plugin_roots);
        self.hooks = HookEngine::load(&self.cwd, "session", &self.extra_plugin_roots);
        self
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

    /// The adapter the next request uses — override wins over the baseline.
    pub fn active_llm(&self) -> Arc<dyn ProviderAdapter> {
        self.llm_override
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.llm.clone())
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
