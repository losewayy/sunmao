//! The interactive REPL — plain stdio loop over `run_turn`. `!cmd` local
//! shell, `/name` command resolution, model switching, session resume.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use sunmao_core::agent::{AgentLoop, Observer};
use sunmao_core::Context;

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
            let name = cmd_line.split_whitespace().next().unwrap_or("");
            let rest = cmd_line[name.len()..].trim();
            match name {
                "quit" | "exit" | "q" => break,
                "compact" => {
                    match agent.compact(&*observer.0, "manual").await {
                        Ok(s) if s.is_empty() => println!("[compacted: nothing to fold]"),
                        Ok(s) => println!("[compacted]\n{s}"),
                        Err(e) => eprintln!("[compact failed] {e:#}"),
                    }
                    continue;
                }
                "help" | "h" | "?" => {
                    println!(
                        "commands — /compact · /model [sel] · /resume [id] · /sessions · /tasks · /help · /quit\n\
                         `!cmd` runs locally; /name resolves .sunmao/commands + .claude/commands"
                    );
                    continue;
                }
                "model" => {
                    if rest.is_empty() {
                        let choices = agent.model_choices();
                        if choices.is_empty() {
                            println!("[no models.json — session model only]");
                        } else {
                            println!("available models:\n{}", choices.join("\n"));
                        }
                    } else {
                        match agent.swap_model(rest) {
                            Some(label) => {
                                agent.record_model_change(rest, &label).await;
                                println!("[model → {label}]");
                            }
                            None => {
                                println!("[unknown selector: {rest} — try /model for the list]")
                            }
                        }
                    }
                    continue;
                }
                "sessions" => {
                    list_sessions(&cwd.join(".sunmao").join("sessions"))?;
                    continue;
                }
                "tasks" => {
                    let tasks = agent.task_roster();
                    if tasks.is_empty() {
                        println!("[no sub-agents this session]");
                    } else {
                        println!("sub-agents:");
                        for t in &tasks {
                            let status = match t.done {
                                None => "running",
                                Some(true) => "done",
                                Some(false) => "failed",
                            };
                            let agent_name = t
                                .agent
                                .as_deref()
                                .map(|a| format!(" @{a}"))
                                .unwrap_or_default();
                            println!("  {status:<7} {}{} — {}", t.id, agent_name, t.prompt);
                        }
                    }
                    continue;
                }
                "resume" => {
                    if rest.is_empty() {
                        list_sessions(&cwd.join(".sunmao").join("sessions"))?;
                    } else {
                        let p = std::path::PathBuf::from(rest);
                        let path = if p.exists() {
                            p
                        } else {
                            cwd.join(".sunmao/sessions").join(format!("{rest}.jsonl"))
                        };
                        match sunmao_core::SessionLog::open_path(&path).await {
                            Ok(log) => {
                                let events = agent.swap_session(log).await;
                                println!("[resumed {rest} — {} events folded in]", events.len());
                            }
                            Err(e) => eprintln!("[resume failed] {e:#}"),
                        }
                    }
                    continue;
                }
                _ => {}
            }
            match crate::tui::slash::command_body(cwd, preset_roots, name) {
                Some(body) => {
                    let prompt = if rest.is_empty() {
                        body
                    } else {
                        format!("{body}\n\n{rest}")
                    };
                    if let Err(e) = agent.run_turn(&prompt, &*observer.0).await {
                        eprintln!("[error] {e:#}");
                    }
                }
                None => println!("[unknown command: /{name}]"),
            }
            continue;
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
