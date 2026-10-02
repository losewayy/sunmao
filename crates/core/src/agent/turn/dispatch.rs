//! One tool call's full dispatch — the fact-order contract
//! (PreToolUse → ToolCall → gate → run → ToolDone → PostToolUse →
//! ToolResult) lives in this single function so run_turn_inner stays
//! stream→loop→outcome.

use crate::agent::*;
use crate::hooks::HookEvent;
use crate::session::SessionEvent;
use sunmao_llm::types::Message;

impl AgentLoop {
    /// One well-formed tool call's full dispatch: PreToolUse hook →
    /// rewrite → ToolCall fact → gate → execution (cancel + watchdog
    /// race) → ToolDone → PostToolUse hooks → ToolResult fact + hook
    /// context. Extracted so run_turn_inner reads as stream → loop →
    /// outcome; every durable fact still lands in this order.
    pub(super) async fn dispatch_tool_call(
        &self,
        call: &sunmao_llm::ToolCall,
        observer: &dyn Observer,
    ) -> anyhow::Result<()> {
        let mut args_value: serde_json::Value =
            serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::Value::Null);

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
                log.append_audit(&SessionEvent::Hook {
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
            call_id: Some(call.id.clone()),
            // the effective args — post-hook-rewrite, the same value
            // the ToolCall fact and the dispatch below see
            args: args_value.clone(),
        });
        let t0 = std::time::Instant::now();

        let result = if let Some(reason) = pre.block_reason {
            // a hook veto is an audit fact too — the transcript's
            // failed ToolResult shows *that* it was blocked, the
            // Hook event keeps *why* durable
            {
                let mut log = self.ctx.sessions.lock().await;
                log.append_audit(&SessionEvent::Hook {
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
                    &args_value,
                    &specifier,
                    pre.permission_decision,
                    observer,
                )
                .await
            {
                Ok(()) => {
                    // Race the call against cancel + the per-tool
                    // watchdog. Cancelling *frees the turn* — for
                    // Bash the kill lands inside run_deno itself
                    // (kill_signal on the !Send blocking thread);
                    // other tools' futures are abandoned. The
                    // watchdog (`tool-timeouts.txt`) covers tools
                    // with no timeout arg of their own; Bash and
                    // Task manage their own lifecycle and aren't
                    // listed.
                    let args_json = args_value.to_string();
                    let call_fut = self
                        .ctx
                        .tools
                        .call(&call.function.name, &args_json, &self.ctx);
                    let secs = tool_timeout_for(&self.ctx, &call.function.name);
                    tokio::pin!(call_fut);
                    // `notify_waiters` only wakes *registered* waiters — a
                    // cancel landing between `tools.call` and the first
                    // `select!` poll would be missed. `enable()` registers
                    // the waiter now, closing that window; the flag recheck
                    // catches a cancel that finished before registration
                    // (the hook/gate awaits above are exactly that window).
                    let cancel = self.ctx.cancel_notify.notified();
                    tokio::pin!(cancel);
                    cancel.as_mut().enable();
                    let aborted = || crate::tool::ToolResult {
                        output: "cancelled by user".into(),
                        ok: false,
                    };
                    if self
                        .ctx
                        .cancelled
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        // cancelled before the call could even start —
                        // settle it failed without polling the tool future
                        aborted()
                    } else {
                        match secs {
                            Some(s) => tokio::select! {
                                r = &mut call_fut => r,
                                () = &mut cancel => aborted(),
                                () = tokio::time::sleep(std::time::Duration::from_secs(s)) => crate::tool::ToolResult {
                                    output: format!("tool {} exceeded its {s}s timeout — raise or remove its row in .sunmao/tool-timeouts.txt, or (for Bash) pass a larger timeout_secs / background:true", call.function.name),
                                    ok: false,
                                },
                            },
                            None => tokio::select! {
                                r = &mut call_fut => r,
                                () = &mut cancel => aborted(),
                            },
                        }
                    }
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
            call_id: Some(call.id.clone()),
            elapsed_ms: t0.elapsed().as_millis() as u64,
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
        // bad results (deny, error, crash), after the general hook.
        // Pure observability → detached; a hung watcher must not hold
        // the tool_result hostage.
        if !result.ok {
            crate::hooks::HookEngine::fire_detached(
                &self.ctx.hooks,
                HookEvent::PostToolUseFailure,
                &self.ctx.cwd,
                &crate::hooks::HookInput {
                    tool_name: Some(&call.function.name),
                    tool_use_id: Some(&call.id),
                    tool_input: Some(&args_value),
                    tool_response: Some(&result.output),
                    ..Default::default()
                },
            );
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
        Ok(())
    }
}
