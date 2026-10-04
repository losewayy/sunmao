use super::*;
use crate::context::{MutexRecover, RwLockRecover};

use futures_util::StreamExt;
use sunmao_llm::assemble::ToolCallAssembler;
use sunmao_llm::types::Message;
use sunmao_llm::{ChatRequest, StreamDelta};

impl AgentLoop {
    /// One round of the contract loop (SPEC §4.5 `full` driver): hooks,
    /// approval gate, compaction, the whole envelope — `turn/chain.rs`
    /// owns the outer loop that re-enters this per goal continuation.
    async fn run_turn_full(
        &self,
        input: &str,
        attachments: &[sunmao_llm::Content],
        observer: &dyn Observer,
    ) -> anyhow::Result<TurnOutcome> {
        match self.run_turn_inner(input, attachments, observer).await {
            Ok(o) => {
                // non-clean outcomes fire StopFailure — Stop itself is emitted
                // inside run_turn_inner on Completed only; the union event
                // marks that the stop wasn't a normal completion
                if !matches!(o, TurnOutcome::Completed) {
                    crate::hooks::HookEngine::fire_detached(
                        &self.ctx.hooks,
                        HookEvent::StopFailure,
                        &self.ctx.cwd,
                        &crate::hooks::HookInput::default(),
                    );
                }
                Ok(o)
            }
            Err(e) => {
                crate::hooks::HookEngine::fire_detached(
                    &self.ctx.hooks,
                    HookEvent::StopFailure,
                    &self.ctx.cwd,
                    &crate::hooks::HookInput::default(),
                );
                Err(e)
            }
        }
    }

