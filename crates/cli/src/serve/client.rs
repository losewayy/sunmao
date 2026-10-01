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

use sunmao_core::agent::Observer as _;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::host::{
    Host, Input, Shared, WsObserver, display_path, fork_or_resume, new_session, slash_candidates,
};

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
                while let Ok(v) = live_rx.recv().await {
                    if out.send(v.to_string()).is_err() {
                        break;
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
            "slash": slash_candidates(&client.s),
            "models": host.as_ref().map(|h| h.agent.model_choices()).unwrap_or_default(),
            "mode": host.as_ref().map(|h| h.agent.approval_mode().as_str()).unwrap_or("auto"),
            "sandbox_port": client.s.sandbox_port,
            "busy": host.as_ref().map(|h| h.busy.load(Ordering::Relaxed) > 0).unwrap_or(false),
            "busy_sessions": client.s.sessions.lock().unwrap().values()
                .filter(|h| h.busy.load(Ordering::Relaxed) > 0)
                .map(|h| h.id.clone()).collect::<Vec<_>>(),
            "steer": host.as_ref().map(|h| h.agent.steer_queue()).unwrap_or_default(),
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
            // pending approval cards re-render — a tab arriving mid-ask
            // must see the card, not a frozen transcript; queued steers
            // surface as chips the same way
            "pending": host.approvals.cards(),
            "steer": host.agent.steer_queue(),
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
                if !text.trim().is_empty()
                    && let Some(h) = self.viewing_host()
                {
                    // busy + not a slash command → steer the running turn;
                    // slash lines keep FIFO order (a queued `/mode` mustn't
                    // jump ahead of the prompt it's queued behind)
                    if h.busy.load(Ordering::Relaxed) > 0 && !text.trim_start().starts_with('/') {
                        h.agent.push_steer(self.id, text);
                        let _ = self.s.live.send(serde_json::json!({
                            "type":"steer_queue","sess":h.id,
                            "items":h.agent.steer_queue(),
                        }));
                    } else {
                        let _ = h.input.send(Input {
                            client: self.id,
                            text,
                        });
                    }
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
                        });
                        let t0 = std::time::Instant::now();
                        let (ok, output, code) =
                            match sunmao_core::tool::run_foreground(&cmd, cwd, 120).await {
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
                    _ => sunmao_core::approval::Approval::Deny,
                };
                // ids are process-global; the card's session owns the slot
                let target = v["sess"]
                    .as_str()
                    .and_then(|id| self.s.host(id))
                    .or_else(|| self.viewing_host());
                if let Some(h) = target
                    && let Some(card) = h.approvals.map.lock().unwrap().remove(&id)
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
                let r = crate::tui::slash::annotate(&dir, name, note);
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
            // ── MCP Apps bridge (SEP-1865 / GUI.md §5) ──
            // island → host requests, forwarded as `ui_result` replies keyed
            // by the request's `id`. Every call goes through the gate —
            // the UI surface is never a permissions bypass.
            "ui_call" => {
                let name = v["name"].as_str().unwrap_or("");
                let (server, tool) = name
                    .strip_prefix("mcp__")
                    .and_then(|r| r.split_once("__"))
                    .unwrap_or(("", ""));
                let reply = match self.viewing_host() {
                    Some(h) => match h
                        .agent
                        .mcp_app_call(
                            server,
                            tool,
                            v["args"].clone(),
                            &WsObserver::new(self.s.live.clone(), h.id.clone()),
                        )
                        .await
                    {
                        Ok(result) => {
                            serde_json::json!({"type":"ui_result","sess":h.id,"id":v["id"],"name":name,"result":result})
                        }
                        Err(e) => {
                            serde_json::json!({"type":"ui_result","sess":h.id,"id":v["id"],"name":name,"error":e})
                        }
                    },
                    None => {
                        serde_json::json!({"type":"ui_result","id":v["id"],"name":name,"error":"no session"})
                    }
                };
                self.emit(reply);
            }
            "ui_read" => {
                let reply = match self.viewing_host() {
                    Some(h) => match h
                        .agent
                        .mcp_resource_read(
                            v["server"].as_str().unwrap_or(""),
                            v["uri"].as_str().unwrap_or(""),
                            &WsObserver::new(self.s.live.clone(), h.id.clone()),
                        )
                        .await
                    {
                        Ok(result) => {
                            serde_json::json!({"type":"ui_result","sess":h.id,"id":v["id"],"name":v["name"],"result":result})
                        }
                        Err(e) => {
                            serde_json::json!({"type":"ui_result","sess":h.id,"id":v["id"],"name":v["name"],"error":e})
                        }
                    },
                    None => {
                        serde_json::json!({"type":"ui_result","id":v["id"],"name":v["name"],"error":"no session"})
                    }
                };
                self.emit(reply);
            }
            // `ui/message`: the View's prompt becomes a normal user prompt —
            // same input queue of the session the island lives in.
            "ui_message" => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                if !text.trim().is_empty()
                    && let Some(h) = self.viewing_host()
                {
                    h.agent
                        .audit_ui_event(
                            "mcp.ui_message",
                            &text,
                            &WsObserver::new(self.s.live.clone(), h.id.clone()),
                        )
                        .await;
                    let _ = h.input.send(Input {
                        client: self.id,
                        text,
                    });
                }
                self.emit(serde_json::json!({"type":"ui_result","id":v["id"],"result":{}}));
            }
            // island-side events worth a durable fact (open-link, logs,
            // context updates) — visible on the audit spine.
            "ui_audit" => {
                if let Some(h) = self.viewing_host() {
                    h.agent
                        .audit_ui_event(
                            v["event"].as_str().unwrap_or("mcp.ui"),
                            v["detail"].as_str().unwrap_or(""),
                            &WsObserver::new(self.s.live.clone(), h.id.clone()),
                        )
                        .await;
                }
                self.emit(serde_json::json!({"type":"ui_result","id":v["id"],"result":{}}));
            }
            _ => {}
        }
    }
}
