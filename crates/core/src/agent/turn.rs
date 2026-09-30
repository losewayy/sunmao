use super::*;

use futures_util::StreamExt;
use sunmao_llm::assemble::ToolCallAssembler;
use sunmao_llm::types::Message;
use sunmao_llm::{ChatRequest, StreamDelta};

impl AgentLoop {
    /// Run one turn: `input` is the user's message; returns when the model
    /// stops calling tools or we hit the iteration ceiling.
    ///
    /// Turns serialize on `ctx.turn_lock` — a second concurrent run_turn
    /// queues instead of interleaving facts into the session log. That's
    /// the replay fence: one turn's ToolCall/ToolResult events can never
    /// straddle a predecessor's, so the fold the next request sees is
    /// always a well-formed transcript.
    ///
    /// Frontends depend on TurnEnd to unwind their "working" state — this
    /// wrapper emits it on every exit (success, veto, cancel, Err), so an
    /// early return inside a driver can never strand a frontend. The
    /// `cancelled` flag resets here too: a stale flag must not survive
    /// into the next turn regardless of how this one ended.
    ///
    /// `ctx.loop_driver` picks the driver (SPEC §4.5): `Full` runs the
    /// contract loop below; `Bare` runs `run_turn_bare` — same session log
    /// and observer, no hooks/gate/compaction.
    pub async fn run_turn(
        &self,
        input: &str,
        observer: &dyn Observer,
    ) -> anyhow::Result<TurnOutcome> {
        let _turn_permit = self.ctx.turn_lock.lock().await;
        let res = match self.ctx.loop_driver {
            crate::agent::LoopDriver::Full => self.run_turn_full(input, observer).await,
            crate::agent::LoopDriver::Bare => self.run_turn_bare(input, observer).await,
        };
        // cancelled resets at turn END on *every* exit path — an Err or an
        // early-returned outcome must not leak the flag into the next turn
        // (a stale flag would make the next turn short-circuit forever).
        self.ctx
            .cancelled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // TurnEnd is the frontend's "unwind working state" signal — emit it
        // here so every inner exit (early returns included) produces exactly
        // one, with Stop/StopFailure already fired inside the driver.
        match &res {
            Ok(o) => observer.on_event(&LiveEvent::TurnEnd { outcome: o.clone() }),
            Err(e) => observer.on_event(&LiveEvent::TurnEnd {
                outcome: TurnOutcome::Other(format!("error: {e:#}")),
            }),
        }
        res
    }

