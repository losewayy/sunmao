//! Telegram adapter — Bot API over long polling (`getUpdates`), so the
//! daemon needs no public endpoint and sits behind any NAT. reqwest +
//! serde_json only — the API surface we use is four REST calls; a client
//! SDK would be dead weight (dependency-budget rule).
//!
//! Non-goals here: groups (updates from non-private chats are dropped on
//! the floor — group semantics are a v0.5+ concern), media, edits, inline
//! keyboards (IM approval is full_access by design — no buttons).

use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::sync::mpsc;

use super::{ChannelAdapter, InboundMsg};
use crate::im::config::TelegramSpec;
use crate::im::route::ImSource;

/// Telegram's hard per-message limit — final replies split on paragraph
/// boundaries under it.
const MSG_LIMIT: usize = 4096;
/// Error backoff — a transient API/network failure retries at this pace.
const ERR_BACKOFF_SECS: u64 = 3;

/// Bot API client + poller for one `{"kind":"telegram"}` channels.json
/// entry. The token never lands in the store or logs — it lives in the
/// `api` URL string for the process's life.
pub struct TelegramAdapter {
    http: reqwest::Client,
    api: String,
    /// `getUpdates` hold time — configured per channel, default 30s.
    hold_secs: u64,
    store: std::sync::Arc<crate::im::store::Store>,
}

impl TelegramAdapter {
    pub fn new(
        spec: &TelegramSpec,
        store: std::sync::Arc<crate::im::store::Store>,
    ) -> Result<Self> {
        let token = spec.token()?;
        Ok(Self {
            http: reqwest::Client::builder()
                // long-poll reads must outlive the server-side hold
                .timeout(Duration::from_secs(spec.poll_timeout_secs + 15))
                .build()?,
            api: format!("https://api.telegram.org/bot{token}"),
            hold_secs: spec.poll_timeout_secs,
            store,
        })
    }

    /// One Bot API call — `params` is the form/JSON body. `description`
    /// wraps a transport error with the method name; Telegram's own
    /// `{ok:false, description}` arrives as the same Err.
    async fn api_call(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let resp: serde_json::Value = self
            .http
            .post(format!("{}/{method}", self.api))
            .json(params)
            .send()
            .await
            .with_context(|| format!("telegram {method}"))?
            .json()
            .await
            .with_context(|| format!("telegram {method} decode"))?;
        if resp["ok"].as_bool() == Some(true) {
            Ok(resp["result"].clone())
        } else {
            Err(anyhow::anyhow!(
                "telegram {method}: {}",
                resp["description"].as_str().unwrap_or("unknown error")
            ))
        }
    }

    /// One `getUpdates` round — `allowed_updates=["message"]` keeps
    /// channel/group noise off the wire entirely.
    async fn poll_once(&self, offset: i64) -> Result<Vec<serde_json::Value>> {
        let res = self
            .api_call(
                "getUpdates",
                &serde_json::json!({
                    "offset": offset,
                    "timeout": self.hold_secs,
                    "allowed_updates": ["message"],
                }),
            )
            .await?;
        Ok(res.as_array().cloned().unwrap_or_default())
    }

    /// Advance the durable cursor past `update_id` — written to the store
    /// BEFORE the message is dispatched, so a crash mid-turn replays at
    /// most a sent-but-unanswered prompt (the delivery ledger owns the
    /// reply side's at-least-once).
    fn advance_offset(&self, update_id: i64) -> Result<i64> {
        let next = update_id + 1;
        self.store.kv_set("tg:offset", &next.to_string())?;
        Ok(next)
    }

    /// Extract the DM fields — anything not a private-chat text message
    /// (group posts, stickers, edited messages — `allowed_updates` filters
    /// most of them) resolves to `None` and the update is still acked.
    fn extract_dm(update: &serde_json::Value) -> Option<InboundMsg> {
        let msg = &update["message"];
        if msg["chat"]["type"].as_str() != Some("private") {
            return None;
        }
        let text = msg["text"].as_str()?.to_string();
        if text.trim().is_empty() {
            return None;
        }
        let from = &msg["from"];
        let sender_id = from["id"].as_i64()?.to_string();
        let sender_name = from["username"]
            .as_str()
            .map(|u| format!("@{u}"))
            .or_else(|| from["first_name"].as_str().map(str::to_string))
            .unwrap_or_else(|| sender_id.clone());
        Some(InboundMsg {
            source: ImSource {
                channel: "telegram".into(),
                chat_id: msg["chat"]["id"].as_i64()?.to_string(),
                sender_id,
                sender_name,
            },
            text,
        })
    }
}

/// Split `text` into MSG_LIMIT-safe chunks — paragraph-first, then a hard
/// cut. Returns at least one (possibly empty) chunk.
fn chunk(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > MSG_LIMIT {
        // split on the last paragraph/line break inside the window — a
        // mid-word cut is the fallback, not the plan
        let window = &rest[..rest.floor_char_boundary(MSG_LIMIT)];
        let cut = window
            .rfind("\n\n")
            .or_else(|| window.rfind('\n'))
            .map(|i| i + 1)
            .unwrap_or(window.len());
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    out.push(rest.to_string());
    out
}

#[async_trait::async_trait]
impl ChannelAdapter for TelegramAdapter {
    fn channel(&self) -> &'static str {
        "telegram"
    }

    async fn poll(&self, tx: mpsc::Sender<InboundMsg>) {
        let mut offset = self
            .store
            .kv_get("tg:offset")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        loop {
            match self.poll_once(offset).await {
                Ok(updates) => {
                    for u in updates {
                        let id = u["update_id"].as_i64().unwrap_or(0);
                        if let Ok(next) = self.advance_offset(id) {
                            offset = next;
                        }
                        if let Some(msg) = Self::extract_dm(&u)
                            && tx.send(msg).await.is_err()
                        {
                            // gateway gone — the process is shutting down
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("telegram poll: {e:#}");
                    tokio::time::sleep(Duration::from_secs(ERR_BACKOFF_SECS)).await;
                }
            }
        }
    }

    async fn send_text(&self, chat_id: &str, text: &str) -> Result<Option<String>> {
        let mut first_id = None;
        for piece in chunk(text) {
            let res = self
                .api_call(
                    "sendMessage",
                    &serde_json::json!({"chat_id": chat_id, "text": piece}),
                )
                .await?;
            if first_id.is_none() {
                first_id = res["message_id"].as_i64().map(|i| i.to_string());
            }
        }
        Ok(first_id)
    }

    async fn edit_text(&self, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        self.api_call(
            "editMessageText",
            &serde_json::json!({
                "chat_id": chat_id,
                "message_id": message_id.parse::<i64>().unwrap_or(0),
                "text": text,
            }),
        )
        .await?;
        Ok(())
    }

    async fn send_typing(&self, chat_id: &str) {
        let _ = self
            .api_call(
                "sendChatAction",
                &serde_json::json!({"chat_id": chat_id, "action": "typing"}),
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_under_limit() {
        assert_eq!(chunk("short"), vec!["short".to_string()]);
        let long = "a".repeat(MSG_LIMIT * 2 + 5);
        let parts = chunk(&long);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() <= MSG_LIMIT));
        assert_eq!(parts.concat(), long);
    }

    #[test]
    fn chunks_prefer_paragraph_breaks() {
        let text = format!("{}\n\n{}", "x".repeat(3000), "y".repeat(2000));
        let parts = chunk(&text);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "x".repeat(3000) + "\n");
    }
}
