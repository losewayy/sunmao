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

use super::{ChannelAdapter, InboundMsg, SendFailure, SendResult};
use crate::im::config::TelegramSpec;
use crate::im::route::ImSource;
use crate::im::{redact, scope};

/// Telegram's hard per-message limit — final replies split on paragraph
/// boundaries under it.
const MSG_LIMIT: usize = 4096;
/// Error backoff — a transient API/network failure retries at this pace.
const ERR_BACKOFF_SECS: u64 = 3;
/// The `getUpdates` hold time a config may ask for. The lower bound keeps a
/// typo from turning long polling into a spin, the upper bound keeps one from
/// parking a connection for years.
const MIN_POLL_SECS: u64 = 5;
const MAX_POLL_SECS: u64 = 300;
/// The HTTP timeout has to outlive the server-side hold.
const POLL_TIMEOUT_MARGIN_SECS: u64 = 15;

/// The hold time this adapter will use, and the ask when it had to be moved.
/// `Some(asked)` is what drives the warning, so a clamp cannot be silent.
fn clamp_poll_secs(asked: u64) -> (u64, Option<u64>) {
    let hold = asked.clamp(MIN_POLL_SECS, MAX_POLL_SECS);
    (hold, (hold != asked).then_some(asked))
}

/// Bot API client + poller for one `{"kind":"telegram"}` channels.json
/// entry. The token lives in the `api` URL string for the process's life —
/// which is why every transport error is stripped of its URL before it can
/// reach a log line or the delivery ledger (see `redact::transport`).
pub struct TelegramAdapter {
    http: reqwest::Client,
    api: String,
    /// `getUpdates` hold time — configured per channel, default 30s.
    hold_secs: u64,
    /// The `getUpdates` offset slot, namespaced to this bot token. The
    /// unscoped `tg:offset` a pre-scoping build wrote is deliberately not
    /// adopted. `offset` is a *confirmation watermark*, not a position in a
    /// local log: the Bot API answers offset 0 with "updates starting with
    /// the earliest unconfirmed update", so losing the local copy costs at
    /// most the last batch received but not yet confirmed. `update_id` on the
    /// other hand is per-bot ("start from a certain positive number and
    /// increase sequentially"), and any offset above a new bot's updates
    /// makes the server forget them ("all previous updates will be
    /// forgotten") — adopting another bot's offset would silently drop the
    /// first messages a fresh bot ever receives.
    offset_key: String,
    store: std::sync::Arc<crate::im::store::Store>,
}

impl TelegramAdapter {
    pub fn new(
        spec: &TelegramSpec,
        store: std::sync::Arc<crate::im::store::Store>,
    ) -> Result<Self> {
        let token = spec.token()?;
        let (hold_secs, clamped) = clamp_poll_secs(spec.poll_timeout_secs);
        if let Some(asked) = clamped {
            // 0 is a legal Bot API value (short polling) but a daemon that
            // spins on getUpdates is not what anyone wants, and a value past
            // the ceiling is a typo. Say so rather than clamping in silence.
            tracing::warn!(
                "telegram: poll_timeout_secs {asked} clamped to {hold_secs} (allowed {MIN_POLL_SECS}..={MAX_POLL_SECS})"
            );
        }
        Ok(Self {
            http: reqwest::Client::builder()
                // long-poll reads must outlive the server-side hold
                .timeout(Duration::from_secs(
                    hold_secs.saturating_add(POLL_TIMEOUT_MARGIN_SECS),
                ))
                .build()?,
            api: format!("https://api.telegram.org/bot{token}"),
            hold_secs,
            offset_key: scope::scoped("tg:offset", &token),
            store,
        })
    }

    /// Test seam: point the client at a base URL a test controls, so the
    /// transport-error path can be exercised without the network.
    #[cfg(test)]
    fn with_api_base(mut self, api: String) -> Self {
        self.api = api;
        self
    }

    /// One Bot API call — `params` is the form/JSON body. `description`
    /// wraps a transport error with the method name; Telegram's own
    /// `{ok:false, description}` arrives as the same Err. The URL is stripped
    /// off every transport error first: it holds the bot token, and reqwest
    /// prints it in `Display`.
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
            .map_err(redact::transport)
            .with_context(|| format!("telegram {method}"))?
            .json()
            .await
            .map_err(redact::transport)
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

    /// The offset a poll resumes from — this bot's scoped slot only. See
    /// `offset_key` for why the legacy unscoped key is not consulted.
    fn resume_offset(&self) -> i64 {
        self.store
            .kv_get(&self.offset_key)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
    }

    /// Advance the durable cursor past `update_id` — written to the store
    /// BEFORE the message is dispatched, so a crash mid-turn replays at
    /// most a sent-but-unanswered prompt (the delivery ledger owns the
    /// reply side's at-least-once).
    fn advance_offset(&self, update_id: i64) -> Result<i64> {
        let next = update_id + 1;
        self.store.kv_set(&self.offset_key, &next.to_string())?;
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
        let mut offset = self.resume_offset();
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

    async fn send_text(&self, chat_id: &str, text: &str) -> SendResult {
        let mut first_id = None;
        let pieces = chunk(text);
        let total = pieces.len();
        for (delivered, piece) in pieces.into_iter().enumerate() {
            let res = self
                .api_call(
                    "sendMessage",
                    &serde_json::json!({"chat_id": chat_id, "text": piece}),
                )
                .await
                .map_err(|e| SendFailure::new(delivered, total, e))?;
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
mod tests;
