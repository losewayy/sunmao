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
use crate::ext::ExtRegistry;
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
    /// The session's id — the log's file stem ("session" for ephemeral
    /// logs). Hook payloads (`session_id`, `transcript_path`) and extension
    /// handshakes read it; a context-mode-style hook that Reads the
    /// transcript gets a file that exists, not a literal placeholder.
    /// Captured at Context build — /resume swaps the log, not this field.
    pub session_id: String,
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
    /// The approval gate's risk table — parsed once from
    /// `.sunmao/risky-patterns.txt` when present, else the shipped default.
    /// The file IS the policy: replace it and the gate's judgement changes.
    pub risk_table: Vec<(String, String)>,
    /// Plugin roots layered on top of the convention dirs — resolved
    /// `--preset` dirs. Install via `with_extra_plugin_roots` so the derived
    /// seams (hooks, permissions) reload; consumers append these last,
    /// meaning presets win where layering implies precedence and fill gaps
    /// where lookup is first-match.
    pub extra_plugin_roots: Vec<PathBuf>,
    /// Extension children spawned for this session (plugin.json
    /// `extensions` specs). Empty until `connect_extensions` runs — the
    /// sync ctor can't spawn. `Arc` because hook dispatch reads through it
    /// while the registry owns teardown.
    pub ext: Arc<ExtRegistry>,
    /// Which loop driver runs turns — SPEC §4.5's replaceable `agentLoop`.
    /// Manifest `loop` keys resolve at build (presets win); `--loop` on
    /// the CLI overrides after the fact. `Bare` turns skip hooks, the
    /// dispatch gate and compaction but keep the session log and observer.
    pub loop_driver: crate::agent::LoopDriver,
    /// Live sub-agent roster — detached `Task` spawns register here,
    /// completion flips `done`. `/tasks` reads it; sub-agent contexts get
    /// their own (a child's roster is its own spawn tree's, not ours).
    /// `Arc` because the detached spawn outlives its `&Context` borrow.
    pub live_tasks: std::sync::Arc<std::sync::Mutex<Vec<TaskEntry>>>,
}

/// One detached sub-agent in the roster.
#[derive(Debug, Clone)]
pub struct TaskEntry {
    /// The `sub-…-l<lane>` id — doubles as the child log's file stem.
    pub id: String,
    /// The lane this spawn claimed — unique across the spawn tree; lets a
    /// frontend (or a test) prove distinctness without racing live events.
    pub lane: u8,
    /// Agent def name, or None for a generic spawn.
    pub agent: Option<String>,
    /// One-line digest of the prompt it was given.
    pub prompt: String,
    /// None while running; Some(ok) once TaskDone landed.
    pub done: Option<bool>,
}

impl Context {
    pub fn new(
        llm: Arc<dyn ProviderAdapter>,
        sessions: SessionLog,
        tools: ToolRegistry,
        cwd: PathBuf,
    ) -> Self {
        let permissions = crate::permissions::Permissions::load(&cwd, &[]);
        let mut risk_table = std::fs::read_to_string(cwd.join(".sunmao/risky-patterns.txt"))
            .map(|t| crate::approval::parse_table(&t))
            .unwrap_or_else(|_| crate::approval::builtin_table());
        // plugin bundles can tighten the gate too — same additive merge as
        // presets; wholesale replacement stays a project-file privilege
        for extra in std::iter::once(cwd.join(".sunmao").join("plugin"))
            .chain(
                crate::sorted_entries(&cwd.join(".sunmao").join("plugins"))
                    .into_iter()
                    .map(|e| e.path()),
            )
            .chain(
                crate::sorted_entries(&cwd.join(".claude").join("plugins"))
                    .into_iter()
                    .map(|e| e.path()),
            )
            .map(|root| root.join("risky-patterns.txt"))
        {
            if let Ok(text) = std::fs::read_to_string(extra) {
                risk_table.extend(crate::approval::parse_table(&text));
            }
        }
        // project/plugin manifests may name a loop driver — resolve before
        // `cwd` moves into the struct below.
        let loop_driver = crate::agent::LoopDriver::resolve(&cwd, &[]);
        // the log's file stem is the session id — file-backed and ephemeral
        // logs share the fallback so every consumer sees the same value.
        let session_id = sessions
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".to_string());
        Self {
            llm,
            llm_override: std::sync::RwLock::new(None),
            sessions: Arc::new(tokio::sync::Mutex::new(sessions)),
            tools,
            audit: AuditLog::new(),
            hooks: HookEngine::load(&cwd, &session_id, &[]),
            cwd,
            session_id,
            permissions,
            risk_table,
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
            ext: Arc::new(ExtRegistry::new()),
            loop_driver,
            live_tasks: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Spawn every extension the plugin manifests declare: resolve specs,
    /// handshake each child, register its `ext__*` tools into `self.tools`,
    /// then attach the registry to the hook engine so `ext/event` rides the
    /// same `fire()` as command hooks. Call after `with_extra_plugin_roots`
    /// and before `SessionStart` fires so extensions can receive it.
    /// Failures degrade per child — an unspawnable extension warns and the
    /// rest still come up.
    pub async fn connect_extensions(&mut self) {
        crate::ext::connect_all(
            &self.ext,
            &self.cwd,
            &self.session_id,
            &self.extra_plugin_roots,
        )
        .await;
        for tool in self.ext.tools() {
            self.tools.register_arc(tool);
        }
        self.hooks.attach_ext(self.ext.clone());
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
        self.hooks = HookEngine::load(&self.cwd, &self.session_id, &self.extra_plugin_roots);
        // preset risky-patterns.txt files merge into the resolved table —
        // tightening the gate is additive; wholesale replacement stays a
        // project-file privilege.
        for root in &self.extra_plugin_roots {
            if let Ok(text) = std::fs::read_to_string(root.join("risky-patterns.txt")) {
                self.risk_table.extend(crate::approval::parse_table(&text));
            }
        }
        // presets re-resolve the loop driver — a preset's `loop:` key is
        // the last layer scanned, so it wins over project/plugin choices.
        self.loop_driver = crate::agent::LoopDriver::resolve(&self.cwd, &self.extra_plugin_roots);
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
