//! `sunmao` — stdin/stdout REPL over the kernel. TUI lands in v0.2.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use sunmao_core::agent::{AgentLoop, LiveEvent, Observer, TurnOutcome};
use sunmao_core::approval::Approver;
use sunmao_core::tool::builtin_registry;
use sunmao_core::{Context, SessionLog};
use sunmao_llm::OaiClient;

mod acp;
mod dataflow;
mod doctor;
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
            LiveEvent::ToolStart { name, summary } => {
                if *in_r {
                    eprintln!("\x1b[0m");
                    *in_r = false;
                }
                if summary.is_empty() {
                    println!("\n\x1b[36m[tool → {name}]\x1b[0m");
                } else {
                    println!("\n\x1b[36m[tool → {name} · {summary}]\x1b[0m");
                }
            }
            LiveEvent::ToolDone { name, ok, .. } => {
                let mark = if *ok { "✓" } else { "✗" };
                println!("\x1b[36m[tool {name} {mark}]\x1b[0m");
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

fn session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("s-{now}")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();

    if cli.sessions {
        return list_sessions(&cli.session_dir);
    }

    if cli.doctor {
        return doctor::run(&cli).await;
    }
    if cli.acp {
        return acp::run(&cli.base_url, &cli.api_key, &cli.model, &cli.provider)
            .await
            .map_err(|e| anyhow::anyhow!("acp: {e}"));
    }

    if let Some(path) = &cli.dataflow {
        let report = dataflow::report(path).await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let cwd = cli.cwd.canonicalize().context("bad --cwd")?;

    let llm: Arc<dyn sunmao_llm::ProviderAdapter> = match cli.provider.as_str() {
        "anthropic" => Arc::new(sunmao_llm::AnthropicClient::new(
            &cli.base_url,
            &cli.api_key,
            &cli.model,
        )),
        _ => Arc::new(OaiClient::new(&cli.base_url, &cli.api_key, &cli.model)),
    };
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
    for tool in sunmao_core::mcp::connect_all(&cwd).await {
        registry.register_boxed(tool);
    }
    let interactive = cli.print.is_none() && !cli.acp;
    let (tx_approval, rx_approval) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx_raw = Context::new(llm, sessions, registry, cwd.clone());
    if cli.tui {
        ctx_raw.approval = Arc::new(tui::TuiApprover { tx: tx_approval });
    } else if interactive {
        ctx_raw.approval = Arc::new(StdinApprover { interactive: true });
    }
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
    let default_system =
        sunmao_core::prompt::PromptAssembler::new(&cwd).assemble(cli.system.as_deref());

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
        std::process::exit(if matches!(outcome, TurnOutcome::Completed) {
            0
        } else {
            1
        });
    }

    if cli.tui {
        return tui::run(agent, &cli.model, cwd.clone(), rx_approval).await;
    }

    let observer = StdoutObserver {
        in_reasoning: std::sync::Mutex::new(false),
    };

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
        if let Some(cmd_line) = line.strip_prefix('/') {
            let name = cmd_line.split_whitespace().next().unwrap_or("");
            let rest = cmd_line[name.len()..].trim();
            if line == "/compact" {
                match agent.compact(&observer, "manual").await {
                    Ok(()) => println!("[compacted]"),
                    Err(e) => eprintln!("[compact failed] {e:#}"),
                }
                continue;
            }
            match tui::slash::command_body(&cwd, name) {
                Some(body) => {
                    let prompt = if rest.is_empty() {
                        body
                    } else {
                        format!(
                            "{body}

{rest}"
                        )
                    };
                    if let Err(e) = agent.run_turn(&prompt, &observer).await {
                        eprintln!("[error] {e:#}");
                    }
                }
                None => println!("[unknown command: /{name}]"),
            }
            continue;
        }
        if let Err(e) = agent.run_turn(line, &observer).await {
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
    Ok(())
}

/// Interactive approver for the REPL/-p: prints the risky command, y/n on stdin.
struct StdinApprover {
    interactive: bool,
}

#[async_trait::async_trait]
impl Approver for StdinApprover {
    async fn approve(&self, tool: &str, detail: &str, why: &str) -> bool {
        if !self.interactive {
            return true; // piped -p mode: don't hang waiting for stdin
        }
        let tool = tool.to_string();
        let detail = detail.to_string();
        let why = why.to_string();
        tokio::task::spawn_blocking(move || {
            eprint!(
                "
[33m[approve?] {tool} — {why}
  {detail}
  allow? [y/N][0m "
            );
            std::io::stderr().flush().ok();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok()?;
            Some(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
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
