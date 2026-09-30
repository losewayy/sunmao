//! `sunmao` — CLI library: the binary (`src/main.rs`) is a thin shell over
//! `run`; the Tauri desktop shell (`crates/gui`) embeds the same library and
//! calls `serve_main` directly — one host implementation, four frontends.

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
mod repl;
mod serve;
mod tui;

pub use serve::{Client, HostHandle, HostResponse, SANDBOX_PAGE};

#[derive(Parser)]
#[command(name = "sunmao", version, about = "agent harness kernel — 榫卯")]
pub struct Cli {
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

/// Resolve `--resume`/`--fork` into the log the session should open —
/// `fork` copies the source to a fresh id first. `None` = fresh session.
/// Shared by the interactive path and `serve` (each session the GUI host
/// adopts gets its own Context; only the log resolution is shared).
async fn open_first_log(cli: &Cli) -> anyhow::Result<Option<(SessionLog, &'static str)>> {
    let mut resume_target = cli.resume.clone();
    let mut source = "resume";
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
        source = "fork";
    }
    match &resume_target {
        Some(r) => {
            let p = PathBuf::from(r);
            let path = if p.exists() {
                p
            } else {
                cli.session_dir.join(format!("{r}.jsonl"))
            };
            Ok(Some((SessionLog::open_path(&path).await?, source)))
        }
        None => Ok(None),
    }
}

/// logging init — every process entry point (bin, gui shell) calls this
/// once before `run`/`serve_main`.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
}

/// The CLI entry point — `main` is `sunmao::run(Cli::parse())`.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    init_tracing();

    // `plugin` ops are pure file management — they never need a provider,
    // a session log, or any of the session setup below.
    if let Some(plugin::Cmd::Plugin(args)) = &cli.command {
        return plugin::run(args, &cli.cwd);
    }

    if cli.sessions {
        return repl::list_sessions(&cli.session_dir);
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

    // ── serve: the GUI is a multi-session host ──
    if let Some(plugin::Cmd::Serve { port }) = cli.command {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))
            .with_context(|| format!("bind 127.0.0.1:{port}"))?;
        return serve_main(&cli, listener).await;
    }

    let llm = provider_adapter(&cli);
    // --fork: copy the source log to a fresh id, then resume the copy
    let (sessions, resumed) = match open_first_log(&cli).await? {
        Some((log, _)) => (log, true),
        None => (
            SessionLog::open(&cli.session_dir, &session_id()).await?,
            false,
        ),
    };
    let mut registry = builtin_registry();
    let mcp = sunmao_core::mcp::connect_all(&cwd, &preset_roots).await;
    for tool in mcp.tools {
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
    ctx_raw.mcp_servers = mcp.servers;
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
    // -p is the non-interactive path — it never prompts and never
    // read_only-blocks; pin the stance so a resumed log's ModeChange
    // can't smuggle a restrictive mode into a headless run. (Recorded
    // posture only — the AllowAll approver is what actually skips asks.)
    if cli.print.is_some() {
        *ctx_raw.approval_mode.write().unwrap() = sunmao_core::agent::ApprovalMode::FullAccess;
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
            catalog: Vec::new(),
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
            message: sunmao_llm::types::Message::system(default_system.clone()),
        })
        .await?;
    }

    let agent = AgentLoop::new(ctx.clone());

    if let Some(prompt) = &cli.print {
        let obs = StdoutObserver {
            in_reasoning: std::sync::Mutex::new(false),
        };
        let outcome = agent.run_turn(prompt, &obs).await?;
        // SessionEnd hooks run in every frontend — a one-shot exit is
        // still a session ending (context-mode-style state capture hooks
        // depend on this event, not on which surface drove it).
        ctx.hooks
            .fire(
                sunmao_core::hooks::HookEvent::SessionEnd,
                &ctx.cwd,
                &sunmao_core::hooks::HookInput::default(),
            )
            .await;
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
        ctx.hooks
            .fire(
                sunmao_core::hooks::HookEvent::SessionEnd,
                &ctx.cwd,
                &sunmao_core::hooks::HookInput::default(),
            )
            .await;
        ctx.ext.shutdown().await;
        return res;
    }

    let observer = Arc::new(StdoutObserver {
        in_reasoning: std::sync::Mutex::new(false),
    });
    repl::run(
        &agent,
        &ctx,
        &repl::StdoutLoop(observer.clone() as Arc<dyn Observer>),
        &cwd,
        &preset_roots,
        resumed,
    )
    .await?;
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

