//! WeChat adapter — the personal-account iLink protocol. `getupdates` is a
//! 40-second long poll; the answer carries the messages *and a new cursor*,
//! which is persisted so a restart resumes instead of replaying or dropping.
//! Replies are windowed: iLink only accepts a message to a peer whose
//! `context_token` is still fresh, so every inbound message's token is
//! cached (24h) and a send without one is refused loudly rather than
//! silently dropped.
//!
//! Non-goals here: QR-code login (`get_bot_qrcode` / `get_qrcode_status` —
//! the bot token is supplied by configuration), media (the AES-128-ECB CDN
//! upload/download path), typing indicators, and groups (this channel has
//! only direct messages).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::sync::mpsc;

use super::{ChannelAdapter, InboundMsg, SendFailure, SendResult};
use crate::im::config::WechatSpec;
use crate::im::redact;
use crate::im::store::Store;

mod protocol;

/// The long poll holds for `LONG_POLL_SECS`; the HTTP timeout rides above it
/// so a slow turn is never read as a dead connection.
const POLL_TIMEOUT_SECS: u64 = protocol::LONG_POLL_SECS + 15;
/// Consecutive failures before the loop reports at `error` level — one
/// timeout is noise, three in a row is a problem. It keeps retrying either
/// way.
const FAILURE_REPORT: usize = 3;
/// Reconnect ladder — the last step repeats.
const BACKOFF_SECS: [u64; 3] = [2, 5, 30];

/// Bot client for one `{"kind":"wechat"}` channels.json entry. The bot token
/// lives here for the process's life — never in the store, a ledger row, or
/// a log line.
pub struct WechatAdapter {
    http: reqwest::Client,
    bot_token: String,
    store: Arc<Store>,
}

