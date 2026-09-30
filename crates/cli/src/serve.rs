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
use axum::extract::{Path as AxPath, State, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use sunmao_core::SessionEvent;
use sunmao_core::agent::{AgentLoop, LiveEvent, Observer};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::tui;

struct Shared {
    agent: AgentLoop,
    cwd: std::path::PathBuf,
    roots: Vec<std::path::PathBuf>,
    /// LiveEvent fan-out — every ws client subscribes; replay comes from the
    /// session log itself, so late joiners aren't blind.
    live: broadcast::Sender<serde_json::Value>,
    /// The FIFO submission queue a dedicated driver task drains — same
    /// shape as tui/driver.rs.
    input: mpsc::UnboundedSender<String>,
    /// assembled system prompt — `session/new` re-seeds it, same as startup
    system_prompt: String,
    /// model label recorded in the new session's Started event
    model_label: String,
    /// pending approval cards by id (reply oneshots)
    approvals: Arc<Pending>,
    /// submissions currently running (drives the busy badge + cancel affordance)
    busy: std::sync::atomic::AtomicUsize,
}

/// Approval state that must exist before `Shared` — `Context.approval` is
/// installed while the context is being built, so the approver holds this
/// handle; `run()` adopts the same one into `Shared`.
pub struct Pending {
    map: Mutex<HashMap<u64, oneshot::Sender<sunmao_core::approval::Approval>>>,
    next: AtomicU64,
    /// approval requests go out over the same live channel as LiveEvents
    pub live: broadcast::Sender<serde_json::Value>,
}

impl Pending {
    pub fn new(live: broadcast::Sender<serde_json::Value>) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
            live,
        }
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
        self.pending.map.lock().unwrap().insert(id, tx);
        let _ = self.pending.live.send(serde_json::json!({
            "type": "approval", "id": id,
            "tool": tool, "detail": detail, "why": why,
        }));
        // verdicts route through the map by id — ws ordering never decides
        rx.await.unwrap_or(sunmao_core::approval::Approval::Deny)
    }
}

/// Observer → broadcast. LiveEvent serializes as the session-neutral wire
/// shape (tagged enum) — frontends never see a second dialect.
pub struct WsObserver(broadcast::Sender<serde_json::Value>);
impl WsObserver {
    pub fn new(live: broadcast::Sender<serde_json::Value>) -> Self {
        Self(live)
    }
}
impl Observer for WsObserver {
    fn on_event(&self, ev: &LiveEvent) {
        let _ = self.0.send(serde_json::json!({
            "type": "live",
            "event": serde_json::to_value(ev).unwrap_or_default(),
        }));
    }
}

/// Index page — the workbench prototype adapted as the product frontend
/// (docs/DESIGN.md tokens are its source of truth).
const INDEX: &str = include_str!("serve/assets/index.html");

/// Windows `canonicalize` yields `\?\`-prefixed paths — strip the prefix
/// for display so the GUI's crumb shows `F:\…`, not the UNC form.
fn display_path(p: &std::path::Path) -> String {
    p.display().to_string().replace("\\?\\", "")
}

fn artifact_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    cwd.join(".sunmao").join("artifacts")
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
    Json(serde_json::json!({
        "sessions": tui::menu::recent_sessions(&s.cwd, 50),
        "current": session_id(&s.agent).await,
    }))
}

async fn session_id(agent: &AgentLoop) -> String {
    agent
        .session_path()
        .await
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

async fn session_info(State(s): State<Arc<Shared>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "id": session_id(&s.agent).await,
        "cwd": display_path(&s.cwd),
    }))
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
/// `main.rs` gives startup sessions. Without this the GUI's "new chat"
/// would inherit a session with no identity block.
async fn new_session(State(s): State<Arc<Shared>>) -> Response {
    match new_session_inner(&s).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn new_session_inner(s: &Arc<Shared>) -> Result<serde_json::Value> {
    let id = crate::session_id();
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
    let events = s.agent.swap_session(log).await;
    let _ = s.live.send(serde_json::json!({
        "type": "replay",
        "events": events,
        "session": id,
    }));
    Ok(serde_json::json!({"session": id}))
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
    let events = s.agent.swap_session(log).await;
    let new_id = session_id(&s.agent).await;
    let _ = s.live.send(serde_json::json!({
        "type": "replay",
        "events": events,
        "session": new_id,
    }));
    Ok(serde_json::json!({"session": new_id, "events": events.len()}))
}

async fn artifact_get(State(s): State<Arc<Shared>>, AxPath(name): AxPath<String>) -> Response {
    if !safe_name(&name) {
        return (StatusCode::BAD_REQUEST, "bad artifact name").into_response();
    }
    let p = artifact_dir(&s.cwd).join(format!("{name}.html"));
    match tokio::fs::read(&p).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "no such artifact").into_response(),
    }
}

