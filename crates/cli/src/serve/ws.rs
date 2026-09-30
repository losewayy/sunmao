//! `sunmao serve` 的 WebSocket 通道 — 客户端连接生命周期、live 扇出、
//! 以及入站消息分派。
//!
//! 多会话模型（GUI.md §7 的 /session/{id} 语义，单通道实现）：
//! 每个 tab 跟踪自己 viewing 的会话 id；prompt/cancel/mode/model 打在
//! viewing 会话上，`view`/`resume`/`fork`/`new` 只重定向发起 tab。
//! 所有 host 出站帧带 `sess` 标签——transcript 只渲染 viewing 会话的
//! 事件，busy/approval 帧更新侧栏每个会话的状态点。

use super::*;

/// Which session a frame belongs to: `v.sess` (tagged by the host) or the
/// tab's current view for session-less replies.
#[allow(dead_code)]
fn sess_of<'a>(v: &'a serde_json::Value, viewing: &'a str) -> &'a str {
    v["sess"].as_str().unwrap_or(viewing)
}

pub(super) async fn ws_upgrade(
    State(s): State<Arc<Shared>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_client(s, socket))
}

async fn ws_send(tx: &mpsc::UnboundedSender<String>, v: serde_json::Value) -> Result<()> {
    tx.send(v.to_string())
        .map_err(|_| anyhow::anyhow!("ws closed"))
}

/// Reply `replay` with the host's durable events — what a fresh view of a
/// session renders (same fold the TUI gets on --resume).
async fn send_replay(out_tx: &mpsc::UnboundedSender<String>, host: &Host) -> Result<()> {
    let evs = host.agent.session_events().await;
    ws_send(
        out_tx,
        serde_json::json!({
            "type": "replay",
            "session": host.id,
            "events": evs,
            "busy": host.busy.load(Ordering::Relaxed) > 0,
            "mode": host.agent.approval_mode().as_str(),
            // pending approval cards re-render — a tab arriving mid-ask
            // must see the card, not a frozen transcript
            "pending": host.approvals.cards(),
        }),
    )
    .await
}

