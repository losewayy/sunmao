//! Submission driver — the background task that consumes `Submit`s from the
//! composer and streams `LiveEvent`s back as `Msg`s. `/name` file commands
//! resolve here (needs cwd); builtins are already resolved into `Submit`
//! variants by the app.

use std::sync::Arc;

use sunmao_core::agent::{AgentLoop, LiveEvent};
use tokio::sync::mpsc;

use super::app::Submit;
use super::{ChanObserver, Msg};
use crate::{commands, sessions};

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
                    let note = commands::compact_note(agent.compact(&obs, "manual").await);
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
                            let _ = tx_msg
                                .send(Msg::Note(commands::models_text(&agent.model_choices())));
                        }
                        Some(sel) => match agent.swap_model(&sel) {
                            Some(label) => {
                                agent.record_model_change(&sel, &label).await;
                                let _ = tx_msg.send(Msg::Model(label.clone()));
                                let _ = tx_msg.send(Msg::Note(format!("[model → {label}]")));
                            }
                            None => {
                                let _ = tx_msg.send(Msg::Note(commands::model_unknown(&sel)));
                            }
                        },
                    }
                    continue;
                }
                Submit::Mode(arg) => {
                    match arg {
                        None => {
                            let _ = tx_msg
                                .send(Msg::Note(commands::mode_list_text(agent.approval_mode())));
                        }
                        Some(name) => match sunmao_core::agent::ApprovalMode::parse(&name) {
                            Some(m) => {
                                agent
                                    .set_approval_mode(m, &ChanObserver(tx_msg.clone()))
                                    .await;
                                let _ = tx_msg.send(Msg::Mode(m));
                                let _ = tx_msg
                                    .send(Msg::Note(format!("[approval mode → {}]", m.as_str())));
                            }
                            None => {
                                let _ = tx_msg.send(Msg::Note(commands::mode_unknown(&name)));
                            }
                        },
                    }
                    continue;
                }
                Submit::Tasks => {
                    // the live roster — detached spawns until done
                    let _ = tx_msg.send(Msg::Note(commands::tasks_text(&agent.task_roster())));
                    continue;
                }
                Submit::Artifacts => {
                    let _ = tx_msg.send(Msg::Note(commands::artifacts_text(&cwd)));
                    continue;
                }
                Submit::Todos => {
                    let _ = tx_msg.send(Msg::Note(commands::todos_text(&agent.todos())));
                    continue;
                }
                Submit::Mcp => {
                    let _ = tx_msg.send(Msg::Note(commands::mcp_text(&agent.mcp_roster())));
                    continue;
                }
                Submit::Status => {
                    let _ = tx_msg.send(Msg::Note(commands::status_text(&agent.status().await)));
                    continue;
                }
                Submit::Annotate(name, note) => {
                    let _ = tx_msg.send(Msg::Note(commands::annotate(&cwd, &name, &note)));
                    continue;
                }
                Submit::Resume(arg) => {
                    match arg {
                        None => {
                            let _ = tx_msg.send(Msg::Note(sessions::recent_sessions_text(&cwd, 8)));
                        }
                        Some(id) => {
                            let path = sessions::resolve_log_path(&cwd, &id);
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
                Submit::Fork(src) => {
                    // /fork <id>: copy the source log to a fresh id, then
                    // resume the copy — same flow as `--fork`.
                    match sessions::fork_copy(&cwd, &src) {
                        Ok((new_id, dst)) => match sunmao_core::SessionLog::open_path(&dst).await {
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
                            let _ = tx_msg.send(Msg::Note(e));
                        }
                    }
                    continue;
                }
                Submit::Rewind(spec) => {
                    match spec {
                        None => {
                            let _ = tx_msg.send(Msg::Note(crate::rewind::list(&agent).await));
                        }
                        Some(spec) => {
                            match crate::rewind::run(&agent, &cwd, spec.turn, spec.mode).await {
                                Ok(crate::rewind::Outcome::Forked { note, events }) => {
                                    let _ = tx_msg.send(Msg::Note(note));
                                    let _ = tx_msg.send(Msg::Replay(events));
                                }
                                Ok(crate::rewind::Outcome::CodeOnly(note)) => {
                                    let _ = tx_msg.send(Msg::Note(note));
                                }
                                Err(e) => {
                                    let _ = tx_msg.send(Msg::Note(e));
                                }
                            }
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
                        call_id: None,
                        args: serde_json::Value::Null,
                    }));
                    let shell_cwd = cwd.clone();
                    let t0 = std::time::Instant::now();
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
                        call_id: None,
                        elapsed_ms: t0.elapsed().as_millis() as u64,
                    }));
                    continue;
                }
                Submit::Turn(input) => {
                    let prompt = if let Some(cmd_line) = input.trim().strip_prefix('/') {
                        let name = cmd_line.split_whitespace().next().unwrap_or("");
                        let rest = cmd_line[name.len()..].trim();
                        match commands::command_body(&cwd, &extra_roots, name) {
                            Some(body) => commands::expand_command(&body, rest),
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
