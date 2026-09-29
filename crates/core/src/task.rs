//! Task tool — spawn nested agent loops for self-contained subtasks.
//!
//! Each sub-agent gets its own Context (fresh session file, own read
//! ledger, hooks fire in its scope) but shares the provider wiring —
//! unless its `agents/*.md` def pins a `model:` selector, in which case
//! the session's ModelResolver swaps the adapter. Depth-capped so agents
//! can't recurse into themselves; every spawn claims a `lane` so parallel
//! children never alias each other's tool events.
//!
//! Two call shapes: flat `{prompt, subagent_type?}` for one task, or
//! `{context, tasks[]}` to fan out a batch — items run concurrently and
//! results merge in order.

use std::sync::Arc;

use anyhow::bail;
use serde::Deserialize;
use serde_json::{json, Value};
use sunmao_llm::types::Tool;

use crate::agent::{AgentLoop, LiveEvent, Observer};
use crate::context::Context;
use crate::session::{SessionEvent, SessionLog};
use crate::tool::{builtin_registry, ToolImpl, ToolResult};

const MAX_DEPTH: u8 = 2;

pub struct TaskTool;

/// Collects the sub-agent's final assistant text for the tool result AND
/// relays its tool lifecycle to the session's `live_sink` — the frontend
/// sees `Task` blocks working through real calls instead of a frozen row.
/// Relayed events are re-stamped with the child's `lane` so parallel
/// siblings don't collide on (name, depth). TurnEnd is swallowed: a
/// sub-agent's end must not unwind the outer turn.
struct RelayObserver {
    text: std::sync::Mutex<String>,
    sink: Option<Arc<dyn Observer>>,
    lane: u8,
}

impl Observer for RelayObserver {
    fn on_event(&self, ev: &LiveEvent) {
        match ev {
            LiveEvent::Content(c) => self.text.lock().unwrap().push_str(c),
            // sub-agent lifecycle is the parent's business, not the UI's —
            // forwarding TurnEnd would close the outer transcript early.
            LiveEvent::TurnEnd { .. } => {}
            LiveEvent::ToolStart {
                name,
                summary,
                depth,
                ..
            } => {
                if let Some(s) = &self.sink {
                    s.on_event(&LiveEvent::ToolStart {
                        name: name.clone(),
                        summary: summary.clone(),
                        depth: *depth,
                        lane: self.lane,
                    });
                }
            }
            LiveEvent::ToolDone {
                name,
                ok,
                output,
                depth,
                ..
            } => {
                if let Some(s) = &self.sink {
                    s.on_event(&LiveEvent::ToolDone {
                        name: name.clone(),
                        ok: *ok,
                        output: output.clone(),
                        depth: *depth,
                        lane: self.lane,
                    });
                }
            }
            _ => {
                if let Some(s) = &self.sink {
                    s.on_event(ev);
                }
            }
        }
    }
}

/// One task in a batch spawn (`tasks[]` items).
#[derive(Deserialize)]
struct TaskItem {
    prompt: String,
    subagent_type: Option<String>,
}

#[async_trait::async_trait]
impl ToolImpl for TaskTool {
    fn name(&self) -> &'static str {
        "Task"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "Task",
            "Delegate self-contained subtask(s) to sub-agents with the same toolset. \
             Returns the sub-agent's final message(s). Flat form: one `prompt`. \
             Batch form: `context` (shared background) + `tasks[]` — items run \
             concurrently. Use for parallelizable or scope-isolated work; \
             sub-agents cannot spawn further sub-agents beyond the depth cap.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Complete instructions for the subtask (flat form)"},
                    "subagent_type": {"type": "string", "description": "Named agent def from .sunmao/agents/*.md or .claude/agents/*.md"},
                    "context": {"type": "string", "description": "Shared background prepended to every task (batch form)"},
                    "tasks": {
                        "type": "array",
                        "description": "Batch form: run all items concurrently",
                        "items": {
                            "type": "object",
                            "properties": {
                                "prompt": {"type": "string"},
                                "subagent_type": {"type": "string"}
                            },
                            "required": ["prompt"]
                        }
                    }
                }
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            prompt: Option<String>,
            subagent_type: Option<String>,
            context: Option<String>,
            tasks: Option<Vec<TaskItem>>,
        }
        let a: Args = serde_json::from_value(args)?;

        if ctx.depth >= MAX_DEPTH {
            bail!("sub-agent depth limit reached ({MAX_DEPTH})");
        }

        // normalize both shapes into (prompt, subagent_type) items
        let items: Vec<TaskItem> = match (a.prompt, a.tasks) {
            (Some(p), None) => vec![TaskItem {
                prompt: p,
                subagent_type: a.subagent_type,
            }],
            (None, Some(tasks)) if !tasks.is_empty() => {
                let ctx_preamble = a.context.unwrap_or_default();
                tasks
                    .into_iter()
                    .map(|t| TaskItem {
                        prompt: if ctx_preamble.is_empty() {
                            t.prompt
                        } else {
                            format!("{ctx_preamble}\n\n# Task\n{}", t.prompt)
                        },
                        subagent_type: t.subagent_type,
                    })
                    .collect()
            }
            (None, None) => bail!("Task needs `prompt` or a non-empty `tasks[]`"),
            _ => bail!("Task takes `prompt` (flat) or `tasks[]` (batch), not both"),
        };

        // fan out — each spawn gets its own lane + session file
        let futs: Vec<_> = items
            .iter()
            .map(|it| spawn_one(ctx, &it.prompt, it.subagent_type.as_deref()))
            .collect();
        let results = futures_util::future::join_all(futs).await;

        if results.len() == 1 {
            return Ok(results.into_iter().next().unwrap());
        }
        // batch: per-item verdict + merged output so the parent sees which
        // child produced what
        let mut merged = String::new();
        let mut all_ok = true;
        for (i, r) in results.into_iter().enumerate() {
            all_ok &= r.ok;
            let mark = if r.ok { "✓" } else { "✗" };
            merged.push_str(&format!("## task {} {mark}\n{}\n\n", i + 1, r.output));
        }
        Ok(ToolResult {
            output: merged.trim_end().to_string(),
            ok: all_ok,
        })
    }
}

