//! The agent loop: request → stream → maybe tool calls → repeat.
//!
//! ```text
//! loop {
//!     stream model → accumulate content + tool_calls (live deltas to observer)
//!     append assistant message to session log
//!     if no tool calls: turn ends
//!     else: run tools (recording results), append results, continue
//! }
//! ```

use crate::context::{MutexRecover, RwLockRecover};
use std::sync::Arc;

use crate::context::Context;
use crate::hooks::HookEvent;
use crate::session::{SessionEvent, SessionLog};

mod bare;
mod cancel;
mod compact;
mod gate;
mod goal;
mod mcp;
pub mod mode;
mod steer;
mod turn;

pub use mode::ApprovalMode;

#[cfg(test)]
mod tests;

mod events;
mod summary;

pub use events::{LiveEvent, Observer, TurnOutcome};
pub(crate) use gate::{audit_fact, gate_call};
pub use summary::call_summary;
pub(crate) use summary::{specifier_for, tool_timeout_for, truncate_output};

/// Folded usage facts for `/status` — sums over every `SessionEvent::Usage`.
#[derive(Debug, Clone, Default)]
pub struct TokenTotals {
    pub prompt: u64,
    pub completion: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// One `/status` snapshot — the session's observable vitals, all derived
/// from ctx + the durable log (no extra state to keep honest).
#[derive(Debug, Clone)]
pub struct SessionStatus {
    pub session_id: String,
    pub cwd: std::path::PathBuf,
    /// display label — `Started.model`, or the last `model.change` label
    pub model: String,
    /// provider dialect ("openai"/"anthropic") resolved off the active
    /// selector; "?" when no resolver/route can pin it
    pub provider: String,
    pub approval_mode: ApprovalMode,
    pub tokens: TokenTotals,
    /// Session-scoped approval grants (`Approval::Session` ledger), each
    /// `"tool specifier"` — the set a `/status` reader wants to audit.
    /// Read-only surface: revoking a grant has no UI (TODO if one lands,
    /// it must clear the shared Arc so sub-agents see it too).
    pub grants: Vec<String>,
}
/// Which built-in loop driver runs turns — SPEC §4.5's "the loop is a
/// plugin" stance made concrete. Selected by the `loop` key in a
/// plugin/preset manifest, or `--loop` on the CLI (highest precedence).
/// Cold-plug rule: the choice is a file/flag, applied at context build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopDriver {
    /// The full contract loop: lifecycle hooks, dispatch gate, approvals,
    /// auto-compaction. This is what "the audit spine" means.
    #[default]
    Full,
    /// The minimal loop: message → stream → dispatch → record. No hooks,
    /// no permission gate, no compaction — turns still fold into the same
    /// event-sourced log and still relay live events. For eval rigs and
    /// air-gapped/minimal deployments where the gate's prompts are noise.
    Bare,
}

impl LoopDriver {
    /// Parse a manifest/flag spelling. Unknown names are refused loudly —
    /// silently falling back to Full would run a stricter session than
    /// the operator asked for without telling them.
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "full" | "default" => Ok(Self::Full),
            "bare" | "minimal" => Ok(Self::Bare),
            other => anyhow::bail!("unknown loop driver {other:?} (known: full, bare)"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Bare => "bare",
        }
    }

    /// Resolve the driver a set of plugin roots declares. Scan order is the
    /// layering order — project manifest first, installed bundles, then
    /// preset roots last so a `--preset` picks the loop. First `"loop"` key
    /// found wins the slot at its layer; the LAST layer's declaration wins
    /// overall (same precedence every preset seam follows).
    pub(crate) fn resolve(cwd: &std::path::Path, extra_roots: &[std::path::PathBuf]) -> Self {
        let mut manifests = vec![
            cwd.join(".sunmao").join("plugin.json"),
            cwd.join(".claude-plugin").join("plugin.json"),
        ];
        for base in [
            cwd.join(".sunmao").join("plugins"),
            cwd.join(".claude").join("plugins"),
        ] {
            for e in crate::sorted_entries(&base) {
                if e.path().is_dir() {
                    manifests.push(e.path().join("plugin.json"));
                }
            }
        }
        for root in extra_roots {
            manifests.push(root.join("plugin.json"));
        }
        let mut chosen = Self::default();
        for manifest in manifests {
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let Ok(file) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let Some(name) = file.get("loop").and_then(|l| l.as_str()) else {
                continue;
            };
            match Self::parse(name) {
                Ok(d) => chosen = d,
                Err(e) => tracing::warn!("{}: {e:#}", manifest.display()),
            }
        }
        chosen
    }
}

