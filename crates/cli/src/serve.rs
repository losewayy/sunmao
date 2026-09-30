//! `sunmao serve` — GUI 阶段 1：localhost HTTP + WebSocket，浏览器即前端。
//! 契约见 docs/GUI.md §7：WS 只载 `LiveEvent`（与 TUI/ACP 同源，不发明第
//! 三套方言），大对象走 `GET /artifacts/{name}` 路径引用。绑 127.0.0.1，
//! 无账号——单用户本地前端。
//!
//! 协议（单一 WS 通道 + REST 拉资源）：
//!   server → client: {"type":"live","event":<LiveEvent>} | {"type":"replay",
//!                    "events":[<SessionEvent>]} | {"type":"approval","id",
//!                    "tool","detail","why"} | {"type":"note","text"} |
//!                    {"type":"session","id"} | {"type":"model","label"} |
//!                    {"type":"busy","busy":bool}
//!   client → server: {"type":"prompt","text"} | {"type":"cancel"} |
//!                    {"type":"approval","id","verdict":"once|session|deny"} |
//!                    {"type":"resume","id"} | {"type":"fork","id"} |
//!                    {"type":"annotate","name","note"} | {"type":"model","sel"}
//!   REST: GET /sessions · GET /session · GET|POST /session/{id}/resume|fork ·
//!         GET /artifacts/{name} · GET /artifacts/{name}/notes ·
//!         POST /artifacts/{name}/annotate · GET /dataflow[/{id}]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use axum::Json;
use axum::extract::{Path as AxPath, Query, State, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use sunmao_core::agent::{AgentLoop, LiveEvent, Observer};
use sunmao_core::{Context, SessionEvent, SessionLog};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::tui;

mod artifacts;
mod driver;
mod ws;

/// A factory `main.rs` installs at startup: `build` reproduces the exact
/// Context assembly a session needs (provider, registry, MCP tools,
/// extensions, approver, model routes, `--loop`) so every session the host
/// adopts — startup, new, resume, fork — gets a fully-seeded kernel, not a
/// borrowed one.
pub struct SessionFactory {
    pub make: Box<
        dyn Fn(SessionLog, Arc<dyn sunmao_core::approval::Approver>) -> SessionBuild + Send + Sync,
    >,
}

/// The future `SessionFactory::make` returns (boxed so the closure stays
/// object-safe — the log/approver move in, nothing is borrowed).
pub type SessionBuild =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Context>> + Send>>;

impl SessionFactory {
    pub async fn build(
        &self,
        log: SessionLog,
        approver: Arc<dyn sunmao_core::approval::Approver>,
    ) -> Result<Arc<Context>> {
        Ok(Arc::new((self.make)(log, approver).await?))
    }
}

/// One live session the host is running — its own AgentLoop, input queue,
/// approval map and busy counter. Tabs are views onto hosts; nothing in
/// here is shared across sessions.
struct Host {
    id: String,
    agent: AgentLoop,
    /// The FIFO submission queue this session's driver drains.
    input: mpsc::UnboundedSender<String>,
    /// pending approval cards by id (reply oneshots)
    approvals: Arc<Pending>,
    /// submissions currently running (drives the busy badge + cancel)
    busy: std::sync::atomic::AtomicUsize,
}

/// The multi-session host: a registry of live sessions plus the global bus
/// every outbound frame fans out over (each carries its `sess` tag — the
/// frontend keeps the transcript scoped per tab, the rail reads every
/// session's state dots).
struct Shared {
    cwd: std::path::PathBuf,
    roots: Vec<std::path::PathBuf>,
    /// All host-originated traffic — one broadcast, `sess` carries the
    /// routing key. Session-less frames omit it.
    live: broadcast::Sender<serde_json::Value>,
    /// live session hosts by id
    sessions: Mutex<HashMap<String, Arc<Host>>>,
    /// builds a fully-seeded Context for a log (the startup assembly,
    /// reusable per session)
    factory: SessionFactory,
    /// assembled system prompt — fresh sessions re-seed it, same as startup
    system_prompt: String,
    /// model label recorded in a fresh session's Started event
    model_label: String,
    /// The MCP Apps sandbox listener's port — the spec's double-iframe
    /// needs a second origin; hello carries it so islands can point at
    /// `http://127.0.0.1:{sandbox_port}/sandbox.html`.
    sandbox_port: u16,
    /// process-wide approval id space (see Pending)
    approval_ids: Arc<AtomicU64>,
    /// host-management channel — session drivers can't `await adopt`
    /// directly (adopt spawns drivers, so that future would be recursive
    /// and rustc can't close the Send proof); ops ride a oneshot reply
    /// through `mgmt_loop` instead
    mgmt: mpsc::UnboundedSender<SessionOp>,
}

/// Host-management op — today just "make this session id live (possibly
/// as a fork)"; the reply carries the live id.
enum SessionOp {
    Adopt {
        id: String,
        fork: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
}

/// Drains `Shared.mgmt`: forks/resumes issued from inside a session driver
/// land here, one adoption at a time.
async fn mgmt_loop(s: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<SessionOp>) {
    while let Some(op) = rx.recv().await {
        match op {
            SessionOp::Adopt { id, fork, reply } => {
                let res = fork_or_resume(&s, &id, fork)
                    .await
                    .map(|v| v["session"].as_str().unwrap_or_default().to_string())
                    .map_err(|e| format!("{e:#}"));
                let _ = reply.send(res);
            }
        }
    }
}

impl Shared {
    /// Fan-out helper — one tagged frame over the global bus.
    fn emit(&self, v: serde_json::Value) {
        let _ = self.live.send(v);
    }

    /// The host for a session id, if it's live.
    fn host(&self, id: &str) -> Option<Arc<Host>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    /// Live session ids (the rail merges these with dormant disk logs).
    fn live_ids(&self) -> Vec<String> {
        self.sessions.lock().unwrap().keys().cloned().collect()
    }

    /// Register `log` as a live session: fresh Pending + Context via the
    /// factory + its own driver task, then SessionStart fires with the
    /// adoption's `source` (same vocabulary as TUI --resume: startup /
    /// resume / fork). Re-adopting a live id is a no-op — the same host
    /// answers every viewer.
    async fn adopt(self: &Arc<Self>, log: SessionLog, source: &str) -> Result<Arc<Host>> {
        let id = log
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".into());
        if let Some(h) = self.host(&id) {
            return Ok(h);
        }
        let pending = Arc::new(Pending::new(
            self.live.clone(),
            self.approval_ids.clone(),
            id.clone(),
        ));
        let approver = Arc::new(ServeApprover {
            pending: pending.clone(),
        });
        let ctx = self.factory.build(log, approver).await?;
        let agent = AgentLoop::new(ctx.clone());
        agent.set_live_sink(Arc::new(WsObserver::new(self.live.clone(), id.clone())));
        let (input_tx, input_rx) = mpsc::unbounded_channel::<String>();
        let host = Arc::new(Host {
            id: id.clone(),
            agent,
            input: input_tx,
            approvals: pending,
            busy: std::sync::atomic::AtomicUsize::new(0),
        });
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), host.clone());
        tokio::spawn(driver::driver(self.clone(), host.clone(), input_rx));
        // SessionStart fires on every adopt — same lifecycle a fresh
        // launch or TUI --resume produces; capture hooks see which path
        ctx.hooks
            .fire(
                sunmao_core::hooks::HookEvent::SessionStart,
                &ctx.cwd,
                &sunmao_core::hooks::HookInput {
                    source: Some(source),
                    ..Default::default()
                },
            )
            .await;
        self.emit(serde_json::json!({"type":"sessions_changed"}));
        Ok(host)
    }
}