/// Run one sub-agent to completion: own context, own session file, own
/// lane; relays its tool lifecycle to the parent's live sink.
async fn spawn_one(ctx: &Context, prompt: &str, subagent_type: Option<&str>) -> ToolResult {
    let sub_id = format!(
        "sub-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let dir = ctx.cwd.join(".sunmao").join("sessions");
    let mut log = SessionLog::open(&dir, &sub_id)
        .await
        .unwrap_or_else(|_| SessionLog::ephemeral());
    // agents/*.md named def wins; else the `subagent-default` prompt
    // section — assembled by the same PromptAssembler as everything else.
    let def = subagent_type.and_then(|t| {
        crate::agents::load_all(&ctx.cwd)
            .into_iter()
            .find(|d| d.name == t)
    });
    let sys_prompt = def
        .as_ref()
        .map(|d| d.system_prompt.clone())
        .unwrap_or_else(|| {
            crate::prompt::PromptAssembler::new(&ctx.cwd).assemble_subagent(subagent_type)
        });
    {
        let _ = log
            .append(&SessionEvent::Message {
                message: sunmao_llm::types::Message::system(sys_prompt),
            })
            .await;
    }

    // model routing: the def's `model:` selector resolves through the
    // session's resolver; unresolvable/absent means inherit the parent.
    let llm = def
        .as_ref()
        .and_then(|d| d.model.as_deref())
        .and_then(|sel| ctx.models.as_ref().and_then(|m| m.adapter_for(sel)))
        .unwrap_or_else(|| ctx.llm.clone());

    // fresh context, one depth deeper, on its own lane
    let lane = ctx
        .lane_counter
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    let sub_ctx = Context {
        llm,
        sessions: tokio::sync::Mutex::new(log),
        tools: builtin_registry(),
        audit: crate::audit::AuditLog::new(),
        permissions: crate::permissions::Permissions::load(&ctx.cwd),
        approval: ctx.approval.clone(),
        hooks: crate::hooks::HookEngine::load(&ctx.cwd, &sub_id),
        cwd: ctx.cwd.clone(),
        depth: ctx.depth + 1,
        lane,
        lane_counter: ctx.lane_counter.clone(),
        cancelled: std::sync::atomic::AtomicBool::new(false),
        read_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
        session_grants: ctx.session_grants.clone(),
        live_sink: std::sync::OnceLock::new(),
        models: ctx.models.clone(),
    };

    let sub_ctx = Arc::new(sub_ctx);
    let _ = sub_ctx
        .hooks
        .fire(
            crate::hooks::HookEvent::SubagentStart,
            &sub_ctx.cwd,
            &crate::hooks::HookInput {
                tool_input: Some(&json!({"prompt": prompt})),
                ..Default::default()
            },
        )
        .await;
    let agent = AgentLoop::new(sub_ctx.clone()).with_max_iterations(24);
    let obs = RelayObserver {
        text: std::sync::Mutex::new(String::new()),
        sink: ctx.live_sink.get().cloned(),
        lane,
    };
    let outcome = agent.run_turn(prompt, &obs).await;
    let _ = sub_ctx
        .hooks
        .fire(
            crate::hooks::HookEvent::SubagentStop,
            &sub_ctx.cwd,
            &crate::hooks::HookInput::default(),
        )
        .await;
    let text = obs.text.lock().unwrap().clone();
    match outcome {
        Ok(_) => ToolResult {
            output: if text.is_empty() {
                "[sub-agent finished with no text output]".into()
            } else {
                text
            },
            ok: true,
        },
        Err(e) => ToolResult {
            output: format!("sub-agent failed: {e:#}"),
            ok: false,
        },
    }
}
