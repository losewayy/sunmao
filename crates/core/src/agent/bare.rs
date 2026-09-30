//! The `bare` loop driver — SPEC §4.5's proof that the loop is a plugin,
//! not kernel privilege. Selected by a manifest `loop: "bare"` key or
//! `--loop bare`.
//!
//! What it drops vs the full loop, and why each drop is deliberate:
//!  - **hooks**: none fire. A bare session is declared by files that
//!    could have loaded hooks — running them anyway would make `bare`
//!    a lie (the preset author opted out of the hook surface).
//!  - **dispatch gate**: no permissions check, no approvals — the
//!    operator accepted tool execution wholesale when choosing bare.
//!  - **auto-compaction**: no pre-vacuum; `compact()` stays callable
//!    for frontends that expose it.
//!
//! What survives unchanged: event-sourced logging (turns fold the
//! same), the cancel flag, the iteration ceiling, lenient tool-call
//! assembly (malformed args become failed results, not aborts), the
//! observer stream, and Stop-free TurnEnd emission.

use super::*;

use futures_util::StreamExt;
use sunmao_llm::assemble::ToolCallAssembler;
use sunmao_llm::types::Message;
use sunmao_llm::{ChatRequest, StreamDelta};

impl AgentLoop {
    /// A straight "prompt → stream → dispatch → record" circuit.
    pub(super) async fn run_turn_bare(
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
            if self
                .ctx
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                outcome = TurnOutcome::Other("cancelled".into());
                break;
            }
            let messages = self.ctx.sessions.lock().await.messages().await?;
            let decls = self.ctx.tools.declarations();
            let req = ChatRequest {
                messages: &messages,
                tools: Some(&decls),
                max_tokens: None,
                temperature: None,
            };
            let mut stream = self.ctx.active_llm().stream(req).await?;
            let mut content = String::new();
            let mut assembler = ToolCallAssembler::new();
            let mut finish_reason: Option<String> = None;
            while let Some(delta) = stream.next().await {
                match delta? {
                    StreamDelta::Content(c) => {
                        observer.on_event(&LiveEvent::Content(c.clone()));
                        content.push_str(&c);
                    }
                    StreamDelta::Reasoning(r) => {
                        observer.on_event(&LiveEvent::Reasoning(r));
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
                let result = if let Some(err) = malformed.get(&call.id) {
                    crate::tool::ToolResult {
                        output: format!("malformed tool call: {err}"),
                        ok: false,
                    }
                } else {
                    let args_value: serde_json::Value =
                        serde_json::from_str(&call.function.arguments)
                            .unwrap_or(serde_json::Value::Null);
                    observer.on_event(&LiveEvent::ToolStart {
                        name: call.function.name.clone(),
                        summary: call_summary(&call.function.name, &args_value),
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                    });
                    self.ctx
                        .tools
                        .call(&call.function.name, &call.function.arguments, &self.ctx)
                        .await
                };
                observer.on_event(&LiveEvent::ToolDone {
                    name: call.function.name.clone(),
                    ok: result.ok,
                    output: truncate_output(&result.output),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
                });
                let mut log = self.ctx.sessions.lock().await;
                log.append(&SessionEvent::ToolCall {
                    call: call.clone(),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
                })
                .await?;
                log.append(&SessionEvent::ToolResult {
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    ok: result.ok,
                    output: result.output.clone(),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
                })
                .await?;
                log.append(&SessionEvent::Message {
                    message: Message::tool_result(call.id.clone(), result.output),
                })
                .await?;
            }
        }
        self.ctx
            .cancelled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        observer.on_event(&LiveEvent::TurnEnd {
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }
}
