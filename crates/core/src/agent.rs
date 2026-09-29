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
use crate::session::SessionEvent;

/// Live events the frontend can observe (stdout printer, later TUI/ACP).
#[derive(Debug, Clone)]
pub enum LiveEvent {
    Content(String),
    Reasoning(String),
    /// Tool call began. `summary` is a one-line digest of the interesting
    /// argument (path/command/pattern/…) for frontends to render.
    ToolStart {
        name: String,
        summary: String,
    },
    /// Tool call finished. `output` carries the raw result so rich frontends
    /// can preview it; simple frontends ignore it.
    ToolDone {
        name: String,
        ok: bool,
        output: String,
    },
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
fn call_summary(name: &str, args: &serde_json::Value) -> String {
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

    /// Signal cooperative cancellation for the in-flight turn.
    pub fn cancel(&self) {
        self.ctx
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
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
            return self.ask(tool, specifier, "matched ask rule").await;
        }
        if let Some(H::Ask) = hook {
            return self.ask(tool, specifier, "hook requested approval").await;
        }
        if let Some(H::Allow) = hook {
            return Ok(());
        }
        // default: the risky-pattern classifier (Bash-shaped patterns today)
        if let Some(why) = crate::approval::classify(specifier) {
            return self.ask(tool, specifier, why).await;
        }
        Ok(())
    }

    /// One approval prompt → verdict. `Session` is recorded in
    /// `session_grants` and audited as a durable `Hook` fact.
    async fn ask(&self, tool: &str, specifier: &str, why: &str) -> Result<(), String> {
        match self.ctx.approval.approve(tool, specifier, why).await {
            crate::approval::Approval::Session => {
                self.ctx.grant_session(tool, specifier);
                let mut log = self.ctx.sessions.lock().await;
                let _ = log
                    .append(&crate::SessionEvent::Hook {
                        event: "approval.session".into(),
                        detail: format!("{tool}: {specifier}"),
                    })
                    .await;
                Ok(())
            }
            crate::approval::Approval::Once => Ok(()),
            crate::approval::Approval::Deny => Err(format!("denied at approval gate ({why})")),
        }
    }

    /// Ask the model to summarize the transcript, then commit a `Compacted`
    /// boundary — the log fold turns it into a fresh system message.
    pub async fn compact(&self, observer: &dyn Observer, trigger: &str) -> anyhow::Result<()> {
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
            return Ok(());
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
            .append(&SessionEvent::Compacted { summary })
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
            output: String::new(),
        });
        Ok(())
    }

    /// Run one turn: `input` is the user's message; returns when the model
    /// stops calling tools or we hit the iteration ceiling.
    pub async fn run_turn(
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
            return Ok(TurnOutcome::Other(format!("blocked by hook: {reason}")));
        }
        {
            let mut log = self.ctx.sessions.lock().await;
            log.append(&SessionEvent::Message {
                message: Message::user(input),
            })
            .await?;
            for extra in prompt_outcome.extra_context {
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
                });
                if let Err(e) = self.compact(observer, "auto").await {
                    observer.on_event(&LiveEvent::ToolDone {
                        name: format!("compact failed: {e:#}"),
                        ok: false,
                        output: String::new(),
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
                    log.append(&SessionEvent::ToolCall { call: call.clone() })
                        .await?;
                    log.append(&SessionEvent::ToolResult {
                        call_id: call.id.clone(),
                        name: call.function.name.clone(),
                        ok: result.ok,
                        output: result.output.clone(),
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
                // what actually ran; the rewrite itself is a durable Hook fact.
                if let Some(updated) = pre.updated_input {
                    {
                        let mut log = self.ctx.sessions.lock().await;
                        let _ = log
                            .append(&SessionEvent::Hook {
                                event: "PreToolUse.updatedInput".into(),
                                detail: format!(
                                    "{}: {} → {}",
                                    call.function.name, call.function.arguments, updated
                                ),
                            })
                            .await;
                    }
                    args_value = updated;
                }

                {
                    let mut log = self.ctx.sessions.lock().await;
                    let mut call = call.clone();
                    call.function.arguments = args_value.to_string();
                    log.append(&SessionEvent::ToolCall { call }).await?;
                }

                observer.on_event(&LiveEvent::ToolStart {
                    name: call.function.name.clone(),
                    summary: call_summary(&call.function.name, &args_value),
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
                        .gate_call(&call.function.name, &specifier, pre.permission_decision)
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
}
