//! QQ adapter — the 开放平台 v2 API. AppID + AppSecret buy an
//! `access_token`, `/gateway` hands out the bot's own WebSocket, events
//! arrive there and replies go back over REST (`/v2/users/…` or
//! `/v2/groups/…`, chosen by the target type the inbound event revealed).
//!
//! Non-goals here: groups (`GROUP_AT_MESSAGE_CREATE` is dropped, see
//! `protocol::extract_event` — a group reply would hand a pairing code to an
//! unpaired stranger in front of the whole room), guild channels, media
//! upload, message edits, and active (non-passive) messages — replies ride
//! the passive `msg_id` window.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::{ChannelAdapter, InboundMsg, SendFailure, SendResult};
use crate::im::config::QqSpec;
use crate::im::redact;
use crate::im::store::Store;
use protocol::{
    ChatTarget, chunk, extract_event, heartbeat_frame, identify_frame, op, resume_frame, send_body,
    token_request,
};

mod protocol;

/// The API host — gateway and messages.
const API_BASE: &str = "https://api.sgroup.qq.com";
/// Refresh the access token this long before it expires.
const TOKEN_MARGIN_SECS: u64 = 300;
/// The token TTL assumed when the platform omits `expires_in`.
const DEFAULT_TTL_SECS: u64 = 7200;
/// The TTL window a token answer is trusted in. Below a minute the token
/// endpoint is hammered; above a day the answer is broken — and an absurd
/// value makes `Instant + Duration` panic, taking the poll loop with it.
const MIN_TTL_SECS: u64 = 60;
const MAX_TTL_SECS: u64 = 86_400;
/// A passive reply is tied to the user's `msg_id` and only lands inside
/// this window; past it there is no way to answer (QQ documents five
/// minutes — the reference keeps the same order of magnitude).
const PASSIVE_TTL_SECS: i64 = 300;
/// Reconnect ladder — the last step repeats.
const RECONNECT_SECS: [u64; 6] = [1, 2, 5, 10, 30, 60];
/// A connection that stayed up this long resets the ladder.
const STABLE_CONN_SECS: u64 = 300;

/// The `expires_in` of a token response — a number or a string — clamped to
/// `MIN_TTL_SECS..=MAX_TTL_SECS`.
fn token_ttl(v: &serde_json::Value) -> u64 {
    v["expires_in"]
        .as_u64()
        .or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(MIN_TTL_SECS, MAX_TTL_SECS)
}

/// The endpoints one send may try, in order. An unknown chat is probed as a
/// DM only: this version carries no group events, so a chat the bot has
/// never heard from is a user, and a probe that guessed "group" would post
/// into a room full of people. The `Group` arm only serves a target written
/// by an earlier version.
fn send_candidates(known: Option<ChatTarget>) -> &'static [ChatTarget] {
    match known {
        Some(ChatTarget::User) => &[ChatTarget::User],
        Some(ChatTarget::Group) => &[ChatTarget::Group],
        None => &[ChatTarget::User],
    }
}

/// Resume material for one WS lifetime — carried across reconnects so a
/// dropped socket resumes instead of replaying IDENTIFY.
#[derive(Default)]
struct Session {
    id: Option<String>,
    seq: Option<i64>,
}

/// Bot client for one `{"kind":"qq"}` channels.json entry. The secret and
/// the access token live here for the process's life — never in a ledger
/// row, the store, or a log line.
pub struct QqAdapter {
    http: reqwest::Client,
    app_id: String,
    app_secret: String,
    /// `(access_token, expires_at)` — an `std` mutex so no guard is ever
    /// held across an await.
    token: std::sync::Mutex<Option<(String, Instant)>>,
    store: Arc<Store>,
    /// Test seam: overrides the API host so the transport-error path can run
    /// against a closed loopback port instead of the network.
    #[cfg(test)]
    api_base: Option<String>,
}

