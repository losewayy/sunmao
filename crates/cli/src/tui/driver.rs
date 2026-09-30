//! Submission driver — the background task that consumes `Submit`s from the
//! composer and streams `LiveEvent`s back as `Msg`s. `/name` file commands
//! resolve here (needs cwd); builtins are already resolved into `Submit`
//! variants by the app.

use std::sync::Arc;

use sunmao_core::agent::{AgentLoop, LiveEvent};
use tokio::sync::mpsc;

use super::app::Submit;
use super::{menu, slash, ChanObserver, Msg};

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn(
    agent: Arc<AgentLoop>,
    tx_msg: mpsc::UnboundedSender<Msg>,
    mut rx_input: mpsc::UnboundedReceiver<Submit>,
    mut rx_cancel: mpsc::UnboundedReceiver<()>,
    cwd: std::path::PathBuf,
    extra_roots: Vec<std::path::PathBuf>,
) {
    tokio::spawn(async move {
        while let Some(sub) = rx_input.recv().await {
            match sub {
                Submit::Quit => {
                    let _ = tx_msg.send(Msg::Quit);
                    return;
                }
                Submit::Note(n) => {
                    if !n.is_empty() {
                        let _ = tx_msg.send(Msg::Note(n));
                    }
                    continue;
                }
                Submit::Compact => {
                    let obs = ChanObserver(tx_msg.clone());
                    let note = match agent.compact(&obs, "manual").await {
                        Ok(s) if s.is_empty() => "[compacted: nothing to fold]".to_string(),
                        Ok(s) => format!("[compacted]\n{s}"),
                        Err(e) => format!("[compact failed] {e:#}"),
                    };
                    let _ = tx_msg.send(Msg::Note(note));
                    continue;
                }
                Submit::Flush => {
                    // the app recalled queued items for editing — drop
                    // everything still pending so nothing runs twice.
                    while rx_input.try_recv().is_ok() {}
                    continue;
                }
                Submit::Model(sel) => {
                    match sel {
                        None => {
                            let choices = agent.model_choices();
                            let _ = tx_msg.send(Msg::Note(if choices.is_empty() {
                                "[no models.json — session model only]".into()
                            } else {
                                format!("available models:\n{}", choices.join("\n"))
                            }));
                        }
                        Some(sel) => match agent.swap_model(&sel) {
                            Some(label) => {
                                agent.record_model_change(&sel, &label).await;
                                let _ = tx_msg.send(Msg::Model(label.clone()));
                                let _ = tx_msg.send(Msg::Note(format!("[model → {label}]")));
                            }
                            None => {
                                let _ = tx_msg.send(Msg::Note(format!(
                                    "[unknown selector: {sel} — try /model for the list]"
                                )));
                            }
                        },
                    }
                    continue;
                }
                Submit::Tasks => {
                    // the live roster — detached spawns until done
                    let tasks = agent.task_roster();
                    let text = if tasks.is_empty() {
                        "[no sub-agents this session]".to_string()
                    } else {
                        let rows = tasks
                            .iter()
                            .map(|t| {
                                let status = match t.done {
                                    None => "running",
                                    Some(true) => "done",
                                    Some(false) => "failed",
                                };
                                let agent = t
                                    .agent
                                    .as_deref()
                                    .map(|a| format!(" @{a}"))
                                    .unwrap_or_default();
                                format!("  {status:<7} {}{} — {}", t.id, agent, t.prompt)
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("sub-agents:\n{rows}")
                    };
                    let _ = tx_msg.send(Msg::Note(text));
                    continue;
                }
                Submit::Artifacts => {
                    let _ = tx_msg.send(Msg::Note(slash::artifacts_text(&cwd)));
                    continue;
                }
                Submit::Todos => {
                    let items = agent.todos();
                    let text = if items.is_empty() {
                        "[no task list — TodoWrite creates it]".to_string()
                    } else {
                        format!("task list:\n{}", sunmao_core::tool::render_todos(&items))
                    };
                    let _ = tx_msg.send(Msg::Note(text));
                    continue;
                }
                Submit::Annotate(name, note) => {
                    let _ = tx_msg.send(Msg::Note(slash::annotate(&cwd, &name, &note)));
                    continue;
                }
                Submit::Resume(arg) => {
                    match arg {
                        None => {
                            // list recent sessions, newest first
                            let entries = menu::recent_sessions(&cwd, 8);
                            let list = entries
                                .iter()
                                .map(|s| format!("  /resume {s}"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            let _ = tx_msg.send(Msg::Note(if list.is_empty() {
                                "[no sessions]".into()
                            } else {
                                format!("recent sessions:\n{list}")
                            }));
                        }
                        Some(id) => {
                            let p = std::path::PathBuf::from(&id);
                            let path = if p.exists() {
                                p
                            } else {
                                cwd.join(".sunmao/sessions").join(format!("{id}.jsonl"))
                            };
                            match sunmao_core::SessionLog::open_path(&path).await {
                                Ok(log) => {
                                    let events = agent.swap_session(log).await;
                                    let _ = tx_msg.send(Msg::Replay(events));
                                }
                                Err(e) => {
                                    let _ =
                                        tx_msg.send(Msg::Note(format!("[resume failed] {e:#}")));
                                }
                            }
                        }
                    }
                    continue;
                }
                Submit::Fork(arg) => {
                    // /fork <id>: copy the source log to a fresh id, then
                    // resume the copy — same flow as `--fork`.
                    let Some(src) = arg else {
                        let _ = tx_msg.send(Msg::Note("[usage: /fork <id>]".into()));
                        continue;
                    };
                    let p = std::path::PathBuf::from(&src);
                    let src_path = if p.exists() {
                        p
                    } else {
                        cwd.join(".sunmao/sessions").join(format!("{src}.jsonl"))
                    };
                    let ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();
                    let new_id = format!("s-{ms}-fork");
                    let dst = cwd.join(".sunmao/sessions").join(format!("{new_id}.jsonl"));
                    match std::fs::copy(&src_path, &dst) {
                        Ok(_) => match sunmao_core::SessionLog::open_path(&dst).await {
                            Ok(log) => {
                                let events = agent.swap_session(log).await;
                                let _ =
                                    tx_msg.send(Msg::Note(format!("[forked {src} → {new_id}]")));
                                let _ = tx_msg.send(Msg::Replay(events));
                            }
                            Err(e) => {
                                let _ = tx_msg.send(Msg::Note(format!("[fork failed] {e:#}")));
                            }
                        },
                        Err(e) => {
                            let _ = tx_msg.send(Msg::Note(format!("[fork {src} failed] {e}")));
                        }
                    }
                    continue;
                }
                Submit::Bash(cmd) => {
                    // `!` local shell — the user runs it, so no approval
                    // gate and no LLM involvement. Same deno_task_shell
                    // engine the Bash tool uses; the durable fact folds
                    // into the next turn's context via LocalShell.
                    let _ = tx_msg.send(Msg::Live(LiveEvent::ToolStart {
                        name: "!".into(),
                        summary: format!("$ {cmd}"),
                        depth: 0,
                        lane: 0,
                    }));
                    let shell_cwd = cwd.clone();
                    let (ok, output, code) =
                        match sunmao_core::tool::run_foreground(&cmd, shell_cwd, 120).await {
                            Ok(run) => {
                                let ok = run.exit_code == 0;
                                (ok, sunmao_core::tool::render_run(&run), run.exit_code)
                            }
                            Err(msg) => (false, msg, -1),
                        };
                    agent.record_local_shell(&cmd, code, &output).await;
                    let _ = tx_msg.send(Msg::Live(LiveEvent::ToolDone {
                        name: "!".into(),
                        ok,
                        output,
                        depth: 0,
                        lane: 0,
                    }));
                    continue;
                }
                Submit::Turn(input) => {
                    let prompt = if let Some(cmd_line) = input.trim().strip_prefix('/') {
                        let name = cmd_line.split_whitespace().next().unwrap_or("");
                        let rest = cmd_line[name.len()..].trim();
                        match slash::command_body(&cwd, &extra_roots, name) {
                            Some(body) => slash::expand_command(&body, rest),
                            None => {
                                let _ =
                                    tx_msg.send(Msg::Note(format!("[unknown command: /{name}]")));
                                continue;
                            }
                        }
                    } else {
                        input
                    };
                    let obs = ChanObserver(tx_msg.clone());
                    let mut turn = Box::pin(agent.run_turn(&prompt, &obs));
                    loop {
                        tokio::select! {
                            res = &mut turn => {
                                let _ = res;
                                break;
                            }
                            _ = rx_cancel.recv() => {
                                agent.cancel(); // cooperative: loop sees it next iteration
                            }
                        }
                    }
                }
            }
        }
    });
}
