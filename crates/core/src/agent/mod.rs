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
mod effort;
pub(crate) mod fusion;
mod gate;
mod goal;
mod mcp;
pub mod mode;
mod steer;
mod turn;
mod turn_mode;

pub use mode::ApprovalMode;
pub use turn_mode::TurnMode;

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
    /// PTC/codemode: the full contract loop, but the model's tool surface
    /// is `RunCode` + `SearchTools` — borrowed tools: find a schema by
    /// query, then call it from a script. Every other tool stays
    /// registered and callable only through the script's `tools.*` bridge
    /// (each nested call still takes the gate/hook pipeline). See
    /// `assets/prompt/ptc-driver.md`.
    Ptc,
}

impl LoopDriver {
    /// Parse a manifest/flag spelling. Unknown names are refused loudly —
    /// silently falling back to Full would run a stricter session than
    /// the operator asked for without telling them.
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "full" | "default" => Ok(Self::Full),
            "bare" | "minimal" => Ok(Self::Bare),
            "ptc" | "codemode" | "code-mode" => Ok(Self::Ptc),
            other => {
                anyhow::bail!("unknown loop driver {other:?} (known: full, bare, ptc)")
            }
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Bare => "bare",
            Self::Ptc => "ptc",
        }
    }

    /// Resolve the driver a set of plugin roots declares. Scan order is the
    /// layering order — project manifest first, installed bundles, then
    /// preset roots last so a `--preset` picks the loop. First `"loop"` key
    /// found wins the slot at its layer; the LAST layer's declaration wins
    /// overall (same precedence every preset seam follows).
    pub fn resolve(cwd: &std::path::Path, extra_roots: &[std::path::PathBuf]) -> Self {
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
        // project-layer manifests must pin their `loop:` claim — a checked
        // out repo could otherwise write `loop: bare` and disarm the whole
        // permission gate (the same trust ledger hook commands and `allow`
        // rules pass). Preset roots outside the cwd are user-invoked.
        let ccwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
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
            let claim = format!("loop:{name}");
            let project_layer = manifest
                .canonicalize()
                .unwrap_or_else(|_| manifest.clone())
                .starts_with(&ccwd);
            if project_layer
                && !crate::hooks::trust::is_trusted(
                    cwd,
                    crate::hooks::trust::Layer::Project,
                    &manifest,
                    &claim,
                )
            {
                tracing::warn!(
                    "{}: unpinned `loop:` declaration skipped — /hooks trust to enable",
                    manifest.display()
                );
                continue;
            }
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
        let agent = Self {
            ctx,
            max_iterations: 64,
            compact_threshold: 180_000,
        };
        if let Some(problem) = agent.reconcile_fusion_mode() {
            tracing::warn!("resumed Fusion mode disabled: {problem}");
        }
        agent
    }

    pub fn with_max_iterations(mut self, n: usize) -> Self {
        self.max_iterations = n;
        self
    }

    /// Install the live-event sink `Task` sub-agents relay their tool
    /// lifecycle through. First install wins — frontends call once at setup.
    /// The hook engine gets the same sink so trust-skip audits surface live.
    pub fn set_live_sink(&self, sink: Arc<dyn Observer>) {
        self.ctx.hooks.set_live(sink.clone());
        let _ = self.ctx.live_sink.set(sink);
    }

    /// `/hooks trust|untrust <n>` — pin or revoke a command in
    /// `.sunmao/trusted-hooks.json` and record the decision as a durable
    /// audit fact (who flipped what is as reconstructible as the skip).
    pub async fn set_hook_trust(&self, index: usize, trust_it: bool) -> Result<String, String> {
        let detail = self.ctx.hooks.set_row_trust(index, trust_it)?;
        {
            let mut log = self.ctx.sessions.lock().await;
            log.append_audit(&SessionEvent::Hook {
                event: if trust_it {
                    "hook.trust".into()
                } else {
                    "hook.untrust".into()
                },
                detail: detail.clone(),
            })
            .await;
        }
        Ok(detail)
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

    /// Reload the models file and reconcile Fusion selectors.
    /// The file read doesn't need the turn fence — a GET /models during a
    /// long turn used to stall behind it for the whole turn, when only the
    /// *reconcile* must serialize against turn boundaries. Reconcile rides
    /// a spawned task: the file view answers immediately while the fusion
    /// check still lands at a real fence.
    pub async fn reload_models(&self) {
        if let Some(models) = self.ctx.models.as_ref() {
            models.reload();
        }
        let agent = self.clone();
        tokio::spawn(async move {
            let _turn_permit = agent.ctx.turn_lock.lock().await;
            if let Some(problem) = agent.reconcile_fusion_mode() {
                tracing::warn!("Fusion mode disabled after model reload: {problem}");
            }
        });
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

    /// The shared Context — frontends need `shell`/`cancel_signal` to run
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
        self.ctx.reseed_effort(&events);
        self.ctx.reseed_turn_mode(&events);
        self.ctx.reseed_fusion_models(&events);
        if let Some(problem) = self.reconcile_fusion_mode() {
            tracing::warn!("Fusion mode disabled after session swap: {problem}");
        }
        // SESSION facts that have no event to reseed from must still not
        // leak across the swap: the abandoned session's approvals, its
        // read-before-write ledger, its model pick, and any un-drained
        // steer backlog belong to the log they were made in.
        self.ctx.session_grants.lock_or_recover().clear();
        self.ctx.read_paths.lock_or_recover().clear();
        self.ctx.steer.lock_or_recover().clear();
        self.ctx
            .input_pending
            .store(0, std::sync::atomic::Ordering::Relaxed);
        // the new log's own model wins too — Started carries the
        // creation-time selector, `model.change` rows override it
        self.ctx.reseed_model(&events);
        let mode = events
            .iter()
            .rev()
            .find_map(|e| match e {
                SessionEvent::ModeChange { mode } => Some(*mode),
                _ => None,
            })
            .unwrap_or_default();
        *self.ctx.approval_mode.write_or_recover() = mode;
        self.ctx.fire_session_start("resume").await;
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
}
