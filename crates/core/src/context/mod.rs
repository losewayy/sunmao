//! Kernel context: the `ctx.*` seams assembled in one place.
//!
//! v0.1 ships concrete seams only — llm, sessions, tools, approvals. No
//! speculative interfaces: each field earns its indirection when a second
//! implementation needs it.

use std::path::PathBuf;
use std::sync::Arc;

use crate::approval::AllowAll;
use crate::approval::Approver;
use crate::ext::ExtRegistry;
use crate::hooks::HookEngine;
use crate::session::SessionLog;
use crate::tool::ToolRegistry;
use sunmao_llm::ProviderAdapter;

/// A Fusion model role a session can override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FusionModelRole {
    /// The planner and verifier model.
    Lead,
    /// The delegated worker model.
    Sidekick,
}

pub struct Context {
    /// Provider adapter (chat-completions dialect for v0.1). The session's
    /// baseline — `llm_override` wins when a mid-session switch landed.
    pub llm: Arc<dyn ProviderAdapter>,
    /// Mid-session model switch — `/model` and future role consumers install
    /// a resolved adapter here; the loop reads through `active_llm()` so the
    /// swap takes effect on the next request, never mid-stream.
    pub llm_override: std::sync::RwLock<Option<Arc<dyn ProviderAdapter>>>,
    /// The selector `/model` last switched to — lets the loop ask
    /// `ModelResolver` for the *current* model's catalog capabilities
    /// (context window) instead of guessing.
    pub active_selector: std::sync::RwLock<Option<String>>,
    /// Active session's event log. `Arc` because detached Task sub-agents
    /// (`run_in_background`) outlive their spawn call — they append their
    /// `TaskDone` result straight into the parent's log when they finish.
    /// Note: they hold the log that was active *at spawn time* — a /resume
    /// mid-flight keeps results in the session that launched them.
    pub sessions: Arc<tokio::sync::Mutex<SessionLog>>,
    /// Tool registry (native + managed + shell).
    pub tools: ToolRegistry,
    /// Hook dispatcher — lifecycle events fire through dialect-compatible
    /// external commands.
    /// Arc'd because `AgentLoop::cancel` is sync — the Interrupt event must
    /// be handed to a detached task without waiting for it. All fires after
    /// construction are `&self`, so one owner everywhere else stays the same.
    pub hooks: Arc<HookEngine>,
    /// Working directory tools resolve paths against.
    pub cwd: PathBuf,
    /// The live session's id — file stem of the active log. RwLock so
    /// `swap_session` (TUI `/resume`, `/fork`) can repoint it under a
    /// shared `Arc<Context>`; hooks/ext payloads read it per fire.
    pub session_id: std::sync::RwLock<String>,
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
    pub lane: u16,
    /// Shared lane allocator — sub-contexts clone the same counter so lanes
    /// are unique across the whole spawn tree, not just siblings.
    pub lane_counter: std::sync::Arc<std::sync::atomic::AtomicU16>,
    /// Cooperative cancellation — `session/cancel` sets it; the loop checks
    /// between iterations and before each tool call. `Arc` so a child's
    /// `TaskEntry` can hold a cheap cancel handle into it. Read through
    /// `cancel_signal()` / `CancelSignal` rather than the bare field: the
    /// flag is the memory, `cancel_notify` only the wake, and a waiter that
    /// takes the wake alone loses every cancel that landed before it armed.
    pub cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Mid-flight cancel wake — `cancel()` sets `cancelled` then notifies;
    /// the turn loop and the shell executor `select!` on this so a cancel
    /// lands while a tool call or a stream delta is still in flight, not
    /// just at iteration boundaries. The flag alone can only be polled
    /// where the loop already checks.
    pub cancel_notify: std::sync::Arc<tokio::sync::Notify>,
    /// Which shell executes `Bash` — resolved once from `SUNMAO_SHELL` /
    /// `.sunmao/shell.txt` / `~/.sunmao/shell.txt` / platform auto-detect at
    /// context build (see `tool::ShellBackend`).
    pub shell: crate::tool::ShellBackend,
    /// Per-tool watchdog seconds (`assets/tool-timeouts.txt` merged with
    /// `.sunmao/tool-timeouts.txt` + plugin dirs). A listed tool's call is
    /// abandoned past its budget — Bash is exempt (its own `timeout_secs`
    /// arg, which also kills the process tree, is the finer control) and
    /// Task is exempt by design (long-running agents are the feature).
    pub tool_timeouts: std::sync::Arc<std::collections::HashMap<String, u64>>,
    /// Files read this session — the Read-before-Write gate's ledger.
    /// (crate-visible so sub-agent contexts can construct one)
    pub(crate) read_paths: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    /// File-checkpoint ledger — Write/Edit commit points snapshot pre-write
    /// bytes under `.sunmao/checkpoints/{session_id}/` the first time a file
    /// is mutated in the session; `/rewind` folds them back. `turn` counts
    /// this session's user-turn ordinals (seeded from the log, bumped once
    /// per `run_turn`) — the manifest's rewind granularity. `pub(crate)` so
    /// the turn loop can bump it.
    pub(crate) checkpoints: std::sync::Mutex<crate::checkpoints::CheckpointState>,
    /// Session-scoped approval grants — `"tool\tspecifier"` keys the user
    /// approved with `Approval::Session`. Exact-match only: a grant covers
    /// the identical call, nothing broader. `Arc` so `Task` sub-agents share
    /// the session's grants (they share the same interactive session).
    pub session_grants: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// The session's approval stance (SPEC §4.6) — the gate reads this
    /// before deciding to prompt. `Arc` shared with sub-agents: a mode
    /// switch mid-session takes effect for a child that's already running.
    /// Durable via `SessionEvent::ModeChange`; seeded from the log on open.
    pub approval_mode: std::sync::Arc<std::sync::RwLock<crate::agent::ApprovalMode>>,
    /// The session's turn shape (`agent::TurnMode` — Standard | Fusion).
    /// Deliberately NOT Arc-shared like `approval_mode`: turn mode is a
    /// per-context property — every sub-agent context runs Standard, so a
    /// Sidekick can't delegate a second level. Durable via
    /// `SessionEvent::TurnModeChange`; seeded from the log on open.
    pub turn_mode: std::sync::RwLock<crate::agent::TurnMode>,
    pub(crate) fusion_models: std::sync::RwLock<FusionModelSettings>,
    /// Per-context read-only flag — the gate's `mode==ReadOnly` block
    /// ORs this in, so `TurnMode::Fusion`'s Lead refuses mutations without
    /// touching the session's shared `approval_mode` (a shared switch
    /// would lock the Sidekick too — that's the whole reason this exists
    /// apart from ApprovalMode::ReadOnly). Per-context means escalation
    /// flips the LEAD's flag only.
    pub read_only: std::sync::atomic::AtomicBool,
    /// Live fusion delegation state (agent/fusion.rs) — the Sidekick
    /// handle, whitelist, verify streak and escalation flag. Empty under
    /// Standard turns; sub-agent contexts get their own (always empty —
    /// Sidekicks are Standard, so `fusion.whitelist` doubles as the
    /// gate marker for "this context is a Sidekick").
    /// (crate-visible like `read_paths` — a mechanism detail, not a seam)
    pub(crate) fusion: std::sync::Mutex<crate::agent::fusion::FusionState>,
    /// The session's reasoning-effort override (`/effort`, GUI chip, ACP
    /// ThoughtLevel) — the turn loops read it into `ChatRequest` each
    /// request, so a mid-session change applies to the next stream.
    /// `Arc` shared with sub-agents for the same reason as `approval_mode`;
    /// durable as a `Hook{event:"effort.change"}` fact (detail = level,
    /// or "default" for back-to-provider-default), seeded on open/resume.
    pub reasoning_effort: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    /// The level the catalog's ladder made this session's default — the
    /// provenance marker for `reasoning_effort`. Equal to it means nobody
    /// dialed since, so a catalog edit is free to move the default; a
    /// different value means the user picked, and the pick stands.
    pub effort_default: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    /// Bash verbs `read_only` mode still permits — builtin list extended by
    /// `.sunmao/readonly-verbs.txt` and plugin dirs at context build.
    pub readonly_verbs: std::sync::Arc<std::collections::HashSet<String>>,
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
    /// Connected MCP servers (MCP Apps host seam — SPEC §4.10/GUI.md §5):
    /// the GUI island bridge proxies `tools/call`/`resources/read` through
    /// these handles and enforces `_meta.ui.visibility`. Empty until the
    /// CLI installs the `connect_all` result — `Context::new` can't spawn
    /// servers synchronously.
    pub mcp_servers: Vec<crate::mcp::McpServerHandle>,
    /// Which loop driver runs turns — SPEC §4.5's replaceable `agentLoop`.
    /// Manifest `loop` keys resolve at build (presets win); `--loop` on
    /// the CLI overrides after the fact. `Bare` turns skip hooks, the
    /// dispatch gate and compaction but keep the session log and observer.
    pub loop_driver: crate::agent::LoopDriver,
    /// The driver the log's `Started` event recorded, if any. A persisted
    /// pick outranks manifest resolution — the baked system prompt in the
    /// log described that surface, and resume must match it. `None` means
    /// an old log or ephemeral session: manifests resolve as before.
    pub persisted_driver: Option<crate::agent::LoopDriver>,
    /// PostToolUse hook output (extra_context, block reasons) buffered
    /// until the iteration's sibling calls all settle — appending a user
    /// message between a tool_call and a sibling's tool_result straddles
    /// the pair and folds to duplicate results. The turn loop drains it
    /// after the last sibling's result lands.
    pub hook_tail: std::sync::Mutex<Vec<String>>,
    /// Cold tools `SearchTools` has already surfaced — they join the
    /// advertised set on later requests (promote-on-search). Only matters
    /// once the lazy surface kicks in; small by construction.
    pub promoted_tools: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Live sub-agent roster — detached `Task` spawns register here,
    /// completion flips `done`. `/tasks` reads it; sub-agent contexts get
    /// their own (a child's roster is its own spawn tree's, not ours).
    /// `Arc` because the detached spawn outlives its `&Context` borrow.
    pub live_tasks: std::sync::Arc<std::sync::Mutex<Vec<TaskEntry>>>,
    /// Live background-job registry — foreground `Bash` calls register here
    /// too, so "what is running right now" has one answer. The durable half
    /// of a job stays `.sunmao/jobs/{id}/`; this table adds the fields the
    /// disk can't answer mid-run (pid, command, foreground, stop wire).
    /// Per-context, like `live_tasks`: a sub-agent's jobs are its own.
    pub jobs: crate::tool::JobTable,
    /// The model's task list (`TodoWrite`) — seeded from the log's latest
    /// `Todos` event at build and on `/resume`, echoed into every request
    /// so compaction never erases the plan. The LOG is source of truth;
    /// this is the hot snapshot for readers (turn injection, `/todos`).
    pub todos: std::sync::Mutex<Vec<crate::tool::TodoItem>>,
    /// The session's standing goal (`/goal` / `UpdateGoal`) — same
    /// seed/inject discipline as `todos`: last `Goal` event wins on open
    /// and resume; the turn loop counts completed turns against
    /// `max_rounds` and chains continuation prompts while `in_progress`.
    pub goal: std::sync::Mutex<Option<crate::tool::GoalState>>,
    /// Frontend submissions waiting behind the current turn — the goal
    /// continuation loop yields while this is non-zero so a typed prompt
    /// interleaves instead of waiting out the whole chain. Frontends bump
    /// on enqueue, drop on dispatch.
    pub input_pending: std::sync::atomic::AtomicUsize,
    /// User steering — messages queued while a turn is running
    /// (`(client id, text)`). The turn loop drains them at each boundary and
    /// appends them as user messages, so they steer THIS turn instead of
    /// becoming the next one. Anything still queued when the turn ends is
    /// claimed by the driver as follow-up input — a steer is never lost.
    /// `Arc` because a sub-agent's queue is the parent's addressing target:
    /// `TaskEntry.steer` holds a clone so `steer_sub`/`Task{steer}` can push
    /// into a live child without owning its Context.
    pub steer: SteerQueue,
    /// The *parent's* steer queue — a child pushes its `SendMessage` text
    /// here so the parent's next request boundary folds it in as a tagged
    /// user message (the reverse direction of `steer_sub`). `None` on the
    /// interactive session's context — there's no parent to address.
    pub parent_steer: Option<SteerQueue>,
    /// `RunCode` sandbox KV — `store()`/`load()` writes fold into this
    /// snapshot; `SessionEvent::PtcStore` lines are the durable record.
    /// Same seed discipline as `todos`: rebuilt from the log on open and
    /// on `swap_session`.
    pub ptc_store: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
    /// One turn at a time per context — the watermark fence. Concurrent
    /// `run_turn` calls (ACP `session/prompt` is per-request spawned, and
    /// any frontend could double-submit) would otherwise interleave
    /// ToolCall/ToolResult facts into the same log and corrupt the
    /// transcript; the mutex makes turns queue instead of weave. A second
    /// turn's events can never straddle a predecessor's — replay stays
    /// honest. Sub-agent contexts hold their own lock: parallel children
    /// stay parallel. `Arc` so a detached child's `TaskDone` append can
    /// take the same fence — a mid-turn child completion must not split
    /// the parent's ToolCall/ToolResult pair.
    pub turn_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

/// The steer queue's shared shape — `(client id, text)` FIFO. Named once:
/// Context carries it, TaskEntry clones the Arc for `steer_sub` addressing.
pub type SteerQueue = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<(u64, String)>>>;

/// Poison-recovery on `std::sync::Mutex` — a panic while holding the lock
/// used to poison it for every subsequent touch, cascading one bad tool
/// call into a dead session. `into_inner` unwraps the panicking writer's
/// value, which is correct: the data was mid-mutation but structurally
/// intact (the panic was in the *logic* of the critical section, not in
/// the container's invariants). `tokio::Mutex` can't poison — its `lock()`
/// returns the guard directly — so no recover form exists for it.
pub trait MutexRecover<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexRecover<T> for std::sync::Mutex<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Same recovery for `RwLock` — read and write arms split because the two
/// guard types differ.
pub trait RwLockRecover<T> {
    fn read_or_recover(&self) -> std::sync::RwLockReadGuard<'_, T>;
    fn write_or_recover(&self) -> std::sync::RwLockWriteGuard<'_, T>;
}

impl<T> RwLockRecover<T> for std::sync::RwLock<T> {
    fn read_or_recover(&self) -> std::sync::RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write_or_recover(&self) -> std::sync::RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|e| e.into_inner())
    }
}