    async fn run_turn_inner(
        &self,
        input: &str,
        attachments: &[sunmao_llm::Content],
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
        if self.est_tokens().await > self.effective_threshold() {
            observer.on_event(&LiveEvent::ToolStart {
                name: "compact".into(),
                summary: String::new(),
                depth: self.ctx.depth,
                lane: self.ctx.lane,
                call_id: None,
                args: serde_json::Value::Null,
            });
            if let Err(e) = self.compact_inner(observer, "auto").await {
                observer.on_event(&LiveEvent::ToolDone {
                    name: format!("compact failed: {e:#}"),
                    ok: false,
                    output: String::new(),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
                    call_id: None,
                    elapsed_ms: 0,
                });
            }
        }
        let mut ordinal = None;
        {
            let mut log = self.ctx.sessions.lock().await;
            let msg = Message::user_blocks(input, attachments.to_vec());
            let ev = SessionEvent::Message { message: msg };
            let boundary = crate::checkpoints::is_turn_boundary(&ev);
            log.append(&ev).await?;
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
            drop(log);
            for notice in &prompt_outcome.notices {
                observer.on_event(&LiveEvent::Hook {
                    event: "hook warning".into(),
                    detail: notice.clone(),
                });
            }
            if boundary {
                // the turn's ordinal is set once the user prompt is durable —
                // snapshot writes stamp manifest entries with it, so
                // /rewind's turn numbering == the boundary ordinals users
                // count. Steer/uplink folds below are boundaries too — the
                // counter counts BOUNDARIES, not run_turn invocations.
                let mut cps = self.ctx.checkpoints.lock_or_recover();
                cps.turn += 1;
                ordinal = Some(cps.turn);
            }
        }
        if let Some(n) = ordinal {
            observer.on_event(&LiveEvent::TurnBoundary { ordinal: n });
        }

        let mut outcome = TurnOutcome::Completed;
        // doom-loop guard: a model repeating the identical (name, args)
        // call past a small run is stuck, not patient — the Nth call
        // settles as a failed ToolResult naming the loop so the model
        // reads WHY it was refused, and a `doom_loop` hook fact lands in
        // the transcript. Counter resets per turn: a long turn may
        // legitimately re-run a cheap probe many iterations apart.
        // doom-loop guard: the identical (name, args) call N times in a ROW
        // is a stuck model, not patient iteration — the streak resets the
        // moment a different call intervenes, so `test → edit → test`
        // never trips it. The Nth call settles failed with a reason the
        // model can read, and a `doom_loop` hook fact lands in the log.
        let mut repeat_key: Option<(String, String)> = None;
        let mut repeat_streak: u32 = 0;
        const REPEAT_LIMIT: u32 = 3;
        // Fusion Lead's contract — a synthetic tail-of-request message like
        // todos/goal, assembled once per turn (prompt.d layering is
        // cold-plug). Dropped the moment the delegation escalates: the text
        // says "you cannot modify files", which an unlocked Lead would
        // read as a lie.
        let fusion_note = (*self.ctx.turn_mode.read_or_recover() == TurnMode::Fusion).then(|| {
            crate::prompt::PromptAssembler::new(&self.ctx.cwd)
                .with_extra_roots(&self.ctx.extra_plugin_roots)
                .assemble_fusion_lead()
        });
        for iter_n in 0..self.max_iterations {
            if self
                .ctx
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                outcome = TurnOutcome::Cancelled;
                break;
            }
            // Steering drain: user messages queued while this turn ran fold
            // in HERE — at a loop boundary the transcript is well-formed
            // (the last block is a complete assistant turn or its settled
            // tool_results), so an appended user message never straddles a
            // tool_call/tool_result pair. Leftovers after a cancelled turn
            // stay queued — the driver claims them as follow-up input.
            for (_cid, steered) in self.drain_steer() {
                observer.on_event(&LiveEvent::Hook {
                    event: "steer".into(),
                    detail: steered.clone(),
                });
                let mut log = self.ctx.sessions.lock().await;
                let ev = SessionEvent::Message {
                    message: Message::user(steered.clone()),
                };
                let boundary = crate::checkpoints::is_turn_boundary(&ev);
                log.append(&ev).await?;
                // attribution: the folded message looks identical to a typed
                // user message — without this audit row, `--dataflow` and
                // resume readers can't tell steer-injection from typed input
                log.append_audit(&SessionEvent::Hook {
                    event: "steer".into(),
                    detail: steered,
                })
                .await;
                if boundary {
                    let mut cps = self.ctx.checkpoints.lock_or_recover();
                    cps.turn += 1;
                    let n = cps.turn;
                    drop(cps);
                    drop(log);
                    observer.on_event(&LiveEvent::TurnBoundary { ordinal: n });
                }
            }
            if self.est_tokens().await > self.effective_threshold() {
                observer.on_event(&LiveEvent::ToolStart {
                    name: "compact".into(),
                    summary: String::new(),
                    depth: self.ctx.depth,
                    lane: self.ctx.lane,
                    call_id: None,
                    args: serde_json::Value::Null,
                });
                if let Err(e) = self.compact_inner(observer, "auto").await {
                    observer.on_event(&LiveEvent::ToolDone {
                        name: format!("compact failed: {e:#}"),
                        ok: false,
                        output: String::new(),
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                        call_id: None,
                        elapsed_ms: 0,
                    });
                }
            }

            let messages = self.ctx.sessions.lock().await.messages().await?;
            let mut messages = messages;
            {
                let items = self.ctx.todos.lock_or_recover().clone();
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
            // same synthetic-tail discipline for the standing goal — the
            // model sees objective + round budget even after compaction,
            // without a durable copy duplicating once per UpdateGoal write
            if let Some(g) = self.ctx.goal.lock_or_recover().clone()
                && g.status == crate::tool::GoalStatus::InProgress
            {
                messages.push(Message::user(g.inject_text()));
            }
            if let Some(note) = &fusion_note
                && !self.ctx.fusion.lock_or_recover().escalated
            {
                messages.push(Message::user(note.clone()));
            }
            let decls = self.ctx.advertised_tools();
            let effort = self.ctx.reasoning_effort.read_or_recover().clone();
            let req = ChatRequest {
                messages: &messages,
                tools: Some(&decls),
                max_tokens: None,
                temperature: None,
                reasoning_effort: effort.as_deref(),
            };

            // cancel during stream ESTABLISHMENT: a slow/hung `stream()`
            // is outside the delta-select below — without this arm a kill
            // waits for the provider's own timeout (connect hangs can be
            // minutes on a bad route). `notify_waiters` only wakes
            // *registered* waiters: enable() pins ours before the await,
            // and the cancelled flag catches a cancel that beat us.
            // Bind the adapter Arc first — the temporary would drop before
            // `select!` could borrow it.
            let llm = self.ctx.active_llm();
            let cancel_wait = self.ctx.cancel_notify.notified();
            tokio::pin!(cancel_wait);
            cancel_wait.as_mut().enable();
            if self
                .ctx
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Ok(TurnOutcome::Cancelled);
            }
            let mut stream = tokio::select! {
                s = llm.stream(req) => s?,
                () = &mut cancel_wait => {
                    return Ok(TurnOutcome::Cancelled);
                }
            };

            let mut content = String::new();
            let mut reasoning = String::new();
            let mut assembler = ToolCallAssembler::new();
            let mut finish_reason: Option<String> = None;
            // cancel mid-stream: dropping `stream` aborts the HTTP body —
            // without this `select!` a queued cancel only lands after the
            // provider finishes generating (the "stop didn't work" bug).
            // The Notified is hoisted and enabled once: a wake between
            // loop iterations lands on the registered waiter instead of
            // dying between polls.
            let mut cancelled_mid_stream = false;
            let cancel_wait = self.ctx.cancel_notify.notified();
            tokio::pin!(cancel_wait);
            cancel_wait.as_mut().enable();
            if self
                .ctx
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                cancelled_mid_stream = true;
            }