    async fn run_turn_full(
        &self,
        input: &str,
        observer: &dyn Observer,
    ) -> anyhow::Result<TurnOutcome> {
        match self.run_turn_inner(input, observer).await {
            Ok(o) => {
                // non-clean outcomes fire StopFailure — Stop itself is emitted
                // inside run_turn_inner on Completed only; the union event
                // marks that the stop wasn't a normal completion
                if !matches!(o, TurnOutcome::Completed) {
                    let _ = self
                        .ctx
                        .hooks
                        .fire(
                            HookEvent::StopFailure,
                            &self.ctx.cwd,
                            &crate::hooks::HookInput::default(),
                        )
                        .await;
                }
                Ok(o)
            }
            Err(e) => {
                let _ = self
                    .ctx
                    .hooks
                    .fire(
                        HookEvent::StopFailure,
                        &self.ctx.cwd,
                        &crate::hooks::HookInput::default(),
                    )
                    .await;
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
        // Auto-compact BEFORE the new prompt is appended — otherwise the
        // Compacted boundary folds the just-submitted question into a
        // summary and the model never sees it as a live user message.
        // The loop-head check below still catches growth mid-turn.
        if self.est_tokens().await > self.compact_threshold {
            observer.on_event(&LiveEvent::ToolStart {
                name: "compact".into(),
                summary: String::new(),
                depth: self.ctx.depth,
                lane: self.ctx.lane,
            });
            if let Err(e) = self.compact_inner(observer, "auto").await {
                observer.on_event(&LiveEvent::ToolDone {
                    name: format!("compact failed: {e:#}"),
                    ok: false,
                    output: String::new(),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
                });
            }
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
                    lane: self.ctx.lane,
                });
                if let Err(e) = self.compact_inner(observer, "auto").await {
                    observer.on_event(&LiveEvent::ToolDone {
                        name: format!("compact failed: {e:#}"),
                        ok: false,
                        output: String::new(),
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                    });
                }
            }

            let messages = self.ctx.sessions.lock().await.messages().await?;
            let mut messages = messages;
            {
                let items = self.ctx.todos.lock().unwrap().clone();
                if !items.is_empty() {
                    // the durable Todos fact lives in the log; the model
                    // needs it *in* the transcript — synthetic tail-of-
                    // request message, never appended. Tail placement is
                    // cache-honest: head injection would invalidate the
                    // provider's prompt prefix every time the plan moves;
                    // appended after the last message it still follows
                    // tool_call/tool_result pairing rules.
                    messages.push(Message::user(crate::tool::todos_inject_text(&items)));
                }
            }
            let decls = self.ctx.tools.declarations();
            let req = ChatRequest {
                messages: &messages,
                tools: Some(&decls),
                max_tokens: None,
                temperature: None,
            };

            let mut stream = self.ctx.active_llm().stream(req).await?;

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
                    // still a settled failure: the live transcript shows the
                    // pair (same as replay), and PostToolUseFailure rings —
                    // the union event for failure listeners.
                    observer.on_event(&LiveEvent::ToolStart {
                        name: call.function.name.clone(),
                        summary: "malformed arguments".into(),
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                    });
                    observer.on_event(&LiveEvent::ToolDone {
                        name: call.function.name.clone(),
                        ok: false,
                        output: result.output.clone(),
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                    });
                    let _ = self
                        .ctx
                        .hooks
                        .fire(
                            HookEvent::PostToolUseFailure,
                            &self.ctx.cwd,
                            &crate::hooks::HookInput {
                                tool_name: Some(&call.function.name),
                                tool_use_id: Some(&call.id),
                                tool_response: Some(&result.output),
                                ..Default::default()
                            },
                        )
                        .await;
                    let mut log = self.ctx.sessions.lock().await;
                    log.append(&SessionEvent::ToolCall {
                        call: call.clone(),
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                    })
                    .await?;
                    // ToolResult event alone carries the result — the fold
                    // derives the protocol message from it; appending
                    // Message::tool_result too would double-report the call
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
                        lane: self.ctx.lane,
                    })
                    .await?;
                }

                observer.on_event(&LiveEvent::ToolStart {
                    name: call.function.name.clone(),
                    summary: call_summary(&call.function.name, &args_value),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
                });

                let result = if let Some(reason) = pre.block_reason {
                    // a hook veto is an audit fact too — the transcript's
                    // failed ToolResult shows *that* it was blocked, the
                    // Hook event keeps *why* durable
                    {
                        let mut log = self.ctx.sessions.lock().await;
                        let _ = log
                            .append(&SessionEvent::Hook {
                                event: "PreToolUse.block".into(),
                                detail: format!("{}: {reason}", call.function.name),
                            })
                            .await;
                    }
                    observer.on_event(&LiveEvent::Hook {
                        event: "PreToolUse.block".into(),
                        detail: format!("{}: {reason}", call.function.name),
                    });
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
                    lane: self.ctx.lane,
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
                // the union event — failure listeners only run on settled
                // bad results (deny, error, crash), after the general hook
                if !result.ok {
                    let _ = self
                        .ctx
                        .hooks
                        .fire(
                            HookEvent::PostToolUseFailure,
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
                }

                let mut log = self.ctx.sessions.lock().await;
                log.append(&SessionEvent::ToolResult {
                    call_id: call.id.clone(),
                    name: call.function.name.clone(),
                    ok: result.ok,
                    output: result.output.clone(),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
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
        // TurnEnd + the cancelled reset moved to run_turn() — every exit
        // path (early returns included) emits exactly one TurnEnd and
        // clears the flag, so a veto or Err can't strand a frontend or
        // poison the next turn.
        // clean turns end with Stop; anything else gets StopFailure (fired
        // by run_turn_full after inner returns) — never both
        if outcome == TurnOutcome::Completed {
            let _ = self
                .ctx
                .hooks
                .fire(
                    HookEvent::Stop,
                    &self.ctx.cwd,
                    &crate::hooks::HookInput::default(),
                )
                .await;
        }
        Ok(outcome)
    }
}
