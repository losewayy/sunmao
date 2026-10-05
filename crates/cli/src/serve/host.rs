//! 传输无关的宿主状态 — 会话注册表、live 总线、审批待办、观察者。
//! `HostHandle` 是 serve（axum 适配器）与 Tauri 壳（scheme/Channel
//! 适配器）共享的同一个宿主句柄；HTTP 本身只活在 `serve::http`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use sunmao_core::context::MutexRecover;

use anyhow::{Context as _, Result};
use sunmao_core::SessionEvent;
use sunmao_core::SessionLog;
use sunmao_core::agent::AgentLoop;
use tokio::sync::{broadcast, mpsc, oneshot};

mod approve;
mod projects;
pub(crate) mod sched;

use super::client::Client;
use super::driver;

/// One queued submission into a session's driver — the text plus which
/// viewer client sent it, so answer-back frames (`session` switches) can
/// target the requester instead of dragging every tab along. `attachments`
/// carries image blocks uploaded ahead of the prompt (`POST /attachments`,
/// then `attachments:[{path,mime}]` on the prompt frame).
pub(crate) struct Input {
    /// per-queue identity — chips in the GUI address a queued input by id,
    /// never by position (reorder/edit all take `id` as the key).
    pub(crate) id: u64,
    pub(crate) client: u64,
    pub(crate) text: String,
    pub(crate) attachments: Vec<sunmao_llm::Content>,
}

/// One live session the host is running — its own AgentLoop, input queue,
/// approval map and busy counter. Tabs are views onto hosts; nothing in
/// here is shared across sessions.
pub(crate) struct Host {
    pub(crate) id: String,
    pub(crate) agent: AgentLoop,
    /// The FIFO submission queue this session's driver drains — a real
    /// `VecDeque` under a mutex so the GUI can list/reorder/edit queued
    /// prompts (mpsc only offers recv-order teardown). `notify` pokes the
    /// driver out of `recv()` when a push lands.
    pub(crate) queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Input>>>,
    /// wakes the driver's dequeue loop — one notify per push, never a
    /// channel. Stale notifies are harmless (the loop re-reads the queue).
    pub(crate) queue_notify: std::sync::Arc<tokio::sync::Notify>,
    /// next queue-entry id (wraps `client` → tickets are unique per queue)
    pub(crate) queue_next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// pending approval cards by id (reply oneshots)
    pub(crate) approvals: Arc<Pending>,
    /// submissions currently running (drives the busy badge + cancel)
    pub(crate) busy: std::sync::atomic::AtomicUsize,
    /// adoption order — reconnecting viewers land on the newest host;
    /// HashMap iteration order can't answer "newest" (a resumed old log
    /// should win over an untouched fresh one it out-sorts lexically)
    pub(crate) adopted: u64,
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
    /// model label recorded in a fresh session's Started event
    pub(crate) model_label: String,
    /// The MCP Apps sandbox listener's port — the spec's double-iframe
    /// needs a second origin; hello carries it so islands can point at
    /// `http://127.0.0.1:{sandbox_port}/sandbox.html`. Under the Tauri
    /// shell there is no listener — it's 0 and the page substitutes the
    /// `sunmao-sandbox` scheme URL instead.
    pub(crate) sandbox_port: u16,
    /// `--system` override, already assembled — `Some` freezes every
    /// session's prompt to it (per-project AGENTS.md stops applying);
    /// `None` re-assembles per session dir so cross-project sessions get
    /// their own project's prompt.
    pub(crate) prompt_override: Option<String>,
    /// `--loop` override — new-session prompts are assembled for this driver.
    pub(crate) driver_override: Option<sunmao_core::agent::LoopDriver>,
    /// Per-session loop picks (`POST /session/new {loop}`) staged for the
    /// adopting Context — keyed by session id, popped once in `adopt`.
    /// `--loop` still outranks them (the gate before stamping).
    pub(crate) pending_drivers: Mutex<HashMap<String, sunmao_core::agent::LoopDriver>>,
    /// process-wide approval id space (see Pending)
    pub(crate) approval_ids: Arc<AtomicU64>,
    /// host-management channel — session drivers can't `await adopt`
    /// directly (adopt spawns drivers, so that future would be recursive
    /// and rustc can't close the Send proof); ops ride a oneshot reply
    /// through `mgmt_loop` instead
    pub(crate) mgmt: mpsc::UnboundedSender<SessionOp>,
    /// Serializes `adopt`'s check-then-insert: `factory.build().await`
    /// opens a window (MCP connects) where two concurrent resumes of one
    /// session id would each build a Context and hold their own writer to
    /// the same jsonl — the second adopter then finds the first's host.
    pub(crate) adopt_lock: tokio::sync::Mutex<()>,
    /// adoption-order counter — each adopted Host stamps its ticket
    pub(crate) adopt_seq: AtomicU64,
}

