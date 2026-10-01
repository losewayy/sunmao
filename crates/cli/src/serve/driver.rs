//! One driver task per live session — same role as tui/driver.rs: drain
//! the session's FIFO input queue, resolve slash builtins locally, stream
//! LiveEvents via the session-tagged WsObserver. Busy frames carry `sess`
//! so every tab's rail dot tracks its own session.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use sunmao_core::agent::{LiveEvent, Observer as _};

use crate::{commands, sessions};

use super::host::{Host, Input, Shared, WsObserver};

/// Slash-command list for the composer menu — same candidates the TUI
/// shows (builtins + file commands + connected servers' `/srv:prompt`
/// names), minus pure-TUI affordances.
pub(crate) fn slash_candidates(s: &Shared) -> Vec<String> {
    let mut out: Vec<String> = crate::commands::candidates(&s.cwd, &s.roots)
        .into_iter()
        .filter(|n| *n != "multiline" && *n != "clear" && *n != "quit")
        .collect();
    // any live host's prompts complete — catalogs are process-shared, so
    // the first session's `srv:prompt` names describe every server's
    if let Some(host) = s.sessions.lock().unwrap().values().next() {
        out.extend(host.agent.mcp_prompt_names());
    }
    out.sort();
    out.dedup();
    out
}

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
                dispatch_input(
                    &s,
                    &host,
                    Input {
                        client,
                        text,
                        attachments: Vec::new(),
                    },
                )
                .await;
            }
        }
        let Some(input) = rx.recv().await else { break };
        dispatch_input(&s, &host, input).await;
    }
}

/// One queued submission: builtin slash commands resolve locally; file
/// commands expand and run as prompts; everything else is a turn.
/// `@`-mentions in the text attach on top of the frame's explicit
/// `attachments` — the composer uploads images itself, but a typed
/// `@img.png` still lands.
async fn dispatch_input(s: &Arc<Shared>, host: &Arc<Host>, input: Input) {
    let Input {
        client,
        text: input,
        attachments,
    } = input;
    let sess = host.id.clone();
    let emit = |v: serde_json::Value| {
        let _ = s.live.send(v);
    };
    host.busy.fetch_add(1, Ordering::Relaxed);
    emit(serde_json::json!({"type":"busy","sess":sess,"busy":true}));
    let cwd = host.agent.session_cwd();
    if let Some(cmd_line) = input.trim().strip_prefix('/') {
        if dispatch_builtin(s, host, cmd_line, client).await {
            // handled locally — no model turn
        } else {
            let name = cmd_line.split_whitespace().next().unwrap_or("");
            let rest = cmd_line[name.len()..].trim();
            // `/srv:prompt` resolves through the MCP server before file
            // commands — the server owns the name
            enum Resolved {
                Prompt(String),
                Failed,
                Missing,
            }
            let resolved = match host.agent.mcp_prompt_text(name, rest).await {
                Some(Ok(text)) => Resolved::Prompt(text),
                Some(Err(e)) => {
                    emit(serde_json::json!({
                        "type": "note", "sess": sess,
                        "text": format!("[mcp prompt /{name} failed] {e:#}"),
                    }));
                    Resolved::Failed
                }
                None => match commands::command_body(&s.cwd, &s.roots, name) {
                    Some(body) => Resolved::Prompt(commands::expand_command(&body, rest)),
                    None => {
                        emit(serde_json::json!({
                            "type": "note", "sess": sess,
                            "text": format!("[unknown command: /{name}]"),
                        }));
                        Resolved::Missing
                    }
                },
            };
            if let Resolved::Prompt(text) = resolved {
                let (prompt, mut atts) = crate::attachments::attach_mentions(&text, &cwd);
                atts.extend(attachments);
                let obs = WsObserver::new(s.live.clone(), sess.clone());
                let _ = host.agent.run_turn_blocks(&prompt, &atts, &obs).await;
            }
        }
    } else {
        let (text, mut atts) = crate::attachments::attach_mentions(&input, &cwd);
        atts.extend(attachments);
        let obs = WsObserver::new(s.live.clone(), sess.clone());
        let _ = host.agent.run_turn_blocks(&text, &atts, &obs).await;
    }
    host.busy.fetch_sub(1, Ordering::Relaxed);
    emit(serde_json::json!({"type":"busy","sess":sess,"busy":false}));
}

