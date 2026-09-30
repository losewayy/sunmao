//! One driver task per live session — same role as tui/driver.rs: drain
//! the session's FIFO input queue, resolve slash builtins locally, stream
//! LiveEvents via the session-tagged WsObserver. Busy frames carry `sess`
//! so every tab's rail dot tracks its own session.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use crate::tui;

use super::host::{Host, Input, Shared, WsObserver, slash_candidates};

/// Ask the host's mgmt lane to adopt a session — drivers can't call
/// `adopt`/`fork_or_resume` directly: adopt spawns drivers, so an awaited
/// call would make the driver's future self-referential and rustc can't
/// close its Send proof. The oneshot keeps the reply explicit.
async fn adopt_via_mgmt(s: &Arc<Shared>, id: &str, fork: bool) -> Result<String, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    s.mgmt
        .send(super::host::SessionOp::Adopt {
            id: id.to_string(),
            fork,
            reply: tx,
        })
        .map_err(|_| "host mgmt channel closed".to_string())?;
    rx.await.map_err(|_| "host mgmt dropped".to_string())?
}

/// The submission driver for ONE session host — one turn at a time,
/// slash builtins resolved here, prompts stream LiveEvents tagged with
/// this session's id.
pub(super) async fn driver(
    s: Arc<Shared>,
    host: Arc<Host>,
    mut rx: mpsc::UnboundedReceiver<Input>,
) {
    let sess = host.id.clone();
    let emit = |v: serde_json::Value| {
        let _ = s.live.send(v);
    };
    while let Some(input) = rx.recv().await {
        let client = input.client;
        let input = input.text;
        host.busy.fetch_add(1, Ordering::Relaxed);
        emit(serde_json::json!({"type":"busy","sess":sess,"busy":true}));
        if let Some(cmd_line) = input.trim().strip_prefix('/') {
            let name = cmd_line.split_whitespace().next().unwrap_or("");
            let rest = cmd_line[name.len()..].trim();
            if dispatch_builtin(&s, &host, name, rest, client).await {
                // handled locally — no model turn
            } else if let Some(body) = crate::tui::slash::command_body(&s.cwd, &s.roots, name) {
                let prompt = crate::tui::slash::expand_command(&body, rest);
                let obs = WsObserver::new(s.live.clone(), sess.clone());
                let _ = host.agent.run_turn(&prompt, &obs).await;
            } else {
                emit(serde_json::json!({
                    "type": "note", "sess": sess,
                    "text": format!("[unknown command: /{name}]"),
                }));
            }
        } else {
            let obs = WsObserver::new(s.live.clone(), sess.clone());
            let _ = host.agent.run_turn(&input, &obs).await;
        }
        host.busy.fetch_sub(1, Ordering::Relaxed);
        emit(serde_json::json!({"type":"busy","sess":sess,"busy":false}));
    }
}

/// TUI-parity builtins — true when handled. Replies go out over the global
/// bus tagged with THIS session (`sess`), so only tabs viewing it render
/// the note. A `session` switch frame carries the issuing client's id so
/// only that tab follows — other tabs viewing the same session stay put.
async fn dispatch_builtin(
    s: &Arc<Shared>,
    host: &Arc<Host>,
    name: &str,
    rest: &str,
    client: u64,
) -> bool {
    let sess = host.id.clone();
    let note = |t: String| {
        let _ = s
            .live
            .send(serde_json::json!({"type":"note","sess":sess,"text":t}));
    };
    let switch = |new_id: String| {
        let _ = s.live.send(serde_json::json!({
            "type": "session", "sess": new_id, "id": new_id, "from": sess,
            "client": client,
        }));
    };
    match name {
        "compact" => {
            let obs = WsObserver::new(s.live.clone(), host.id.clone());
            match host.agent.compact(&obs, "manual").await {
                Ok(sum) if sum.is_empty() => note("[compacted: nothing to fold]".into()),
                Ok(sum) => note(format!("[compacted]\n{sum}")),
                Err(e) => note(format!("[compact failed] {e:#}")),
            }
            true
        }
        "mode" => {
            if rest.is_empty() {
                note(format!(
                    "approval mode: {}",
                    host.agent.approval_mode().as_str()
                ));
            } else {
                match sunmao_core::agent::ApprovalMode::parse(rest) {
                    Some(m) => {
                        host.agent
                            .set_approval_mode(m, &WsObserver::new(s.live.clone(), sess.clone()))
                            .await;
                        let _ = s.live.send(serde_json::json!({
                            "type":"mode","sess":sess,"mode":m.as_str(),
                        }));
                    }
                    None => note(format!("[unknown mode: {rest}]")),
                }
            }
            true
        }
        "resume" => {
            if rest.is_empty() {
                let list = tui::menu::recent_sessions(&host.agent.session_cwd(), 8)
                    .iter()
                    .map(|i| format!("  {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                note(if list.is_empty() {
                    "[no sessions]".into()
                } else {
                    format!("recent sessions:\n{list}")
                });
            } else {
                match adopt_via_mgmt(s, rest, false).await {
                    Ok(id) => switch(id),
                    Err(e) => note(format!("[resume failed] {e}")),
                }
            }
            true
        }
        "sessions" => {
            let list = tui::menu::recent_sessions(&host.agent.session_cwd(), 8).join("\n");
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
            } else {
                match adopt_via_mgmt(s, rest, true).await {
                    Ok(id) => {
                        note(format!("[forked {rest} → {id}]"));
                        switch(id);
                    }
                    Err(e) => note(format!("[fork failed] {e}")),
                }
            }
            true
        }
        "model" => {
            if rest.is_empty() {
                let c = host.agent.model_choices();
                note(if c.is_empty() {
                    "[no models.json — session model only]".into()
                } else {
                    format!("available models:\n{}", c.join("\n"))
                });
            } else {
                match host.agent.swap_model(rest) {
                    Some(label) => {
                        host.agent.record_model_change(rest, &label).await;
                        let _ = s.live.send(serde_json::json!({
                            "type":"model","sess":sess,"label":label,
                        }));
                    }
                    None => note(format!("[unknown selector: {rest}]")),
                }
            }
            true
        }
        "tasks" => {
            let tasks = host.agent.task_roster();
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
            let items = host.agent.todos();
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
