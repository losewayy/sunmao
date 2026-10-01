//! One driver task per live session — same role as tui/driver.rs: drain
//! the session's FIFO input queue, resolve slash builtins locally, stream
//! LiveEvents via the session-tagged WsObserver. Busy frames carry `sess`
//! so every tab's rail dot tracks its own session.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use sunmao_core::agent::{LiveEvent, Observer as _};

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

/// Same mgmt lane for `/rewind` — the reply is the JSON payload
/// `{"session","restored"}` serialized to a string.
async fn rewind_via_mgmt(
    s: &Arc<Shared>,
    id: &str,
    upto_turn: u64,
    mode: super::host::RewindMode,
) -> Result<String, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    s.mgmt
        .send(super::host::SessionOp::Rewind {
            id: id.to_string(),
            upto_turn,
            mode,
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
    loop {
        // Leftover steering becomes follow-up input: a steer queued as the
        // turn ended would otherwise wait for the next prompt (or silently
        // queue) — claim it before blocking on the FIFO. Emit the emptied
        // queue + the same "steer" live event the in-turn drain sends, so
        // the tab's chip row clears and the message lands as a user bubble.
        let leftovers = host.agent.drain_steer();
        if !leftovers.is_empty() {
            let _ = s.live.send(serde_json::json!({
                "type":"steer_queue","sess":host.id,"items":[],
            }));
            let obs = WsObserver::new(s.live.clone(), host.id.clone());
            for (client, text) in leftovers {
                obs.on_event(&LiveEvent::Hook {
                    event: "steer".into(),
                    detail: text.clone(),
                });
                dispatch_input(&s, &host, client, text).await;
            }
        }
        let Some(input) = rx.recv().await else { break };
        dispatch_input(&s, &host, input.client, input.text).await;
    }
}

/// One queued submission: builtin slash commands resolve locally; file
/// commands expand and run as prompts; everything else is a turn.
async fn dispatch_input(s: &Arc<Shared>, host: &Arc<Host>, client: u64, input: String) {
    let sess = host.id.clone();
    let emit = |v: serde_json::Value| {
        let _ = s.live.send(v);
    };
    host.busy.fetch_add(1, Ordering::Relaxed);
    emit(serde_json::json!({"type":"busy","sess":sess,"busy":true}));
    if let Some(cmd_line) = input.trim().strip_prefix('/') {
        let name = cmd_line.split_whitespace().next().unwrap_or("");
        let rest = cmd_line[name.len()..].trim();
        if dispatch_builtin(s, host, name, rest, client).await {
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
        "rewind" => {
            if rest.is_empty() {
                let bounds =
                    sunmao_core::checkpoints::turn_boundaries(&host.agent.session_path().await);
                note(if bounds.is_empty() {
                    "[no turns to rewind to]".into()
                } else {
                    let rows = bounds
                        .iter()
                        .map(|b| format!("  {}  {}", b.n, b.preview))
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("turn boundaries — /rewind <n> [session|code|both]:\n{rows}")
                });
                return true;
            }
            let mut it = rest.split_whitespace();
            let n: Option<u64> = it.next().and_then(|t| t.parse().ok()).filter(|n| *n >= 1);
            let mode = crate::rewind::Mode::parse(it.next());
            let (Some(n), Some(mode)) = (n, mode) else {
                note("[usage: /rewind <n> [session|code|both]]".into());
                return true;
            };
            if it.next().is_some() {
                note("[usage: /rewind <n> [session|code|both]]".into());
                return true;
            }
            let rewind_mode = match mode {
                crate::rewind::Mode::Both => super::host::RewindMode::Both,
                crate::rewind::Mode::Session => super::host::RewindMode::Session,
                crate::rewind::Mode::Code => super::host::RewindMode::Code,
            };
            match rewind_via_mgmt(s, &host.id, n, rewind_mode).await {
                Ok(body) => {
                    let v: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                    let restored = v["restored"].as_array().map(|a| a.len()).unwrap_or(0);
                    let new_id = v["session"].as_str().map(|s| s.to_string());
                    let files = if restored == 0 {
                        "no files to restore".to_string()
                    } else {
                        format!("{restored} file(s) restored")
                    };
                    match new_id {
                        Some(id) => {
                            note(format!("[rewound to turn {n} — {files}, session → {id}]"));
                            switch(id);
                        }
                        None => note(format!("[rewound to turn {n} — {files}]")),
                    }
                }
                Err(e) => note(format!("[rewind failed] {e}")),
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
