//! `sunmao serve` 的 WebSocket 通道 — 客户端连接生命周期、live 扇出、
//! 以及入站消息分派（prompt/cancel/approval/session/annotate/model/mode
//! + MCP Apps 岛的 ui_* 桥）。REST 端点留在 `serve.rs`。

use super::*;

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
                "mode": s.agent.approval_mode().as_str(),
                "sandbox_port": s.sandbox_port,
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
            "mode" => {
                let sel = v["sel"].as_str().unwrap_or("");
                match sunmao_core::agent::ApprovalMode::parse(sel) {
                    Some(m) => {
                        s.agent
                            .set_approval_mode(m, &WsObserver(s.live.clone()))
                            .await;
                        let _ = s
                            .live
                            .send(serde_json::json!({"type":"mode","mode":m.as_str()}));
                    }
                    None => {
                        let _ = ws_send(
                            &out_tx,
                            serde_json::json!({"type":"note","text":format!("[unknown mode: {sel}]")}),
                        )
                        .await;
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
                let reply = match s
                    .agent
                    .mcp_app_call(server, tool, v["args"].clone(), &WsObserver(s.live.clone()))
                    .await
                {
                    Ok(result) => {
                        serde_json::json!({"type":"ui_result","id":v["id"],"name":name,"result":result})
                    }
                    Err(e) => {
                        serde_json::json!({"type":"ui_result","id":v["id"],"name":name,"error":e})
                    }
                };
                let _ = ws_send(&out_tx, reply).await;
            }
            "ui_read" => {
                let reply = match s
                    .agent
                    .mcp_resource_read(
                        v["server"].as_str().unwrap_or(""),
                        v["uri"].as_str().unwrap_or(""),
                        &WsObserver(s.live.clone()),
                    )
                    .await
                {
                    Ok(result) => {
                        serde_json::json!({"type":"ui_result","id":v["id"],"name":v["name"],"result":result})
                    }
                    Err(e) => {
                        serde_json::json!({"type":"ui_result","id":v["id"],"name":v["name"],"error":e})
                    }
                };
                let _ = ws_send(&out_tx, reply).await;
            }
            // `ui/message`: the View's prompt becomes a normal user prompt —
            // same input queue, same driver.
            "ui_message" => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                if !text.trim().is_empty() {
                    s.agent
                        .audit_ui_event("mcp.ui_message", &text, &WsObserver(s.live.clone()))
                        .await;
                    let _ = s.input.send(text);
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
                s.agent
                    .audit_ui_event(
                        v["event"].as_str().unwrap_or("mcp.ui"),
                        v["detail"].as_str().unwrap_or(""),
                        &WsObserver(s.live.clone()),
                    )
                    .await;
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
