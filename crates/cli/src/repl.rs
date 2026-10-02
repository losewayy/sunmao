//! The interactive REPL — plain stdio loop over `run_turn`. `!cmd` local
//! shell, `/name` command resolution, model switching, session resume.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use sunmao_core::context::MutexRecover;

use sunmao_core::Context;
use sunmao_core::agent::{AgentLoop, LiveEvent, Observer, TurnOutcome};

/// The plain-stdio live observer — REPL transcript lines and the `-p`
/// one-shot output both render through this.
pub(crate) struct StdoutObserver {
    in_reasoning: std::sync::Mutex<bool>,
}

impl StdoutObserver {
    pub(crate) fn new() -> Self {
        Self {
            in_reasoning: std::sync::Mutex::new(false),
        }
    }
}

impl Observer for StdoutObserver {
    fn on_event(&self, ev: &LiveEvent) {
        let mut in_r = self.in_reasoning.lock_or_recover();
        match ev {
            LiveEvent::Reasoning { text } => {
                if !*in_r {
                    eprint!("\x1b[2m"); // dim
                    *in_r = true;
                }
                eprint!("{text}");
            }
            LiveEvent::Content { text } => {
                if *in_r {
                    eprintln!("\x1b[0m");
                    *in_r = false;
                }
                print!("{text}");
                std::io::stdout().flush().ok();
            }
            LiveEvent::ToolStart {
                name,
                summary,
                depth,
                ..
            } => {
                if *in_r {
                    eprintln!("\x1b[0m");
                    *in_r = false;
                }
                let nest = if *depth > 0 { "↳" } else { "" };
                if summary.is_empty() {
                    println!("\n\x1b[36m[tool → {nest}{name}]\x1b[0m");
                } else {
                    println!("\n\x1b[36m[tool → {nest}{name} · {summary}]\x1b[0m");
                }
            }
            LiveEvent::ToolDone {
                name, ok, depth, ..
            } => {
                let mark = if *ok { "✓" } else { "✗" };
                let nest = if *depth > 0 { "↳" } else { "" };
                println!("\x1b[36m[tool {nest}{name} {mark}]\x1b[0m");
            }
            LiveEvent::Hook { event, detail } => {
                if *in_r {
                    eprintln!("\x1b[0m");
                    *in_r = false;
                }
                println!("\x1b[33m[⚙ {event} — {detail}]\x1b[0m");
            }
            LiveEvent::Artifact {
                name,
                path,
                bytes,
                rev,
            } => {
                if *in_r {
                    eprintln!("\x1b[0m");
                    *in_r = false;
                }
                let v = if *rev > 1 {
                    format!(" · rev {rev}")
                } else {
                    String::new()
                };
                println!("\x1b[36m[artifact '{name}' → {path} ({bytes} B{v})]\x1b[0m");
            }
            LiveEvent::Usage(_) => {} // durable in the log; REPL stays quiet
            LiveEvent::Compacted { summary } => {
                println!("\n\x1b[33m[context compacted] {summary}\x1b[0m");
            }
            LiveEvent::Todos { .. } => {} // the tool's echo text already printed it
            LiveEvent::Goal { goal } => {
                // goal facts are the loop's progress markers — visible like
                // hook audit lines or the REPL would silently run forever
                if *in_r {
                    eprintln!("\x1b[0m");
                    *in_r = false;
                }
                println!(
                    "\n\x1b[36m[goal · {} · round {}/{}]\x1b[0m",
                    sunmao_core::tool::status_name(goal.status),
                    goal.rounds,
                    goal.max_rounds
                );
            }
            // serve-only live mirror of the durable user message — the REPL
            // echoes its own prompt before run_turn, so it never lands here
            LiveEvent::UserMessage { .. } => {}
            LiveEvent::TaskDone { id, ok, .. } => {
                let mark = if *ok { "✓" } else { "✗" };
                println!("\n\x1b[36m[sub-agent {id} {mark}]\x1b[0m");
            }
            LiveEvent::TurnEnd { outcome } => {
                if *in_r {
                    eprintln!("\x1b[0m");
                    *in_r = false;
                }
                match outcome {
                    TurnOutcome::Completed => println!(),
                    other => println!("\n[turn ended: {other:?}]"),
                }
            }
        }
    }
}