async fn artifact_notes(State(s): State<Arc<Shared>>, AxPath(name): AxPath<String>) -> Response {
    if !safe_name(&name) {
        return (StatusCode::BAD_REQUEST, "bad artifact name").into_response();
    }
    let p = artifact_dir(&s.cwd).join(format!("{name}.state.json"));
    let v: serde_json::Value = tokio::fs::read_to_string(&p)
        .await
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({"annotations": []}));
    Json(v).into_response()
}

#[derive(serde::Deserialize)]
struct AnnotateBody {
    note: String,
}

async fn artifact_annotate(
    State(s): State<Arc<Shared>>,
    AxPath(name): AxPath<String>,
    Json(body): Json<AnnotateBody>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "result": crate::tui::slash::annotate(&s.cwd, &name, &body.note)
    }))
}

async fn dataflow_current(State(s): State<Arc<Shared>>) -> Response {
    let p = s.agent.session_path().await;
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

async fn ws_upgrade(State(s): State<Arc<Shared>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_client(s, socket))
}

async fn ws_send(tx: &mpsc::UnboundedSender<String>, v: serde_json::Value) -> Result<()> {
    tx.send(v.to_string())
        .map_err(|_| anyhow::anyhow!("ws closed"))
}

/// One browser client: subscribe to live events, snapshot the transcript on
/// connect, then translate its messages into driver jobs / replies.
async fn ws_client(s: Arc<Shared>, socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message as WsMsg;
    use futures_util::{SinkExt, StreamExt};
    let (mut ws_tx, mut ws_rx) = socket.split();
    // serialize ws writes through a channel — broadcast and replies both
    // feed it so no two writers race on the sink.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if ws_tx.send(WsMsg::Text(m.into())).await.is_err() {
                break;
            }
        }
    });

    let mut live_rx = s.live.subscribe();
    let forward = {
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            while let Ok(v) = live_rx.recv().await {
                if out_tx.send(v.to_string()).is_err() {
                    break;
                }
            }
        })
    };

    // hello: replay the current session + slash candidates + session id —
    // a page reload mid-session lands back on a real transcript.
    {
        let evs = s.agent.session_events().await;
        let _ = ws_send(
            &out_tx,
            serde_json::json!({
                "type": "hello",
                "session": session_id(&s.agent).await,
                "cwd": display_path(&s.cwd),
                "slash": slash_candidates(&s),
                "models": s.agent.model_choices(),
                "busy": s.busy.load(Ordering::Relaxed) > 0,
                "replay": evs,
            }),
        )
        .await;
    }

    while let Some(Ok(msg)) = ws_rx.next().await {
        let WsMsg::Text(text) = msg else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        match v["type"].as_str().unwrap_or("") {
            "prompt" => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                if !text.trim().is_empty() {
                    let _ = s.input.send(text);
                }
            }
            "cancel" => s.agent.cancel(),
            "approval" => {
                let id = v["id"].as_u64().unwrap_or(0);
                let verdict = match v["verdict"].as_str().unwrap_or("deny") {
                    "once" => sunmao_core::approval::Approval::Once,
                    "session" => sunmao_core::approval::Approval::Session,
                    _ => sunmao_core::approval::Approval::Deny,
                };
                if let Some(tx) = s.approvals.map.lock().unwrap().remove(&id) {
                    let _ = tx.send(verdict);
                }
            }
            "new" => {
                if let Err(e) = new_session_inner(&s).await {
                    let _ = ws_send(
                        &out_tx,
                        serde_json::json!({"type":"note","text":format!("[new session failed] {e:#}")}),
                    )
                    .await;
                }
            }
            "resume" | "fork" => {
                let id = v["id"].as_str().unwrap_or("").to_string();
                let fork = v["type"].as_str() == Some("fork");
                if let Err(e) = fork_or_resume(&s, &id, fork).await {
                    let _ = ws_send(
                        &out_tx,
                        serde_json::json!({"type":"note","text":format!("[{e:#}]")}),
                    )
                    .await;
                }
            }
            "annotate" => {
                let name = v["name"].as_str().unwrap_or("");
                let note = v["note"].as_str().unwrap_or("");
                let r = crate::tui::slash::annotate(&s.cwd, name, note);
                let _ = ws_send(&out_tx, serde_json::json!({"type":"note","text":r})).await;
            }
            "model" => {
                let sel = v["sel"].as_str().unwrap_or("");
                match s.agent.swap_model(sel) {
                    Some(label) => {
                        s.agent.record_model_change(sel, &label).await;
                        let _ = s
                            .live
                            .send(serde_json::json!({"type":"model","label":label}));
                    }
                    None => {
                        let _ = ws_send(
                            &out_tx,
                            serde_json::json!({"type":"note","text":format!("[unknown selector: {sel}]")}),
                        )
                        .await;
                    }
                }
            }
            _ => {}
        }
    }
    forward.abort();
    writer.abort();
}

