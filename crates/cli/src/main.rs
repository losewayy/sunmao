//! `sunmao` — stdin/stdout REPL over the kernel. TUI lands in v0.2.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use sunmao_core::agent::{AgentLoop, LiveEvent, Observer, TurnOutcome};
use sunmao_core::approval::{Approval, Approver};
use sunmao_core::tool::builtin_registry;
use sunmao_core::{Context, SessionLog};
use sunmao_llm::OaiClient;

mod acp;
mod dataflow;
mod doctor;
mod eval;
mod plugin;
mod tui;

#[derive(Parser)]
#[command(name = "sunmao", version, about = "agent harness kernel — 榫卯")]
struct Cli {
    /// Launch the ratatui TUI instead of the REPL.
    #[arg(long)]
    tui: bool,
    /// OpenAI-compatible base URL.
    #[arg(
        long,
        env = "SUNMAO_BASE_URL",
        default_value = "http://127.0.0.1:7863/v1"
    )]
    base_url: String,
    /// API key.
    #[arg(long, env = "SUNMAO_API_KEY", default_value = "your-api-key-here")]
    api_key: String,
    /// Model id.
    #[arg(
        long,
        env = "SUNMAO_MODEL",
        default_value = "global:deepseek-v4.1-flash"
    )]
    model: String,
    /// Provider dialect: openai (default) or anthropic.
    #[arg(long, default_value = "openai", env = "SUNMAO_PROVIDER")]
    provider: String,
    /// Session log directory.
    #[arg(long, default_value = ".sunmao/sessions")]
    session_dir: PathBuf,
    /// Working directory for tools.
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// System prompt override.
    #[arg(long)]
    system: Option<String>,
    /// Print a data-flow report for a session log file and exit.
    #[arg(long)]
    dataflow: Option<PathBuf>,
    /// Environment self-check.
    #[arg(long)]
    doctor: bool,
    /// Run as an Agent Client Protocol server on stdio (Zed etc.).
    #[arg(long)]
    acp: bool,
    /// List session logs and exit.
    #[arg(long)]
    sessions: bool,
    /// Resume an existing session log (id like `s-123` or a .jsonl path).
    #[arg(long)]
    resume: Option<String>,
    /// Fork a session: copy its log to a new id and resume the copy.
    #[arg(long)]
    fork: Option<String>,
    /// One-shot mode: run a single prompt and exit (scriptable).
    #[arg(long, short = 'p')]
    print: Option<String>,
    /// Enable a preset — a plugin-bundle dir under `.sunmao/presets/<name>/`
    /// (project) or `~/.sunmao/presets/<name>/` (user). Repeatable; later
    /// presets layer after earlier ones. A leading `+` is decorative.
    #[arg(long)]
    preset: Vec<String>,
    /// Loop driver override — `full` (contract loop) or `bare` (no hooks,
    /// no gate, no auto-compaction). Wins over any manifest `loop:` key.
    #[arg(long = "loop", value_parser = parse_driver)]
    driver: Option<sunmao_core::agent::LoopDriver>,
    #[command(subcommand)]
    command: Option<plugin::Cmd>,
}

/// clap needs a String error, not anyhow — same refusal surface.
fn parse_driver(s: &str) -> Result<sunmao_core::agent::LoopDriver, String> {
    sunmao_core::agent::LoopDriver::parse(s).map_err(|e| e.to_string())
}

struct StdoutObserver {
    in_reasoning: std::sync::Mutex<bool>,
}