/// The multi-session GUI host — `sunmao serve` and the Tauri shell both
/// funnel here. `listener` is pre-bound by the caller; pass a port-0 bind
/// for an ephemeral port. Runs until the process exits.
pub async fn serve_main(cli: &Cli, listener: std::net::TcpListener) -> anyhow::Result<()> {
    let spec = host_spec(cli).await?;
    serve::run(spec, listener).await
}

/// The host half of `serve_main` — everything except the TCP listeners.
/// The Tauri shell calls this and serves the same surface over its
/// `sunmao`/`sunmao-sandbox` schemes + an IPC Channel instead of HTTP/WS
/// (GUI.md §8); the returned handle is the transport-free front door.
pub async fn serve_host(cli: &Cli) -> anyhow::Result<HostHandle> {
    let spec = host_spec(cli).await?;
    serve::spawn_host(spec, 0).await
}

/// `Cli` → `HostSpec`: the Context assembly both host paths share —
/// provider, MCP servers, presets, model routes, `--loop` override.
/// Every session tab gets its own Context/AgentLoop (built by the
/// factory) — no shared Context swapping mid-tab. The MCP connections are
/// process-wide; each Context registers fresh tool handles sharing them.
async fn host_spec(cli: &Cli) -> anyhow::Result<serve::HostSpec> {
    let cwd = cli.cwd.canonicalize().context("bad --cwd")?;
    let preset_roots = sunmao_core::presets::resolve(&cwd, &cli.preset)?;
    let llm = provider_adapter(cli);
    let mcp = sunmao_core::mcp::connect_all(&cwd, &preset_roots).await;
    let first_log = open_first_log(cli).await?;
    let system_prompt = sunmao_core::prompt::PromptAssembler::new(&cwd)
        .with_extra_roots(&preset_roots)
        .assemble(cli.system.as_deref());
    let default_provider = sunmao_core::models::ProviderDef {
        base_url: cli.base_url.clone(),
        api_key_env: None,
        api_key: Some(cli.api_key.clone()),
        dialect: cli.provider.clone(),
        catalog: Vec::new(),
    };
    let model_label = cli.model.clone();
    let driver_override = cli.driver;
    let serve_roots = preset_roots.clone();
    let factory = serve::SessionFactory {
        make: Box::new(move |log, approver, cwd| {
            let llm = llm.clone();
            let mcp_servers = mcp.servers.clone();
            let preset_roots = preset_roots.clone();
            let driver = driver_override;
            let default_provider = default_provider.clone();
            Box::pin(async move {
                let mut registry = builtin_registry();
                for h in &mcp_servers {
                    for t in h.tool_impls() {
                        registry.register_boxed(t);
                    }
                }
                // `cwd` is the *session's* project — adopted logs carry
                // their own root (cross-project resume keeps it)
                let mut ctx_raw = Context::new(llm, log, registry, cwd.clone())
                    .with_extra_plugin_roots(preset_roots);
                ctx_raw.mcp_servers = mcp_servers;
                if let Some(d) = driver {
                    ctx_raw.loop_driver = d;
                }
                ctx_raw.connect_extensions().await;
                ctx_raw.approval = approver;
                ctx_raw.models = Some(Arc::new(sunmao_core::models::ModelResolver::load(
                    &cwd,
                    default_provider,
                    "default",
                )));
                Ok(ctx_raw)
            })
        }),
    };
    Ok(serve::HostSpec {
        factory,
        cwd: cwd.clone(),
        roots: serve_roots,
        // --system freezes the prompt; absent it each session's project
        // dir assembles its own (AGENTS.md etc. follow the project)
        prompt_override: cli.system.as_ref().map(|_| system_prompt.clone()),
        model_label,
        first_log,
    })
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