            loop {
                if cancelled_mid_stream {
                    break;
                }
                let delta = tokio::select! {
                    d = stream.next() => d,
                    () = &mut cancel_wait => {
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
                        observer.on_event(&LiveEvent::Reasoning { text: r.clone() });
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
                            // audit facts must fail loudly — a swallowed
                            // Usage append silently zeroes token accounting
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
                // a cancel racing any finish reads as a user stop — never
                // as length-truncation or a clean end
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

            let had_calls = !tool_calls.is_empty();
            for call in tool_calls {
                // Cancel between sibling calls: settle the remaining calls
                // as failed ToolResults so tool_call/tool_result pairing
                // stays legal, then the loop-head check exits the turn.
                if self
                    .ctx
                    .cancelled
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
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
                        // arguments never parsed — Null reads as "no payload",
                        // not an empty-object call
                        args: serde_json::Value::Null,
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                        call_id: Some(call.id.clone()),
                    });
                    observer.on_event(&LiveEvent::ToolDone {
                        name: call.function.name.clone(),
                        ok: false,
                        output: result.output.clone(),
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                        call_id: Some(call.id.clone()),
                        elapsed_ms: 0,
                    });
                    crate::hooks::HookEngine::fire_detached(
                        &self.ctx.hooks,
                        HookEvent::PostToolUseFailure,
                        &self.ctx.cwd,
                        &crate::hooks::HookInput {
                            tool_name: Some(&call.function.name),
                            tool_use_id: Some(&call.id),
                            tool_response: Some(&result.output),
                            ..Default::default()
                        },
                    );
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
                // same (tool, args) the Nth time this turn → doom loop:
                // settle it failed without dispatching. The model still
                // sees the call's ToolResult — transcript stays legal.
                let key = (call.function.name.clone(), call.function.arguments.clone());
                if repeat_key.as_ref() == Some(&key) {
                    repeat_streak += 1;
                } else {
                    repeat_key = Some(key);
                    repeat_streak = 1;
                }
                let n = repeat_streak;
                if n >= REPEAT_LIMIT {
                    let detail = format!(
                        "identical {} call x{} — refusing a repeat loop",
                        call.function.name, n
                    );
                    observer.on_event(&LiveEvent::Hook {
                        event: "doom_loop".into(),
                        detail: detail.clone(),
                    });
                    crate::hooks::HookEngine::fire_detached(
                        &self.ctx.hooks,
                        HookEvent::PostToolUseFailure,
                        &self.ctx.cwd,
                        &crate::hooks::HookInput {
                            tool_name: Some(&call.function.name),
                            tool_use_id: Some(&call.id),
                            tool_response: Some(&detail),
                            ..Default::default()
                        },
                    );
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
                        ok: false,
                        output: format!(
                            "doom-loop guard: identical call repeated {n} times this turn — vary the call or report the blocker"
                        ),
                        depth: self.ctx.depth,
                        lane: self.ctx.lane,
                    })
                    .await?;
                    continue;
                }
                self.dispatch_tool_call(&call, observer).await?;
            }
            // hook output buffered during dispatch lands now — every
            // sibling result is settled, so the injected context can
            // never straddle a call/result pair
            {
                let tail = std::mem::take(&mut *self.ctx.hook_tail.lock_or_recover());
                if !tail.is_empty() {
                    let mut log = self.ctx.sessions.lock().await;
                    for extra in tail {
                        log.append(&SessionEvent::Message {
                            message: Message::user(format!("[hook context] {extra}")),
                        })
                        .await?;
                    }
                }
            }
            // the ceiling consumed its last iteration while tool calls were
            // still pending — a `Completed` outcome + Stop hook would read
            // as a normal finish; surface the truncation instead. A cancel
            // observed mid-dispatch reads as Cancelled — the ceiling
            // message would misreport a user stop as a truncation.
            if iter_n + 1 == self.max_iterations && had_calls {
                outcome = if self
                    .ctx
                    .cancelled
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    TurnOutcome::Cancelled
                } else {
                    TurnOutcome::Other(format!(
                        "hit {}-iteration ceiling with tool calls pending",
                        self.max_iterations
                    ))
                };
            }
        }
        // TurnEnd + the cancelled reset moved to run_turn() — every exit
        // path (early returns included) emits exactly one TurnEnd and
        // clears the flag, so a veto or Err can't strand a frontend or
        // poison the next turn.
        // clean turns end with Stop; anything else gets StopFailure (fired
        // by run_turn_full after inner returns) — never both
        if outcome == TurnOutcome::Completed {
            crate::hooks::HookEngine::fire_detached(
                &self.ctx.hooks,
                HookEvent::Stop,
                &self.ctx.cwd,
                &crate::hooks::HookInput::default(),
            );
        }
        Ok(outcome)
    }
}

mod chain;
mod dispatch;