impl Observer for StdoutObserver {
    fn on_event(&self, ev: &LiveEvent) {
        let mut in_r = self.in_reasoning.lock().unwrap();
        match ev {
            LiveEvent::Reasoning(r) => {
                if !*in_r {
                    eprint!("\x1b[2m"); // dim
                    *in_r = true;
                }
                eprint!("{r}");
            }
            LiveEvent::Content(c) => {
                if *in_r {
                    eprintln!("\x1b[0m");
                    *in_r = false;
                }
                print!("{c}");
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

fn session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("s-{now}")
}

/// Session provider adapter from the cli flags — shared by the interactive
/// path and `sunmao eval`.
fn provider_adapter(cli: &Cli) -> Arc<dyn sunmao_llm::ProviderAdapter> {
    match cli.provider.as_str() {
        "anthropic" => Arc::new(sunmao_llm::AnthropicClient::new(
            &cli.base_url,
            &cli.api_key,
            &cli.model,
        )),
        _ => Arc::new(OaiClient::new(&cli.base_url, &cli.api_key, &cli.model)),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();

    // `plugin` ops are pure file management — they never need a provider,
    // a session log, or any of the session setup below.
    if let Some(plugin::Cmd::Plugin(args)) = &cli.command {
        return plugin::run(args, &cli.cwd);
    }

    if cli.sessions {
        return list_sessions(&cli.session_dir);
    }

    if cli.doctor {
        return doctor::run(&cli).await;
    }
    if cli.acp {
        return acp::run(
            &cli.base_url,
            &cli.api_key,
            &cli.model,
            &cli.provider,
            &cli.preset,
        )
        .await
        .map_err(|e| anyhow::anyhow!("acp: {e}"));
    }

    if let Some(path) = &cli.dataflow {
        let report = dataflow::report(path).await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let cwd = cli.cwd.canonicalize().context("bad --cwd")?;

    // presets resolve once here — unknown names fail fast before any
    // session state exists. The resolved dirs become extra plugin roots
    // every consumer layers after its convention dirs.
    let preset_roots = sunmao_core::presets::resolve(&cwd, &cli.preset)?;
    if !preset_roots.is_empty() {
        eprintln!("presets enabled: {}", cli.preset.join(", "));
    }

    // eval builds its own Context per case — each needs a fresh session log,
    // per-case cwd, and isolated permission/read ledgers, so the shared one
    // below can't be reused.
    if let Some(plugin::Cmd::Eval(args)) = &cli.command {
        return eval::run(args, &cli, &cwd, &preset_roots).await;
    }

    let llm = provider_adapter(&cli);
    // --fork: copy the source log to a fresh id, then resume the copy
    let mut resume_target = cli.resume.clone();
    if let Some(src) = &cli.fork {
        let p = PathBuf::from(src);
        let src_path = if p.exists() {
            p
        } else {
            cli.session_dir.join(format!("{src}.jsonl"))
        };
        let new_id = session_id();
        let dst = cli.session_dir.join(format!("{new_id}.jsonl"));
        std::fs::copy(&src_path, &dst)
            .map_err(|e| anyhow::anyhow!("fork {}: {e}", src_path.display()))?;
        eprintln!("forked {src} -> {new_id}");
        resume_target = Some(dst.to_string_lossy().to_string());
    }
    let (sessions, resumed) = match &resume_target {
        Some(r) => {
            let p = PathBuf::from(r);
            let path = if p.exists() {
                p
            } else {
                cli.session_dir.join(format!("{r}.jsonl"))
            };
            let log = SessionLog::open_path(&path).await?;
            (log, true)
        }
        None => (
            SessionLog::open(&cli.session_dir, &session_id()).await?,
            false,
        ),
    };
    let mut registry = builtin_registry();
    for tool in sunmao_core::mcp::connect_all(&cwd, &preset_roots).await {
        registry.register_boxed(tool);
    }
    // For --tui --resume: snapshot the durable events before the log moves
    // into Context — the TUI replays them into transcript blocks.
    let replay_events = if cli.tui && resumed {
        sessions.events().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let interactive = cli.print.is_none() && !cli.acp;
    let (tx_approval, rx_approval) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx_raw = Context::new(llm, sessions, registry, cwd.clone())
        .with_extra_plugin_roots(preset_roots.clone());
    // --loop outranks every manifest `loop:` key — explicit beats declared
    if let Some(d) = cli.driver {
        ctx_raw.loop_driver = d;
    }
    // extension children spawn before SessionStart so they can receive it
    ctx_raw.connect_extensions().await;
    if cli.tui {
        ctx_raw.approval = Arc::new(tui::TuiApprover { tx: tx_approval });
    } else if interactive {
        ctx_raw.approval = Arc::new(StdinApprover { interactive: true });
    }
    // Model routing seam: `.sunmao/models.json` (+ `.claude` compat) names
    // providers and routes; agent `model:` selectors resolve through it.
    // The session's own provider registers as "default" so bare model ids
    // keep working.
    ctx_raw.models = Some(Arc::new(sunmao_core::models::ModelResolver::load(
        &cwd,
        sunmao_core::models::ProviderDef {
            base_url: cli.base_url.clone(),
            api_key_env: None,
            api_key: Some(cli.api_key.clone()),
            dialect: cli.provider.clone(),
        },
        "default",
    )));
    let ctx = Arc::new(ctx_raw);
    ctx.hooks
        .fire(
            sunmao_core::hooks::HookEvent::SessionStart,
            &ctx.cwd,
            &sunmao_core::hooks::HookInput {
                source: Some(if resumed { "resume" } else { "startup" }),
                ..Default::default()
            },
        )
        .await;

    // The system prompt is assembled, not constant: built-in section files →
    // ~/.sunmao/prompt{,.d} → .sunmao/prompt{,.d} → project context →
    // --system as the complete override. All frontends share this path.
    let default_system = sunmao_core::prompt::PromptAssembler::new(&cwd)
        .with_extra_roots(&preset_roots)
        .assemble(cli.system.as_deref());

    if !resumed {
        let mut log = ctx.sessions.lock().await;
        log.append(&sunmao_core::SessionEvent::Started {
            model: cli.model.clone(),
            cwd: ctx.cwd.display().to_string(),
        })
        .await?;
        log.append(&sunmao_core::SessionEvent::Message {
            message: sunmao_llm::types::Message::system(default_system),
        })
        .await?;
    }

    let agent = AgentLoop::new(ctx.clone());

    if let Some(prompt) = &cli.print {
        let obs = StdoutObserver {
            in_reasoning: std::sync::Mutex::new(false),
        };
        let outcome = agent.run_turn(prompt, &obs).await?;
        // process::exit skips destructors — extension children need the
        // graceful shutdown (ext/shutdown → EOF → kill) run explicitly.
        ctx.ext.shutdown().await;
        std::process::exit(if matches!(outcome, TurnOutcome::Completed) {
            0
        } else {
            1
        });
    }

    if cli.tui {
        let res = tui::run(
            agent,
            &cli.model,
            cwd.clone(),
            rx_approval,
            replay_events,
            preset_roots,
        )
        .await;
        ctx.ext.shutdown().await;
        return res;
    }

    let observer = Arc::new(StdoutObserver {
        in_reasoning: std::sync::Mutex::new(false),
    });
    // sub-agent tool lifecycle relays through the same output channel
    agent.set_live_sink(observer.clone() as Arc<dyn Observer>);

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
            match sunmao_core::tool::run_foreground(cmd, cwd.clone(), 120).await {
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
                    match agent.compact(&*observer, "manual").await {
                        Ok(s) if s.is_empty() => println!("[compacted: nothing to fold]"),
                        Ok(s) => println!("[compacted]\n{s}"),
                        Err(e) => eprintln!("[compact failed] {e:#}"),
                    }
                    continue;
                }
                "help" | "h" | "?" => {
                    println!(
                        "commands — /compact · /model [sel] · /resume [id] · /sessions · /help · /quit\n\
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
            match tui::slash::command_body(&cwd, &preset_roots, name) {
                Some(body) => {
                    let prompt = if rest.is_empty() {
                        body
                    } else {
                        format!("{body}\n\n{rest}")
                    };
                    if let Err(e) = agent.run_turn(&prompt, &*observer).await {
                        eprintln!("[error] {e:#}");
                    }
                }
                None => println!("[unknown command: /{name}]"),
            }
            continue;
        }
        if let Err(e) = agent.run_turn(line, &*observer).await {
            eprintln!("[error] {e:#}");
        }
    }
    ctx.hooks
        .fire(
            sunmao_core::hooks::HookEvent::SessionEnd,
            &ctx.cwd,
            &sunmao_core::hooks::HookInput::default(),
        )
        .await;
    ctx.ext.shutdown().await;
    Ok(())
}

/// Interactive approver for the REPL/-p: prints the risky command, y/n on stdin.
struct StdinApprover {
    interactive: bool,
}

#[async_trait::async_trait]
impl Approver for StdinApprover {
    async fn approve(&self, tool: &str, detail: &str, why: &str) -> Approval {
        if !self.interactive {
            return Approval::Once; // piped -p mode: don't hang waiting for stdin
        }
        let tool = tool.to_string();
        let detail = detail.to_string();
        let why = why.to_string();
        tokio::task::spawn_blocking(move || {
            eprint!(
                "
[33m[approve?] {tool} — {why}
  {detail}
  allow once [y] · allow session [a] · deny [N][0m "
            );
            std::io::stderr().flush().ok();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok()?;
            Some(match line.trim().to_lowercase().as_str() {
                "y" | "yes" => Approval::Once,
                "a" | "always" => Approval::Session,
                _ => Approval::Deny,
            })
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(Approval::Deny)
    }
}

/// `sunmao sessions` — list local session logs.
fn list_sessions(dir: &std::path::Path) -> anyhow::Result<()> {
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
