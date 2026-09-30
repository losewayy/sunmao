use super::*;

use futures_util::StreamExt;
use sunmao_llm::assemble::ToolCallAssembler;
use sunmao_llm::types::Message;
use sunmao_llm::{ChatRequest, StreamDelta};

impl AgentLoop {
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
        if let Some(why) = crate::approval::classify(specifier, &self.ctx.risk_table) {
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
            crate::prompt::PromptAssembler::new(&self.ctx.cwd).assemble_compact(),
        ));
        let req = ChatRequest {
            messages: &msgs,
            tools: None,
            max_tokens: Some(2048),
            temperature: None,
        };
        let mut stream = self.ctx.active_llm().stream(req).await?;
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
            lane: self.ctx.lane,
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
                    lane: self.ctx.lane,
                });
                if let Err(e) = self.compact(observer, "auto").await {
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