/// One unanswered approval card — the reply oneshot plus the payload the
/// card was raised with (a tab switching into a session re-renders cards
/// from these, so a pending approval survives the view switch).
pub struct PendingCard {
    tx: oneshot::Sender<sunmao_core::approval::Approval>,
    tool: String,
    detail: String,
    why: String,
}

/// Approval state for ONE session — `Context.approval` installs the
/// approver holding this handle while the context is being built; the host
/// adopts the same instance. The id space is process-global so a verdict
/// can never hit the wrong session's card.
pub struct Pending {
    map: Mutex<HashMap<u64, PendingCard>>,
    /// shared across hosts — ids are already unique per process, a counter
    /// per session would collide cards from concurrent sessions
    next: Arc<AtomicU64>,
    /// approval requests go out over the same live bus as LiveEvents,
    /// tagged with this session's id
    live: broadcast::Sender<serde_json::Value>,
    sess: String,
}

impl Pending {
    fn new(live: broadcast::Sender<serde_json::Value>, next: Arc<AtomicU64>, sess: String) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            next,
            live,
            sess,
        }
    }

    /// Cards still awaiting a verdict — replayed to a tab that starts
    /// viewing this session.
    fn cards(&self) -> Vec<serde_json::Value> {
        self.map
            .lock()
            .unwrap()
            .iter()
            .map(|(id, c)| {
                serde_json::json!({"id": id, "tool": c.tool, "detail": c.detail, "why": c.why})
            })
            .collect()
    }
}