/// Print every `.jsonl` log in `dir` as `<id>\t<size>` rows — the
/// `/sessions` and `--sessions` surface.
pub fn list_sessions(dir: &Path) -> anyhow::Result<()> {
    let mut rows = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                let meta = std::fs::metadata(&p).ok();
                let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let name = p
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                rows.push((name, size));
            }
        }
    }
    rows.sort();
    for (name, size) in &rows {
        println!("{name}\t{size} B");
    }
    if rows.is_empty() {
        println!("[no sessions in {}]", dir.display());
    }
    Ok(())
}

/// Drive the prompt loop until quit/EOF. `ctx` is needed for the trailing
/// SessionEnd + extension teardown; `resumed` controls the fold-in banner.
pub async fn run(
    agent: &AgentLoop,
    ctx: &Arc<Context>,
    observer: &StdoutLoop,
    cwd: &Path,
    preset_roots: &[std::path::PathBuf],
    resumed: bool,
) -> anyhow::Result<()> {
    // sub-agent tool lifecycle relays through the same output channel
    agent.set_live_sink(observer.0.clone());

    // Resumed sessions announce themselves — a silent resume reads as a
    // fresh session and the fold-in context is invisible to the user.
    if resumed {
        let n_msgs = ctx
            .sessions
            .lock()
            .await
            .messages()
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        println!("[resumed — {n_msgs} messages folded in]");
    }
    println!("sunmao — agent kernel v0.1 (ctrl-c / empty line to exit)");
    let stdin = std::io::stdin();
    loop {
        print!("\n\x1b[1m>\x1b[0m ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        // `!cmd` — local shell, same engine as the TUI's bash mode. Output
        // prints here and folds into the session as SessionEvent::LocalShell.
        if let Some(cmd) = line.strip_prefix('!') {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                continue;
            }
            let ctx = agent.context().clone();
            match sunmao_core::tool::run_foreground(
                cmd,
                cwd.to_path_buf(),
                120,
                ctx.shell,
                Some(ctx.cancel_notify.clone()),
            )
            .await
            {
                Ok(run) => {
                    let out = sunmao_core::tool::render_run(&run);
                    println!("{out}");
                    agent.record_local_shell(cmd, run.exit_code, &out).await;
                }
                Err(msg) => println!("{msg}"),
            }
            continue;
        }
        if let Some(cmd_line) = line.strip_prefix('/') {
            match crate::commands::parse(cmd_line) {
                crate::commands::Command::Quit => break,
                crate::commands::Command::Compact => {
                    println!(
                        "{}",
                        crate::commands::compact_note(agent.compact(&*observer.0, "manual").await)
                    );
                    continue;
                }
                crate::commands::Command::Help => {
                    println!("{}\n`!cmd` runs locally", crate::commands::help_text(""));
                    continue;
                }
                crate::commands::Command::Model(sel) => {
                    match sel {
                        None => {
                            println!("{}", crate::commands::models_text(&agent.model_choices()))
                        }
                        Some(sel) => match agent.swap_model(&sel) {
                            Some(label) => {
                                agent.record_model_change(&sel, &label).await;
                                println!("[model → {label}]");
                            }
                            None => println!("{}", crate::commands::model_unknown(&sel)),
                        },
                    }
                    continue;
                }
                crate::commands::Command::Mode(arg) => {
                    match arg {
                        None => {
                            println!("{}", crate::commands::mode_list_text(agent.approval_mode()))
                        }
                        Some(name) => match sunmao_core::agent::ApprovalMode::parse(&name) {
                            Some(m) => {
                                agent.set_approval_mode(m, &*observer.0).await;
                                println!("[approval mode → {}]", m.as_str());
                            }
                            None => println!("{}", crate::commands::mode_unknown(&name)),
                        },
                    }
                    continue;
                }
                // /sessions <id> is the REPL's resume-by-id — same arm as
                // /resume, the bare listing stays for the empty form.
                crate::commands::Command::Sessions(None)
                | crate::commands::Command::Resume(None) => {
                    println!("{}", crate::sessions::recent_sessions_text(cwd, 8));
                    continue;
                }
                crate::commands::Command::Tasks => {
                    println!("{}", crate::commands::tasks_text(&agent.task_roster()));
                    continue;
                }
                crate::commands::Command::Stop(id) => {
                    // slash-path kill — same `cancel_sub` as the GUI
                    // roster's 终止 button and serve's `task_cancel` frame
                    match agent.cancel_sub(&id).await {
                        Ok(()) => println!("cancelled {id}"),
                        Err(e) => println!("[stop failed] {e}"),
                    }
                    continue;
                }
                crate::commands::Command::Todos => {
                    println!("{}", crate::commands::todos_text(&agent.todos()));
                    continue;
                }
                crate::commands::Command::Goal(arg) => {
                    match arg {
                        None => println!("{}", crate::commands::goal_text(agent.goal().as_ref())),
                        Some(objective) => {
                            if let Err(e) = agent.set_goal(&objective, &*observer.0).await {
                                println!("[goal failed] {e:#}");
                                continue;
                            }
                            println!("[goal set — the agent keeps at it until complete/blocked]");
                            // the objective itself is the kickoff prompt —
                            // the continuation loop chains from there
                            let (prompt, atts) =
                                crate::attachments::attach_mentions(&objective, cwd);
                            if let Err(e) =
                                agent.run_turn_blocks(&prompt, &atts, &*observer.0).await
                            {
                                eprintln!("[error] {e:#}");
                            }
                        }
                    }
                    continue;
                }
                crate::commands::Command::GoalClear => {
                    match agent.clear_goal(&*observer.0).await {
                        Ok(()) => println!("[goal cleared]"),
                        Err(e) => println!("[goal clear failed] {e:#}"),
                    }
                    continue;
                }
                crate::commands::Command::Mcp => {
                    println!("{}", crate::commands::mcp_text(&agent.mcp_roster()));
                    continue;
                }
                crate::commands::Command::Hooks(op) => {
                    use crate::commands::HookOp;
                    match op {
                        HookOp::List => {
                            println!("{}", crate::commands::hooks_text(&ctx.hooks.roster()))
                        }
                        HookOp::Trust(n) | HookOp::Untrust(n) => {
                            match agent
                                .set_hook_trust(n, matches!(op, HookOp::Trust(_)))
                                .await
                            {
                                Ok(d) => println!("[{d}]"),
                                Err(e) => println!("[hooks: {e}]"),
                            }
                        }
                    }
                    continue;
                }
                crate::commands::Command::Status => {
                    println!("{}", crate::commands::status_text(&agent.status().await));
                    continue;
                }
                crate::commands::Command::Artifacts => {
                    println!("{}", crate::commands::artifacts_text(cwd));
                    continue;
                }
                crate::commands::Command::Annotate(name, note) => {
                    println!("{}", crate::commands::annotate(cwd, &name, &note));
                    continue;
                }
                crate::commands::Command::Search(q) => {
                    println!(
                        "{}",
                        crate::sessions::search_text(&[crate::sessions::sessions_dir(cwd)], &q)
                    );
                    continue;
                }
                crate::commands::Command::Note(n) => {
                    println!("{n}");
                    continue;
                }
                crate::commands::Command::Sessions(Some(id))
                | crate::commands::Command::Resume(Some(id)) => {
                    let path = crate::sessions::resolve_log_path(cwd, &id);
                    match sunmao_core::SessionLog::open_path(&path).await {
                        Ok(log) => {
                            let events = agent.swap_session(log).await;
                            println!("[resumed {id} — {} events folded in]", events.len());
                        }
                        Err(e) => println!("[resume failed] {e:#}"),
                    }
                    continue;
                }
                crate::commands::Command::Fork(id) => {
                    match crate::sessions::fork_copy(cwd, &id) {
                        Ok((new_id, dst)) => match sunmao_core::SessionLog::open_path(&dst).await {
                            Ok(log) => {
                                let events = agent.swap_session(log).await;
                                println!(
                                    "[forked {id} → {new_id} — {} events folded in]",
                                    events.len()
                                );
                            }
                            Err(e) => println!("[fork failed] {e:#}"),
                        },
                        Err(e) => println!("{e}"),
                    }
                    continue;
                }
                crate::commands::Command::Export => {
                    let events = agent.session_events().await;
                    match crate::commands::export_md(cwd, &agent.session_id(), &events) {
                        Ok(p) => println!("[exported → {p}]"),
                        Err(e) => println!("[export failed] {e:#}"),
                    }
                    continue;
                }
                crate::commands::Command::ExportZip => {
                    let events = agent.session_events().await;
                    let log = agent.session_path().await;
                    match crate::commands::export_zip(cwd, &agent.session_id(), &events, &log) {
                        Ok(p) => println!("[exported → {p}]"),
                        Err(e) => println!("[export failed] {e:#}"),
                    }
                    continue;
                }
                crate::commands::Command::Rewind(spec) => {
                    match spec {
                        None => println!("{}", crate::rewind::list(agent).await),
                        Some(spec) => {
                            // checkpoint dirs key off the SESSION's project —
                            // a resumed session restores under its own root.
                            match crate::rewind::run(
                                agent,
                                &agent.session_cwd(),
                                spec.turn,
                                spec.mode,
                            )
                            .await
                            {
                                Ok(crate::rewind::Outcome::Forked { note, events }) => {
                                    println!("{note} — {} events folded in", events.len());
                                }
                                Ok(crate::rewind::Outcome::CodeOnly(note)) => println!("{note}"),
                                Err(e) => println!("{e}"),
                            }
                        }
                    }
                    continue;
                }
                // Frontend-local builtins are builtins FIRST — `/clear` on an
                // MCP server must never shadow the command the user typed.
                // Only `Other` (a name no frontend owns) falls through to the
                // MCP-prompt/command-file chain.
                crate::commands::Command::Clear | crate::commands::Command::Multiline => {
                    println!(
                        "[/{name} is a TUI-local command — no effect in the REPL]",
                        name = cmd_line.split_whitespace().next().unwrap_or("")
                    );
                    continue;
                }
                crate::commands::Command::Other => {
                    let name = cmd_line.split_whitespace().next().unwrap_or("");
                    let rest = cmd_line[name.len()..].trim();
                    match agent.mcp_prompt_text(name, rest).await {
                        Some(Ok(text)) => {
                            let (prompt, atts) = crate::attachments::attach_mentions(&text, cwd);
                            if let Err(e) =
                                agent.run_turn_blocks(&prompt, &atts, &*observer.0).await
                            {
                                eprintln!("[error] {e:#}");
                            }
                        }
                        Some(Err(e)) => println!("[mcp prompt /{name} failed] {e:#}"),
                        None => match crate::commands::command_body(cwd, preset_roots, name) {
                            Some(body) => {
                                let prompt = crate::commands::expand_command(&body, rest);
                                let (prompt, atts) =
                                    crate::attachments::attach_mentions(&prompt, cwd);
                                if let Err(e) =
                                    agent.run_turn_blocks(&prompt, &atts, &*observer.0).await
                                {
                                    eprintln!("[error] {e:#}");
                                }
                            }
                            None => println!("[unknown command: /{name}]"),
                        },
                    }
                    continue;
                }
            }
        }
        let (line, atts) = crate::attachments::attach_mentions(line, cwd);
        if let Err(e) = agent.run_turn_blocks(&line, &atts, &*observer.0).await {
            eprintln!("[error] {e:#}");
        }
    }
    Ok(())
}

/// Wraps the live observer the REPL prints through — kept opaque so main
/// only hands us the sink it already constructed.
pub struct StdoutLoop(pub Arc<dyn Observer>);