/// Host-management op — "make this session id live (possibly as a fork)"
/// plus rewind (which is a boundary-trimmed fork + optional file restore);
/// the reply carries a JSON payload `{"session","restored"}`.
pub(crate) enum SessionOp {
    Adopt {
        id: String,
        fork: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
    Rewind {
        id: String,
        /// 1-based turn ordinal — rewind to just before its boundary
        upto_turn: u64,
        mode: RewindMode,
        reply: oneshot::Sender<Result<String, String>>,
    },
}

/// What `/rewind` restores — `Both` forks the session at the boundary and
/// reverts code, `Session` forks only, `Code` reverts files in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RewindMode {
    Both,
    Session,
    Code,
}

/// Drains `Shared.mgmt`: forks/resumes/rewinds issued from inside a
/// session driver land here, one adoption at a time.
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
            SessionOp::Rewind {
                id,
                upto_turn,
                mode,
                reply,
            } => {
                let res = rewind_session(&s, &id, upto_turn, mode)
                    .await
                    .map(|v| v.to_string())
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
        self.sessions.lock_or_recover().get(id).cloned()
    }

    /// Live session ids (the rail merges these with dormant disk logs).
    pub(crate) fn live_ids(&self) -> Vec<String> {
        self.sessions.lock_or_recover().keys().cloned().collect()
    }

    /// The most recently adopted live host's id — the deterministic
    /// "newest" a reconnecting viewer lands on.
    pub(crate) fn newest_live_id(&self) -> Option<String> {
        self.sessions
            .lock_or_recover()
            .values()
            .max_by_key(|h| h.adopted)
            .map(|h| h.id.clone())
    }

    /// Register `log` as a live session: fresh Pending + Context via the
    /// factory + its own driver task, then SessionStart fires with the
    /// adoption's `source` (same vocabulary as TUI --resume: startup /
    /// resume / fork). Re-adopting a live id is a no-op — the same host
    /// answers every viewer. The session's project dir comes from the
    /// log's own path (`<project>/.sunmao/sessions/<id>.jsonl`) so a
    /// session adopted from another project keeps running in it.
    pub(crate) async fn adopt(
        self: &Arc<Self>,
        log: SessionLog,
        source: &str,
    ) -> Result<Arc<Host>> {
        let _guard = self.adopt_lock.lock().await;
        let id = log
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".into());
        if let Some(h) = self.host(&id) {
            return Ok(h);
        }
        let session_cwd = session_project(self, log.path());
        register_project(&self.cwd, &session_cwd);
        let pending = Arc::new(Pending::new(
            self.live.clone(),
            self.approval_ids.clone(),
            id.clone(),
        ));
        let approver = Arc::new(ServeApprover {
            pending: pending.clone(),
        });
        let mut ctx = self.factory.build(log, approver, session_cwd).await?;
        // a per-creation loop pick stamps the fresh Context before any
        // turn reads it — cold-plug stays cold, just not manifest-bound
        if self.driver_override.is_none()
            && let Some(d) = self.pending_drivers.lock_or_recover().remove(&id)
            && let Some(c) = Arc::get_mut(&mut ctx)
        {
            c.loop_driver = d;
        }
        let agent = AgentLoop::new(ctx.clone());
        agent.set_live_sink(Arc::new(WsObserver::new(self.live.clone(), id.clone())));
        let host = Arc::new(Host {
            id: id.clone(),
            agent,
            queue: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            queue_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            queue_next_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            approvals: pending,
            busy: std::sync::atomic::AtomicUsize::new(0),
            adopted: self.adopt_seq.fetch_add(1, Ordering::Relaxed) + 1,
        });
        self.sessions
            .lock_or_recover()
            .insert(id.clone(), host.clone());
        tokio::spawn(driver::driver(self.clone(), host.clone()));
        // SessionStart fires on every adopt — same lifecycle a fresh
        // launch or TUI --resume produces; capture hooks see which path
        ctx.fire_session_start(source).await;
        self.emit(serde_json::json!({"type":"sessions_changed"}));
        Ok(host)
    }
}

