//! Task tool — spawn a nested agent loop for a self-contained subtask.
//!
//! The subagent gets its own Context (fresh session file, own read ledger,
//! hooks fire in its scope) but shares the provider adapter. Depth-capped so
//! agents can't recurse into themselves.

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
/// TurnEnd is swallowed: a sub-agent's end must not unwind the outer turn.
struct RelayObserver {
    text: std::sync::Mutex<String>,
    sink: Option<Arc<dyn Observer>>,
}

impl Observer for RelayObserver {
    fn on_event(&self, ev: &LiveEvent) {
        match ev {
            LiveEvent::Content(c) => self.text.lock().unwrap().push_str(c),
            // sub-agent lifecycle is the parent's business, not the UI's —
            // forwarding TurnEnd would close the outer transcript early.
            LiveEvent::TurnEnd { .. } => {}
            LiveEvent::ToolStart { .. } | LiveEvent::ToolDone { .. } => {
                if let Some(s) = &self.sink {
                    s.on_event(ev);
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

#[async_trait::async_trait]
impl ToolImpl for TaskTool {
    fn name(&self) -> &'static str {
        "Task"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "Task",
            "Delegate a self-contained subtask to a sub-agent with the same toolset. \
             Returns the sub-agent's final message. Use for parallelizable or \
             scope-isolated work; sub-agents cannot spawn further sub-agents.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Complete instructions for the subtask"},
                    "subagent_type": {"type": "string", "description": "Named agent def from .sunmao/agents/*.md or .claude/agents/*.md"}
                },
                "required": ["prompt"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            prompt: String,
            subagent_type: Option<String>,
        }
        let a: Args = serde_json::from_value(args)?;

        if ctx.depth >= MAX_DEPTH {
            bail!("sub-agent depth limit reached ({MAX_DEPTH})");
        }

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
        let def = a.subagent_type.as_deref().and_then(|t| {
            crate::agents::load_all(&ctx.cwd)
                .into_iter()
                .find(|d| d.name == t)
        });
        let sys_prompt = def
            .as_ref()
            .map(|d| d.system_prompt.clone())
            .unwrap_or_else(|| {
                crate::prompt::PromptAssembler::new(&ctx.cwd)
                    .assemble_subagent(a.subagent_type.as_deref())
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

        // fresh registry + context, one depth deeper
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
                    tool_input: Some(&json!({"prompt": a.prompt})),
                    ..Default::default()
                },
            )
            .await;
        let agent = AgentLoop::new(sub_ctx.clone()).with_max_iterations(24);
        let obs = RelayObserver {
            text: std::sync::Mutex::new(String::new()),
            sink: ctx.live_sink.get().cloned(),
        };
        let outcome = agent.run_turn(&a.prompt, &obs).await;
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
            Ok(_) => Ok(ToolResult {
                output: if text.is_empty() {
                    "[sub-agent finished with no text output]".into()
                } else {
                    text
                },
                ok: true,
            }),
            Err(e) => Ok(ToolResult {
                output: format!("sub-agent failed: {e:#}"),
                ok: false,
            }),
        }
    }
}
