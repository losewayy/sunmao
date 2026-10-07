//! `TodoWrite` — the model's session task list, a first-class native tool
//! (SPEC §4.3). The list is *product semantics*: the model keeps it, the
//! kernel persists it (`SessionEvent::Todos`), the session log survives
//! compaction, and every turn re-injects the snapshot as a synthetic user
//! message so a resumed/compacted session never silently loses the plan.
//!
//! Semantics: each call REPLACES the whole list (replace-all, like the
//! Claude tool this name mirrors). Exactly one item may be `in_progress`;
//! extras are demoted to `pending` and the demotion is reported, never a
//! hard error — the tool exists to help the model self-organize, not to
//! reject it.

use crate::context::MutexRecover;
use crate::tool::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// Model-facing render of the list — injected at the head of every
/// request's folded messages so the snapshot survives compaction and
/// resume (the log is source of truth; this is just the reminder).
pub fn inject_text(items: &[TodoItem]) -> String {
    let mut s = String::from("[task list — maintained via TodoWrite]\n");
    s.push_str(&render(items));
    s
}

/// Human-facing render — `/todos` output and the tool's own echo.
pub fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "(empty)".to_string();
    }
    items
        .iter()
        .map(|t| {
            let mark = match t.status {
                TodoStatus::Done => "x",
                TodoStatus::InProgress => ">",
                TodoStatus::Pending => " ",
            };
            format!("- [{mark}] {}", t.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `read_dir`-style fast prefilter for log lines — the serializer emits
/// `"type"` first, so a `Todos` line always starts with this literal.
/// Used by reseed scans that walk possibly-large logs.
pub(crate) const TODOS_LINE_PREFIX: &str = "{\"type\":\"todos\"";

pub struct TodoWriteTool;

#[async_trait::async_trait]
impl ToolImpl for TodoWriteTool {
    fn name(&self) -> &'static str {
        "TodoWrite"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "TodoWrite",
            "Replace the session task list wholesale. Use it to plan \
             multi-step work and record progress: the list persists across \
             compaction and /resume. Exactly one item may be in_progress.",
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string", "description": "imperative task"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "done"]}
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        )
    }

    async fn call(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            todos: Vec<TodoItem>,
        }
        let a: Args = serde_json::from_value(args)?;
        if a.todos.iter().any(|t| t.content.trim().is_empty()) {
            return Ok(ToolResult {
                exit_code: None,
                output: "TodoWrite rejected: every item needs non-empty content".into(),
                ok: false,
            });
        }
        // at most one in_progress — demote extras to pending and say so;
        // the model sees the fix in the echo instead of a refusal.
        let mut items = a.todos;
        let mut seen = false;
        let mut demoted = 0usize;
        for t in &mut items {
            if t.status == TodoStatus::InProgress {
                if seen {
                    t.status = TodoStatus::Pending;
                    demoted += 1;
                } else {
                    seen = true;
                }
            }
        }
        // durable fact first — the log is source of truth for resume —
        // then the live snapshot readers (/todos, turn injection) see it.
        {
            let mut log = ctx.sessions.lock().await;
            log.append_audit(&crate::session::SessionEvent::Todos {
                items: items.clone(),
            })
            .await;
        }
        *ctx.todos.lock_or_recover() = items.clone();
        // live mirror of the durable Todos fact — a watching frontend renders
        // the same list a replay would fold, not just the tool's echo text
        if let Some(sink) = ctx.live_sink.get() {
            sink.on_event(&crate::agent::LiveEvent::Todos {
                items: items.clone(),
            });
        }
        let mut out = format!(
            "task list updated ({} items):\n{}",
            items.len(),
            render(&items)
        );
        if demoted > 0 {
            out.push_str(&format!(
                "\n[{demoted} extra in_progress item(s) demoted to pending — keep exactly one]"
            ));
        }
        Ok(ToolResult {
            exit_code: None,
            output: out,
            ok: true,
        })
    }
}