#[derive(Clone)]
pub struct AgentLoop {
    ctx: Arc<Context>,
    max_iterations: usize,
    /// Estimated-token ceiling before auto-compaction (bytes/4 heuristic).
    compact_threshold: usize,
}

impl AgentLoop {
    pub fn new(ctx: Arc<Context>) -> Self {
        Self {
            ctx,
            max_iterations: 64,
            compact_threshold: 180_000,
        }
    }

    pub fn with_max_iterations(mut self, n: usize) -> Self {
        self.max_iterations = n;
        self
    }

    /// Install the live-event sink `Task` sub-agents relay their tool
    /// lifecycle through. First install wins — frontends call once at setup.
    pub fn set_live_sink(&self, sink: Arc<dyn Observer>) {
        let _ = self.ctx.live_sink.set(sink);
    }

    /// Mid-session model switch: resolve `selector` through the session's
    /// ModelResolver and install it as the active adapter. Returns a display
    /// label (resolved model id when known, else the selector) on success;
    /// `None` when it resolves nowhere (caller keeps the old model and can
    /// say so). Takes effect next request — a streaming turn finishes on
    /// the old adapter.
    pub fn swap_model(&self, selector: &str) -> Option<String> {
        let models = self.ctx.models.as_ref()?;
        let adapter = models.adapter_for(selector)?;
        *self.ctx.llm_override.write_or_recover() = Some(adapter);
        *self.ctx.active_selector.write_or_recover() = Some(selector.to_string());
        let label = models
            .resolve(selector)
            .map(|t| t.model)
            .unwrap_or_else(|| selector.to_string());
        Some(label)
    }

    /// Record a model switch as a durable session fact — replays show the
    /// swap alongside the turns it split.
    pub async fn record_model_change(&self, selector: &str, label: &str) {
        let mut log = self.ctx.sessions.lock().await;
        log.append_audit(&SessionEvent::Hook {
            event: "model.change".into(),
            detail: format!("{selector} → {label}"),
        })
        .await;
    }

    /// The session's current approval stance (SPEC §4.6).
    pub fn approval_mode(&self) -> ApprovalMode {
        *self.ctx.approval_mode.read_or_recover()
    }

    /// Switch the approval stance mid-session — durable as
    /// `SessionEvent::ModeChange` and announced live as a `Hook` audit so
    /// "who switched to full_access when" is a reconstructible fact, not a
    /// memory toggle. Takes the turn fence: a mode flip must not land
    /// between a turn's ToolCall and its ToolResult.
    /// `observer` is the frontend's sink for the audit line; pass the turn
    /// observer when called inside a turn, or the live sink otherwise.
    pub async fn set_approval_mode(&self, mode: ApprovalMode, observer: &dyn Observer) {
        let _turn_permit = self.ctx.turn_lock.lock().await;
        *self.ctx.approval_mode.write_or_recover() = mode;
        let detail = mode.as_str();
        {
            let mut log = self.ctx.sessions.lock().await;
            if let Err(e) = log.append(&SessionEvent::ModeChange { mode }).await {
                // audit fact failed to persist — never silent (stderr via
                // tracing; the live audit line below still reflects intent)
                tracing::warn!("mode change not durable: {e:#}");
            }
        }
        observer.on_event(&LiveEvent::Hook {
            event: "approval.mode".into(),
            detail: detail.to_string(),
        });
    }

    /// Hot-swap the models file after a GUI edit — every session re-reads
    /// `.sunmao/models.json` and clears its adapter cache so edited keys
    /// and catalogs take effect without a restart.
    pub fn reload_models(&self) {
        if let Some(m) = self.ctx.models.as_ref() {
            m.reload();
        }
    }

    /// The project dir this session runs in — a resume/fork across
    /// projects keeps its own root (tools, sessions dir, models.json).
    pub fn session_cwd(&self) -> std::path::PathBuf {
        self.ctx.cwd.clone()
    }

    /// The session's model resolver — the settings surface reads the
    /// merged provider table through it.
    pub fn models_resolver(&self) -> Option<Arc<crate::models::ModelResolver>> {
        self.ctx.models.clone()
    }

    /// List what `/model` can switch to — route names + provider names.
    pub fn model_choices(&self) -> Vec<String> {
        self.ctx
            .models
            .as_ref()
            .map(|m| m.describe())
            .unwrap_or_default()
    }

    /// Completable `/model` selectors — `@route` names + `provider/`
    /// prefixes. The slash menu completes args against these.
    pub fn model_selectors(&self) -> Vec<String> {
        self.ctx
            .models
            .as_ref()
            .map(|m| m.selectors())
            .unwrap_or_default()
    }

