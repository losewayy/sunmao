//! MCP Apps bridge arms (SEP-1865 / GUI.md §5) — `ui_call`, `ui_read`,
//! `ui_message`, `ui_audit`. Island → host requests forwarded as
//! `ui_result` replies keyed by the request's `id`; every call goes through
//! the gate — the UI surface is never a permissions bypass.

use std::sync::atomic::Ordering;
use sunmao_core::context::MutexRecover;

use super::Client;
use crate::serve::host::{Input, WsObserver, input_queue_frame};

impl Client {
    /// Dispatch one `ui_*` inbound frame. Split from `handle` only for the
    /// file-shape budget — same receiver, same semantics.
    pub(super) async fn handle_ui(&mut self, v: serde_json::Value) {
        match v["type"].as_str().unwrap_or("") {
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
                    // same queue path as the composer prompt — the island's
                    // prompt is a real user turn; the bubble paints when the
                    // driver picks it up, not ahead of a running turn
                    let id = h.queue_next_id.fetch_add(1, Ordering::Relaxed);
                    h.queue.lock_or_recover().push_back(Input {
                        id,
                        client: self.id,
                        text,
                        attachments: Vec::new(),
                    });
                    // same pending bookkeeping as `prompt` — a queued island
                    // message interleaves a goal chain too
                    h.agent
                        .context()
                        .input_pending
                        .fetch_add(1, Ordering::Relaxed);
                    h.queue_notify.notify_one();
                    let _ = self.s.live.send(input_queue_frame(&h));
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
