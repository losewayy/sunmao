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

pub struct AgentLoop {
    ctx: Arc<Context>,
    max_iterations: usize,
}

impl AgentLoop {
    pub fn new(ctx: Arc<Context>) -> Self {
        Self {
            ctx,
            max_iterations: 64,
        }
    }

    pub fn with_max_iterations(mut self, n: usize) -> Self {
        self.max_iterations = n;
        self
    }

    /// Run one turn: `input` is the user's message; returns when the model
    /// stops calling tools or we hit the iteration ceiling.
    pub async fn run_turn(
        &self,
        input: &str,
        observer: &dyn Observer,
    ) -> anyhow::Result<TurnOutcome> {
        {
            let mut log = self.ctx.sessions.lock().await;
            log.append(&SessionEvent::Message {
                message: Message::user(input),
            })
            .await?;
        }

        let mut outcome = TurnOutcome::Completed;
        for _ in 0..self.max_iterations {
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
                    Some(other) => TurnOutcome::Other(other.to_string()),
                    None => TurnOutcome::Completed,
                };
                break;
            }

            for call in tool_calls {
                observer.on_event(&LiveEvent::ToolStart {
                    name: call.function.name.clone(),
                });
                let result = self
                    .ctx
                    .tools
                    .call(&call.function.name, &call.function.arguments, &self.ctx)
                    .await;
                observer.on_event(&LiveEvent::ToolDone {
                    name: call.function.name.clone(),
                    ok: result.ok,
                });
                let mut log = self.ctx.sessions.lock().await;
                log.append(&SessionEvent::ToolResult {
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    ok: result.ok,
                    output: result.output.clone(),
                })
                .await?;
            }
        }
        observer.on_event(&LiveEvent::TurnEnd {
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }
}
