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

/// Subagent events aren't streamed to the outer observer — collect the final
/// assistant text and return it as the tool result.
struct CollectObserver {
    text: std::sync::Mutex<String>,
}

impl Observer for CollectObserver {
    fn on_event(&self, ev: &LiveEvent) {
        if let LiveEvent::Content(c) = ev {
            self.text.lock().unwrap().push_str(c);
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
                    "prompt": {"type": "string", "description": "Complete instructions for the subtask"}
                },
                "required": ["prompt"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            prompt: String,
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
        {
            let _ = log
                .append(&SessionEvent::Message {
                    message: sunmao_llm::types::Message::system(
                        "You are a sunmao sub-agent. Complete the delegated task and reply \
                         concisely with the result.",
                    ),
                })
                .await;
        }

        // fresh registry + context, one depth deeper
        let sub_ctx = Context {
            llm: ctx.llm.clone(),
            sessions: tokio::sync::Mutex::new(log),
            tools: builtin_registry(),
            audit: crate::audit::AuditLog::new(),
            hooks: crate::hooks::HookEngine::load(&ctx.cwd, &sub_id),
            cwd: ctx.cwd.clone(),
            depth: ctx.depth + 1,
            cancelled: std::sync::atomic::AtomicBool::new(false),
            read_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
        };

        let agent = AgentLoop::new(Arc::new(sub_ctx)).with_max_iterations(24);
        let obs = CollectObserver {
            text: std::sync::Mutex::new(String::new()),
        };
        let outcome = agent.run_turn(&a.prompt, &obs).await;
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