    /// The shared Context — frontends need `shell`/`cancel_notify` to run
    /// `!cmd` through the same backend the turn loop uses.
    pub fn context(&self) -> &Arc<Context> {
        &self.ctx
    }

    /// Snapshot the live sub-agent roster (`/tasks`) — detached spawns
    /// register at launch, `done` flips when TaskDone lands.
    pub fn task_roster(&self) -> Vec<crate::context::TaskEntry> {
        self.ctx.live_tasks.lock_or_recover().clone()
    }

    /// The connected MCP servers (`/mcp`) — name, transport, tool count,
    /// connection liveness. Configured-but-failed servers never reached the
    /// roster: `connect_all` degrades them to a startup warning.
    pub fn mcp_roster(&self) -> Vec<crate::mcp::McpServerStatus> {
        self.ctx
            .mcp_servers
            .iter()
            .map(crate::mcp::McpServerHandle::status)
            .collect()
    }

    /// Session vitals (`/status`) — everything is folded out of the log
    /// or read off ctx, no dedicated bookkeeping: model is the `Started`
    /// label unless a `model.change` hook fact supersedes it (selector wins
    /// resolution, label wins display), provider dialect resolves through
    /// the session's `ModelResolver`, tokens sum the `Usage` facts.
    pub async fn status(&self) -> SessionStatus {
        let events = self
            .ctx
            .sessions
            .lock()
            .await
            .events()
            .await
            .unwrap_or_default();
        let mut model: Option<String> = None;
        let mut selector: Option<String> = None;
        let mut tokens = TokenTotals::default();
        for ev in &events {
            match ev {
                SessionEvent::Started { model: m, .. } => {
                    if model.is_none() {
                        model = Some(m.clone());
                    }
                }
                SessionEvent::Hook { event, detail } if event == "model.change" => {
                    // `record_model_change` writes "selector → label"
                    if let Some((sel, lbl)) = detail.split_once(" → ") {
                        selector = Some(sel.to_string());
                        model = Some(lbl.to_string());
                    } else {
                        model = Some(detail.clone());
                    }
                }
                SessionEvent::Usage { usage } => {
                    tokens.prompt += usage.prompt_tokens;
                    tokens.completion += usage.completion_tokens;
                    tokens.cache_read += usage.cache_read_input_tokens;
                    tokens.cache_write += usage.cache_creation_input_tokens;
                }
                _ => {}
            }
        }
        let provider = selector
            .as_deref()
            .or(model.as_deref())
            .and_then(|sel| self.ctx.models.as_ref().and_then(|m| m.resolve(sel)))
            .map(|t| t.provider.dialect)
            .unwrap_or_else(|| "?".to_string());
        SessionStatus {
            session_id: self.ctx.session_id.read_or_recover().clone(),
            cwd: self.ctx.cwd.clone(),
            model: model.unwrap_or_else(|| "?".to_string()),
            provider,
            approval_mode: self.approval_mode(),
            tokens,
            grants: self.session_grants(),
        }
    }

    /// The model's current task list (`/todos`) — the hot snapshot the
    /// `Todos` events keep durable.
    pub fn todos(&self) -> Vec<crate::tool::TodoItem> {
        self.ctx.todos.lock_or_recover().clone()
    }

    /// The session-scoped approval grants — the `Approval::Session` ledger,
    /// sorted `"tool specifier"` strings (the stored keys are
    /// `"tool\tspecifier"`; the tab is display noise here).
    pub fn session_grants(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .ctx
            .session_grants
            .lock()
            .unwrap()
            .iter()
            .map(|g| g.replace('\t', " "))
            .collect();
        v.sort();
        v
    }

    /// Record a `!` local-shell run as a durable session fact. The event
    /// folds into the message stream as a tagged user message, so the next
    /// turn sees the evidence the user just produced. Takes the turn fence:
    /// folding a user message mid-tool-cycle would tear the transcript.
    pub async fn record_local_shell(&self, command: &str, exit_code: i32, output: &str) {
        let _turn_permit = self.ctx.turn_lock.lock().await;
        let mut log = self.ctx.sessions.lock().await;
        log.append_audit(&SessionEvent::LocalShell {
            command: command.to_string(),
            exit_code,
            output: output.to_string(),
        })
        .await;
    }

