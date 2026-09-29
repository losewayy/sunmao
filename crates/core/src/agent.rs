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
    ToolStart { name: String },
    ToolDone { name: String, ok: bool },
    TurnEnd { outcome: TurnOutcome },
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

    /// Ask the model to summarize the transcript, then commit a `Compacted`
    /// boundary — the log fold turns it into a fresh system message.
    pub async fn compact(&self, observer: &dyn Observer) -> anyhow::Result<()> {
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
        observer.on_event(&LiveEvent::ToolDone {
            name: "compact".into(),
            ok: true,
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
            .fire(HookEvent::UserPromptSubmit, &self.ctx.cwd, None, None, None)
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
                });
                if let Err(e) = self.compact(observer).await {
                    observer.on_event(&LiveEvent::ToolDone {
                        name: format!("compact failed: {e:#}"),
                        ok: false,
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
                    StreamDelta::Finish { reason, .. } => {
                        finish_reason = reason.or(finish_reason);
                    }
                }
            }

            let tool_calls = assembler.finish()?;

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

            {
                let mut log = self.ctx.sessions.lock().await;
                for call in &tool_calls {
                    log.append(&SessionEvent::ToolCall { call: call.clone() })
                        .await?;
                }
            }

            for call in tool_calls {
                let args_value: serde_json::Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or(serde_json::Value::Null);

                // PreToolUse: a hook may veto the call outright.
                let pre = self
                    .ctx
                    .hooks
                    .fire(
                        HookEvent::PreToolUse,
                        &self.ctx.cwd,
                        Some(&call.function.name),
                        Some(&args_value),
                        None,
                    )
                    .await;
                observer.on_event(&LiveEvent::ToolStart {
                    name: call.function.name.clone(),
                });

                let result = if let Some(reason) = pre.block_reason {
                    crate::tool::ToolResult {
                        output: format!("blocked by hook: {reason}"),
                        ok: false,
                    }
                } else {
                    self.ctx
                        .tools
                        .call(&call.function.name, &call.function.arguments, &self.ctx)
                        .await
                };
                observer.on_event(&LiveEvent::ToolDone {
                    name: call.function.name.clone(),
                    ok: result.ok,
                });

                // PostToolUse: hooks may inject context for the next turn.
                let post = self
                    .ctx
                    .hooks
                    .fire(
                        HookEvent::PostToolUse,
                        &self.ctx.cwd,
                        Some(&call.function.name),
                        Some(&args_value),
                        Some(&result.output),
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
            .fire(HookEvent::Stop, &self.ctx.cwd, None, None, None)
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
}
