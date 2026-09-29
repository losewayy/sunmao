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

use futures_util::StreamExt;
use sunmao_llm::assemble::ToolCallAssembler;
use sunmao_llm::types::Message;
use sunmao_llm::{ChatRequest, StreamDelta};

use crate::context::Context;
use crate::hooks::HookEvent;
use crate::session::{SessionEvent, SessionLog};

/// Live events the frontend can observe (stdout printer, later TUI/ACP).
#[derive(Debug, Clone)]
pub enum LiveEvent {
    Content(String),
    Reasoning(String),
    /// Tool call began. `summary` is a one-line digest of the interesting
    /// argument (path/command/pattern/…) for frontends to render. `depth`
    /// is the agent's nesting level — 0 for the interactive agent, 1+ for
    /// `Task` sub-agents relayed through `ctx.live_sink`.
    ToolStart {
        name: String,
        summary: String,
        depth: u8,
    },
    /// Tool call finished. `output` carries the raw result so rich frontends
    /// can preview it; simple frontends ignore it.
    ToolDone {
        name: String,
        ok: bool,
        output: String,
        depth: u8,
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
fn truncate_output(s: &str) -> String {
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

    /// The dispatch gate — declarative rules, then the hook's
    /// `permissionDecision`, session grants, then the risky-pattern
    /// classifier as the default prompt. `deny` rules are a hard refusal
    /// nothing overrides — a session grant never bypasses them.
    async fn gate_call(
        &self,
        tool: &str,
        specifier: &str,
        hook: Option<crate::hooks::HookPermission>,
        observer: &dyn Observer,
    ) -> Result<(), String> {
        use crate::hooks::HookPermission as H;
        use crate::permissions::Verdict;
        match self.ctx.permissions.check(tool, specifier) {
            Verdict::Deny => return Err("denied by permission rules".into()),
            Verdict::PreApproved => return Ok(()),
            Verdict::Ask | Verdict::Default => {}
        }
        if let Some(H::Deny) = hook {
            return Err("denied by hook".into());
        }
        // Session grants sit after both deny gates but before every ask: a
        // grant is a standing answer to a prompt, not an override of a veto.
        if self.ctx.session_granted(tool, specifier) {
            return Ok(());
        }
        if self.ctx.permissions.check(tool, specifier) == Verdict::Ask {
            return self
                .ask(tool, specifier, "matched ask rule", observer)
                .await;
        }
        if let Some(H::Ask) = hook {
            return self
                .ask(tool, specifier, "hook requested approval", observer)
                .await;
        }
        if let Some(H::Allow) = hook {
            return Ok(());
        }
        // default: the risky-pattern classifier (Bash-shaped patterns today)
        if let Some(why) = crate::approval::classify(specifier) {
            return self.ask(tool, specifier, why, observer).await;
        }
        Ok(())
    }

    /// One approval prompt → verdict. `Session` is recorded in
    /// `session_grants` and audited as a durable `Hook` fact.
    async fn ask(
        &self,
        tool: &str,
        specifier: &str,
        why: &str,
        observer: &dyn Observer,
    ) -> Result<(), String> {
        match self.ctx.approval.approve(tool, specifier, why).await {
            crate::approval::Approval::Session => {
                self.ctx.grant_session(tool, specifier);
                let detail = format!("{tool}: {specifier}");
                {
                    let mut log = self.ctx.sessions.lock().await;
                    let _ = log
                        .append(&crate::SessionEvent::Hook {
                            event: "approval.session".into(),
                            detail: detail.clone(),
                        })
                        .await;
                }
                observer.on_event(&LiveEvent::Hook {
                    event: "approval.session".into(),
                    detail,
                });
                Ok(())
            }
            crate::approval::Approval::Once => Ok(()),
            crate::approval::Approval::Deny => Err(format!("denied at approval gate ({why})")),
        }
    }

    /// Ask the model to summarize the transcript, then commit a `Compacted`
    /// boundary — the log fold turns it into a fresh system message.
    /// Returns the summary so frontends can show what the fold produced.
    pub async fn compact(&self, observer: &dyn Observer, trigger: &str) -> anyhow::Result<String> {
        // PreCompact may veto or annotate the compaction (the dialect's
        // snapshot hook point — context-mode hangs its state capture here).
        let pre = self
            .ctx
            .hooks
            .fire(
                HookEvent::PreCompact,
                &self.ctx.cwd,
                &crate::hooks::HookInput {
                    source: Some(trigger),
                    ..Default::default()
                },
            )
            .await;
        if let Some(reason) = pre.block_reason {
            anyhow::bail!("compaction blocked by hook: {reason}");
        }
        let mut msgs = self.ctx.sessions.lock().await.messages().await?;
        if msgs.is_empty() {
            return Ok(String::new());
        }
        msgs.push(Message::user(
            "Summarize this conversation so far for context compaction: key decisions,              files touched, current state, and what remains. Be terse and factual.",
        ));
        let req = ChatRequest {
            messages: &msgs,
            tools: None,
            max_tokens: Some(2048),
            temperature: None,
        };
        let mut stream = self.ctx.llm.stream(req).await?;
        let mut summary = String::new();
        while let Some(d) = stream.next().await {
            if let StreamDelta::Content(c) = d? {
                summary.push_str(&c);
            }
        }
        if summary.trim().is_empty() {
            anyhow::bail!("compaction produced empty summary");
        }
        self.ctx
            .sessions
            .lock()
            .await
            .append(&SessionEvent::Compacted {
                summary: summary.clone(),
            })
            .await?;
        let _ = self
            .ctx
            .hooks
            .fire(
                HookEvent::PostCompact,
                &self.ctx.cwd,
                &crate::hooks::HookInput {
                    source: Some(trigger),
                    ..Default::default()
                },
            )
            .await;
        observer.on_event(&LiveEvent::ToolDone {
            name: "compact".into(),
            ok: true,
            output: summary.clone(),
            depth: self.ctx.depth,
        });
        Ok(summary)
    }

    /// Run one turn: `input` is the user's message; returns when the model
    /// stops calling tools or we hit the iteration ceiling.
    ///
    /// Frontends depend on TurnEnd to unwind their "working" state — so
    /// even an Err path emits one (`Other("<error>")`) before propagating.
    /// The error string doubles as the outcome detail; the observer's
    /// transcript shows why the turn died instead of hanging.
    pub async fn run_turn(
        &self,
        input: &str,
        observer: &dyn Observer,
    ) -> anyhow::Result<TurnOutcome> {
        match self.run_turn_inner(input, observer).await {
            Ok(o) => Ok(o),
            Err(e) => {
                observer.on_event(&LiveEvent::TurnEnd {
                    outcome: TurnOutcome::Other(format!("error: {e:#}")),
                });
                Err(e)
            }
        }
    }

    async fn run_turn_inner(
        &self,
        input: &str,
        observer: &dyn Observer,
    ) -> anyhow::Result<TurnOutcome> {
        // NOTE: cancelled flag is cleared at turn END, not start — a cancel
        // issued before the turn must still take effect; a mid-turn cancel
        // is consumed here and the next turn starts clean.
        // UserPromptSubmit hooks may inject context or veto the prompt.
        let prompt_outcome = self
            .ctx
            .hooks
            .fire(
                HookEvent::UserPromptSubmit,
                &self.ctx.cwd,
                &crate::hooks::HookInput {
                    prompt: Some(input),
                    ..Default::default()
                },
            )
            .await;
        if let Some(reason) = prompt_outcome.block_reason {
            observer.on_event(&LiveEvent::Hook {
                event: "UserPromptSubmit veto".into(),
                detail: reason.clone(),
            });
            return Ok(TurnOutcome::Other(format!("blocked by hook: {reason}")));
        }
        {
            let mut log = self.ctx.sessions.lock().await;
            log.append(&SessionEvent::Message {
                message: Message::user(input),
            })
            .await?;
            for extra in prompt_outcome.extra_context {
                observer.on_event(&LiveEvent::Hook {
                    event: "hook injected context".into(),
                    detail: extra.clone(),
                });
                log.append(&SessionEvent::Message {
                    message: Message::user(format!("[hook context] {extra}")),
                })
                .await?;
            }
        }

        let mut outcome = TurnOutcome::Completed;
        for _ in 0..self.max_iterations {
            if self
                .ctx
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                outcome = TurnOutcome::Other("cancelled".into());
                break;
            }
            if self.est_tokens().await > self.compact_threshold {
                observer.on_event(&LiveEvent::ToolStart {
                    name: "compact".into(),
                    summary: String::new(),
                    depth: self.ctx.depth,
                });
                if let Err(e) = self.compact(observer, "auto").await {
                    observer.on_event(&LiveEvent::ToolDone {
                        name: format!("compact failed: {e:#}"),
                        ok: false,
                        output: String::new(),
                        depth: self.ctx.depth,
                    });
                }
            }

            let messages = self.ctx.sessions.lock().await.messages().await?;
            let decls = self.ctx.tools.declarations();
            let req = ChatRequest {
                messages: &messages,
                tools: Some(&decls),
                max_tokens: None,
                temperature: None,
            };

            let mut stream = self.ctx.llm.stream(req).await?;

            let mut content = String::new();
            let mut reasoning = String::new();
            let mut assembler = ToolCallAssembler::new();
            let mut finish_reason: Option<String> = None;

            while let Some(delta) = stream.next().await {
                match delta? {
                    StreamDelta::Content(c) => {
                        observer.on_event(&LiveEvent::Content(c.clone()));
                        content.push_str(&c);
                    }
                    StreamDelta::Reasoning(r) => {
                        observer.on_event(&LiveEvent::Reasoning(r.clone()));
                        reasoning.push_str(&r);
                    }
                    StreamDelta::ToolCalls(frags) => {
                        for f in &frags {
                            assembler.push(f);
                        }
                    }
                    StreamDelta::Finish { reason, usage } => {
                        if let Some(u) = &usage {
                            let mut log = self.ctx.sessions.lock().await;
                            let _ = log.append(&SessionEvent::Usage { usage: u.clone() }).await;
                            drop(log);
                            observer.on_event(&LiveEvent::Usage(u.clone()));
                        }
                        finish_reason = reason.or(finish_reason);
                    }
                }
            }

            let (tool_calls, malformed) = assembler.finish_lenient();
            let malformed: std::collections::HashMap<String, String> =
                malformed.into_iter().collect();

            {
                let mut log = self.ctx.sessions.lock().await;
                log.append(&SessionEvent::Message {
                    message: Message::assistant(
                        (!content.is_empty()).then_some(content.clone()),
                        tool_calls.clone(),
                    ),
                })
                .await?;
            }

            if tool_calls.is_empty() {
                outcome = match finish_reason.as_deref() {
                    Some("length") => TurnOutcome::LengthLimited,
                    Some("stop") | Some("end_turn") | None => TurnOutcome::Completed,
                    Some(other) => TurnOutcome::Other(other.to_string()),
                };
                break;
            }

            for call in tool_calls {
                // malformed JSON args → failed result fed back, no dispatch
                if let Some(err) = malformed.get(&call.id) {
                    let result = crate::tool::ToolResult {
                        output: format!("malformed tool call: {err}"),
                        ok: false,
                    };
                    let mut log = self.ctx.sessions.lock().await;
                    log.append(&SessionEvent::ToolCall {
                        call: call.clone(),
                        depth: self.ctx.depth,
                    })
                    .await?;
                    log.append(&SessionEvent::ToolResult {
                        call_id: call.id.clone(),
                        name: call.function.name.clone(),
                        ok: result.ok,
                        output: result.output.clone(),
                        depth: self.ctx.depth,
                    })
                    .await?;
                    log.append(&SessionEvent::Message {
                        message: Message::tool_result(call.id.clone(), result.output),
                    })
                    .await?;
                    continue;
                }
                let mut args_value: serde_json::Value =
                    serde_json::from_str(&call.function.arguments)
                        .unwrap_or(serde_json::Value::Null);

                // PreToolUse: a hook may veto the call outright, rewrite its
                // input (updatedInput — rtk's transparent command rewrite),
                // or hand the gate a permissionDecision verdict.
                let pre = self
                    .ctx
                    .hooks
                    .fire(
                        HookEvent::PreToolUse,
                        &self.ctx.cwd,
                        &crate::hooks::HookInput {
                            tool_name: Some(&call.function.name),
                            tool_use_id: Some(&call.id),
                            tool_input: Some(&args_value),
                            ..Default::default()
                        },
                    )
                    .await;

                // Apply the rewrite before logging ToolCall: the log records
                // what actually ran; the rewrite itself is a durable Hook fact
                // — and a live one: audit must be visible, not just durable.
                if let Some(updated) = pre.updated_input {
                    let detail = format!(
                        "{}: {} → {}",
                        call.function.name, call.function.arguments, updated
                    );
                    {
                        let mut log = self.ctx.sessions.lock().await;
                        let _ = log
                            .append(&SessionEvent::Hook {
                                event: "PreToolUse.updatedInput".into(),
                                detail: detail.clone(),
                            })
                            .await;
                    }
                    observer.on_event(&LiveEvent::Hook {
                        event: "hook rewrite".into(),
                        detail,
                    });
                    args_value = updated;
                }

                {
                    let mut log = self.ctx.sessions.lock().await;
                    let mut call = call.clone();
                    call.function.arguments = args_value.to_string();
                    log.append(&SessionEvent::ToolCall {
                        call,
                        depth: self.ctx.depth,
                    })
                    .await?;
                }

                observer.on_event(&LiveEvent::ToolStart {
                    name: call.function.name.clone(),
                    summary: call_summary(&call.function.name, &args_value),
                    depth: self.ctx.depth,
                });

                let result = if let Some(reason) = pre.block_reason {
                    crate::tool::ToolResult {
                        output: format!("blocked by hook: {reason}"),
                        ok: false,
                    }
                } else {
                    // Dispatch gate: declarative rules first (deny is a hard
                    // refusal), then the hook's permissionDecision, then the
                    // risky-pattern classifier as the default prompt.
                    let specifier = specifier_for(&call.function.name, &args_value);
                    match self
                        .gate_call(
                            &call.function.name,
                            &specifier,
                            pre.permission_decision,
                            observer,
                        )
                        .await
                    {
                        Ok(()) => {
                            self.ctx
                                .tools
                                .call(&call.function.name, &args_value.to_string(), &self.ctx)
                                .await
                        }
                        Err(denial) => crate::tool::ToolResult {
                            output: denial,
                            ok: false,
                        },
                    }
                };
                observer.on_event(&LiveEvent::ToolDone {
                    name: call.function.name.clone(),
                    ok: result.ok,
                    output: truncate_output(&result.output),
                    depth: self.ctx.depth,
                });

                // PostToolUse: hooks may inject context for the next turn.
                let post = self
                    .ctx
                    .hooks
                    .fire(
                        HookEvent::PostToolUse,
                        &self.ctx.cwd,
                        &crate::hooks::HookInput {
                            tool_name: Some(&call.function.name),
                            tool_use_id: Some(&call.id),
                            tool_input: Some(&args_value),
                            tool_response: Some(&result.output),
                            ..Default::default()
                        },
                    )
                    .await;

                let mut log = self.ctx.sessions.lock().await;
                log.append(&SessionEvent::ToolResult {
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    ok: result.ok,
                    output: result.output.clone(),
                    depth: self.ctx.depth,
                })
                .await?;
                for extra in post.extra_context {
                    log.append(&SessionEvent::Message {
                        message: Message::user(format!("[hook context] {extra}")),
                    })
                    .await?;
                }
            }
        }
        self.ctx
            .cancelled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let _ = self
            .ctx
            .hooks
            .fire(
                HookEvent::Stop,
                &self.ctx.cwd,
                &crate::hooks::HookInput::default(),
            )
            .await;
        observer.on_event(&LiveEvent::TurnEnd {
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionLog;
    use crate::tool::builtin_registry;
    use futures_util::stream;
    use sunmao_llm::types::Usage;
    use sunmao_llm::{ChatRequest, DeltaStream, ProviderAdapter, StreamDelta, ToolCallFragment};

    /// Scripted provider: each queued response is a Vec of deltas replayed in
    /// order. The seam being a trait is what makes the whole loop testable.
    struct MockProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<Vec<StreamDelta>>>,
        /// how many times stream() was invoked
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for MockProvider {
        async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let deltas = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    vec![
                        StreamDelta::Content("done".into()),
                        StreamDelta::Finish {
                            reason: Some("stop".into()),
                            usage: Some(Usage::default()),
                        },
                    ]
                });
            Ok(Box::pin(stream::iter(deltas.into_iter().map(Ok))))
        }
    }

    struct NullObserver;
    impl Observer for NullObserver {
        fn on_event(&self, _ev: &LiveEvent) {}
    }

    /// Records every LiveEvent — used to assert TurnEnd fires on the error
    /// path (frontends unwind busy/spinner state from it; a missing TurnEnd
    /// leaves the TUI stuck).
    struct RecObserver(std::sync::Mutex<Vec<String>>);
    impl Observer for RecObserver {
        fn on_event(&self, ev: &LiveEvent) {
            let tag = match ev {
                LiveEvent::TurnEnd { outcome } => format!("TurnEnd:{outcome:?}"),
                LiveEvent::Content(_) => "Content".into(),
                LiveEvent::Reasoning(_) => "Reasoning".into(),
                LiveEvent::ToolStart { .. } => "ToolStart".into(),
                LiveEvent::ToolDone { .. } => "ToolDone".into(),
                LiveEvent::Hook { .. } => "Hook".into(),
                LiveEvent::Usage(_) => "Usage".into(),
            };
            self.0.lock().unwrap().push(tag);
        }
    }

    #[tokio::test]
    async fn error_path_still_emits_turn_end() {
        // provider that always fails to establish the stream
        struct FailProvider;
        #[async_trait::async_trait]
        impl ProviderAdapter for FailProvider {
            async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
                anyhow::bail!("provider down")
            }
        }
        let ctx = Arc::new(Context::new(
            Arc::new(FailProvider),
            SessionLog::ephemeral(),
            builtin_registry(),
            std::env::temp_dir(),
        ));
        let agent = AgentLoop::new(ctx);
        let rec = RecObserver(std::sync::Mutex::new(Vec::new()));
        let res = agent.run_turn("hi", &rec).await;
        assert!(res.is_err(), "stream failure must propagate");
        let events = rec.0.lock().unwrap();
        assert!(
            events.iter().any(|t| t.starts_with("TurnEnd:Other")),
            "frontends need TurnEnd even on error — got {events:?}"
        );
    }