impl WechatAdapter {
    pub fn new(spec: &WechatSpec, store: Arc<Store>) -> Result<Self> {
        // The QR-code login flow is not part of this version, so the error
        // has to say where the token is supposed to come from instead.
        let bot_token = spec.bot_token().map_err(|e| {
            anyhow::anyhow!(
                "{e:#}; wechat QR-code login is not shipped yet, so the iLink \
                 bot token has to be configured by hand"
            )
        })?;
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(POLL_TIMEOUT_SECS))
                .build()?,
            bot_token,
            store,
        })
    }

    /// One iLink call. Both the HTTP status and the business `ret` are error
    /// surfaces; the business failure travels as `IlinkError` so the poll
    /// loop can tell a revoked session from a transient one. The URL is
    /// stripped off every transport error, so the same discipline holds here
    /// as on the channels that do put identifiers in a path.
    async fn api_post(
        &self,
        endpoint: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/{endpoint}", protocol::BASE_URL);
        let resp = self
            .http
            .post(&url)
            .header("AuthorizationType", "ilink_bot_token")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("X-WECHAT-UIN", protocol::random_uin())
            .json(body)
            .send()
            .await
            .map_err(redact::transport)
            .with_context(|| format!("wechat {endpoint}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(redact::transport)
            .with_context(|| format!("wechat {endpoint} body"))?;
        if !status.is_success() {
            anyhow::bail!("wechat {endpoint}: http={status} {}", brief(&text));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("wechat {endpoint} decode {}", brief(&text)))?;
        if let Some(err) = protocol::business_error(endpoint, &v) {
            return Err(err.into());
        }
        Ok(v)
    }

    /// One long poll. The answer's `get_updates_buf` is the cursor for the
    /// *next* call — the platform keeps the position server-side.
    async fn get_updates(&self, cursor: &str) -> Result<serde_json::Value> {
        self.api_post(
            protocol::GET_UPDATES_PATH,
            &protocol::get_updates_body(cursor),
        )
        .await
    }

    /// The persisted cursor, empty on a first run. Scoped to this bot token:
    /// another token's cursor is a position in another update stream.
    fn load_cursor(&self) -> String {
        self.store
            .kv_get(&protocol::cursor_key(&self.bot_token))
            .unwrap_or_default()
    }

    /// Persist the cursor before its messages are dispatched, so a crash
    /// replays at most the batch already in hand.
    fn remember_cursor(&self, cursor: &str) {
        if let Err(e) = self
            .store
            .kv_set(&protocol::cursor_key(&self.bot_token), cursor)
        {
            tracing::warn!("wechat: cursor not persisted: {e:#}");
        }
    }

    /// Open (or refresh) one peer's reply window, under this bot token.
    fn remember_context(&self, chat_id: &str, token: &str) {
        let key = protocol::context_key(&self.bot_token, chat_id);
        let value = protocol::encode_context(token, crate::im::store::now());
        if let Err(e) = self.store.kv_set(&key, &value) {
            tracing::warn!("wechat: reply window not persisted: {e:#}");
        }
    }

    /// The peer's live reply window, or `None` once it is older than the
    /// platform's 24h. An expired window is cleared, not left to expire
    /// again on every send.
    fn context_of(&self, chat_id: &str) -> Option<String> {
        let key = protocol::context_key(&self.bot_token, chat_id);
        let raw = self.store.kv_get(&key)?;
        match protocol::decode_context(&raw, crate::im::store::now()) {
            Some((token, _)) => Some(token),
            None => {
                let _ = self.store.kv_set(&key, "");
                None
            }
        }
    }

    /// One inbound message: record the reply window it carries, then map it.
    /// The window is recorded even when there is nothing to read — an image
    /// still opens the door for the reply that follows it.
    fn absorb(&self, msg: &serde_json::Value) -> Option<InboundMsg> {
        let chat_id = protocol::peer_id(msg)?;
        if let Some(token) = protocol::context_token(msg) {
            self.remember_context(chat_id, token);
        }
        protocol::extract_dm(msg)
    }

    async fn send_message(&self, chat_id: &str, text: &str) -> SendResult {
        let Some(context_token) = self.context_of(chat_id) else {
            // The peer id is a person, and this string travels (log lines, the
            // delivery ledger, a copy-pasted bug report); the caller's log line
            // already names the chat, so the error itself does not have to.
            return Err(SendFailure::before_send(anyhow::anyhow!(
                "wechat: no reply window for {}; the peer has to message \
                 first, and a window older than 24h is closed",
                redact::MASK
            )));
        };
        let pieces = protocol::chunk(text);
        let total = pieces.len();
        for (delivered, piece) in pieces.into_iter().enumerate() {
            let body = protocol::send_body(chat_id, &piece, &protocol::uuid_v4(), &context_token);
            self.api_post(protocol::SEND_PATH, &body)
                .await
                .map_err(|e| SendFailure::new(delivered, total, e))?;
        }
        // iLink answers with no message id, and there is no edit API — the
        // delivery ledger owns the reply, the progress draft re-posts.
        Ok(None)
    }
}

/// A response body can be anything a proxy feels like (HTML, a stack trace);
/// a diagnosis only ever needs enough of it to name the failure.
fn brief(body: &str) -> String {
    const MAX: usize = 200;
    let body = body.trim();
    match body.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}...", &body[..i]),
        None => body.to_string(),
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for WechatAdapter {
    fn channel(&self) -> &'static str {
        "wechat"
    }

    async fn poll(&self, tx: mpsc::Sender<InboundMsg>) {
        let mut cursor = self.load_cursor();
        let mut failures = 0usize;
        loop {
            match self.get_updates(&cursor).await {
                Ok(v) => {
                    failures = 0;
                    if let Some(next) = v["get_updates_buf"].as_str().filter(|c| !c.is_empty()) {
                        self.remember_cursor(next);
                        cursor = next.to_string();
                    }
                    for msg in v["msgs"].as_array().into_iter().flatten() {
                        if let Some(inbound) = self.absorb(msg)
                            && tx.send(inbound).await.is_err()
                        {
                            return; // gateway gone — the process is shutting down
                        }
                    }
                }
                Err(e) => {
                    // A revoked token cannot be retried into working: stop
                    // polling and say so, instead of hammering the endpoint.
                    if let Some(err) = e.downcast_ref::<protocol::IlinkError>()
                        && err.is_session_expired()
                    {
                        tracing::error!(
                            "wechat: session expired ({err}); stopping the poll loop, \
                             the bot token has to be issued again"
                        );
                        return;
                    }
                    failures += 1;
                    if failures >= FAILURE_REPORT {
                        tracing::error!("wechat poll failed {failures} times: {e:#}");
                    } else {
                        tracing::warn!("wechat poll: {e:#}");
                    }
                    let delay = BACKOFF_SECS[(failures - 1).min(BACKOFF_SECS.len() - 1)];
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            }
        }
    }

    async fn send_text(&self, chat_id: &str, text: &str) -> SendResult {
        self.send_message(chat_id, text).await
    }

    // edit_text: iLink has no message-edit call — the progress draft is
    // re-posted, and the default no-op is the honest behavior.
    // send_typing: iLink exposes `getconfig`/`sendtyping`; the typing
    // keep-alive is out of scope for this batch.
}

#[cfg(test)]
mod tests;