// `TaskEntry` lives with the roster controls in `sub_agent.rs` — it's the
// row shape that module's steer/cancel paths operate on.
pub use sub_agent::TaskEntry;

impl Context {
    pub fn new(
        llm: Arc<dyn ProviderAdapter>,
        sessions: SessionLog,
        tools: ToolRegistry,
        cwd: PathBuf,
    ) -> Self {
        let permissions = crate::permissions::Permissions::load(&cwd, &[]);
        // a project risky-patterns table widens nothing — it can only
        // replace the builtin ask table, and unpinned replacement would
        // disarm it entirely. Untrusted file = additive merge (tighten-
        // only direction); pinned = the author's wholesale replacement.
        let risk_path = cwd.join(".sunmao/risky-patterns.txt");
        let mut risk_table = match std::fs::read_to_string(&risk_path) {
            Ok(text)
                if crate::hooks::trust::is_trusted(
                    &cwd,
                    crate::hooks::trust::Layer::Project,
                    &risk_path,
                    &text,
                ) =>
            {
                crate::approval::parse_table(&text)
            }
            Ok(text) => {
                tracing::warn!(
                    "{}: unpinned risky-patterns merged additively — /hooks trust to replace the builtin table",
                    risk_path.display()
                );
                let mut t = crate::approval::builtin_table();
                t.extend(crate::approval::parse_table(&text));
                t
            }
            Err(_) => crate::approval::builtin_table(),
        };
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
        // the log's own `Started.driver` outranks manifest resolution — it
        // is what the baked system prompt described; the manifest scan is
        // the fallback for logs written before the field existed.
        let persisted_driver = crate::session::started_driver(sessions.path());
        let loop_driver =
            persisted_driver.unwrap_or_else(|| crate::agent::LoopDriver::resolve(&cwd, &[]));
        // the log's file stem is the session id — file-backed and ephemeral
        // logs share the fallback so every consumer sees the same value.
        let session_id = sessions
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".to_string());
        // seed the task-list snapshot + approval mode + goal before
        // `sessions` moves — a resumed log carries the last TodoWrite, the
        // last ModeChange, and the last Goal event.
        let todos = seed_todos(sessions.path());
        let goal = seed_goal(sessions.path());
        let approval_mode = seed_mode(sessions.path());
        let turn_mode = seed_turn_mode(sessions.path());
        let fusion_models = seed_fusion_models(sessions.path());
        let effort = seed_effort(sessions.path());
        let ptc_store = seed_ptc_store(sessions.path());
        // builtin skills land on disk so the prompt's skills index can
        // list them (name+desc) — body stays a file the agent Reads
        seeds::materialize_builtin_skills();
        // checkpoints: rebuild `taken`/`seq` from any existing manifest so a
        // resumed session doesn't re-snapshot already-preserved files, and
        // seed the turn counter from the log's user-turn boundaries so the
        // next manifest entry lands under the right ordinal.
        let mut checkpoints = crate::checkpoints::load(&cwd, &session_id);
        checkpoints.turn = crate::checkpoints::turn_boundaries(sessions.path()).len() as u64;
        // readonly whitelist: builtin verbs + project file + plugin dirs,
        // merged the same way risky-patterns stacks
        let mut verb_extra = Vec::new();
        for f in [
            cwd.join(".sunmao/readonly-verbs.txt"),
            cwd.join(".sunmao/plugin/readonly-verbs.txt"),
        ]
        .into_iter()
        .chain(
            crate::sorted_entries(&cwd.join(".sunmao").join("plugins"))
                .into_iter()
                .chain(crate::sorted_entries(&cwd.join(".claude").join("plugins")))
                .map(|e| e.path().join("readonly-verbs.txt")),
        ) {
            if let Ok(text) = std::fs::read_to_string(&f) {
                // every entry widens read-only's verb set — a checked-out
                // repo could name `rm`/`iex` read-only, so unpinned files
                // are skipped like untrusted allow rules
                if crate::hooks::trust::is_trusted(
                    &cwd,
                    crate::hooks::trust::Layer::Project,
                    &f,
                    &text,
                ) {
                    verb_extra.extend(
                        text.lines()
                            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
                            .map(|l| l.trim().to_string()),
                    );
                } else {
                    tracing::warn!(
                        "{}: unpinned readonly-verbs skipped — /hooks trust to enable",
                        f.display()
                    );
                }
            }
        }
        let readonly_verbs = crate::agent::mode::readonly_verbs(&verb_extra);
        let shell = crate::tool::ShellBackend::resolve(&cwd);
        let tool_timeouts = std::sync::Arc::new(tool_timeout_table(&cwd));
        let sessions = Arc::new(tokio::sync::Mutex::new(sessions));
        let mut hook_engine = HookEngine::load(&cwd, &session_id, &[]);
        // the engine audits trust-pin skips into this log — the Arc
        // identity survives swap_session (which replaces the log inside)
        hook_engine.attach_sessions(sessions.clone());
        Self {
            llm,
            llm_override: std::sync::RwLock::new(None),
            active_selector: std::sync::RwLock::new(None),
            sessions,
            tools,
            hooks: Arc::new(hook_engine),
            cwd,
            session_id: std::sync::RwLock::new(session_id),
            permissions,
            risk_table,
            approval: Arc::new(AllowAll),
            depth: 0,
            lane: 0,
            lane_counter: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cancel_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            shell,
            tool_timeouts,
            read_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
            checkpoints: std::sync::Mutex::new(checkpoints),
            session_grants: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            approval_mode: std::sync::Arc::new(std::sync::RwLock::new(approval_mode)),
            turn_mode: std::sync::RwLock::new(turn_mode),
            fusion_models: std::sync::RwLock::new(fusion_models),
            // Fusion arms it; a resumed fusion log seeds it — the flag
            // follows the mode, never the shared approval stance
            read_only: std::sync::atomic::AtomicBool::new(
                turn_mode == crate::agent::TurnMode::Fusion,
            ),
            fusion: std::sync::Mutex::new(crate::agent::fusion::FusionState::default()),
            reasoning_effort: std::sync::Arc::new(std::sync::RwLock::new(effort)),
            effort_default: std::sync::Arc::new(std::sync::RwLock::new(None)),
            readonly_verbs: std::sync::Arc::new(readonly_verbs),
            live_sink: std::sync::OnceLock::new(),
            models: None,
            agent_name: None,
            extra_plugin_roots: Vec::new(),
            ext: Arc::new(ExtRegistry::new()),
            mcp_servers: Vec::new(),
            loop_driver,
            persisted_driver,
            hook_tail: std::sync::Mutex::new(Vec::new()),
            promoted_tools: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            live_tasks: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            jobs: crate::tool::jobs::new_table(),
            todos: std::sync::Mutex::new(todos),
            ptc_store: std::sync::Mutex::new(ptc_store),
            goal: std::sync::Mutex::new(goal),
            input_pending: std::sync::atomic::AtomicUsize::new(0),
            steer: SteerQueue::default(),
            parent_steer: None,
            turn_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
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
        let session_id = self.session_id.read_or_recover().clone();
        crate::ext::connect_all(
            &self.ext,
            &self.cwd,
            &session_id,
            &self.extra_plugin_roots,
            &self.sessions,
        )
        .await;
        for tool in self.ext.tools() {
            self.tools.register_arc(tool);
        }
        // still sole owner here — the Arc wrap is for cancel()'s detached
        // Interrupt fire, nothing clones it during setup
        if let Some(hooks) = Arc::get_mut(&mut self.hooks) {
            hooks.attach_ext(self.ext.clone());
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
        let mut hook_engine = HookEngine::load(
            &self.cwd,
            self.session_id.read_or_recover().as_str(),
            &self.extra_plugin_roots,
        );
        hook_engine.attach_sessions(self.sessions.clone());
        self.hooks = Arc::new(hook_engine);
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
        // A persisted `Started.driver` outranks even that (see the field).
        if self.persisted_driver.is_none() {
            self.loop_driver =
                crate::agent::LoopDriver::resolve(&self.cwd, &self.extra_plugin_roots);
        }
        self
    }
}

mod fusion;
pub(crate) use fusion::FusionModelSettings;
mod seeds;
mod surface;
pub(crate) use seeds::tool_timeout_table;
use seeds::{
    seed_effort, seed_fusion_models, seed_goal, seed_mode, seed_ptc_store, seed_todos,
    seed_turn_mode,
};

mod cancel;
pub use cancel::CancelSignal;

mod sub_agent;
pub use sub_agent::SubSteerError;
