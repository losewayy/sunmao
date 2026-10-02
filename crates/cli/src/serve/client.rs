//! 一个查看者客户端的状态机 — `viewing` 指针 + 出站槽。传输无关：
//! `serve::ws` 把帧接到 WebSocket，`crates/gui` 把帧接到 Tauri
//! `Channel`/`invoke`；入站分派（`Client::handle`）与 hello+replay
//! 握手两条路共享同一份代码。
//!
//! 多会话模型（GUI.md §7 的 /session/{id} 语义，单通道实现）：
//! 每个 tab 跟踪自己 viewing 的会话 id；prompt/cancel/mode/model 打在
//! viewing 会话上，`view`/`resume`/`fork`/`new` 只重定向发起 tab。
//! 所有 host 出站帧带 `sess` 标签——transcript 只渲染 viewing 会话的
//! 事件，busy/approval 帧更新侧栏每个会话的状态点。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use sunmao_core::context::MutexRecover;

use sunmao_core::agent::Observer as _;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::driver::slash_candidates;
use super::host::{
    Host, Input, Shared, WsObserver, display_path, fork_or_resume, input_queue_frame, new_session,
    queue_items,
};

mod ui;

/// Process-global client ids — a `session` switch frame names its issuer
/// so only that tab follows (`0` = the caller opted out of the tag).
static CLIENT_IDS: AtomicU64 = AtomicU64::new(0);

/// One viewer client (one tab / one IPC peer): subscribes to the global
/// bus — every frame carries `sess`, this tab only renders the session
/// it's viewing; session-scoped input goes to that session's own driver
/// queue.
pub struct Client {
    s: Arc<Shared>,
    /// outbound frames as serialized JSON — transports serialize once and
    /// hand the text downstream (ws text frame / Channel payload string)
    out: mpsc::UnboundedSender<String>,
    /// this tab's viewed session — prompts/cancels/mode switches route
    /// here; the newest live host is the default view
    viewing: String,
    /// stable id for this viewer — echoes into `client`-tagged frames
    id: u64,
    /// live-bus forwarder — aborted when the client drops
    forward: JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.forward.abort();
    }
}