/// Queued inputs as the GUI renders them — `{id,text}` rows, stable order.
/// `client`/`attachments` stay server-side; chips only need identity +
/// editable text.
pub(crate) fn queue_items(host: &Host) -> Vec<serde_json::Value> {
    host.queue
        .lock_or_recover()
        .iter()
        .map(|i| serde_json::json!({"id": i.id, "text": i.text}))
        .collect()
}

/// `input_queue` live frame — broadcast after every mutation (push, pop,
/// move, edit, remove) so every tab's chip row reflects kernel truth.
pub(crate) fn input_queue_frame(host: &Host) -> serde_json::Value {
    serde_json::json!({
        "type": "input_queue",
        "sess": host.id,
        "items": queue_items(host),
    })
}

/// `effort` live frame — the level in force plus the vocabulary the active
/// model advertises. Broadcast on `/effort`, after a `/model` swap, and
/// when the catalog itself changes: reading the ladder is also what fixes a
/// session's default level, so the frame carries whatever the edit implies.
pub(crate) async fn effort_frame(host: &Host) -> serde_json::Value {
    host.agent.resolve_effort_default().await;
    serde_json::json!({
        "type": "effort",
        "sess": host.id,
        "level": host.agent.reasoning_effort(),
        "levels": host.agent.effort_levels().await,
    })
}

/// Apply one queued-input control frame (`input_remove` / `input_move` /
/// `input_edit`) to the FIFO — the mutation lives here next to the queue;
/// the caller broadcasts `input_queue_frame` afterwards. Unknown ids and
/// out-of-range moves clamp harmlessly instead of erroring: a chip click
/// racing a pop is normal traffic, not a protocol fault.
pub(crate) fn queue_op(host: &Host, v: &serde_json::Value) {
    let Some(id) = v["id"].as_u64() else { return };
    // every removal drops input_pending the same way a driver pop does —
    // the counter tracks *queued* submissions regardless of how they leave
    let dropped = || {
        host.agent
            .context()
            .input_pending
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    };
    let mut q = host.queue.lock_or_recover();
    match v["type"].as_str() {
        Some("input_remove") => {
            if let Some(p) = q.iter().position(|i| i.id == id) {
                q.remove(p);
                dropped();
            }
        }
        Some("input_move") => {
            if let Some(p) = q.iter().position(|i| i.id == id) {
                let dir = v["dir"].as_i64().unwrap_or(0);
                let to = (p as i64 + dir).clamp(0, (q.len() as i64).saturating_sub(1)) as usize;
                if to != p {
                    let item = q.remove(p).unwrap();
                    q.insert(to, item);
                }
            }
        }
        Some("input_edit") => {
            if let Some(text) = v["text"].as_str()
                && let Some(i) = q.iter_mut().find(|i| i.id == id)
            {
                i.text = text.to_string();
            }
        }
        _ => {}
    }
}

