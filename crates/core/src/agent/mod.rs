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

use std::sync::Arc;

use crate::context::Context;
use crate::hooks::HookEvent;
use crate::session::{SessionEvent, SessionLog};

mod turn;

#[cfg(test)]
mod tests;

/// Live events the frontend can observe (stdout printer, later TUI/ACP).
#[derive(Debug, Clone)]
pub enum LiveEvent {
    Content(String),
    Reasoning(String),
    /// Tool call began. `summary` is a one-line digest of the interesting
    /// argument (path/command/pattern/…) for frontends to render. `depth`
    /// is the agent's nesting level — 0 for the interactive agent, 1+ for
    /// `Task` sub-agents relayed through `ctx.live_sink`; `lane` tells
    /// parallel siblings apart (each spawn claims its own).
    ToolStart {
        name: String,
        summary: String,
        depth: u8,
        lane: u8,
    },
    /// Tool call finished. `output` carries the raw result so rich frontends
    /// can preview it; simple frontends ignore it.
    ToolDone {
        name: String,
        ok: bool,
        output: String,
        depth: u8,
        lane: u8,
    },
    /// A hook changed the turn — input rewrite, veto, injected context, or a
    /// session-scoped approval grant. Mirrors `SessionEvent::Hook` so the
    /// audit spine is *visible* live, not just durable.
    Hook {
        event: String,
        detail: String,
    },
    /// Token accounting for one completed LLM request — mirrors the durable
    /// `SessionEvent::Usage` so footers can show context pressure live.
    Usage(sunmao_llm::types::Usage),
    TurnEnd {
        outcome: TurnOutcome,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    Completed,
    LengthLimited,
    Other(String),
}

/// Observer sink — the REPL prints these, a GUI would render them.
pub trait Observer: Send + Sync {
    fn on_event(&self, ev: &LiveEvent);
}

/// One-line argument digest for `LiveEvent::ToolStart.summary`: the single
/// most interesting value per tool (the command for Bash, the path for file
/// tools, …), falling back to compact `k=v` pairs for unknown tools.
/// One-line digest of a tool call's interesting argument — the transcript
/// header string. Public so frontends replaying a session log render the
/// same headers a live turn would have produced.
pub fn call_summary(name: &str, args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let preferred: &[&str] = match name {
        "Bash" => &["command"],
        "Read" | "Write" | "Edit" => &["path"],
        "Glob" | "Grep" => &["pattern", "path"],
        "WebFetch" => &["url"],
        "Task" => &["prompt"],
        "JobOutput" => &["id"],
        "HtmlArtifact" => &["name"],
        _ => &[],
    };
    let mut out = String::new();
    for k in preferred {
        if let Some(v) = obj.get(*k).and_then(|v| v.as_str()) {
            out = v.to_string();
            break;
        }
    }
    if out.is_empty() {
        for (k, v) in obj.iter().take(3) {
            let vs = v
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| v.to_string());
            if !out.is_empty() {
                out.push_str("  ");
            }
            out.push_str(k);
            out.push('=');
            out.push_str(&vs);
        }
    }
    if name == "Bash" && obj.get("background").and_then(|v| v.as_bool()) == Some(true) {
        out.push_str("  &");
    }
    ellipsize(&out, 90)
}

/// Flatten whitespace and cap at `max` chars, adding `…` when cut.
fn ellipsize(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut it = flat.chars();
    let kept: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{kept}…")
    } else {
        kept
    }
}

/// Cap tool output carried in `LiveEvent::ToolDone` — frontends only need a
/// preview; the full text already lands in the session log.
pub(crate) fn truncate_output(s: &str) -> String {
    const MAX: usize = 8 * 1024;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…\n[truncated — {} bytes total]", &s[..end], s.len())
}

/// The string declarative rules glob over for a given tool: the command for
/// Bash, the path for file tools, the pattern for search — whatever a rule
/// like `Bash(npm *)` or `Read(./src/**)` is meant to match.
fn specifier_for(tool: &str, args: &serde_json::Value) -> String {
    let key = match tool {
        "Bash" => "command",
        "Read" | "Write" | "Edit" => "path",
        "Glob" | "Grep" => "pattern",
        "WebFetch" => "url",
        "Task" => "prompt",
        _ => "",
    };
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
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
        *self.ctx.llm_override.write().unwrap() = Some(adapter);
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
        let _ = log
            .append(&SessionEvent::Hook {
                event: "model.change".into(),
                detail: format!("{selector} → {label}"),
            })
            .await;
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

    /// Signal cooperative cancellation for the in-flight turn.
    pub fn cancel(&self) {
        self.ctx
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record a `!` local-shell run as a durable session fact. The event
    /// folds into the message stream as a tagged user message, so the next
    /// turn sees the evidence the user just produced.
    pub async fn record_local_shell(&self, command: &str, exit_code: i32, output: &str) {
        let mut log = self.ctx.sessions.lock().await;
        let _ = log
            .append(&SessionEvent::LocalShell {
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
        let events = log.events().await.unwrap_or_default();
        {
            let mut cur = self.ctx.sessions.lock().await;
            *cur = log;
        }
        let _ = self
            .ctx
            .hooks
            .fire(
                HookEvent::SessionStart,
                &self.ctx.cwd,
                &crate::hooks::HookInput {
                    source: Some("resume"),
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

    pub fn with_compact_threshold(mut self, n: usize) -> Self {
        self.compact_threshold = n;
        self
    }

    /// Rough token estimate for the current transcript.
    async fn est_tokens(&self) -> usize {
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
}
