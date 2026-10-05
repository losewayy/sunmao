//! `sunmao` — CLI library: the binary (`src/main.rs`) is a thin shell over
//! `run`; the Tauri desktop shell (`crates/gui`) embeds the same library and
//! calls `serve_main` directly — one host implementation, four frontends.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use sunmao_core::context::RwLockRecover;

use anyhow::Context as _;
use clap::Parser;
use sunmao_core::agent::{AgentLoop, Observer, TurnOutcome};
use sunmao_core::approval::{Approval, Approver};
use sunmao_core::tool::builtin_registry;
use sunmao_core::{Context, SessionLog};
use sunmao_llm::OaiClient;

mod acp;
mod attachments;
mod commands;
mod dataflow;
mod doctor;
mod eval;
mod im;
mod plugin;
mod repl;
mod rewind;
mod serve;
mod sessions;
mod tui;

pub use serve::{ANNOTATE_JS, Client, HostHandle, HostResponse, I18N_EN_JS, I18N_JS, SANDBOX_PAGE};

#[derive(Parser, Clone)]
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
    /// Loop driver override — `full` (contract loop), `bare` (no hooks,
    /// no gate, no auto-compaction) or `ptc` (full loop, RunCode+SearchTools
    /// tool surface — every other tool is reachable via the sandbox's `tools.*`).
    /// Wins over any manifest `loop:` key.
    #[arg(long = "loop", value_parser = parse_driver)]
    driver: Option<sunmao_core::agent::LoopDriver>,
    /// Approval stance for the session: always_ask · auto · read_only ·
    /// full_access. Under `-p` this is the only way to grant ask-prompted
    /// calls — `full_access` skips them, an explicit allow rule or session
    /// grant answers them; everything else that would prompt is denied.
    #[arg(long, value_parser = parse_approval_mode)]
    mode: Option<sunmao_core::agent::ApprovalMode>,
    #[command(subcommand)]
    command: Option<plugin::Cmd>,
}

/// clap needs a String error, not anyhow — same refusal surface.
fn parse_driver(s: &str) -> Result<sunmao_core::agent::LoopDriver, String> {
    sunmao_core::agent::LoopDriver::parse(s).map_err(|e| e.to_string())
}

/// `--mode` spellings are `ApprovalMode::parse`'s — unknown names refuse.
fn parse_approval_mode(s: &str) -> Result<sunmao_core::agent::ApprovalMode, String> {
    sunmao_core::agent::ApprovalMode::parse(s)
        .ok_or_else(|| "unknown mode — always_ask · auto · read_only · full_access".to_string())
}

fn session_id() -> String {
    // same-second spawns (double-clicked GUI + a TUI share the project)
    // used to collide on `s-{secs}` and silently overwrite each other's
    // log — pid + a per-process counter keeps every id unique.
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("s-{now}-{:x}-{seq:x}", std::process::id())
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
    init_tracing_to(std::io::stdout);
}

/// Sink-parametrized init — ACP owns stdout for JSON-RPC, so it routes
/// tracing to stderr; anything else keeps the default stdout sink.
fn init_tracing_to(
    w: impl for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(w)
        .init();
}