/// Approver seam — the risky-call gate suspends on a oneshot while the
/// browser shows the card. Same contract as TuiApprover.
pub struct ServeApprover {
    pub pending: Arc<Pending>,
}

#[async_trait::async_trait]
impl sunmao_core::approval::Approver for ServeApprover {
    async fn approve(
        &self,
        tool: &str,
        detail: &str,
        why: &str,
    ) -> sunmao_core::approval::Approval {
        let id = self.pending.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.map.lock().unwrap().insert(
            id,
            PendingCard {
                tx,
                tool: tool.to_string(),
                detail: detail.to_string(),
                why: why.to_string(),
            },
        );
        let _ = self.pending.live.send(serde_json::json!({
            "type": "approval", "id": id, "sess": self.pending.sess,
            "tool": tool, "detail": detail, "why": why,
        }));
        // verdicts route through the map by id — ws ordering never decides
        rx.await.unwrap_or(sunmao_core::approval::Approval::Deny)
    }
}

/// Observer → broadcast, tagged with its session id. LiveEvent serializes
/// as the session-neutral wire shape (tagged enum) — frontends never see
/// a second dialect; `sess` is the tab's routing key.
pub struct WsObserver {
    live: broadcast::Sender<serde_json::Value>,
    sess: String,
}
impl WsObserver {
    pub fn new(live: broadcast::Sender<serde_json::Value>, sess: impl Into<String>) -> Self {
        Self {
            live,
            sess: sess.into(),
        }
    }
}
impl Observer for WsObserver {
    fn on_event(&self, ev: &LiveEvent) {
        let _ = self.live.send(serde_json::json!({
            "type": "live",
            "sess": self.sess,
            "event": serde_json::to_value(ev).unwrap_or_default(),
        }));
    }
}

/// Index page — the workbench prototype adapted as the product frontend
/// (docs/DESIGN.md tokens are its source of truth).
const INDEX: &str = include_str!("serve/assets/index.html");
/// MCP Apps sandbox proxy — a separate origin serving a single static
/// page (`serve/assets/sandbox.html`); see `sandbox_page`/`ui/` bridge.
const SANDBOX: &str = include_str!("serve/assets/sandbox.html");

/// Windows `canonicalize` yields `\?\`-prefixed paths — strip the prefix
/// for display so the GUI's crumb shows `F:\…`, not the UNC form.
fn display_path(p: &std::path::Path) -> String {
    p.display().to_string().replace("\\?\\", "")
}

/// artifact names are `[a-z0-9_-]` — the same whitelist /annotate enforces;
/// anything else can't resolve to a file under .sunmao/artifacts anyway.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

async fn sessions_list(State(s): State<Arc<Shared>>) -> impl IntoResponse {
    // the rail = dormant logs on disk ∪ live hosts (a session the host is
    // running exists even when its log hasn't flushed a fresh name yet)
    let mut ids = tui::menu::recent_sessions(&s.cwd, 50);
    for id in s.live_ids() {
        if !ids.contains(&id) {
            ids.insert(0, id);
        }
    }
    Json(serde_json::json!({
        "sessions": ids,
        "live": s.live_ids(),
    }))
}

async fn session_info(
    State(s): State<Arc<Shared>>,
    Query(q): Query<InfoQuery>,
) -> impl IntoResponse {
    let id = s.host(&q.id.unwrap_or_default());
    Json(serde_json::json!({
        "id": id.as_ref().map(|h| h.id.clone()),
        "live": s.live_ids(),
        "cwd": display_path(&s.cwd),
    }))
}

