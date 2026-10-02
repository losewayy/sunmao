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
        // Submissions FIFO the app mirrors as `queue`. `Flush(n)` pops the
        // tail of what hasn't run yet — recall-by-count, not a full drain,
        // so items queued *before* the recalled one survive.
        let mut pending: std::collections::VecDeque<Submit> = std::collections::VecDeque::new();
        // `Flush(n)` isn't a runnable submission — it's a tail-pop on the
        // backlog, applied the moment it's seen so the recalled item never
        // gets a chance to run before the flush catches up with it.
        // Every enqueued submission bumps ctx.input_pending — the goal
        // chain yields while the counter is non-zero so a typed prompt
        // interleaves instead of waiting out the whole loop. The count
        // follows pending exactly: push +1, pop/flush −1.
        let ctr = &agent.context().input_pending;
        fn intake(
            pending: &mut std::collections::VecDeque<Submit>,
            s: Submit,
            ctr: &std::sync::atomic::AtomicUsize,
        ) {
            match s {
                Submit::Flush(n) => {
                    let keep = pending.len().saturating_sub(n);
                    let dropped = pending.len() - keep;
                    pending.truncate(keep);
                    for _ in 0..dropped {
                        ctr.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                _ => {
                    ctr.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    pending.push_back(s);
                }
            }
        }
        loop {
            if pending.is_empty() {
                match rx_input.recv().await {
                    Some(s) => intake(&mut pending, s, ctr),
                    None => return,
                }
            }
            while let Ok(s) = rx_input.try_recv() {
                intake(&mut pending, s, ctr);
            }
            let Some(sub) = pending.pop_front() else {
                continue;
            };
            ctr.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
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
                Submit::Flush(_) => {
                    // intake() consumes Flush before it can reach the
                    // backlog — defensive no-op if a sender bypassed it.
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
                                // the new model advertises a different level
                                // vocabulary — the Args pool follows
                                let _ = tx_msg.send(Msg::Effort(
                                    agent.reasoning_effort(),
                                    agent.effort_levels().await,
                                ));
                                let _ = tx_msg.send(Msg::Note(format!("[model → {label}]")));
                            }
                            None => {
                                let _ = tx_msg.send(Msg::Note(commands::model_unknown(&sel)));
                            }
                        },
                    }
                    continue;
                }
                Submit::Effort(arg) => {
                    match arg {
                        None => {
                            let _ = tx_msg.send(Msg::Note(commands::effort_text(
                                agent.reasoning_effort().as_deref(),
                                &agent.effort_levels().await,
                            )));
                        }
                        Some(level) => {
                            agent
                                .set_reasoning_effort(Some(&level), &ChanObserver(tx_msg.clone()))
                                .await;
                            let _ = tx_msg.send(Msg::Effort(
                                agent.reasoning_effort(),
                                agent.effort_levels().await,
                            ));
                            let _ = tx_msg.send(Msg::Note(commands::effort_note(
                                agent.reasoning_effort().as_deref(),
                            )));
                        }
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
                Submit::Stop(id) => {
                    // slash-path kill — same `cancel_sub` as serve's
                    // `task_cancel` frame and the GUI roster's 终止 button
                    let note = match agent.cancel_sub(&id).await {
                        Ok(()) => format!("cancelled {id}"),
                        Err(e) => format!("[stop failed] {e}"),
                    };
                    let _ = tx_msg.send(Msg::Note(note));
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
                Submit::Goal(arg) => {
                    match arg {
                        None => {
                            let _ =
                                tx_msg.send(Msg::Note(commands::goal_text(agent.goal().as_ref())));
                        }
                        Some(objective) => {
                            let obs = ChanObserver(tx_msg.clone());
                            if let Err(e) = agent.set_goal(&objective, &obs).await {
                                let _ = tx_msg.send(Msg::Note(format!("[goal failed] {e:#}")));
                                continue;
                            }
                            let _ = tx_msg.send(Msg::Note(
                                "[goal set — the agent keeps at it until complete/blocked]".into(),
                            ));
                            // the objective itself is the kickoff prompt —
                            // run_turn's goal chain continues from there
                            let (prompt, atts) =
                                crate::attachments::attach_mentions(&objective, &cwd);
                            let mut turn = Box::pin(agent.run_turn_blocks(&prompt, &atts, &obs));
                            loop {
                                tokio::select! {
                                    res = &mut turn => {
                                        let _ = res;
                                        break;
                                    }
                                    _ = rx_cancel.recv() => {
                                        agent.cancel();
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }
                Submit::GoalClear => {
                    let obs = ChanObserver(tx_msg.clone());
                    match agent.clear_goal(&obs).await {
                        Ok(()) => {
                            let _ = tx_msg.send(Msg::Note("[goal cleared]".into()));
                        }
                        Err(e) => {
                            let _ = tx_msg.send(Msg::Note(format!("[goal clear failed] {e:#}")));
                        }
                    }
                    continue;
                }
                Submit::Mcp => {
                    let _ = tx_msg.send(Msg::Note(commands::mcp_text(&agent.mcp_roster())));
                    continue;
                }
                Submit::Hooks(op) => {
                    let note = match op {
                        commands::HookOp::List => {
                            commands::hooks_text(&agent.context().hooks.roster())
                        }
                        commands::HookOp::Trust(n) | commands::HookOp::Untrust(n) => {
                            match agent
                                .set_hook_trust(n, matches!(op, commands::HookOp::Trust(_)))
                                .await
                            {
                                Ok(d) => format!("[{d}]"),
                                Err(e) => format!("[hooks: {e}]"),
                            }
                        }
                    };
                    let _ = tx_msg.send(Msg::Note(note));
                    continue;
                }
                Submit::Status => {
                    let _ = tx_msg.send(Msg::Note(commands::status_text(&agent.status().await)));
                    continue;
                }
                Submit::Export => {
                    let events = agent.session_events().await;
                    let msg = match commands::export_md(&cwd, &agent.session_id(), &events) {
                        Ok(p) => format!("[exported → {p}]"),
                        Err(e) => format!("[export failed] {e:#}"),
                    };
                    let _ = tx_msg.send(Msg::Note(msg));
                    continue;
                }
                Submit::ExportZip => {
                    let events = agent.session_events().await;
                    let log = agent.session_path().await;
                    let msg = match commands::export_zip(&cwd, &agent.session_id(), &events, &log) {
                        Ok(p) => format!("[exported → {p}]"),
                        Err(e) => format!("[export failed] {e:#}"),
                    };
                    let _ = tx_msg.send(Msg::Note(msg));
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
                                    // the swapped log reseeds approval_mode
                                    // — tell the footer before the replay,
                                    // else it keeps showing the old stance.
                                    let _ = tx_msg.send(Msg::Mode(agent.approval_mode()));
                                    let _ = tx_msg.send(Msg::Effort(
                                        agent.reasoning_effort(),
                                        agent.effort_levels().await,
                                    ));
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
                                let _ = tx_msg.send(Msg::Mode(agent.approval_mode()));
                                let _ = tx_msg.send(Msg::Effort(
                                    agent.reasoning_effort(),
                                    agent.effort_levels().await,
                                ));
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
                            // checkpoint dirs key off the SESSION's project,
                            // not the shell cwd — a session resumed from
                            // elsewhere restores nothing under a wrong root.
                            match crate::rewind::run(
                                &agent,
                                &agent.session_cwd(),
                                spec.turn,
                                spec.mode,
                            )
                            .await
                            {
                                Ok(crate::rewind::Outcome::Forked { note, events }) => {
                                    let _ = tx_msg.send(Msg::Note(note));
                                    let _ = tx_msg.send(Msg::Mode(agent.approval_mode()));
                                    let _ = tx_msg.send(Msg::Effort(
                                        agent.reasoning_effort(),
                                        agent.effort_levels().await,
                                    ));
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
                    // gate and no LLM involvement. Runs on `ctx.shell` —
                    // whichever backend `shell.txt` / `SUNMAO_SHELL` picked
                    // (Posix|deno_task_shell, or Pwsh); the durable fact
                    // folds into the next turn's context via LocalShell.
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
                    let ctx = agent.context().clone();
                    let (ok, output, code) = match sunmao_core::tool::run_foreground(
                        &cmd,
                        shell_cwd,
                        120,
                        ctx.shell,
                        Some(ctx.cancel_notify.clone()),
                    )
                    .await
                    {
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
                    // a `!` emits no TurnEnd — without this the queued
                    // copy stays parked in app.queue as a phantom entry.
                    let _ = tx_msg.send(Msg::QueuePop);
                    continue;
                }
                Submit::Turn(input, atts) => {
                    let resolved = if let Some(cmd_line) = input.trim().strip_prefix('/') {
                        let name = cmd_line.split_whitespace().next().unwrap_or("");
                        let rest = cmd_line[name.len()..].trim();
                        // `/srv:prompt` resolves through the MCP server
                        // before file commands — the server owns the name
                        match agent.mcp_prompt_text(name, rest).await {
                            Some(Ok(text)) => Some(text),
                            Some(Err(e)) => {
                                let _ = tx_msg
                                    .send(Msg::Note(format!("[mcp prompt /{name} failed] {e:#}")));
                                continue;
                            }
                            None => match commands::command_body(&cwd, &extra_roots, name) {
                                Some(body) => Some(commands::expand_command(&body, rest)),
                                None => {
                                    let _ = tx_msg
                                        .send(Msg::Note(format!("[unknown command: /{name}]")));
                                    continue;
                                }
                            },
                        }
                    } else {
                        Some(input)
                    };
                    let Some(text) = resolved else {
                        continue;
                    };
                    // mentions in the text attach too — a file command's
                    // `$1` arg or an mcp prompt's tail can name an image
                    let (prompt, mut a) = crate::attachments::attach_mentions(&text, &cwd);
                    a.extend(atts);
                    let obs = ChanObserver(tx_msg.clone());
                    let mut turn = Box::pin(agent.run_turn_blocks(&prompt, &a, &obs));
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