/// One browser client (one tab): subscribes to the global bus — every
/// frame carries `sess`, this tab only renders the session it's viewing;
/// session-scoped input goes to that session's own driver queue.
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

    // this tab's viewed session — prompts/cancels/mode switches route here;
    // the newest live host is the default view
    let mut viewing = s.live_ids().into_iter().next().unwrap_or_default();

    // hello: host-wide facts + the viewed session's replay — a page reload
    // mid-session lands back on a real transcript.
    {
        let host = s.host(&viewing);
        let evs = match &host {
            Some(h) => h.agent.session_events().await,
            None => Vec::new(),
        };
        let _ = ws_send(
            &out_tx,
            serde_json::json!({
                "type": "hello",
                "session": viewing,
                "cwd": display_path(&s.cwd),
                "slash": slash_candidates(&s),
                "models": host.as_ref().map(|h| h.agent.model_choices()).unwrap_or_default(),
                "mode": host.as_ref().map(|h| h.agent.approval_mode().as_str()).unwrap_or("auto"),
                "sandbox_port": s.sandbox_port,
                "busy": host.as_ref().map(|h| h.busy.load(Ordering::Relaxed) > 0).unwrap_or(false),
                "busy_sessions": s.sessions.lock().unwrap().values()
                    .filter(|h| h.busy.load(Ordering::Relaxed) > 0)
                    .map(|h| h.id.clone()).collect::<Vec<_>>(),
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
        let host = || s.host(&viewing);
        match v["type"].as_str().unwrap_or("") {
            "prompt" => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                if !text.trim().is_empty()
                    && let Some(h) = host()
                {
                    let _ = h.input.send(text);
                }
            }
            "cancel" => {
                if let Some(h) = host() {
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
                let target = v["sess"].as_str().and_then(|id| s.host(id)).or_else(host);
                if let Some(h) = target
                    && let Some(card) = h.approvals.map.lock().unwrap().remove(&id)
                {
                    let _ = s.live.send(serde_json::json!({
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
                if let Some(h) = s.host(id) {
                    viewing = h.id.clone();
                    let _ = send_replay(&out_tx, &h).await;
                }
            }
            "resume" | "fork" => {
                let id = v["id"].as_str().unwrap_or("").to_string();
                let fork = v["type"].as_str() == Some("fork");
                match fork_or_resume(&s, &id, fork).await {
                    Ok(r) => {
                        let new_id = r["session"].as_str().unwrap_or("").to_string();
                        if let Some(h) = s.host(&new_id) {
                            viewing = h.id.clone();
                            let _ = send_replay(&out_tx, &h).await;
                        }
                        // other tabs still viewing the old session stay —
                        // only the requester moves
                    }
                    Err(e) => {
                        let _ = ws_send(
                            &out_tx,
                            serde_json::json!({"type":"note","sess":viewing,"text":format!("[{e:#}]")}),
                        )
                        .await;
                    }
                }
            }
            "new" => match new_session_inner(&s).await {
                Ok(r) => {
                    let new_id = r["session"].as_str().unwrap_or("").to_string();
                    if let Some(h) = s.host(&new_id) {
                        viewing = h.id.clone();
                        let _ = send_replay(&out_tx, &h).await;
                    }
                }
                Err(e) => {
                    let _ = ws_send(
                        &out_tx,
                        serde_json::json!({"type":"note","text":format!("[new session failed] {e:#}")}),
                    )
                    .await;
                }
            },
            "annotate" => {
                let name = v["name"].as_str().unwrap_or("");
                let note = v["note"].as_str().unwrap_or("");
                let r = crate::tui::slash::annotate(&s.cwd, name, note);
                let _ = ws_send(
                    &out_tx,
                    serde_json::json!({"type":"note","sess":viewing,"text":r}),
                )
                .await;
            }
            "model" => {
                let sel = v["sel"].as_str().unwrap_or("");
                if let Some(h) = host() {
                    match h.agent.swap_model(sel) {
                        Some(label) => {
                            h.agent.record_model_change(sel, &label).await;
                            let _ = s.live.send(serde_json::json!({
                                "type":"model","sess":h.id,"label":label,
                            }));
                        }
                        None => {
                            let _ = ws_send(
                                &out_tx,
                                serde_json::json!({"type":"note","sess":h.id,"text":format!("[unknown selector: {sel}]")}),
                            )
                            .await;
                        }
                    }
                }
            }
            "mode" => {
                let sel = v["sel"].as_str().unwrap_or("");
                if let Some(h) = host() {
                    match sunmao_core::agent::ApprovalMode::parse(sel) {
                        Some(m) => {
                            h.agent
                                .set_approval_mode(
                                    m,
                                    &WsObserver::new(s.live.clone(), h.id.clone()),
                                )
                                .await;
                            let _ = s.live.send(serde_json::json!({
                                "type":"mode","sess":h.id,"mode":m.as_str(),
                            }));
                        }
                        None => {
                            let _ = ws_send(
                                &out_tx,
                                serde_json::json!({"type":"note","sess":h.id,"text":format!("[unknown mode: {sel}]")}),
                            )
                            .await;
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
                let reply = match host() {
                    Some(h) => match h
                        .agent
                        .mcp_app_call(
                            server,
                            tool,
                            v["args"].clone(),
                            &WsObserver::new(s.live.clone(), h.id.clone()),
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
                let _ = ws_send(&out_tx, reply).await;
            }
            "ui_read" => {
                let reply = match host() {
                    Some(h) => match h
                        .agent
                        .mcp_resource_read(
                            v["server"].as_str().unwrap_or(""),
                            v["uri"].as_str().unwrap_or(""),
                            &WsObserver::new(s.live.clone(), h.id.clone()),
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
                let _ = ws_send(&out_tx, reply).await;
            }
            // `ui/message`: the View's prompt becomes a normal user prompt —
            // same input queue of the session the island lives in.
            "ui_message" => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                if !text.trim().is_empty()
                    && let Some(h) = host()
                {
                    h.agent
                        .audit_ui_event(
                            "mcp.ui_message",
                            &text,
                            &WsObserver::new(s.live.clone(), h.id.clone()),
                        )
                        .await;
                    let _ = h.input.send(text);
                }
                let _ = ws_send(
                    &out_tx,
                    serde_json::json!({"type":"ui_result","id":v["id"],"result":{}}),
                )
                .await;
            }
            // island-side events worth a durable fact (open-link, logs,
            // context updates) — visible on the audit spine.
            "ui_audit" => {
                if let Some(h) = host() {
                    h.agent
                        .audit_ui_event(
                            v["event"].as_str().unwrap_or("mcp.ui"),
                            v["detail"].as_str().unwrap_or(""),
                            &WsObserver::new(s.live.clone(), h.id.clone()),
                        )
                        .await;
                }
                let _ = ws_send(
                    &out_tx,
                    serde_json::json!({"type":"ui_result","id":v["id"],"result":{}}),
                )
                .await;
            }
            _ => {}
        }
    }
    forward.abort();
    writer.abort();
}