impl QqAdapter {
    pub fn new(spec: &QqSpec, store: Arc<Store>) -> Result<Self> {
        anyhow::ensure!(!spec.app_id.trim().is_empty(), "qq channel needs app_id");
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            app_id: spec.app_id.trim().to_string(),
            app_secret: spec.app_secret()?,
            token: std::sync::Mutex::new(None),
            store,
            #[cfg(test)]
            api_base: None,
        })
    }

    /// Test seam: point the REST calls at a host a test controls.
    #[cfg(test)]
    fn with_api_base(mut self, api_base: String) -> Self {
        self.api_base = Some(api_base);
        self
    }

    /// The API host — the test seam wins when one is set.
    fn api_base(&self) -> &str {
        #[cfg(test)]
        if let Some(base) = &self.api_base {
            return base;
        }
        API_BASE
    }

    /// The cached token, if it is still good for `TOKEN_MARGIN_SECS`.
    fn cached_token(&self) -> Option<String> {
        let cache = self.token.lock().unwrap();
        cache.as_ref().and_then(|(token, expires)| {
            (Instant::now() + Duration::from_secs(TOKEN_MARGIN_SECS) < *expires)
                .then(|| token.clone())
        })
    }

    /// The v2 `access_token`, refreshed five minutes early.
    async fn access_token(&self) -> Result<String> {
        if let Some(token) = self.cached_token() {
            return Ok(token);
        }
        let (url, body) = token_request(&self.app_id, &self.app_secret);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(redact::transport)
            .context("qq token")?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(redact::transport)
            .context("qq token decode")?;
        let token = v["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .with_context(|| {
                format!(
                    "qq token: code={} msg={}",
                    v["code"].as_i64().unwrap_or(-1),
                    v["message"]
                        .as_str()
                        .or_else(|| v["msg"].as_str())
                        .unwrap_or("-")
                )
            })?
            .to_string();
        let ttl = token_ttl(&v);
        *self.token.lock().unwrap() =
            Some((token.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(token)
    }

    /// One v2 REST call — `stage` names it in diagnostics, the way the
    /// reference's outbound helper does.
    async fn api_call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
        token: &str,
        stage: &str,
    ) -> Result<serde_json::Value> {
        let url = format!("{}{path}", self.api_base());
        let mut req = self
            .http
            .request(method, &url)
            .header("Authorization", format!("QQBot {token}"));
        if let Some(body) = body {
            req = req.json(body);
        }
        // The path carries the chat's openid, and reqwest prints the URL of a
        // failed request in its `Display` — an un-scrubbed error would put a
        // peer identifier into every log line and ledger row.
        let resp = req
            .send()
            .await
            .map_err(redact::transport)
            .with_context(|| format!("qq {stage}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(redact::transport)
            .with_context(|| format!("qq {stage} body"))?;
        if !status.is_success() {
            let v: serde_json::Value =
                serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
            anyhow::bail!(
                "qq {stage}: http={status} code={} msg={}",
                v["code"].as_i64().unwrap_or(-1),
                v["message"]
                    .as_str()
                    .or_else(|| v["msg"].as_str())
                    .unwrap_or("-")
            );
        }
        if text.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("qq {stage} decode"))
    }

    /// `GET /gateway` — this bot's WebSocket URL.
    async fn gateway(&self, token: &str) -> Result<String> {
        let v = self
            .api_call(reqwest::Method::GET, "/gateway", None, token, "gateway")
            .await?;
        v["url"]
            .as_str()
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .context("qq gateway: no url")
    }

    /// The chat's reply target, as remembered from its inbound event.
    fn target_of(&self, chat_id: &str) -> Option<ChatTarget> {
        ChatTarget::parse(&self.store.kv_get(&format!("qq:target:{chat_id}"))?)
    }

    fn remember_target(&self, chat_id: &str, target: ChatTarget) {
        let _ = self
            .store
            .kv_set(&format!("qq:target:{chat_id}"), target.as_str());
    }

    /// Remember the message a reply may attach to — the passive window
    /// starts when the user's message arrives, not when we answer.
    fn remember_passive(&self, chat_id: &str, message_id: &str) {
        if message_id.is_empty() {
            return;
        }
        let _ = self.store.kv_set(
            &format!("qq:reply:{chat_id}"),
            &format!("{message_id}\t0\t{}", crate::im::store::now()),
        );
    }

    /// The next passive-reply cursor for `chat`: QQ requires the user's
    /// `msg_id` plus a `msg_seq` that increases with every reply to it.
    /// `None` once the window has closed — an active message is out of
    /// scope, so the reply then goes out unattached.
    fn next_passive(&self, chat_id: &str) -> Option<(String, i64)> {
        let key = format!("qq:reply:{chat_id}");
        let raw = self.store.kv_get(&key)?;
        let mut parts = raw.split('\t');
        let id = parts.next()?.to_string();
        let seq = parts
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let ts = parts
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        if id.is_empty() || crate::im::store::now() - ts > PASSIVE_TTL_SECS {
            return None;
        }
        let next = seq + 1;
        let _ = self.store.kv_set(&key, &format!("{id}\t{next}\t{ts}"));
        Some((id, next))
    }

    /// One WS lifetime. `Ok(())` means the gateway side is gone
    /// (shutdown); every other ending is an error the caller backs off
    /// from and reconnects.
    async fn ws_session(&self, tx: &mpsc::Sender<InboundMsg>, session: &mut Session) -> Result<()> {
        let token = self.access_token().await?;
        let url = self.gateway(&token).await?;
        let (ws, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .context("qq ws connect")?;
        let (mut sink, mut stream) = ws.split();
        // armed by HELLO; sent every `heartbeat_interval` the server named
        let mut heartbeat: Option<tokio::time::Interval> = None;
        let mut acked = true;
        loop {
            tokio::select! {
                _ = async {
                    match heartbeat.as_mut() {
                        Some(tick) => {
                            tick.tick().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    // No ACK for a full interval means the socket is a
                    // zombie even though it reads as open — drop it.
                    anyhow::ensure!(acked, "qq ws heartbeat unacked");
                    acked = false;
                    sink.send(Message::text(heartbeat_frame(session.seq).to_string()))
                        .await
                        .context("qq ws heartbeat")?;
                }
                msg = stream.next() => {
                    let Some(msg) = msg else {
                        anyhow::bail!("qq ws closed");
                    };
                    let msg = msg.context("qq ws read")?;
                    match msg {
                        Message::Text(raw) => {
                            let payload: serde_json::Value = match serde_json::from_str(&raw) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("qq ws: bad frame: {e}");
                                    continue;
                                }
                            };
                            if let Some(s) = payload["s"].as_i64() {
                                session.seq = Some(s);
                            }
                            match payload["op"].as_i64().unwrap_or(-1) {
                                op::HELLO => {
                                    let ms = payload["d"]["heartbeat_interval"]
                                        .as_u64()
                                        .unwrap_or(30_000)
                                        .max(1_000);
                                    let mut tick =
                                        tokio::time::interval(Duration::from_millis(ms));
                                    // the first tick is immediate — consume it
                                    // so the first beat waits one interval
                                    tick.tick().await;
                                    heartbeat = Some(tick);
                                    let token = format!("QQBot {token}");
                                    let frame = match &session.id {
                                        Some(id) => resume_frame(&token, id, session.seq),
                                        None => identify_frame(&token),
                                    };
                                    sink.send(Message::text(frame.to_string()))
                                        .await
                                        .context("qq ws identify")?;
                                }
                                op::DISPATCH => {
                                    let kind = payload["t"].as_str().unwrap_or("");
                                    match kind {
                                        "READY" => {
                                            session.id = payload["d"]["session_id"]
                                                .as_str()
                                                .map(str::to_string);
                                            tracing::info!("qq ws ready");
                                        }
                                        "RESUMED" => tracing::info!("qq ws resumed"),
                                        _ => {
                                            if let Some((msg, target)) =
                                                extract_event(kind, &payload["d"])
                                            {
                                                // the reply path needs both the
                                                // target type and the passive id
                                                let chat = &msg.source.chat_id;
                                                self.remember_target(chat, target);
                                                if let Some(id) = payload["d"]["id"].as_str() {
                                                    self.remember_passive(chat, id);
                                                }
                                                if tx.send(msg).await.is_err() {
                                                    return Ok(());
                                                }
                                            }
                                        }
                                    }
                                }
                                op::HEARTBEAT_ACK => acked = true,
                                op::RECONNECT => anyhow::bail!("qq ws reconnect requested"),
                                op::INVALID_SESSION => {
                                    session.id = None;
                                    session.seq = None;
                                    anyhow::bail!("qq ws session invalidated");
                                }
                                _ => {}
                            }
                        }
                        Message::Close(_) => anyhow::bail!("qq ws closed by peer"),
                        Message::Ping(p) => sink.send(Message::Pong(p)).await?,
                        _ => {}
                    }
                }
            }
        }
    }

    /// One body per chunk. Every chunk is its own reply to the same
    /// `msg_id`, and QQ de-duplicates on `(msg_id, msg_seq)`, so the cursor
    /// has to advance per chunk: sharing one sequence across a long answer
    /// has the platform drop everything after the first chunk.
    fn reply_bodies(&self, chat_id: &str, text: &str) -> Vec<serde_json::Value> {
        chunk(text)
            .into_iter()
            .map(|piece| {
                let passive = self.next_passive(chat_id);
                send_body(
                    &piece,
                    passive.as_ref().map(|(id, seq)| (id.as_str(), *seq)),
                )
            })
            .collect()
    }

    /// One message per chunk. The chat's target type is whatever its inbound
    /// event wrote; an unknown chat is probed as a DM only.
    async fn send_message(&self, chat_id: &str, text: &str) -> SendResult {
        // the token is fetched before anything is sent
        let token = self
            .access_token()
            .await
            .map_err(SendFailure::before_send)?;
        let bodies = self.reply_bodies(chat_id, text);
        let total = bodies.len();
        let mut first_id = None;
        for (delivered, body) in bodies.into_iter().enumerate() {
            let res = self
                .post_message(&token, chat_id, &body)
                .await
                .map_err(|e| SendFailure::new(delivered, total, e))?;
            if first_id.is_none() {
                first_id = res["id"].as_str().map(str::to_string);
            }
        }
        Ok(first_id)
    }

    async fn post_message(
        &self,
        token: &str,
        chat_id: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let known = self.target_of(chat_id);
        let candidates = send_candidates(known);
        let mut last = None;
        for target in candidates {
            let path = target.path(chat_id);
            match self
                .api_call(reqwest::Method::POST, &path, Some(body), token, "send")
                .await
            {
                Ok(v) => {
                    if known.is_none() {
                        self.remember_target(chat_id, *target);
                    }
                    return Ok(v);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("qq send: no endpoint")))
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for QqAdapter {
    fn channel(&self) -> &'static str {
        "qq"
    }

    async fn poll(&self, tx: mpsc::Sender<InboundMsg>) {
        let mut session = Session::default();
        let mut failures = 0usize;
        loop {
            let started = Instant::now();
            match self.ws_session(&tx, &mut session).await {
                Ok(()) => return, // gateway gone — shutting down
                Err(e) => tracing::warn!("qq ws: {e:#}"),
            }
            // a long-lived connection is not a failing one — reset the
            // ladder so a healthy bot does not reconnect slowly
            if started.elapsed() > Duration::from_secs(STABLE_CONN_SECS) {
                failures = 0;
            }
            let delay = RECONNECT_SECS[failures.min(RECONNECT_SECS.len() - 1)];
            failures += 1;
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    }

    async fn send_text(&self, chat_id: &str, text: &str) -> SendResult {
        self.send_message(chat_id, text).await
    }

    // edit_text: QQ v2 edits exist for streamed replies but no caller in
    // this gateway needs one — the progress draft is a fresh message.
}

#[cfg(test)]
mod tests;
