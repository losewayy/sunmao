//! The submission driver and its builtin slash commands — same role as
//! tui/driver.rs: one turn at a time over a FIFO channel, builtins
//! resolved locally, prompts stream LiveEvents via WsObserver.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use crate::tui;

use super::{Shared, WsObserver, fork_or_resume, slash_candidates};

/// The submission driver: same role as tui/driver.rs — one turn at a time,
/// slash builtins resolved here, prompts stream LiveEvents via WsObserver.
pub(super) async fn driver(s: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(input) = rx.recv().await {
        s.busy.fetch_add(1, Ordering::Relaxed);
        let _ = s.live.send(serde_json::json!({"type":"busy","busy":true}));
        if let Some(cmd_line) = input.trim().strip_prefix('/') {
            let name = cmd_line.split_whitespace().next().unwrap_or("");
            let rest = cmd_line[name.len()..].trim();
            if dispatch_builtin(&s, name, rest).await {
                // handled locally — no model turn
            } else if let Some(body) = crate::tui::slash::command_body(&s.cwd, &s.roots, name) {
                let prompt = crate::tui::slash::expand_command(&body, rest);
                let obs = WsObserver(s.live.clone());
                let _ = s.agent.run_turn(&prompt, &obs).await;
            } else {
                let _ = s.live.send(serde_json::json!({
                    "type": "note",
                    "text": format!("[unknown command: /{name}]"),
                }));
            }
        } else {
            let obs = WsObserver(s.live.clone());
            let _ = s.agent.run_turn(&input, &obs).await;
        }
        s.busy.fetch_sub(1, Ordering::Relaxed);
        let _ = s.live.send(serde_json::json!({"type":"busy","busy":false}));
    }
}

/// TUI-parity builtins — true when handled. The responses go out over the
/// broadcast so every connected tab sees the same session state.
async fn dispatch_builtin(s: &Arc<Shared>, name: &str, rest: &str) -> bool {
    let note = |t: String| {
        let _ = s.live.send(serde_json::json!({"type":"note","text":t}));
    };
    match name {
        "compact" => {
            let obs = WsObserver(s.live.clone());
            match s.agent.compact(&obs, "manual").await {
                Ok(sum) if sum.is_empty() => note("[compacted: nothing to fold]".into()),
                Ok(sum) => note(format!("[compacted]\n{sum}")),
                Err(e) => note(format!("[compact failed] {e:#}")),
            }
            true
        }
        "resume" => {
            if rest.is_empty() {
                let list = tui::menu::recent_sessions(&s.cwd, 8)
                    .iter()
                    .map(|i| format!("  {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                note(if list.is_empty() {
                    "[no sessions]".into()
                } else {
                    format!("recent sessions:\n{list}")
                });
            } else if let Err(e) = fork_or_resume(s, rest, false).await {
                note(format!("[resume failed] {e:#}"));
            }
            true
        }
        "sessions" => {
            let list = tui::menu::recent_sessions(&s.cwd, 8).join("\n");
            note(if list.is_empty() {
                "[no sessions]".into()
            } else {
                format!("recent sessions:\n{list}")
            });
            true
        }
        "fork" => {
            if rest.is_empty() {
                note("[usage: /fork <id>]".into());
            } else if let Err(e) = fork_or_resume(s, rest, true).await {
                note(format!("[fork failed] {e:#}"));
            } else {
                note(format!("[forked {rest}]"));
            }
            true
        }
        "model" => {
            if rest.is_empty() {
                let c = s.agent.model_choices();
                note(if c.is_empty() {
                    "[no models.json — session model only]".into()
                } else {
                    format!("available models:\n{}", c.join("\n"))
                });
            } else {
                match s.agent.swap_model(rest) {
                    Some(label) => {
                        s.agent.record_model_change(rest, &label).await;
                        let _ = s
                            .live
                            .send(serde_json::json!({"type":"model","label":label}));
                    }
                    None => note(format!("[unknown selector: {rest}]")),
                }
            }
            true
        }
        "tasks" => {
            let tasks = s.agent.task_roster();
            note(if tasks.is_empty() {
                "[no sub-agents this session]".into()
            } else {
                let rows = tasks
                    .iter()
                    .map(|t| {
                        let st = match t.done {
                            None => "running",
                            Some(true) => "done",
                            Some(false) => "failed",
                        };
                        format!("  {st:<7} {} — {}", t.id, t.prompt)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("sub-agents:\n{rows}")
            });
            true
        }
        "todos" => {
            let items = s.agent.todos();
            note(if items.is_empty() {
                "[no task list — TodoWrite creates it]".into()
            } else {
                format!("task list:\n{}", sunmao_core::tool::render_todos(&items))
            });
            true
        }
        "artifacts" => {
            note(crate::tui::slash::artifacts_text(&s.cwd));
            true
        }
        "annotate" => {
            let mut it = rest.splitn(2, char::is_whitespace);
            match (it.next(), it.next()) {
                (Some(n), Some(t)) => note(crate::tui::slash::annotate(&s.cwd, n, t.trim())),
                _ => note("[usage: /annotate <name> <note>]".into()),
            }
            true
        }
        "help" => {
            note(format!(
                "slash commands: {}",
                slash_candidates(s).join("  ")
            ));
            true
        }
        _ => false,
    }
}
