//! The interactive REPL — plain stdio loop over `run_turn`. `!cmd` local
//! shell, `/name` command resolution, model switching, session resume.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

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
        let mut in_r = self.in_reasoning.lock().unwrap();
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
            match sunmao_core::tool::run_foreground(cmd, cwd.to_path_buf(), 120).await {
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
                crate::commands::Command::Sessions(_) | crate::commands::Command::Resume(None) => {
                    println!("{}", crate::sessions::recent_sessions_text(cwd, 8));
                    continue;
                }
                crate::commands::Command::Tasks => {
                    println!("{}", crate::commands::tasks_text(&agent.task_roster()));
                    continue;
                }
                crate::commands::Command::Todos => {
                    println!("{}", crate::commands::todos_text(&agent.todos()));
                    continue;
                }
                crate::commands::Command::Mcp => {
                    println!("{}", crate::commands::mcp_text(&agent.mcp_roster()));
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
                crate::commands::Command::Resume(Some(id)) => {
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
                crate::commands::Command::Rewind(spec) => {
                    match spec {
                        None => println!("{}", crate::rewind::list(agent).await),
                        Some(spec) => {
                            match crate::rewind::run(agent, cwd, spec.turn, spec.mode).await {
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
                // local-only builtins and unknown names — try a command file
                // first, else report unknown.
                crate::commands::Command::Clear
                | crate::commands::Command::Multiline
                | crate::commands::Command::Other => {
                    let name = cmd_line.split_whitespace().next().unwrap_or("");
                    match crate::commands::command_body(cwd, preset_roots, name) {
                        Some(body) => {
                            let rest = cmd_line[name.len()..].trim();
                            let prompt = crate::commands::expand_command(&body, rest);
                            if let Err(e) = agent.run_turn(&prompt, &*observer.0).await {
                                eprintln!("[error] {e:#}");
                            }
                        }
                        None => println!("[unknown command: /{name}]"),
                    }
                    continue;
                }
            }
        }
        if let Err(e) = agent.run_turn(line, &*observer.0).await {
            eprintln!("[error] {e:#}");
        }
    }
    Ok(())
}

/// Wraps the live observer the REPL prints through — kept opaque so main
/// only hands us the sink it already constructed.
pub struct StdoutLoop(pub Arc<dyn Observer>);
