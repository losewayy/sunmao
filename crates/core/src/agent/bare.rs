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
use crate::context::{MutexRecover, RwLockRecover};

use futures_util::StreamExt;
use sunmao_llm::assemble::ToolCallAssembler;
use sunmao_llm::types::Message;
use sunmao_llm::{ChatRequest, StreamDelta};

impl AgentLoop {
    /// A straight "prompt → stream → dispatch → record" circuit.
    /// `attachments` ride as content blocks on the user message, same as
    /// the full loop — bare skips hooks and the gate, not the wire shape.
    pub(super) async fn run_turn_bare(
        &self,
        input: &str,
        attachments: &[sunmao_llm::Content],
        observer: &dyn Observer,
    ) -> anyhow::Result<TurnOutcome> {
        let mut ordinal = None;
        {
            let mut log = self.ctx.sessions.lock().await;
            let ev = SessionEvent::Message {
                message: Message::user_blocks(input, attachments.to_vec()),
            };
            if crate::checkpoints::is_turn_boundary(&ev) {
                // same ordinal accounting as the full loop — bare skips
                // hooks and the gate, not the checkpoint ledger
                let mut cps = self.ctx.checkpoints.lock_or_recover();
                cps.turn += 1;
                ordinal = Some(cps.turn);
            }
            log.append(&ev).await?;
        }
        if let Some(n) = ordinal {
            observer.on_event(&crate::agent::LiveEvent::TurnBoundary { ordinal: n });
        }
        let mut outcome = TurnOutcome::Completed;
        for _ in 0..self.max_iterations {
            if self
                .ctx
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                outcome = TurnOutcome::Cancelled;
                break;
            }
            let messages = self.ctx.sessions.lock().await.messages().await?;
            let mut messages = messages;
            {
                let items = self.ctx.todos.lock_or_recover().clone();
                if !items.is_empty() {
                    // same tail-of-request injection as the full loop —
                    // bare skips hooks and the gate, not the task list.
                    messages.push(Message::user(crate::tool::todos_inject_text(&items)));
                }
            }
            let decls = self.ctx.advertised_tools();
            self.ctx.persist_tool_surface().await;
            let effort = self.ctx.reasoning_effort.read_or_recover().clone();
            let req = ChatRequest {
                messages: &messages,
                tools: Some(&decls),
                max_tokens: None,
                temperature: None,
                reasoning_effort: effort.as_deref(),
            };
            // same cancel-during-establishment arm as the turn loop — a
            // hung stream() must not wait out its provider timeout. Bind
            // the adapter Arc first: the temporary would drop before
            // `select!` could borrow it. `CancelSignal::wait` carries the
            // flag, so a cancel that beat this arm still returns.
            let llm = self.ctx.active_llm();
            let cancel = self.ctx.cancel_signal();
            let mut stream = tokio::select! {
                s = llm.stream(req) => s?,
                () = cancel.wait() => {
                    return Ok(TurnOutcome::Cancelled);
                }
            };
            let mut content = String::new();
            let mut assembler = ToolCallAssembler::new();
            let mut finish_reason: Option<String> = None;
            let mut cancelled_mid_stream = cancel.is_cancelled();
            loop {
                if cancelled_mid_stream {
                    break;
                }
                let delta = tokio::select! {
                    d = stream.next() => d,
                    // fresh arm per delta — `wait` re-reads the flag, so a
                    // cancel that landed while the last delta was handled
                    // still ends the stream here
                    () = cancel.wait() => {
                        cancelled_mid_stream = true;
                        None
                    }
                };
                let Some(delta) = delta else { break };
                match delta? {
                    StreamDelta::Content(c) => {
                        observer.on_event(&LiveEvent::Content { text: c.clone() });
                        content.push_str(&c);
                    }
                    StreamDelta::Reasoning(r) => {
                        observer.on_event(&LiveEvent::Reasoning { text: r });
                    }
                    StreamDelta::ToolCalls(frags) => {
                        for f in &frags {
                            assembler.push(f);
                        }
                    }
                    StreamDelta::Finish { reason, usage } => {
                        if let Some(u) = &usage {
                            let mut log = self.ctx.sessions.lock().await;
                            log.append(&SessionEvent::Usage { usage: u.clone() })
                                .await?;
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
                outcome = if cancelled_mid_stream {
                    TurnOutcome::Cancelled
                } else {
                    match finish_reason.as_deref() {
                        Some("length") => TurnOutcome::LengthLimited,
                        Some("stop") | Some("end_turn") | None => TurnOutcome::Completed,
                        Some(other) => TurnOutcome::Other(other.to_string()),
                    }
                };
                break;
            }
            for call in tool_calls {
                if self
                    .ctx
                    .cancelled
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    // settle the remaining calls — bare keeps the
                    // tool_call/tool_result pairing legal even on cancel
                    let mut log = self.ctx.sessions.lock().await;
                    log.append(&SessionEvent::ToolResult {
                        call_id: call.id.clone(),
                        name: call.function.name.clone(),
                        ok: false,
                        output: "cancelled by user".into(),
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                    })
                    .await?;
                    continue;
                }
                let t0 = std::time::Instant::now();
                let result = if let Some(err) = malformed.get(&call.id) {
                    observer.on_event(&LiveEvent::ToolStart {
                        name: call.function.name.clone(),
                        summary: "malformed arguments".into(),
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                        call_id: Some(call.id.clone()),
                        args: serde_json::Value::Null,
                    });
                    crate::tool::ToolResult {
                        exit_code: None,
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
                        call_id: Some(call.id.clone()),
                        args: args_value.clone(),
                    });
                    let call_fut = self.ctx.tools.call(
                        &call.function.name,
                        &call.function.arguments,
                        &self.ctx,
                    );
                    let secs = tool_timeout_for(&self.ctx, &call.function.name);
                    tokio::pin!(call_fut);
                    // same cancel discipline as the turn loop's dispatch —
                    // `wait` reads the flag before and after registration,
                    // so an already-requested cancel aborts the call here
                    let cancel = self.ctx.cancel_signal();
                    let aborted = || crate::tool::ToolResult {
                        exit_code: None,
                        output: "cancelled by user".into(),
                        ok: false,
                    };
                    if cancel.is_cancelled() {
                        aborted()
                    } else {
                        match secs {
                            Some(s) => tokio::select! {
                                r = &mut call_fut => r,
                                () = cancel.wait() => aborted(),
                                () = tokio::time::sleep(std::time::Duration::from_secs(s)) => crate::tool::ToolResult { exit_code: None,
                                    output: format!("tool {} exceeded its {s}s timeout — see tool-timeouts.txt", call.function.name),
                                    ok: false,
                                },
                            },
                            None => tokio::select! {
                                r = &mut call_fut => r,
                                () = cancel.wait() => aborted(),
                            },
                        }
                    }
                };
                observer.on_event(&LiveEvent::ToolDone {
                    name: call.function.name.clone(),
                    ok: result.ok,
                    output: truncate_output(&result.output),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
                    call_id: Some(call.id.clone()),
                    elapsed_ms: t0.elapsed().as_millis() as u64,
                });
                let mut log = self.ctx.sessions.lock().await;
                log.append(&SessionEvent::ToolCall {
                    call: call.clone(),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
                })
                .await?;
                // ToolResult event alone carries the result — the fold
                // derives the protocol message from it; an extra
                // Message::tool_result append would double-report the call
                // and providers hard-reject the transcript.
                log.append(&SessionEvent::ToolResult {
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    ok: result.ok,
                    output: result.output.clone(),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
                })
                .await?;
            }
        }
        // TurnEnd + the cancelled reset live in run_turn() — shared across
        // drivers so no exit path can skip them.
        Ok(outcome)
    }
}