#[derive(serde::Deserialize)]
struct InfoQuery {
    id: Option<String>,
}

fn log_path(s: &Shared, id: &str) -> Option<std::path::PathBuf> {
    // `id` may be a bare session id (resolved under .sunmao/sessions) or a
    // path — paths are allowed only if they stay inside the sessions dir,
    // otherwise a GET could read arbitrary files as JSONL.
    let p = std::path::PathBuf::from(id);
    let cand = if p.exists() {
        p
    } else {
        s.cwd.join(".sunmao/sessions").join(format!("{id}.jsonl"))
    };
    cand.exists().then_some(cand)
}

/// `POST /session/new` — a fresh log with the same Started+system seeding
/// `main.rs` gives startup sessions, adopted as its own live host.
async fn new_session(State(s): State<Arc<Shared>>) -> Response {
    match new_session_inner(&s).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn new_session_inner(s: &Arc<Shared>) -> Result<serde_json::Value> {
    // millis suffix: /new within the same second must not collide
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let id = format!("{}-{}", crate::session_id(), ms % 1000);
    let dir = s.cwd.join(".sunmao/sessions");
    let mut log = sunmao_core::SessionLog::open(&dir, &id).await?;
    log.append(&SessionEvent::Started {
        model: s.model_label.clone(),
        cwd: display_path(&s.cwd),
    })
    .await?;
    log.append(&SessionEvent::Message {
        message: sunmao_llm::types::Message::system(s.system_prompt.clone()),
    })
    .await?;
    let host = s.adopt(log, "startup").await?;
    Ok(serde_json::json!({"session": host.id}))
}

async fn resume_session(State(s): State<Arc<Shared>>, AxPath(id): AxPath<String>) -> Response {
    match fork_or_resume(&s, &id, false).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

async fn fork_session(State(s): State<Arc<Shared>>, AxPath(id): AxPath<String>) -> Response {
    match fork_or_resume(&s, &id, true).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

/// resume/fork = adopt the target (or its copy) as a live host and let the
/// caller's tab switch views — the previous session keeps running in its
/// own host instead of being swapped away mid-tab.
async fn fork_or_resume(s: &Arc<Shared>, id: &str, fork: bool) -> Result<serde_json::Value> {
    let src = log_path(s, id).context("no such session")?;
    let path = if fork {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let dst = s
            .cwd
            .join(".sunmao/sessions")
            .join(format!("s-{ms}-fork.jsonl"));
        std::fs::copy(&src, &dst).with_context(|| format!("copy {}", src.display()))?;
        dst
    } else {
        src
    };
    let log = sunmao_core::SessionLog::open_path(&path).await?;
    let host = s.adopt(log, if fork { "fork" } else { "resume" }).await?;
    Ok(serde_json::json!({"session": host.id}))
}

/// The MCP Apps sandbox proxy page (spec: host and sandbox MUST be
/// different origins — this rides its own listener, port reported in the
/// ws hello). Same embedded file every request; it has no state.
async fn sandbox_page() -> Html<&'static str> {
    Html(SANDBOX)
}

async fn dataflow_current(State(s): State<Arc<Shared>>, Query(q): Query<InfoQuery>) -> Response {
    // ?sess=<id> picks a live host's log; without it the report reads the
    // newest session log on disk (a dormant-but-just-finished session is
    // still reportable)
    let p = match q.id.as_deref().and_then(|id| s.host(id)) {
        Some(h) => h.agent.session_path().await,
        None => match s.live_ids().first().and_then(|id| s.host(id)) {
            Some(h) => h.agent.session_path().await,
            None => match tui::menu::recent_sessions(&s.cwd, 1).first() {
                Some(id) => match log_path(&s, id) {
                    Some(p) => p,
                    None => return (StatusCode::NOT_FOUND, "no sessions").into_response(),
                },
                None => return (StatusCode::NOT_FOUND, "no sessions").into_response(),
            },
        },
    };
    match crate::dataflow::report(&p).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn dataflow_by_id(State(s): State<Arc<Shared>>, AxPath(id): AxPath<String>) -> Response {
    match log_path(&s, &id) {
        Some(p) => match crate::dataflow::report(&p).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        },
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

/// Slash-command list for the composer menu — same candidates the TUI
/// shows (builtins + file commands), minus pure-TUI affordances.
fn slash_candidates(s: &Shared) -> Vec<String> {
    crate::tui::slash::candidates(&s.cwd, &s.roots)
        .into_iter()
        .filter(|n| *n != "multiline" && *n != "clear" && *n != "quit")
        .collect()
}

/// Serve until the process exits. The web assets are embedded — no node,
/// no build step, `sunmao serve` is the whole deploy story. `listener` is
/// the caller's pre-bound main socket (the Tauri shell binds port 0 so it
/// can learn the port before opening its webview); the MCP Apps sandbox
/// proxy still gets its own listener — the spec's double-iframe needs a
/// second origin, reported as `sandbox_port` in the ws hello.
/// `factory` rebuilds the startup Context assembly per session — the host
/// adopts every log as its own AgentLoop instead of swapping one shared
/// Context between tabs.
pub async fn run(
    factory: SessionFactory,
    cwd: std::path::PathBuf,
    roots: Vec<std::path::PathBuf>,
    listener: std::net::TcpListener,
    system_prompt: String,
    model_label: String,
    // `first_log`: `--resume`/`--fork` target resolved by main — `None`
    // boots a fresh seeded session
    first_log: Option<(SessionLog, &'static str)>,
) -> Result<()> {
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let sandbox_port_hint = port.saturating_add(1);
    let sandbox_app = axum::Router::new().route("/sandbox.html", get(sandbox_page));
    let sandbox_listener = tokio::net::TcpListener::bind(("127.0.0.1", sandbox_port_hint))
        .await
        .or(tokio::net::TcpListener::bind(("127.0.0.1", 0u16)).await)?;
    let sandbox_port = sandbox_listener.local_addr()?.port();
    tokio::spawn(async move {
        let _ = axum::serve(sandbox_listener, sandbox_app).await;
    });

    let (live, _) = broadcast::channel::<serde_json::Value>(512);
    let (mgmt_tx, mgmt_rx) = mpsc::unbounded_channel::<SessionOp>();
    let shared = Arc::new(Shared {
        cwd,
        roots,
        live,
        sessions: Mutex::new(HashMap::new()),
        factory,
        system_prompt,
        model_label,
        sandbox_port,
        approval_ids: Arc::new(AtomicU64::new(0)),
        mgmt: mgmt_tx,
    });
    tokio::spawn(mgmt_loop(shared.clone(), mgmt_rx));

    // bootstrap session: `first_log` carries --resume/--fork; otherwise a
    // fresh seeded log — the host, not main.rs, owns session assembly
    {
        match first_log {
            Some((log, source)) => {
                shared.adopt(log, source).await?;
            }
            None => {
                let id = crate::session_id();
                let dir = shared.cwd.join(".sunmao/sessions");
                let mut log = sunmao_core::SessionLog::open(&dir, &id).await?;
                log.append(&SessionEvent::Started {
                    model: shared.model_label.clone(),
                    cwd: display_path(&shared.cwd),
                })
                .await?;
                log.append(&SessionEvent::Message {
                    message: sunmao_llm::types::Message::system(shared.system_prompt.clone()),
                })
                .await?;
                shared.adopt(log, "startup").await?;
            }
        }
    }

    let app = axum::Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/ws", get(ws::ws_upgrade))
        .route("/sessions", get(sessions_list))
        .route("/session", get(session_info))
        .route("/session/new", post(new_session))
        .route("/session/{id}/resume", post(resume_session))
        .route("/session/{id}/fork", post(fork_session))
        .route("/artifacts/{name}", get(artifacts::artifact_get))
        .route("/artifacts/{name}/revs", get(artifacts::artifact_revs))
        .route("/artifacts/{name}/ui", get(artifacts::artifact_ui))
        .route("/artifacts/{name}/notes", get(artifacts::artifact_notes))
        .route(
            "/artifacts/{name}/annotate",
            post(artifacts::artifact_annotate),
        )
        .route("/dataflow", get(dataflow_current))
        .route("/dataflow/{id}", get(dataflow_by_id))
        .with_state(shared.clone());

    let listener = tokio::net::TcpListener::from_std(listener)?;
    eprintln!("sunmao serve → http://127.0.0.1:{port}  (Ctrl-C to stop)");
    axum::serve(listener, app).await?;
    Ok(())
}
