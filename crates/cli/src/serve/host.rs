//! 传输无关的宿主状态 — 会话注册表、live 总线、审批待办、观察者。
//! `HostHandle` 是 serve（axum 适配器）与 Tauri 壳（scheme/Channel
//! 适配器）共享的同一个宿主句柄；HTTP 本身只活在 `serve::http`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use sunmao_core::SessionEvent;
use sunmao_core::SessionLog;
use sunmao_core::agent::{AgentLoop, LiveEvent, Observer};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::client::Client;
use super::driver;

/// One live session the host is running — its own AgentLoop, input queue,
/// approval map and busy counter. Tabs are views onto hosts; nothing in
/// here is shared across sessions.
pub(crate) struct Host {
    pub(crate) id: String,
    pub(crate) agent: AgentLoop,
    /// The FIFO submission queue this session's driver drains.
    pub(crate) input: mpsc::UnboundedSender<String>,
    /// pending approval cards by id (reply oneshots)
    pub(crate) approvals: Arc<Pending>,
    /// submissions currently running (drives the busy badge + cancel)
    pub(crate) busy: std::sync::atomic::AtomicUsize,
}

/// The multi-session host: a registry of live sessions plus the global bus
/// every outbound frame fans out over (each carries its `sess` tag — the
/// frontend keeps the transcript scoped per tab, the rail reads every
/// session's state dots).
pub(crate) struct Shared {
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) roots: Vec<std::path::PathBuf>,
    /// All host-originated traffic — one broadcast, `sess` carries the
    /// routing key. Session-less frames omit it.
    pub(crate) live: broadcast::Sender<serde_json::Value>,
    /// live session hosts by id
    pub(crate) sessions: Mutex<HashMap<String, Arc<Host>>>,
    /// builds a fully-seeded Context for a log (the startup assembly,
    /// reusable per session)
    pub(crate) factory: super::SessionFactory,
    /// assembled system prompt — fresh sessions re-seed it, same as startup
    pub(crate) system_prompt: String,
    /// model label recorded in a fresh session's Started event
    pub(crate) model_label: String,
    /// The MCP Apps sandbox listener's port — the spec's double-iframe
    /// needs a second origin; hello carries it so islands can point at
    /// `http://127.0.0.1:{sandbox_port}/sandbox.html`. Under the Tauri
    /// shell there is no listener — it's 0 and the page substitutes the
    /// `sunmao-sandbox` scheme URL instead.
    pub(crate) sandbox_port: u16,
    /// process-wide approval id space (see Pending)
    pub(crate) approval_ids: Arc<AtomicU64>,
    /// host-management channel — session drivers can't `await adopt`
    /// directly (adopt spawns drivers, so that future would be recursive
    /// and rustc can't close the Send proof); ops ride a oneshot reply
    /// through `mgmt_loop` instead
    pub(crate) mgmt: mpsc::UnboundedSender<SessionOp>,
}