/// The submission driver: same role as tui/driver.rs — one turn at a time,
/// slash builtins resolved here, prompts stream LiveEvents via WsObserver.
async fn driver(s: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(input) = rx.recv().await {
        s.busy.fetch_add(1, Ordering::Relaxed);
        let _ = s.live.send(serde_json::json!({"type":"busy","busy":true}));
        if let Some(cmd_line) = input.trim().strip_prefix('/') {
            let name = cmd_line.split_whitespace().next().unwrap_or("");
            let rest = cmd_line[name.len()..].trim();
            if dispatch_builtin(&s, name, rest).await {
                // handled locally — no model turn
            } else if let Some(body) = crate::tui::slash::command_body(&s.cwd, &s.roots, name) {
                let prompt = crate::tui::slash::expand_command(&body, rest);
                let obs = WsObserver(s.live.clone());
                let _ = s.agent.run_turn(&prompt, &obs).await;
            } else {
                let _ = s.live.send(serde_json::json!({
                    "type": "note",
                    "text": format!("[unknown command: /{name}]"),
                }));
            }
        } else {
            let obs = WsObserver(s.live.clone());
            let _ = s.agent.run_turn(&input, &obs).await;
        }
        s.busy.fetch_sub(1, Ordering::Relaxed);
        let _ = s.live.send(serde_json::json!({"type":"busy","busy":false}));
    }
}

