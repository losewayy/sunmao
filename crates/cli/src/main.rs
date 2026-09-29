//! `sunmao` — stdin/stdout REPL over the kernel. TUI lands in v0.2.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use sunmao_core::agent::{AgentLoop, LiveEvent, Observer, TurnOutcome};
use sunmao_core::tool::builtin_registry;
use sunmao_core::{Context, SessionLog};
use sunmao_llm::OaiClient;

mod acp;
mod dataflow;
mod tui;

#[derive(Parser)]
#[command(name = "sunmao", about = "agent harness kernel — 榫卯")]
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
    /// Run as an Agent Client Protocol server on stdio (Zed etc.).
    #[arg(long)]
    acp: bool,
    /// Resume an existing session log (id like `s-123` or a .jsonl path).
    #[arg(long)]
    resume: Option<String>,
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
            LiveEvent::ToolStart { name } => {
                if *in_r {
                    eprintln!("\x1b[0m");
                    *in_r = false;
                }
                println!("\n\x1b[36m[tool → {name}]\x1b[0m");
            }
            LiveEvent::ToolDone { name, ok } => {
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

    if cli.acp {
        return acp::run(&cli.base_url, &cli.api_key, &cli.model)
            .await
            .map_err(|e| anyhow::anyhow!("acp: {e}"));
    }

    if let Some(path) = &cli.dataflow {
        let report = dataflow::report(path).await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let cwd = cli.cwd.canonicalize().context("bad --cwd")?;

    let llm = Arc::new(OaiClient::new(&cli.base_url, &cli.api_key, &cli.model));
    let (sessions, resumed) = match &cli.resume {
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
    let ctx = Arc::new(Context::new(llm, sessions, registry, cwd.clone()));

    let default_system = concat!(
        "You are sunmao, a coding agent. Use tools to act on the filesystem. ",
        "Prefer dedicated tools (Read/Write/Edit) over Bash for file work. ",
        "Be concise."
    );
    let sys_extra = project_context(&cwd);
    let default_system = if sys_extra.is_empty() {
        default_system.to_string()
    } else {
        format!(
            "{default_system}

# Project context
{sys_extra}"
        )
    };
    let default_system = cli.system.unwrap_or(default_system);

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

    let agent = AgentLoop::new(ctx);

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
        return tui::run(agent, &cli.model).await;
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
        if line == "/compact" {
            match agent.compact(&observer).await {
                Ok(()) => println!("[compacted]"),
                Err(e) => eprintln!("[compact failed] {e:#}"),
            }
            continue;
        }
        if let Err(e) = agent.run_turn(line, &observer).await {
            eprintln!("[error] {e:#}");
        }
    }
    Ok(())
}

/// Collect project-level context: AGENTS.md / CLAUDE.md bodies plus a skills
/// index from `.sunmao/skills/*/SKILL.md` (name + description frontmatter —
/// bodies are Read on demand).
fn project_context(cwd: &std::path::Path) -> String {
    let mut out = String::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let p = cwd.join(name);
        if let Ok(text) = std::fs::read_to_string(&p) {
            let text: String = text.chars().take(8_000).collect();
            out.push_str(&format!("## {name}\n{text}\n\n"));
        }
    }
    let skills_dir = cwd.join(".sunmao").join("skills");
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        let mut lines = Vec::new();
        for e in entries.flatten() {
            let skill = e.path().join("SKILL.md");
            if let Ok(text) = std::fs::read_to_string(&skill) {
                let mut name = e.file_name().to_string_lossy().to_string();
                let mut desc = String::new();
                for line in text.lines().take(20) {
                    if let Some(v) = line.strip_prefix("name:") {
                        name = v.trim().to_string();
                    }
                    if let Some(v) = line.strip_prefix("description:") {
                        desc = v.trim().to_string();
                    }
                }
                lines.push(format!("- {} — {} ({})", name, desc, skill.display()));
            }
        }
        if !lines.is_empty() {
            out.push_str("## Available skills (Read the SKILL.md path to load)\n");
            out.push_str(&lines.join("\n"));
            out.push('\n');
        }
    }
    out
}
