//! MCP Apps bridge arms (SEP-1865 / GUI.md §5) — `ui_call`, `ui_read`,
//! `ui_message`, `ui_audit`. Island → host requests forwarded as
//! `ui_result` replies keyed by the request's `id`; every call goes through
//! the gate — the UI surface is never a permissions bypass.

use std::sync::atomic::Ordering;
use sunmao_core::context::MutexRecover;

use super::Client;
use crate::serve::host::{Input, WsObserver, input_queue_frame};

/// A scalar control value is data, never executable code. Do not persist
/// unchecked arbitrary JSON (or large strings) into the session log.
fn valid_ui_key(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.as_bytes()[0].is_ascii_alphabetic()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn valid_ui_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => true,
        serde_json::Value::String(s) => s.len() <= 256,
        _ => false,
    }
}

impl Client {
    /// Dispatch one `ui_*` inbound frame. Split from `handle` only for the
    /// file-shape budget — same receiver, same semantics.
    pub(super) async fn handle_ui(&mut self, v: serde_json::Value) {
        match v["type"].as_str().unwrap_or("") {
            "ui_state" => {
                let surface = v["surface"].as_str().unwrap_or("");
                let key = v["key"].as_str().unwrap_or("");
                let value = &v["value"];
                if !valid_ui_key(surface) || !valid_ui_key(key) || !valid_ui_value(value) {
                    return;
                }
                let Some(h) = self.viewing_host() else { return };
                // Do not let delayed UI control frames leak into a newly
                // selected session, even if the browser is still connected.
                if v["sess"].as_str() != Some(h.id.as_str()) {
                    return;
                }
                let event = sunmao_core::SessionEvent::UiState {
                    surface: surface.into(),
                    key: key.into(),
                    value: value.clone(),
                };
                let result = {
                    let mut log = h.agent.context().sessions.lock().await;
                    log.append(&event).await
                };
                if result.is_ok() {
                    self.s.live.send(serde_json::json!({
                        "type":"ui_state","sess":h.id,
                        "surface":surface,"key":key,"value":value,
                    }));
                }
            }
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
                    self.s.live.send(input_queue_frame(&h));
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

#[cfg(test)]
mod native_state_tests {
    use super::{valid_ui_key, valid_ui_value};
    #[test]
    fn only_small_scalar_values_and_safe_keys_are_accepted() {
        assert!(valid_ui_key("batch_1"));
        assert!(!valid_ui_key("constructor/__proto__"));
        assert!(valid_ui_value(&serde_json::json!(22)));
        assert!(valid_ui_value(&serde_json::json!(true)));
        assert!(!valid_ui_value(&serde_json::json!({"__proto__":1})));
        assert!(!valid_ui_value(&serde_json::json!("x".repeat(257))));
    }
}