/// resume/fork = adopt the target (or its copy) as a live host and let the
/// caller's tab switch views — the previous session keeps running in its
/// own host instead of being swapped away mid-tab. A fork stays in the
/// source session's project (the copy lands next to the source log).
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
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dst = src.parent().unwrap_or(&s.cwd).join(format!(
            "s-{ms}-{:x}-fork.jsonl",
            (std::process::id() as u64) << 20 | (ns as u64 >> 12)
        ));
        std::fs::copy(&src, &dst).with_context(|| format!("copy {}", src.display()))?;
        dst
    } else {
        src
    };
    let log = sunmao_core::SessionLog::open_path(&path).await?;
    let host = s.adopt(log, if fork { "fork" } else { "resume" }).await?;
    Ok(serde_json::json!({"session": host.id}))
}

/// rewind = the log's byte prefix up to the boundary line becomes a fork
/// (`session`/`both` modes), plus a checkpoint-ledger restore of files
/// touched at or after the boundary turn (`code`/`both`). Restore runs
/// first — a fork that can't write back files shouldn't orphan a new
/// session. The fork's ledger inherits the source's snapshots truncated to
/// turns before the boundary, so rewinds inside the fork stay honest.
/// Replies `{"session": <new id|null>, "restored": [rel paths]}` — the
/// same shape the REST route hands back.
pub(crate) async fn rewind_session(
    s: &Arc<Shared>,
    id: &str,
    upto_turn: u64,
    mode: RewindMode,
) -> Result<serde_json::Value> {
    let src = log_path(s, id).context("no such session")?;
    let project = session_project(s, &src);
    // file restore has no fence against a running turn — a Write that
    // lands mid-restore would get clobbered by the checkpoint snapshot.
    // Refuse rather than queue: the caller can wait or cancel first.
    // (Session-mode rewinds only copy log bytes — safe mid-turn.)
    if mode != RewindMode::Session
        && s.host(id)
            .is_some_and(|h| h.busy.load(std::sync::atomic::Ordering::Relaxed) > 0)
    {
        anyhow::bail!("session is mid-turn — wait or cancel first");
    }
    let bounds = sunmao_core::checkpoints::turn_boundaries(&src);
    let boundary = bounds
        .iter()
        .find(|b| b.n == upto_turn)
        .with_context(|| format!("no turn {upto_turn} — {} boundaries", bounds.len()))?;

    let mut restored: Vec<String> = Vec::new();
    if mode != RewindMode::Session {
        restored = sunmao_core::checkpoints::restore_files(&project, id, upto_turn)?;
    }

    let mut session = serde_json::Value::Null;
    if mode != RewindMode::Code {
        // byte-prefix copy — raw lines, not a parsed-events rewrite, so
        // unknown/corrupt event lines survive the fork verbatim
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let new_id = format!(
            "s-{ms}-{:x}-fork",
            (std::process::id() as u64) << 20 | (ns as u64 >> 12)
        );
        let dst = src
            .parent()
            .unwrap_or(&s.cwd)
            .join(format!("{new_id}.jsonl"));
        sunmao_core::checkpoints::copy_log_prefix(&src, &dst, boundary.line)?;
        sunmao_core::checkpoints::fork_checkpoints(&project, id, &new_id, upto_turn)?;
        let mut log = sunmao_core::SessionLog::open_path(&dst).await?;
        // the fork's prefix ends mid-story — stamp its provenance so a
        // replay knows this is a rewind's continuation, not a session that
        // happened to start at turn n
        sunmao_core::checkpoints::stamp_rewind_provenance(
            &mut log,
            id,
            upto_turn,
            match mode {
                RewindMode::Both => "both",
                RewindMode::Session => "session",
                RewindMode::Code => "code",
            },
        )
        .await;
        let host = s.adopt(log, "rewind").await?;
        session = serde_json::Value::String(host.id.clone());
    }
    Ok(serde_json::json!({"session": session, "restored": restored}))
}