    #[tokio::test]
    async fn turn_completes_on_plain_text() {
        let provider = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = Arc::new(Context::new(
            provider.clone(),
            SessionLog::ephemeral(),
            builtin_registry(),
            std::env::temp_dir(),
        ));
        let agent = AgentLoop::new(ctx.clone());
        let outcome = agent.run_turn("hi", &NullObserver).await.unwrap();
        assert!(matches!(outcome, TurnOutcome::Completed));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        // log holds user + assistant messages
        let msgs = ctx.sessions.lock().await.messages().await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, sunmao_llm::types::Role::Assistant);
    }

    #[tokio::test]
    async fn tool_call_roundtrip_feeds_back() {
        // first stream: a Glob tool call (real filesystem tool), then finish
        // second stream: plain text → turn completes
        let provider = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                vec![
                    StreamDelta::ToolCalls(vec![
                        ToolCallFragment {
                            index: 0,
                            id: Some("call_1".into()),
                            name: Some("Glob".into()),
                            arguments: None,
                        },
                        ToolCallFragment {
                            index: 0,
                            arguments: Some("{\"pattern\":\"**/*.rs\"}".into()),
                            ..Default::default()
                        },
                    ]),
                    StreamDelta::Finish {
                        reason: Some("tool_calls".into()),
                        usage: None,
                    },
                ],
                vec![
                    StreamDelta::Content("I found the files".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
            ])),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = Arc::new(Context::new(
            provider.clone(),
            SessionLog::ephemeral(),
            builtin_registry(),
            std::env::temp_dir(),
        ));
        let agent = AgentLoop::new(ctx.clone());
        let outcome = agent.run_turn("list files", &NullObserver).await.unwrap();
        assert!(matches!(outcome, TurnOutcome::Completed));
        // two provider calls: original turn + post-tool continuation
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        // events: user msg, assistant msg (w/ tool_calls), tool_call fact,
        // tool_result, assistant text
        let msgs = ctx.sessions.lock().await.messages().await.unwrap();
        assert!(msgs
            .iter()
            .any(|m| matches!(m.role, sunmao_llm::types::Role::Tool)));
    }

    #[tokio::test]
    async fn malformed_tool_args_become_failed_result() {
        // provider emits a Glob call with broken JSON args, then a text reply —
        // turn must complete and the bad call must surface as a ToolResult fail,
        // not an abort.
        let provider = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                vec![
                    StreamDelta::ToolCalls(vec![
                        ToolCallFragment {
                            index: 0,
                            id: Some("bad1".into()),
                            name: Some("Glob".into()),
                            arguments: None,
                        },
                        ToolCallFragment {
                            index: 0,
                            arguments: Some("{not json".into()),
                            ..Default::default()
                        },
                    ]),
                    StreamDelta::Finish {
                        reason: Some("tool_calls".into()),
                        usage: None,
                    },
                ],
                vec![
                    StreamDelta::Content("sorry, retrying".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
            ])),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = Arc::new(Context::new(
            provider.clone(),
            SessionLog::ephemeral(),
            builtin_registry(),
            std::env::temp_dir(),
        ));
        let agent = AgentLoop::new(ctx.clone());
        let outcome = agent.run_turn("list", &NullObserver).await.unwrap();
        assert!(matches!(outcome, TurnOutcome::Completed));
        // two provider calls: the model got the failure fed back
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        let msgs = ctx.sessions.lock().await.messages().await.unwrap();
        // tool message present containing the malformed-call error
        let tool_msg = msgs
            .iter()
            .find(|m| matches!(m.role, sunmao_llm::types::Role::Tool))
            .expect("tool result message");
        assert!(tool_msg.content.as_deref().unwrap().contains("malformed"));
    }

    #[tokio::test]
    async fn cancel_flag_breaks_loop() {
        // provider would return tool_calls forever; cancel must interrupt
        let mut responses = std::collections::VecDeque::new();
        for _ in 0..10 {
            responses.push_back(vec![
                StreamDelta::ToolCalls(vec![ToolCallFragment {
                    index: 0,
                    id: Some("c".into()),
                    name: Some("Glob".into()),
                    arguments: Some("{\"pattern\":\"*\"}".into()),
                }]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ]);
        }
        let provider = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(responses),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = Arc::new(Context::new(
            provider.clone(),
            SessionLog::ephemeral(),
            builtin_registry(),
            std::env::temp_dir(),
        ));
        ctx.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let agent = AgentLoop::new(ctx);
        let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
        assert!(matches!(outcome, TurnOutcome::Other(ref s) if s == "cancelled"));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    /// The rtk contract end-to-end through the real loop: PreToolUse hook
    /// returns `hookSpecificOutput.updatedInput` → the tool executes the
    /// REWRITTEN command, the log records a Hook fact + the effective
    /// ToolCall, and the model sees the result of the rewritten command.
    #[tokio::test]
    async fn pretooluse_updated_input_rewrites_dispatch() {
        let dir = std::env::temp_dir().join(format!("sunmao-rtk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
        // the rtk hook shape: JSON on stdin, updatedInput on stdout
        // write the hook's stdout JSON to a file the hook cats — avoids
        // quoting an entire JSON doc inside a shell command string
        std::fs::write(
            dir.join("hook-response.json"),
            r#"{"hookSpecificOutput":{"updatedInput":{"command":"echo rewritten"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".sunmao/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"cat hook-response.json"}]}]}}"#,
        )
        .unwrap();
        let provider = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                vec![
                    StreamDelta::ToolCalls(vec![
                        ToolCallFragment {
                            index: 0,
                            id: Some("call_1".into()),
                            name: Some("Bash".into()),
                            arguments: None,
                        },
                        ToolCallFragment {
                            index: 0,
                            arguments: Some("{\"command\":\"git status\"}".into()),
                            ..Default::default()
                        },
                    ]),
                    StreamDelta::Finish {
                        reason: Some("tool_calls".into()),
                        usage: None,
                    },
                ],
                vec![
                    StreamDelta::Content("done".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
            ])),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = Arc::new(Context::new(
            provider,
            SessionLog::ephemeral(),
            builtin_registry(),
            dir.clone(),
        ));
        let agent = AgentLoop::new(ctx.clone());
        let outcome = agent.run_turn("run it", &NullObserver).await.unwrap();
        assert!(matches!(outcome, TurnOutcome::Completed));
        // the tool result must be the REWRITTEN command's output
        let msgs = ctx.sessions.lock().await.messages().await.unwrap();
        let tool_msg = msgs
            .iter()
            .find(|m| matches!(m.role, sunmao_llm::types::Role::Tool))
            .expect("tool result message");
        assert_eq!(
            tool_msg.content.as_deref().map(str::trim_end),
            Some("rewritten")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A PreToolUse `permissionDecision:"deny"` (the newer dialect spelling)
    /// must block the call before dispatch — same as exit-2 veto.
    #[tokio::test]
    async fn pretooluse_permission_deny_blocks() {
        let dir = std::env::temp_dir().join(format!("sunmao-deny-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
        std::fs::write(
            dir.join("hook-response.json"),
            r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"policy"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".sunmao/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash|Read|Write","hooks":[{"type":"command","command":"cat hook-response.json"}]}]}}"#,
        )
        .unwrap();
        let provider = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                vec![
                    StreamDelta::ToolCalls(vec![ToolCallFragment {
                        index: 0,
                        id: Some("c".into()),
                        name: Some("Bash".into()),
                        arguments: Some("{\"command\":\"echo hi\"}".into()),
                    }]),
                    StreamDelta::Finish {
                        reason: Some("tool_calls".into()),
                        usage: None,
                    },
                ],
                vec![
                    StreamDelta::Content("ok".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
            ])),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = Arc::new(Context::new(
            provider,
            SessionLog::ephemeral(),
            builtin_registry(),
            dir.clone(),
        ));
        let agent = AgentLoop::new(ctx.clone());
        agent.run_turn("go", &NullObserver).await.unwrap();
        let msgs = ctx.sessions.lock().await.messages().await.unwrap();
        let tool_msg = msgs
            .iter()
            .find(|m| matches!(m.role, sunmao_llm::types::Role::Tool))
            .expect("tool result message");
        assert!(tool_msg.content.as_deref().unwrap().contains("policy"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Session grant: the first ask-rule hit prompts; a Session verdict is
    /// recorded and the *identical* call passes without prompting again.
    #[tokio::test]
    async fn session_grant_skips_repeated_prompt() {
        use crate::approval::{Approval, Approver};

        struct SessionOnce(std::sync::atomic::AtomicUsize);
        #[async_trait::async_trait]
        impl Approver for SessionOnce {
            async fn approve(&self, _t: &str, _d: &str, _w: &str) -> Approval {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Approval::Session
            }
        }

        let dir = std::env::temp_dir().join(format!("sunmao-grant-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
        // an ask rule forces the approval path for this exact Glob pattern
        std::fs::write(
            dir.join(".sunmao/permissions.json"),
            r#"{"permissions":{"ask":["Glob(**/*.rs)"]}}"#,
        )
        .unwrap();

        let glob_call = || {
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("c".into()),
                        name: Some("Glob".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some("{\"pattern\":\"**/*.rs\"}".into()),
                        ..Default::default()
                    },
                ]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ]
        };
        let provider = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                glob_call(),
                glob_call(), // identical second call — grant should cover it
                vec![
                    StreamDelta::Content("done".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
            ])),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let approver = Arc::new(SessionOnce(std::sync::atomic::AtomicUsize::new(0)));
        let mut ctx_raw = Context::new(
            provider,
            SessionLog::ephemeral(),
            builtin_registry(),
            dir.clone(),
        );
        ctx_raw.approval = approver.clone();
        let ctx = Arc::new(ctx_raw);
        let agent = AgentLoop::new(ctx.clone());
        let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
        assert!(matches!(outcome, TurnOutcome::Completed));
        // the second identical call never reached the approver
        assert_eq!(approver.0.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(ctx.session_granted("Glob", "**/*.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn subagent_tool_events_reach_live_sink_at_depth() {
        // The turn observer sees the parent's `Task` call itself; the
        // sub-agent's *inner* tool lifecycle must reach the frontend through
        // `live_sink` tagged depth=1 — and its TurnEnd must NOT leak
        // (forwarding it would unwind the outer turn's busy state).
        struct DepthRec(std::sync::Mutex<Vec<String>>);
        impl Observer for DepthRec {
            fn on_event(&self, ev: &LiveEvent) {
                match ev {
                    LiveEvent::ToolStart { name, depth, .. } => self
                        .0
                        .lock()
                        .unwrap()
                        .push(format!("start:{name}:d{depth}")),
                    LiveEvent::ToolDone {
                        name, ok, depth, ..
                    } => self
                        .0
                        .lock()
                        .unwrap()
                        .push(format!("done:{name}:{ok}:d{depth}")),
                    LiveEvent::TurnEnd { outcome } => {
                        self.0.lock().unwrap().push(format!("turnend:{outcome:?}"))
                    }
                    _ => {}
                }
            }
        }

        let glob_call = || {
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("g".into()),
                        name: Some("Glob".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some("{\"pattern\":\"**/*.rs\"}".into()),
                        ..Default::default()
                    },
                ]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ]
        };
        let provider = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                // parent turn: calls Task
                vec![
                    StreamDelta::ToolCalls(vec![
                        ToolCallFragment {
                            index: 0,
                            id: Some("t".into()),
                            name: Some("Task".into()),
                            arguments: None,
                        },
                        ToolCallFragment {
                            index: 0,
                            arguments: Some("{\"prompt\":\"find files\"}".into()),
                            ..Default::default()
                        },
                    ]),
                    StreamDelta::Finish {
                        reason: Some("tool_calls".into()),
                        usage: None,
                    },
                ],
                // sub-agent turn: calls Glob (shared provider — same queue)
                glob_call(),
                // sub-agent post-tool: text reply
                vec![
                    StreamDelta::Content("sub found them".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
                // parent post-tool: text reply
                vec![
                    StreamDelta::Content("all done".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
            ])),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let dir = std::env::temp_dir().join(format!("sunmao-depth-{}", std::process::id()));
        let ctx = Arc::new(Context::new(
            provider,
            SessionLog::ephemeral(),
            builtin_registry(),
            dir.clone(),
        ));
        let sink = Arc::new(DepthRec(std::sync::Mutex::new(Vec::new())));
        let agent = AgentLoop::new(ctx.clone());
        agent.set_live_sink(sink.clone() as Arc<dyn Observer>);
        let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
        assert!(matches!(outcome, TurnOutcome::Completed));

        let events = sink.0.lock().unwrap();
        assert!(
            events.iter().any(|e| e == "start:Glob:d1"),
            "inner tool start must surface at depth=1 — got {events:?}"
        );
        assert!(
            events.iter().any(|e| e == "done:Glob:true:d1"),
            "inner tool done must surface at depth=1 — got {events:?}"
        );
        assert!(
            !events.iter().any(|e| e.starts_with("turnend:")),
            "sub-agent TurnEnd must never reach the sink — got {events:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn agent_def_model_selector_routes_the_spawn() {
        // A `model:` frontmatter selector must swap the sub-agent's adapter —
        // the parent's provider serves the Task call + continuation, while a
        // separate (observable) provider serves everything inside the child.
        let dir = std::env::temp_dir().join(format!("sunmao-route-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".sunmao/agents")).unwrap();
        std::fs::write(
            dir.join(".sunmao/agents/scout.md"),
            "---\nname: scout\ndescription: cheap scout\nmodel: other/x\n---\nYou scout.",
        )
        .unwrap();

        let parent = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                vec![
                    StreamDelta::ToolCalls(vec![
                        ToolCallFragment {
                            index: 0,
                            id: Some("t".into()),
                            name: Some("Task".into()),
                            arguments: None,
                        },
                        ToolCallFragment {
                            index: 0,
                            arguments: Some(
                                "{\"prompt\":\"scout it\",\"subagent_type\":\"scout\"}".into(),
                            ),
                            ..Default::default()
                        },
                    ]),
                    StreamDelta::Finish {
                        reason: Some("tool_calls".into()),
                        usage: None,
                    },
                ],
                vec![
                    StreamDelta::Content("parent done".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
            ])),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let routed = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut ctx_raw = Context::new(
            parent.clone(),
            SessionLog::ephemeral(),
            builtin_registry(),
            dir.clone(),
        );
        ctx_raw.models = Some(Arc::new(
            crate::models::ModelResolver::load(
                &dir,
                crate::models::ProviderDef {
                    base_url: "http://unused".into(),
                    api_key_env: None,
                    api_key: None,
                    dialect: "openai".into(),
                },
                "default",
            )
            .with_adapter("other/x", routed.clone()),
        ));
        let ctx = Arc::new(ctx_raw);
        let outcome = AgentLoop::new(ctx)
            .run_turn("go", &NullObserver)
            .await
            .unwrap();
        assert!(matches!(outcome, TurnOutcome::Completed));
        // parent: Task call + post-tool continuation; child: its whole turn
        // went to the routed adapter.
        assert_eq!(parent.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert!(routed.calls.load(std::sync::atomic::Ordering::Relaxed) >= 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
