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

    if let Some(path) = &cli.dataflow {
        let report = dataflow::report(path).await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let cwd = cli.cwd.canonicalize().context("bad --cwd")?;

    let llm = Arc::new(OaiClient::new(&cli.base_url, &cli.api_key, &cli.model));
    let sessions = SessionLog::open(&cli.session_dir, &session_id()).await?;
    let mut registry = builtin_registry();
    for tool in sunmao_core::mcp::connect_all(&cwd).await {
        registry.register_boxed(tool);
    }
    let ctx = Arc::new(Context::new(llm, sessions, registry, cwd));

    let default_system = concat!(
        "You are sunmao, a coding agent. Use tools to act on the filesystem. ",
        "Prefer dedicated tools (Read/Write/Edit) over Bash for file work. ",
        "Be concise."
    );
    {
        let mut log = ctx.sessions.lock().await;
        log.append(&sunmao_core::SessionEvent::Started {
            model: cli.model.clone(),
            cwd: ctx.cwd.display().to_string(),
        })
        .await?;
        log.append(&sunmao_core::SessionEvent::Message {
            message: sunmao_llm::types::Message::system(
                cli.system.unwrap_or_else(|| default_system.to_string()),
            ),
        })
        .await?;
    }

    let agent = AgentLoop::new(ctx);

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