/// `POST /session/new {"cwd"?}` — a fresh log under the chosen project's
/// `.sunmao/sessions`, seeded like startup; `cwd` defaults to the launch
/// dir. The session's prompt is assembled from *that* dir unless a
/// `--system` override froze it.
pub(crate) async fn new_session(
    s: &Arc<Shared>,
    cwd: Option<std::path::PathBuf>,
    loop_drv: Option<sunmao_core::agent::LoopDriver>,
) -> Result<serde_json::Value> {
    let cwd = match cwd {
        Some(p) => {
            let p = if p.is_absolute() { p } else { s.cwd.join(p) };
            if !p.is_dir() {
                anyhow::bail!("not a directory: {}", p.display());
            }
            p.canonicalize().unwrap_or(p)
        }
        None => s.cwd.clone(),
    };
    // millis suffix: /new within the same second must not collide
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let id = format!("{}-{}", crate::session_id(), ms % 1000);
    let dir = cwd.join(".sunmao/sessions");
    // precedence: --loop flag > explicit per-creation pick > manifest scan —
    // the GUI's 执行模式 picker is the middle tier
    let driver = s
        .driver_override
        .or(loop_drv)
        .unwrap_or_else(|| sunmao_core::agent::LoopDriver::resolve(&cwd, &s.roots));
    let prompt = match &s.prompt_override {
        Some(p) => p.clone(),
        None => sunmao_core::prompt::PromptAssembler::new(&cwd)
            .with_extra_roots(&s.roots)
            .with_driver(driver)
            .assemble(None),
    };
    if let Some(d) = loop_drv {
        s.pending_drivers.lock_or_recover().insert(id.clone(), d);
    }
    let mut log = sunmao_core::SessionLog::open(&dir, &id).await?;
    log.append(&SessionEvent::Started {
        model: s.model_label.clone(),
        cwd: display_path(&cwd),
        driver: Some(driver.as_str().into()),
    })
    .await?;
    log.append(&SessionEvent::Message {
        message: sunmao_llm::types::Message::system(prompt),
    })
    .await?;
    let host = s.adopt(log, "startup").await?;
    Ok(serde_json::json!({"session": host.id}))
}

pub(crate) use approve::{Pending, ServeApprover, WsObserver};
pub(crate) use projects::{projects, register_project, session_dirs, session_project};

/// Windows `canonicalize` yields `\\?\`-prefixed verbatim paths — strip the
/// prefix for display so the GUI shows `F:\…` (and `\\server\…` for the
/// `\\?\UNC\` form), not the verbatim spelling.
pub(crate) fn display_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    }
}

/// artifact names are `[a-z0-9_-]` — the same whitelist /annotate enforces;
/// anything else can't resolve to a file under .sunmao/artifacts anyway.
pub(crate) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `id` may be a bare session id (resolved under the launch dir's
/// .sunmao/sessions, then every registered project's) or a path — paths
/// are allowed only if they canonicalize inside a known sessions dir,
/// otherwise a GET/POST could read (or append to) arbitrary local files
/// through the loopback surface.
pub(crate) fn log_path(s: &Shared, id: &str) -> Option<std::path::PathBuf> {
    let dirs = session_dirs(s);
    let p = std::path::PathBuf::from(id);
    if p.exists() {
        // path-shaped id: must live inside a sessions dir — `..` and
        // bare `C:\…` escapes are rejected by the prefix check.
        let canon = p.canonicalize().ok()?;
        for dir in &dirs {
            if let Ok(d) = dir.canonicalize()
                && canon.starts_with(&d)
            {
                return Some(canon);
            }
        }
        return None;
    }
    // bare id: joined under a sessions dir. Reject anything that could
    // escape it — separators, `..`, rooted/prefixed components — the join
    // itself is not a sandbox.
    let safe = !id.is_empty()
        && std::path::Path::new(id)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !safe {
        return None;
    }
    for dir in dirs {
        let cand = dir.join(format!("{id}.jsonl"));
        if cand.exists() {
            return Some(cand);
        }
    }
    None
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

#[cfg(test)]
mod tests;