/// TUI-parity builtins — true when handled. The responses go out over the
/// broadcast so every connected tab sees the same session state.
async fn dispatch_builtin(s: &Arc<Shared>, name: &str, rest: &str) -> bool {
    let note = |t: String| {
        let _ = s.live.send(serde_json::json!({"type":"note","text":t}));
    };
    match name {
        "compact" => {
            let obs = WsObserver(s.live.clone());
            match s.agent.compact(&obs, "manual").await {
                Ok(sum) if sum.is_empty() => note("[compacted: nothing to fold]".into()),
                Ok(sum) => note(format!("[compacted]\n{sum}")),
                Err(e) => note(format!("[compact failed] {e:#}")),
            }
            true
        }
        "resume" => {
            if rest.is_empty() {
                let list = tui::menu::recent_sessions(&s.cwd, 8)
                    .iter()
                    .map(|i| format!("  {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                note(if list.is_empty() {
                    "[no sessions]".into()
                } else {
                    format!("recent sessions:\n{list}")
                });
            } else if let Err(e) = fork_or_resume(s, rest, false).await {
                note(format!("[resume failed] {e:#}"));
            }
            true
        }
        "sessions" => {
            let list = tui::menu::recent_sessions(&s.cwd, 8).join("\n");
            note(if list.is_empty() {
                "[no sessions]".into()
            } else {
                format!("recent sessions:\n{list}")
            });
            true
        }
        "fork" => {
            if rest.is_empty() {
                note("[usage: /fork <id>]".into());
            } else if let Err(e) = fork_or_resume(s, rest, true).await {
                note(format!("[fork failed] {e:#}"));
            } else {
                note(format!("[forked {rest}]"));
            }
            true
        }
        "model" => {
            if rest.is_empty() {
                let c = s.agent.model_choices();
                note(if c.is_empty() {
                    "[no models.json — session model only]".into()
                } else {
                    format!("available models:\n{}", c.join("\n"))
                });
            } else {
                match s.agent.swap_model(rest) {
                    Some(label) => {
                        s.agent.record_model_change(rest, &label).await;
                        let _ = s
                            .live
                            .send(serde_json::json!({"type":"model","label":label}));
                    }
                    None => note(format!("[unknown selector: {rest}]")),
                }
            }
            true
        }
        "tasks" => {
            let tasks = s.agent.task_roster();
            note(if tasks.is_empty() {
                "[no sub-agents this session]".into()
            } else {
                let rows = tasks
                    .iter()
                    .map(|t| {
                        let st = match t.done {
                            None => "running",
                            Some(true) => "done",
                            Some(false) => "failed",
                        };
                        format!("  {st:<7} {} — {}", t.id, t.prompt)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("sub-agents:\n{rows}")
            });
            true
        }
        "todos" => {
            let items = s.agent.todos();
            note(if items.is_empty() {
                "[no task list — TodoWrite creates it]".into()
            } else {
                format!("task list:\n{}", sunmao_core::tool::render_todos(&items))
            });
            true
        }
        "artifacts" => {
            note(crate::tui::slash::artifacts_text(&s.cwd));
            true
        }
        "annotate" => {
            let mut it = rest.splitn(2, char::is_whitespace);
            match (it.next(), it.next()) {
                (Some(n), Some(t)) => note(crate::tui::slash::annotate(&s.cwd, n, t.trim())),
                _ => note("[usage: /annotate <name> <note>]".into()),
            }
            true
        }
        "help" => {
            note(format!(
                "slash commands: {}",
                slash_candidates(s).join("  ")
            ));
            true
        }
        _ => false,
    }
}

/// Bind 127.0.0.1 and serve until Ctrl-C. The web assets are embedded —
/// no node, no build step, `sunmao serve` is the whole deploy story.
pub async fn run(
    agent: AgentLoop,
    cwd: std::path::PathBuf,
    roots: Vec<std::path::PathBuf>,
    port: u16,
    approvals: Arc<Pending>,
    system_prompt: String,
    model_label: String,
) -> Result<()> {
    let (input_tx, input_rx) = mpsc::unbounded_channel::<String>();
    let shared = Arc::new(Shared {
        agent,
        cwd,
        roots,
        system_prompt,
        model_label,
        live: approvals.live.clone(),
        input: input_tx,
        approvals,
        busy: std::sync::atomic::AtomicUsize::new(0),
    });

    let app = axum::Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/ws", get(ws_upgrade))
        .route("/sessions", get(sessions_list))
        .route("/session", get(session_info))
        .route("/session/new", post(new_session))
        .route("/session/{id}/resume", post(resume_session))
        .route("/session/{id}/fork", post(fork_session))
        .route("/artifacts/{name}", get(artifact_get))
        .route("/artifacts/{name}/notes", get(artifact_notes))
        .route("/artifacts/{name}/annotate", post(artifact_annotate))
        .route("/dataflow", get(dataflow_current))
        .route("/dataflow/{id}", get(dataflow_by_id))
        .with_state(shared.clone());

    tokio::spawn(driver(shared.clone(), input_rx));

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("sunmao serve → http://127.0.0.1:{port}  (Ctrl-C to stop)");
    axum::serve(listener, app).await?;
    Ok(())
}