/// Builtin slash commands — `commands::parse` owns the vocabulary and
/// arg grammar; the arms here are only the serve-side execution (mgmt
/// oneshots + broadcast frames; REPL/TUI call the same core APIs
/// directly). Replies go out over the global bus tagged with THIS
/// session (`sess`), so only tabs viewing it render the note. A
/// `session` switch frame carries the issuing client's id so only that
/// tab follows — other tabs viewing the same session stay put. True when
/// handled; `Other` and frontend-local commands fall through to the
/// file-command path.
async fn dispatch_builtin(s: &Arc<Shared>, host: &Arc<Host>, cmd_line: &str, client: u64) -> bool {
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
    match commands::parse(cmd_line) {
        commands::Command::Compact => {
            let obs = WsObserver::new(s.live.clone(), host.id.clone());
            note(commands::compact_note(
                host.agent.compact(&obs, "manual").await,
            ));
            true
        }
        commands::Command::Mode(arg) => {
            match arg {
                None => note(commands::mode_list_text(host.agent.approval_mode())),
                Some(name) => match sunmao_core::agent::ApprovalMode::parse(&name) {
                    Some(m) => {
                        host.agent
                            .set_approval_mode(m, &WsObserver::new(s.live.clone(), sess.clone()))
                            .await;
                        let _ = s.live.send(serde_json::json!({
                            "type":"mode","sess":sess,"mode":m.as_str(),
                        }));
                    }
                    None => note(commands::mode_unknown(&name)),
                },
            }
            true
        }
        commands::Command::Resume(arg) | commands::Command::Sessions(arg) => {
            match arg {
                None => {
                    note(sessions::recent_sessions_text(&host.agent.session_cwd(), 8));
                }
                Some(id) => match adopt_via_mgmt(s, &id, false).await {
                    Ok(id) => switch(id),
                    Err(e) => note(format!("[resume failed] {e}")),
                },
            }
            true
        }
        commands::Command::Fork(id) => {
            match adopt_via_mgmt(s, &id, true).await {
                Ok(new_id) => {
                    note(format!("[forked {id} → {new_id}]"));
                    switch(new_id);
                }
                Err(e) => note(format!("[fork failed] {e}")),
            }
            true
        }
        commands::Command::Rewind(spec) => {
            match spec {
                None => note(crate::rewind::list(&host.agent).await),
                Some(spec) => {
                    let mode = match spec.mode {
                        crate::rewind::Mode::Both => super::host::RewindMode::Both,
                        crate::rewind::Mode::Session => super::host::RewindMode::Session,
                        crate::rewind::Mode::Code => super::host::RewindMode::Code,
                    };
                    match rewind_via_mgmt(s, &host.id, spec.turn, mode).await {
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
                                    note(format!(
                                        "[rewound to turn {} — {files}, session → {id}]",
                                        spec.turn
                                    ));
                                    switch(id);
                                }
                                None => note(format!("[rewound to turn {} — {files}]", spec.turn)),
                            }
                        }
                        Err(e) => note(format!("[rewind failed] {e}")),
                    }
                }
            }
            true
        }
        commands::Command::Model(arg) => {
            match arg {
                None => note(commands::models_text(&host.agent.model_choices())),
                Some(sel) => match host.agent.swap_model(&sel) {
                    Some(label) => {
                        host.agent.record_model_change(&sel, &label).await;
                        let _ = s.live.send(serde_json::json!({
                            "type":"model","sess":sess,"label":label,
                        }));
                    }
                    None => note(commands::model_unknown(&sel)),
                },
            }
            true
        }
        commands::Command::Tasks => {
            note(commands::tasks_text(&host.agent.task_roster()));
            true
        }
        commands::Command::Todos => {
            note(commands::todos_text(&host.agent.todos()));
            true
        }
        commands::Command::Mcp => {
            note(commands::mcp_text(&host.agent.mcp_roster()));
            true
        }
        commands::Command::Status => {
            note(commands::status_text(&host.agent.status().await));
            true
        }
        commands::Command::Artifacts => {
            note(commands::artifacts_text(&host.agent.session_cwd()));
            true
        }
        commands::Command::Search(q) => {
            // cross-session grep over every known sessions dir — the same
            // helper the REST `GET /sessions?q=` arm runs
            note(sessions::search_text(&super::host::session_dirs(s), &q));
            true
        }
        commands::Command::Annotate(name, text) => {
            note(commands::annotate(&host.agent.session_cwd(), &name, &text));
            true
        }
        commands::Command::Help => {
            note(format!(
                "slash commands: {}",
                slash_candidates(s).join("  ")
            ));
            true
        }
        commands::Command::Note(n) => {
            note(n);
            true
        }
        // frontend-local (quit/clear/multiline) and unknown names fall
        // through to the file-command path
        commands::Command::Quit
        | commands::Command::Clear
        | commands::Command::Multiline
        | commands::Command::Other => false,
    }
}