/// Host-management op — today just "make this session id live (possibly
/// as a fork)"; the reply carries the live id.
pub(crate) enum SessionOp {
    Adopt {
        id: String,
        fork: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
}

/// Drains `Shared.mgmt`: forks/resumes issued from inside a session driver
/// land here, one adoption at a time.
pub(crate) async fn mgmt_loop(s: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<SessionOp>) {
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
    pub(crate) fn emit(&self, v: serde_json::Value) {
        let _ = self.live.send(v);
    }

    /// The host for a session id, if it's live.
    pub(crate) fn host(&self, id: &str) -> Option<Arc<Host>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    /// Live session ids (the rail merges these with dormant disk logs).
    pub(crate) fn live_ids(&self) -> Vec<String> {
        self.sessions.lock().unwrap().keys().cloned().collect()
    }

    /// Register `log` as a live session: fresh Pending + Context via the
    /// factory + its own driver task, then SessionStart fires with the
    /// adoption's `source` (same vocabulary as TUI --resume: startup /
    /// resume / fork). Re-adopting a live id is a no-op — the same host
    /// answers every viewer.
    pub(crate) async fn adopt(
        self: &Arc<Self>,
        log: SessionLog,
        source: &str,
    ) -> Result<Arc<Host>> {
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

/// resume/fork = adopt the target (or its copy) as a live host and let the
/// caller's tab switch views — the previous session keeps running in its
/// own host instead of being swapped away mid-tab.
pub(crate) async fn fork_or_resume(
    s: &Arc<Shared>,
    id: &str,
    fork: bool,
) -> Result<serde_json::Value> {
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

/// `POST /session/new` — a fresh log with the same Started+system seeding
/// `main.rs` gives startup sessions, adopted as its own live host.
pub(crate) async fn new_session(s: &Arc<Shared>) -> Result<serde_json::Value> {
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

/// One unanswered approval card — the reply oneshot plus the payload the
/// card was raised with (a tab switching into a session re-renders cards
/// from these, so a pending approval survives the view switch).
pub(crate) struct PendingCard {
    pub(crate) tx: oneshot::Sender<sunmao_core::approval::Approval>,
    tool: String,
    detail: String,
    why: String,
}

/// Approval state for ONE session — `Context.approval` installs the
/// approver holding this handle while the context is being built; the host
/// adopts the same instance. The id space is process-global so a verdict
/// can never hit the wrong session's card.
pub(crate) struct Pending {
    pub(crate) map: Mutex<HashMap<u64, PendingCard>>,
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
    pub(crate) fn cards(&self) -> Vec<serde_json::Value> {
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
pub(crate) struct ServeApprover {
    pub(crate) pending: Arc<Pending>,
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
pub(crate) struct WsObserver {
    live: broadcast::Sender<serde_json::Value>,
    sess: String,
}
impl WsObserver {
    pub(crate) fn new(live: broadcast::Sender<serde_json::Value>, sess: impl Into<String>) -> Self {
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

/// Windows `canonicalize` yields `\?\`-prefixed paths — strip the prefix
/// for display so the GUI's crumb shows `F:\…`, not the UNC form.
pub(crate) fn display_path(p: &std::path::Path) -> String {
    p.display().to_string().replace("\\?\\", "")
}

/// artifact names are `[a-z0-9_-]` — the same whitelist /annotate enforces;
/// anything else can't resolve to a file under .sunmao/artifacts anyway.
pub(crate) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `id` may be a bare session id (resolved under .sunmao/sessions) or a
/// path — paths are allowed only if they stay inside the sessions dir,
/// otherwise a GET could read arbitrary files as JSONL.
pub(crate) fn log_path(s: &Shared, id: &str) -> Option<std::path::PathBuf> {
    let p = std::path::PathBuf::from(id);
    let cand = if p.exists() {
        p
    } else {
        s.cwd.join(".sunmao/sessions").join(format!("{id}.jsonl"))
    };
    cand.exists().then_some(cand)
}

/// Slash-command list for the composer menu — same candidates the TUI
/// shows (builtins + file commands), minus pure-TUI affordances.
pub(crate) fn slash_candidates(s: &Shared) -> Vec<String> {
    crate::tui::slash::candidates(&s.cwd, &s.roots)
        .into_iter()
        .filter(|n| *n != "multiline" && *n != "clear" && *n != "quit")
        .collect()
}

/// The transport-free host handle — the GUI's seam (GUI.md §8). `spawn_host`
/// builds it once; the ws/HTTP surface (`serve::http`) and the Tauri scheme/
/// Channel surface (`crates/gui`) are both thin adapters over it.
#[derive(Clone)]
pub struct HostHandle {
    pub(crate) s: Arc<Shared>,
}

impl HostHandle {
    /// A fresh viewer client: hello + replay already pushed to `out`, live
    /// bus forwarding running — exactly what a new `/ws` connection gets.
    /// `Client::handle` then feeds inbound frames (`prompt`, `view`, …).
    /// Must be called on a tokio runtime — it spawns the bus forwarder.
    pub async fn client(&self, out: mpsc::UnboundedSender<String>) -> Client {
        Client::connect(self.s.clone(), out).await
    }
}