    /// Swap the active session log (TUI `/resume`): `log` becomes the fold
    /// source for subsequent turns; returns its events so the frontend can
    /// rebuild the transcript. Fires SessionStart(source=resume) like a
    /// `--resume` startup would.
    pub async fn swap_session(&self, log: SessionLog) -> Vec<SessionEvent> {
        // a log swap mid-turn would orphan the in-flight fold — the fence
        // makes resume queue behind (or abort) a running turn.
        let _turn_permit = self.ctx.turn_lock.lock().await;
        let events = log.events().await.unwrap_or_default();
        // identity follows the log: hooks see the new session id and
        // transcript path, otherwise SessionStart/resume payloads still
        // describe the abandoned session.
        let new_id = log
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "session".into());
        let new_path = log.path().to_path_buf();
        {
            let mut cur = self.ctx.sessions.lock().await;
            *cur = log;
        }
        *self.ctx.session_id.write_or_recover() = new_id.clone();
        self.ctx.hooks.retarget(&new_id, new_path);
        // the new log's task list + approval stance + checkpoint ledger
        // become the live state — resume must not inherit the abandoned
        // session's plan, its mode (a full_access session shouldn't follow
        // the next prompt into a different log), or its snapshot ledger.
        self.ctx.reseed_checkpoints(&events);
        self.ctx.reseed_todos(&events);
        self.ctx.reseed_goal(&events);
        self.ctx.reseed_ptc_store(&events);
        let mode = events
            .iter()
            .rev()
            .find_map(|e| match e {
                SessionEvent::ModeChange { mode } => Some(*mode),
                _ => None,
            })
            .unwrap_or_default();
        *self.ctx.approval_mode.write_or_recover() = mode;
        let _ = self
            .ctx
            .hooks
            .fire(
                HookEvent::SessionStart,
                &self.ctx.cwd,
                &crate::hooks::HookInput {
                    source: Some("resume"),
                    mcp_servers: Some(self.mcp_server_names()),
                    ..Default::default()
                },
            )
            .await;
        events
    }

    /// Path of the active session log — the session's public identity
    /// (`--resume`, `--dataflow`, `--fork` all take it).
    pub async fn session_path(&self) -> std::path::PathBuf {
        self.ctx.sessions.lock().await.path().to_path_buf()
    }

    /// The active session's id — `session_path`'s file stem without the
    /// async hop. `/export-md` and the header bar read this.
    pub fn session_id(&self) -> String {
        self.ctx.session_id.read_or_recover().clone()
    }

    /// Durable events of the active session — frontends replay them to
    /// rebuild the transcript (web `serve` hello, TUI --resume).
    pub async fn session_events(&self) -> Vec<SessionEvent> {
        self.ctx
            .sessions
            .lock()
            .await
            .events()
            .await
            .unwrap_or_default()
    }

    pub fn with_compact_threshold(mut self, n: usize) -> Self {
        self.compact_threshold = n;
        self
    }

    /// Rough token estimate for the current transcript.
    /// Estimated tokens for the *next* request. Trust the provider's own
    /// counter first — the last `Usage` fact's `prompt_tokens` is exact.
    /// The byte heuristic is the fallback for a session that hasn't
    /// reported usage yet (or a dialect that never does), not the
    /// primary source: `serde_json` bytes over-count structure and
    /// under-count CJK by ~2×.
    async fn est_tokens(&self) -> usize {
        let events = self
            .ctx
            .sessions
            .lock()
            .await
            .events()
            .await
            .unwrap_or_default();
        if let Some(u) = events.iter().rev().find_map(|ev| match ev {
            SessionEvent::Usage { usage } => Some(usage.prompt_tokens),
            _ => None,
        }) {
            return u as usize;
        }
        // no usage yet — estimate from the folded messages
        let msgs = self
            .ctx
            .sessions
            .lock()
            .await
            .messages()
            .await
            .unwrap_or_default();
        msgs.iter()
            .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
            .sum::<usize>()
            / 4
    }

    /// The auto-compact tripwire, sized to the model the session is *on*.
    /// `context_length_for` reads the provider catalog; a selector that
    /// resolves nowhere or a provider that doesn't advertise a window falls
    /// back to `compact_threshold`. 85% headroom leaves room for the turn
    /// that tips it over.
    fn effective_threshold(&self) -> usize {
        let window = self
            .ctx
            .active_selector
            .read()
            .unwrap()
            .as_deref()
            .and_then(|sel| {
                self.ctx
                    .models
                    .as_ref()
                    .and_then(|m| m.context_length_for(sel))
            });
        match window {
            Some(w) => ((w * 85 / 100) as usize).max(1),
            None => self.compact_threshold,
        }
    }
}
