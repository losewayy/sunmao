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

/// Bot API client + poller for one `{"kind":"telegram"}` channels.json
/// entry. The token lives in the `api` URL string for the process's life —
/// which is why every transport error is stripped of its URL before it can
/// reach a log line or the delivery ledger (see `redact::transport`).
pub struct TelegramAdapter {
    http: reqwest::Client,
    api: String,
    /// `getUpdates` hold time — configured per channel, default 30s.
    hold_secs: u64,
    /// The `getUpdates` offset slot, namespaced to this bot token.
    offset_key: String,
    store: std::sync::Arc<crate::im::store::Store>,
}

impl TelegramAdapter {
    pub fn new(
        spec: &TelegramSpec,
        store: std::sync::Arc<crate::im::store::Store>,
    ) -> Result<Self> {
        let token = spec.token()?;
        let hold_secs = spec.poll_timeout_secs.clamp(MIN_POLL_SECS, MAX_POLL_SECS);
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
        let mut offset = self
            .store
            .kv_get(&self.offset_key)
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
    use crate::im::store::Store;
    use std::sync::Arc;

    const TOKEN: &str = "123456:AAHtesttoken";

    fn store() -> (Arc<Store>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "sunmao-im-tg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (Arc::new(Store::open(&dir).unwrap()), dir)
    }

    /// A token by file, so the tests never touch process environment.
    fn adapter(store: Arc<Store>, dir: &std::path::Path, poll_secs: u64) -> TelegramAdapter {
        let secret = dir.join("token.txt");
        std::fs::write(&secret, TOKEN).unwrap();
        TelegramAdapter::new(
            &TelegramSpec {
                token_env: None,
                token_file: Some(secret),
                poll_timeout_secs: poll_secs,
                owner: None,
                dm_policy: None,
                allowlist: Vec::new(),
                enabled: true,
            },
            store,
        )
        .unwrap()
    }

    /// A loopback port nothing listens on.
    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    fn update(chat_type: &str, from: serde_json::Value, text: Option<&str>) -> serde_json::Value {
        let mut update = serde_json::json!({
            "update_id": 41,
            "message": {"chat": {"id": 7, "type": chat_type}, "from": from},
        });
        if let Some(text) = text {
            update["message"]["text"] = serde_json::json!(text);
        }
        update
    }

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

    /// The cut is by byte, so a multi-byte character at the limit is where a
    /// naive slice would panic or split a glyph.
    #[test]
    fn chunks_never_split_a_character() {
        let cjk = "中".repeat(MSG_LIMIT);
        let parts = chunk(&cjk);
        assert_eq!(parts.concat(), cjk);
        assert!(parts.iter().all(|p| p.len() <= MSG_LIMIT));
        assert!(
            parts.iter().all(|p| p.chars().all(|c| c == '中')),
            "{parts:?}"
        );

        // an emoji is four bytes: the cut has to land on a boundary
        let crab = "🦀".repeat(MSG_LIMIT / 4 + 3);
        let parts = chunk(&crab);
        assert_eq!(parts.concat(), crab);
        assert!(parts.iter().all(|p| p.len() <= MSG_LIMIT));
        assert!(
            parts.iter().all(|p| p.chars().all(|c| c == '🦀')),
            "{parts:?}"
        );
    }

    #[test]
    fn a_private_text_update_maps_to_a_dm() {
        let msg = TelegramAdapter::extract_dm(&update(
            "private",
            serde_json::json!({"id": 9, "first_name": "Ada"}),
            Some("hi bot"),
        ))
        .unwrap();
        assert_eq!(msg.source.channel, "telegram");
        assert_eq!(msg.source.chat_id, "7");
        assert_eq!(msg.source.sender_id, "9");
        assert_eq!(msg.source.sender_name, "Ada");
        assert_eq!(msg.text, "hi bot");
    }

    #[test]
    fn the_sender_label_prefers_the_username_then_the_real_name() {
        let named = TelegramAdapter::extract_dm(&update(
            "private",
            serde_json::json!({"id": 9, "username": "ada", "first_name": "Ada"}),
            Some("hi"),
        ))
        .unwrap();
        assert_eq!(named.source.sender_name, "@ada");

        // no name at all: the numeric id is the honest label
        let bare = TelegramAdapter::extract_dm(&update(
            "private",
            serde_json::json!({"id": 9}),
            Some("hi"),
        ))
        .unwrap();
        assert_eq!(bare.source.sender_name, "9");
    }

    /// Non-private chats never map to a session, so a room can never be
    /// answered — `allowed_updates` filters most of this, but the mapper is
    /// the guarantee.
    #[test]
    fn group_and_channel_updates_are_dropped() {
        for chat_type in ["group", "supergroup", "channel"] {
            assert!(
                TelegramAdapter::extract_dm(&update(
                    chat_type,
                    serde_json::json!({"id": 9}),
                    Some("hi")
                ))
                .is_none(),
                "{chat_type} must not enter the DM-only gateway"
            );
        }
    }

    #[test]
    fn updates_without_a_readable_sender_or_body_are_dropped() {
        let cases = [
            update("private", serde_json::json!({"id": 9}), None),
            update("private", serde_json::json!({"id": 9}), Some("   ")),
            update("private", serde_json::json!({}), Some("hi")),
            update(
                "private",
                serde_json::json!({"id": "not-a-number"}),
                Some("hi"),
            ),
            serde_json::json!({"update_id": 1}),
        ];
        for case in cases {
            assert!(TelegramAdapter::extract_dm(&case).is_none(), "{case}");
        }
    }

    #[test]
    fn the_get_updates_offset_is_scoped_to_the_bot_token() {
        let (store, dir) = store();
        let first = adapter(store.clone(), &dir, 30);
        assert_eq!(first.advance_offset(41).unwrap(), 42);
        assert_eq!(
            store.kv_get(&scope::scoped("tg:offset", TOKEN)).as_deref(),
            Some("42")
        );
        assert_eq!(
            store.kv_get("tg:offset"),
            None,
            "one shared offset slot replays or drops updates after a token swap"
        );
        // a rebuilt adapter for the same token resumes where it stopped
        assert_eq!(
            adapter(store.clone(), &dir, 30).offset_key,
            first.offset_key
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_absurd_poll_timeout_is_clamped_not_overflowing() {
        let (store, dir) = store();
        for (asked, expected) in [
            (u64::MAX, MAX_POLL_SECS),
            (0, MIN_POLL_SECS),
            (30, 30),
            (10_000, MAX_POLL_SECS),
        ] {
            let adapter = adapter(store.clone(), &dir, asked);
            assert_eq!(adapter.hold_secs, expected, "poll_timeout_secs={asked}");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    /// The bot token rides the request URL, and reqwest prints that URL into
    /// its error `Display` — which is exactly what the poll loop logs and the
    /// delivery ledger records.
    #[tokio::test]
    async fn a_transport_error_never_carries_the_bot_token() {
        let (store, dir) = store();
        let adapter = adapter(store, &dir, MIN_POLL_SECS)
            .with_api_base(format!("http://127.0.0.1:{}/bot{TOKEN}", closed_port()));
        let err = adapter
            .api_call(
                "sendMessage",
                &serde_json::json!({"chat_id": "1", "text": "hi"}),
            )
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            !text.contains(TOKEN),
            "the bot token reached the error: {text}"
        );
        assert!(
            !text.contains("http://"),
            "the URL reached the error: {text}"
        );
        assert!(
            text.contains("sendMessage"),
            "the diagnosis is gone: {text}"
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