/// The CLI entry point — `main` is `sunmao::run(Cli::parse())`.
pub async fn run(mut cli: Cli) -> anyhow::Result<()> {
    // ACP protocol stdout must stay JSON-RPC-only — a stray warn! on
    // stdout corrupts the wire, so that branch gets its own sink before
    // any log line can be emitted.
    if cli.acp {
        init_tracing_to(std::io::stderr);
    } else {
        init_tracing();
    }

    // `--session-dir` is relative to `--cwd`, not the shell's cwd — the
    // default `.sunmao/sessions` MUST land inside the project being
    // worked on, and a `--cwd` without a re-anchor writes/reads two
    // different directories (session.log opens in one, /resume lists
    // the other). Absolute values pass through unchanged.
    if cli.session_dir.is_relative() {
        cli.session_dir = cli.cwd.join(&cli.session_dir);
    }

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

    // pairing ops are pure state-file management — same early-exit class
    // as `plugin`: no provider, no session setup
    if let Some(plugin::Cmd::Pairing(args)) = &cli.command {
        return im::pairing(&args.op);
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

    // ── im: IM gateway daemon — same host machinery, channel adapters
    // as the front door ──
    if let Some(plugin::Cmd::Im) = cli.command {
        return im::run(&cli).await;
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
    let mut sessions = sessions;
    let registry = builtin_registry();
    let mcp = sunmao_core::mcp::connect_all(&cwd, &preset_roots).await;
    sunmao_core::mcp::audit_skips(&mcp.skipped, &mut sessions).await;
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
    let (tx_approval, rx_approval) = tokio::sync::mpsc::unbounded_channel::<tui::ApprovalMsg>();
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
    } else if cli.print.is_some() {
        // non-interactive: no human will ever answer a card, so every ask
        // downgrades to a deny — the reason rides the failed ToolResult so
        // the model sees why. full_access (via --mode) and allow rules
        // never reach the approver, so they stay the escape hatches.
        ctx_raw.approval = Arc::new(sunmao_core::approval::PipedApprover);
    } else if interactive {
        ctx_raw.approval = Arc::new(StdinApprover);
    }
    // --mode picks the stance explicitly; under -p an unset flag clamps a
    // resumed log's full_access back to auto — headless runs deny asks by
    // default, never silently skip them. Restrictive resumed modes
    // (read_only/always_ask) are honored: they only refuse more.
    if let Some(mode) = cli.mode {
        *ctx_raw.approval_mode.write_or_recover() = mode;
    } else if cli.print.is_some()
        && *ctx_raw.approval_mode.read_or_recover() == sunmao_core::agent::ApprovalMode::FullAccess
    {
        *ctx_raw.approval_mode.write_or_recover() = sunmao_core::agent::ApprovalMode::Auto;
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
    ctx.fire_session_start(if resumed { "resume" } else { "startup" })
        .await;

    // The system prompt is assembled, not constant: built-in section files →
    // ~/.sunmao/prompt{,.d} → .sunmao/prompt{,.d} → project context →
    // --system as the complete override. All frontends share this path.
    let default_system = sunmao_core::prompt::PromptAssembler::new(&cwd)
        .with_extra_roots(&preset_roots)
        .with_driver(ctx.loop_driver)
        .assemble(cli.system.as_deref());

    if !resumed {
        let mut log = ctx.sessions.lock().await;
        log.append(&sunmao_core::SessionEvent::Started {
            model: cli.model.clone(),
            cwd: ctx.cwd.display().to_string(),
            driver: Some(ctx.loop_driver.as_str().into()),
        })
        .await?;
        log.append(&sunmao_core::SessionEvent::Message {
            message: sunmao_llm::types::Message::system(default_system.clone()),
        })
        .await?;
    }

    let agent = AgentLoop::new(ctx.clone());
    // settle the session's default effort before the first request: the
    // local frontends have no frame to carry it, and the level in force is
    // what the prompt line / TUI chip shows
    agent.resolve_effort_default().await;

    if let Some(prompt) = &cli.print {
        let obs = repl::StdoutObserver::new();
        let (prompt, atts) = attachments::attach_mentions(prompt, &ctx.cwd);
        let outcome = agent.run_turn_blocks(&prompt, &atts, &obs).await?;
        // SessionEnd hooks run in every frontend — a one-shot exit is
        // still a session ending (context-mode-style state capture hooks
        // depend on this event, not on which surface drove it).
        // bounded advisory: a wedged capture hook gets 5s, not the
        // default hook timeout — the process is exiting either way
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            ctx.hooks.fire(
                sunmao_core::hooks::HookEvent::SessionEnd,
                &ctx.cwd,
                &sunmao_core::hooks::HookInput::default(),
            ),
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
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            ctx.hooks.fire(
                sunmao_core::hooks::HookEvent::SessionEnd,
                &ctx.cwd,
                &sunmao_core::hooks::HookInput::default(),
            ),
        )
        .await;
        ctx.ext.shutdown().await;
        return res;
    }

    let observer = Arc::new(repl::StdoutObserver::new());
    repl::run(
        &agent,
        &ctx,
        &repl::StdoutLoop(observer.clone() as Arc<dyn Observer>),
        &cwd,
        &preset_roots,
        resumed,
    )
    .await?;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ctx.hooks.fire(
            sunmao_core::hooks::HookEvent::SessionEnd,
            &ctx.cwd,
            &sunmao_core::hooks::HookInput::default(),
        ),
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
    // untrusted-server skips audit into every session tab's own log —
    // serve connects MCP process-wide before any session exists
    let mcp_skips = mcp.skipped.clone();
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
            let mcp_skips = mcp_skips.clone();
            let preset_roots = preset_roots.clone();
            let driver = driver_override;
            let default_provider = default_provider.clone();
            Box::pin(async move {
                let mut log = log;
                sunmao_core::mcp::audit_skips(&mcp_skips, &mut log).await;
                let registry = builtin_registry();
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
        driver_override: cli.driver,
    })
}

/// Interactive approver for the REPL: prints the risky command, y/n on stdin.
/// (Piped `-p` never installs this — core's `PipedApprover` denies asks
/// instead of hanging on a prompt nobody can answer.)
struct StdinApprover;

#[async_trait::async_trait]
impl Approver for StdinApprover {
    async fn approve(&self, tool: &str, detail: &str, why: &str) -> Approval {
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
                _ => Approval::Deny { reason: None },
            })
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(Approval::Deny { reason: None })
    }
}