impl Client {
    /// Attach a viewer: subscribe the live bus, then push hello +
    /// the viewed session's replay — a page reload mid-session lands back
    /// on a real transcript.
    pub(super) async fn connect(s: Arc<Shared>, out: mpsc::UnboundedSender<String>) -> Self {
        let mut live_rx = s.live.subscribe();
        let forward = {
            let out = out.clone();
            tokio::spawn(async move {
                loop {
                    match live_rx.recv().await {
                        // a slow tab skips the missed frames instead of
                        // dying — Lagged is recoverable; dropping it used
                        // to leave the client permanently deaf
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                        Ok(v) => {
                            if out.send(v.to_string()).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
        };

        let viewing = s.live_ids().into_iter().next().unwrap_or_default();
        let host = s.host(&viewing);
        let evs = match &host {
            Some(h) => h.agent.session_events().await,
            None => Vec::new(),
        };
        let client = Self {
            s,
            out,
            viewing,
            id: CLIENT_IDS.fetch_add(1, Ordering::Relaxed) + 1,
            forward,
        };
        client.emit(serde_json::json!({
            "type": "hello",
            "client": client.id,
            "session": client.viewing,
            // cwd is the *viewed session's* project — a session adopted
            // from elsewhere reports its own root
            "cwd": host.as_ref()
                .map(|h| display_path(&h.agent.session_cwd()))
                .unwrap_or_else(|| display_path(&client.s.cwd)),
            "slash": slash_candidates(&client.s).iter()
                .map(|(n, d)| serde_json::json!({"name": n, "desc": d}))
                .collect::<Vec<_>>(),
            "models": host.as_ref().map(|h| h.agent.model_choices()).unwrap_or_default(),
            "mode": host.as_ref().map(|h| h.agent.approval_mode().as_str()).unwrap_or("auto"),
            // the effort override + the model's level vocabulary — seeds the
            // composer chip and its picker before any replay lands
            "effort": match &host { Some(h) => h.agent.reasoning_effort(), None => None },
            "effort_levels": match &host { Some(h) => h.agent.effort_levels().await, None => Vec::new() },
            "sandbox_port": client.s.sandbox_port,
            "busy": host.as_ref().map(|h| h.busy.load(Ordering::Relaxed) > 0).unwrap_or(false),
            "busy_sessions": client.s.sessions.lock_or_recover().values()
                .filter(|h| h.busy.load(Ordering::Relaxed) > 0)
                .map(|h| h.id.clone()).collect::<Vec<_>>(),
            "steer": host.as_ref().map(|h| h.agent.steer_queue()).unwrap_or_default(),
            // the standing goal — the composer chip renders it live; a
            // reconnect must seed the same state a replay would fold
            "goal": host.as_ref().and_then(|h| h.agent.goal()),
            // queued inputs render as chips on every tab — a rejoining one
            // must see them too, not just the tab that submitted
            "queue": host.as_ref().map(|h| queue_items(h)).unwrap_or_default(),
            // a reconnect mid-ask must re-render the pending card — the
            // kernel is still blocked on it; without it the transcript
            // replays clean and the approval is unanswerable. Same for
            // every session that has a card outstanding (rail badges).
            "pending": host.as_ref().map(|h| h.approvals.cards()).unwrap_or_default(),
            "waiting_sessions": client.s.sessions.lock_or_recover().values()
                .filter(|h| !h.approvals.cards().is_empty())
                .map(|h| h.id.clone()).collect::<Vec<_>>(),
            "replay": evs,
        }));
        client
    }

    fn emit(&self, v: serde_json::Value) {
        // a dead sink means the transport is already gone — frames drop
        let _ = self.out.send(v.to_string());
    }

    /// Reply `replay` with the host's durable events — what a fresh view
    /// of a session renders (same fold the TUI gets on --resume).
    async fn send_replay(&self, host: &Host) {
        let evs = host.agent.session_events().await;
        self.emit(serde_json::json!({
            "type": "replay",
            "session": host.id,
            "events": evs,
            "cwd": display_path(&host.agent.session_cwd()),
            "busy": host.busy.load(Ordering::Relaxed) > 0,
            "mode": host.agent.approval_mode().as_str(),
            "effort": host.agent.reasoning_effort(),
            "effort_levels": host.agent.effort_levels().await,
            // pending approval cards re-render — a tab arriving mid-ask
            // must see the card, not a frozen transcript; queued steers
            // surface as chips the same way
            "pending": host.approvals.cards(),
            "steer": host.agent.steer_queue(),
            "queue": queue_items(host),
            // same goal seeding hello carries — a `view` switch replays it
            "goal": host.agent.goal(),
        }));
    }

    fn viewing_host(&self) -> Option<Arc<Host>> {
        self.s.host(&self.viewing)
    }

    /// One inbound frame (already-parsed JSON — transports own decoding).
    pub async fn handle(&mut self, v: serde_json::Value) {
        match v["type"].as_str().unwrap_or("") {
            "prompt" => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                // attachments arrive as {path,mime} — the path must resolve
                // inside the viewed session's attachments dir (a browser-
                // supplied absolute path elsewhere is dropped, not trusted)
                let mut attachments: Vec<sunmao_llm::Content> = Vec::new();
                if let Some(h) = self.viewing_host() {
                    let dir = crate::attachments::dir(&h.agent.session_cwd());
                    // canonicalize both sides — Windows verbatim prefixes
                    // (`\\?\`) would otherwise make starts_with always fail
                    let dir = dir.canonicalize().unwrap_or(dir);
                    for a in v["attachments"].as_array().into_iter().flatten() {
                        let Some(path) = a["path"].as_str() else {
                            continue;
                        };
                        let ok = std::path::Path::new(path)
                            .canonicalize()
                            .map(|c| c.starts_with(&dir))
                            .unwrap_or(false);
                        if ok && let Some(mime) = a["mime"].as_str() {
                            attachments.push(sunmao_llm::Content::Image {
                                path: path.to_string(),
                                mime: mime.to_string(),
                            });
                        }
                    }
                }
                // attachments-only prompts count — a pasted screenshot with
                // no caption is still a turn, not a no-op.
                if !(text.trim().is_empty() && attachments.is_empty())
                    && let Some(h) = self.viewing_host()
                {
                    // Enter always enqueues — busy no longer routes to steer
                    // (that's Ctrl+Enter's explicit wire type now). Idle =
                    // empty queue: notify wakes the driver and it pops this
                    // same turn. The user-bubble echo moved to dispatch_input
                    // — a queued prompt must not paint ahead of the turn
                    // that's still running.
                    {
                        let id = h
                            .queue_next_id
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        h.queue.lock_or_recover().push_back(Input {
                            id,
                            client: self.id,
                            text,
                            attachments,
                        });
                        // queued = pending input the goal chain yields to —
                        // the driver decrements when it claims the slot
                        h.agent
                            .context()
                            .input_pending
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        h.queue_notify.notify_one();
                        let _ = self.s.live.send(input_queue_frame(&h));
                    }
                }
            }
            // explicit steering — Ctrl+Enter sends this instead of `prompt`.
            // The turn loop folds it in at the next request boundary; if the
            // turn already ended, the driver drains it as the next input —
            // ahead of the FIFO, which is exactly the "加急" semantic:
            // a queued prompt never out-ranks a live steer.
            "steer" => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                if !text.trim().is_empty()
                    && let Some(h) = self.viewing_host()
                {
                    h.agent.push_steer(self.id, text);
                    let _ = self.s.live.send(serde_json::json!({
                        "type":"steer_queue","sess":h.id,
                        "items":h.agent.steer_queue(),
                    }));
                }
            }
            // input-queue controls — the GUI chips ride these. Mutation
            // discipline lives in host::queue_op next to the queue itself;
            // every op broadcasts the refreshed list so tabs stay in sync.
            "input_remove" | "input_move" | "input_edit" => {
                if let Some(h) = self.viewing_host() {
                    super::host::queue_op(&h, &v);
                    let _ = self.s.live.send(input_queue_frame(&h));
                }
            }
            "steer_cancel" => {
                if let Some(h) = self.viewing_host() {
                    h.agent
                        .cancel_steer(v["idx"].as_u64().unwrap_or(0) as usize);
                    let _ = self.s.live.send(serde_json::json!({
                        "type":"steer_queue","sess":h.id,
                        "items":h.agent.steer_queue(),
                    }));
                }
            }
            // {type:"task_steer", sess, id, text} — steer a running sub-agent
            // of that session; the roster decides (finished → use resume).
            "task_steer" => {
                let sub_id = v["id"].as_str().unwrap_or("").to_string();
                let text = v["text"].as_str().unwrap_or("").to_string();
                let target = v["sess"]
                    .as_str()
                    .and_then(|id| self.s.host(id))
                    .or_else(|| self.viewing_host());
                let sess = v["sess"].as_str().unwrap_or(&self.viewing).to_string();
                let reply = match target {
                    Some(h) if !sub_id.is_empty() && !text.is_empty() => {
                        match h.agent.steer_sub(&sub_id, text).await {
                            Ok(()) => format!("steered {sub_id}"),
                            Err(e) => e.to_string(),
                        }
                    }
                    Some(_) => "task_steer needs id + text".into(),
                    None => format!("no session {sess}"),
                };
                self.emit(serde_json::json!({
                    "type":"note","sess":sess,"text":reply,
                }));
            }
            // {type:"task_cancel", sess, id} — kill ONE running sub-agent
            // (the roster popover's stop control); `agent.cancel()` nukes
            // the whole turn, this is the surgical version.
            "task_cancel" => {
                let sub_id = v["id"].as_str().unwrap_or("").to_string();
                let target = v["sess"]
                    .as_str()
                    .and_then(|id| self.s.host(id))
                    .or_else(|| self.viewing_host());
                let sess = v["sess"].as_str().unwrap_or(&self.viewing).to_string();
                let reply = match target {
                    Some(h) if !sub_id.is_empty() => match h.agent.cancel_sub(&sub_id).await {
                        Ok(()) => format!("cancelled {sub_id}"),
                        Err(e) => e.to_string(),
                    },
                    Some(_) => "task_cancel needs id".into(),
                    None => format!("no session {sess}"),
                };
                self.emit(serde_json::json!({
                    "type":"note","sess":sess,"text":reply,
                }));
            }
            // `!` local shell — the user runs it, no approval gate, no LLM.
            // Same deno_task_shell path the TUI's `!` takes; the durable
            // LocalShell fact folds into the next turn's context.
            "local_shell" => {
                let cmd = v["cmd"].as_str().unwrap_or("").to_string();
                if !cmd.trim().is_empty()
                    && let Some(h) = self.viewing_host()
                {
                    let obs = WsObserver::new(self.s.live.clone(), h.id.clone());
                    let cwd = h.agent.session_cwd();
                    tokio::spawn(async move {
                        use sunmao_core::agent::LiveEvent;
                        obs.on_event(&LiveEvent::ToolStart {
                            name: "!".into(),
                            summary: format!("$ {cmd}"),
                            depth: 0,
                            lane: 0,
                            call_id: None,
                            args: serde_json::Value::Null,
                        });
                        let t0 = std::time::Instant::now();
                        let ctx = h.agent.context().clone();
                        let (ok, output, code) = match sunmao_core::tool::run_foreground(
                            &cmd,
                            cwd,
                            120,
                            ctx.shell,
                            Some(ctx.cancel_notify.clone()),
                        )
                        .await
                        {
                            Ok(run) => (
                                run.exit_code == 0,
                                sunmao_core::tool::render_run(&run),
                                run.exit_code,
                            ),
                            Err(msg) => (false, msg, -1),
                        };
                        h.agent.record_local_shell(&cmd, code, &output).await;
                        obs.on_event(&LiveEvent::ToolDone {
                            name: "!".into(),
                            ok,
                            output,
                            depth: 0,
                            lane: 0,
                            call_id: None,
                            elapsed_ms: t0.elapsed().as_millis() as u64,
                        });
                    });
                }
            }
            "cancel" => {
                if let Some(h) = self.viewing_host() {
                    h.agent.cancel();
                }
            }
            "approval" => {
                let id = v["id"].as_u64().unwrap_or(0);
                let verdict = match v["verdict"].as_str().unwrap_or("deny") {
                    "once" => sunmao_core::approval::Approval::Once,
                    "session" => sunmao_core::approval::Approval::Session,
                    _ => sunmao_core::approval::Approval::Deny { reason: None },
                };
                // ids are process-global; the card's session owns the slot
                let target = v["sess"]
                    .as_str()
                    .and_then(|id| self.s.host(id))
                    .or_else(|| self.viewing_host());
                if let Some(h) = target
                    && let Some(card) = h.approvals.map.lock_or_recover().remove(&id)
                {
                    let _ = self.s.live.send(serde_json::json!({
                        "type": "approval_done", "sess": h.id, "id": id,
                    }));
                    let _ = card.tx.send(verdict);
                }
            }
            // ── view switching — the tab, not the session ──
            // `view` just re-points this tab (the target must already be a
            // live host); `resume`/`fork`/`new` adopt the log first, then
            // re-point. A dormantly-listed session becomes live on view.
            "view" => {
                let id = v["id"].as_str().unwrap_or("");
                if let Some(h) = self.s.host(id) {
                    self.viewing = h.id.clone();
                    self.send_replay(&h).await;
                }
            }
            "resume" | "fork" => {
                let id = v["id"].as_str().unwrap_or("").to_string();
                let fork = v["type"].as_str() == Some("fork");
                match fork_or_resume(&self.s, &id, fork).await {
                    Ok(r) => {
                        let new_id = r["session"].as_str().unwrap_or("").to_string();
                        if let Some(h) = self.s.host(&new_id) {
                            self.viewing = h.id.clone();
                            self.send_replay(&h).await;
                        }
                        // other tabs still viewing the old session stay —
                        // only the requester moves
                    }
                    Err(e) => {
                        self.emit(serde_json::json!({
                            "type":"note","sess":self.viewing,
                            "text":format!("[{e:#}]"),
                        }));
                    }
                }
            }
            "new" => {
                let cwd = v["cwd"].as_str().map(std::path::PathBuf::from);
                match new_session(&self.s, cwd).await {
                    Ok(r) => {
                        let new_id = r["session"].as_str().unwrap_or("").to_string();
                        if let Some(h) = self.s.host(&new_id) {
                            self.viewing = h.id.clone();
                            self.send_replay(&h).await;
                        }
                    }
                    Err(e) => {
                        self.emit(serde_json::json!({
                            "type":"note","text":format!("[new session failed] {e:#}"),
                        }));
                    }
                }
            }
            "annotate" => {
                let name = v["name"].as_str().unwrap_or("");
                let note = v["note"].as_str().unwrap_or("");
                // annotations write into the *viewed session's* project —
                // same dir its artifacts resolve under
                let dir = self
                    .viewing_host()
                    .map(|h| h.agent.session_cwd())
                    .unwrap_or_else(|| self.s.cwd.clone());
                let r = crate::commands::annotate(&dir, name, note);
                self.emit(serde_json::json!({"type":"note","sess":self.viewing,"text":r}));
            }
            "model" => {
                let sel = v["sel"].as_str().unwrap_or("");
                if let Some(h) = self.viewing_host() {
                    match h.agent.swap_model(sel) {
                        Some(label) => {
                            h.agent.record_model_change(sel, &label).await;
                            let _ = self.s.live.send(serde_json::json!({
                                "type":"model","sess":h.id,"label":label,
                            }));
                            let _ = self.s.live.send(super::host::effort_frame(&h).await);
                        }
                        None => {
                            self.emit(serde_json::json!({
                                "type":"note","sess":h.id,
                                "text":format!("[unknown selector: {sel}]"),
                            }));
                        }
                    }
                }
            }
            // {type:"effort", level} — the composer chip's picker; "default"
            // (or an empty level) clears back to the provider's own.
            "effort" => {
                let level = v["level"].as_str().unwrap_or("default");
                if let Some(h) = self.viewing_host() {
                    h.agent
                        .set_reasoning_effort(
                            Some(level),
                            &WsObserver::new(self.s.live.clone(), h.id.clone()),
                        )
                        .await;
                    let _ = self.s.live.send(super::host::effort_frame(&h).await);
                }
            }
            "mode" => {
                let sel = v["sel"].as_str().unwrap_or("");
                if let Some(h) = self.viewing_host() {
                    match sunmao_core::agent::ApprovalMode::parse(sel) {
                        Some(m) => {
                            h.agent
                                .set_approval_mode(
                                    m,
                                    &WsObserver::new(self.s.live.clone(), h.id.clone()),
                                )
                                .await;
                            let _ = self.s.live.send(serde_json::json!({
                                "type":"mode","sess":h.id,"mode":m.as_str(),
                            }));
                        }
                        None => {
                            self.emit(serde_json::json!({
                                "type":"note","sess":h.id,
                                "text":format!("[unknown mode: {sel}]"),
                            }));
                        }
                    }
                }
            }
            // ── MCP Apps bridge (SEP-1865 / GUI.md §5) — `client/ui.rs` ──
            "ui_call" | "ui_read" | "ui_message" | "ui_audit" => self.handle_ui(v).await,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slow tab that lets the live bus lag used to die at the first
    /// `Lagged` — every later frame (busy, approvals, live rows) never
    /// reached it until reconnect. `Lagged` is recoverable: skip the gap.
    #[tokio::test]
    async fn lagged_bus_survives() {
        let (live, _) = tokio::sync::broadcast::channel::<serde_json::Value>(4);
        let (mgmt, _rx) = mpsc::unbounded_channel();
        let s = Arc::new(Shared {
            cwd: std::env::temp_dir(),
            roots: Vec::new(),
            live,
            sessions: std::sync::Mutex::new(Default::default()),
            factory: crate::serve::SessionFactory {
                make: Box::new(|_, _, _| Box::pin(async { anyhow::bail!("test factory") })),
            },
            model_label: String::new(),
            sandbox_port: 0,
            prompt_override: None,
            driver_override: None,
            approval_ids: Arc::new(AtomicU64::new(0)),
            mgmt,
            adopt_lock: tokio::sync::Mutex::new(()),
        });
        let (out, mut rx) = mpsc::unbounded_channel::<String>();
        let _client = Client::connect(s.clone(), out).await;
        // the forwarder hasn't polled yet (this test runtime yields only
        // when we await) — the burst wraps the ring → Lagged on next recv
        for _ in 0..8 {
            let _ = s.live.send(serde_json::json!({"type":"noise"}));
        }
        // drain the hello + lagged gap; the marker must still arrive
        let _ = s.live.send(serde_json::json!({"type":"marker"}));
        let mut seen = false;
        for _ in 0..16 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Some(v)) if v.contains("marker") => {
                    seen = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(seen, "the forwarder must survive a Lagged gap");
    }
}
