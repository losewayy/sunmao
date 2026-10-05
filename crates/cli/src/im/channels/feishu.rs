//! Feishu/Lark adapter — the bot's WebSocket long connection subscribed to
//! `im.message.receive_v1`, plus the three REST calls around it:
//! `tenant_access_token` exchange, `im.message.create`, `im.message.update`.
//! reqwest + serde_json + tokio-tungstenite only; the long connection is
//! why the daemon needs no callback URL and sits behind any NAT.
//!
//! Non-goals here: groups (`chat_type: "group"` is dropped — routing and
//! authz are DM-only, see `authz.rs`), media, CardKit streaming, the
//! contact API (the sender label falls back to the `open_id`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::{ChannelAdapter, InboundMsg, SendFailure, SendResult};
use crate::im::config::{FeishuRegion, FeishuSpec};
use crate::im::route::ImSource;

mod frame;

/// Outbound chunk size. The reference never chunks (it lets the platform
/// cap a single post body); 4000 keeps one bubble per paragraph-sized
/// reply and stays clear of the documented `post` body limit.
const MSG_LIMIT: usize = 4000;
/// Reconnect backoff after a drop or a failed handshake.
const ERR_BACKOFF_SECS: u64 = 3;
/// Refresh the tenant token this long before it expires.
const TOKEN_MARGIN_SECS: u64 = 300;
/// The token TTL assumed when the platform omits `expire`.
const DEFAULT_TTL_SECS: u64 = 7200;
/// The TTL window a token answer is trusted in. Below a minute the token
/// endpoint is hammered; an absurd value makes `Instant + Duration` panic.
const MIN_TTL_SECS: u64 = 60;
const MAX_TTL_SECS: u64 = 86_400;
/// A half-assembled chunked event is abandoned after this long (the SDK's
/// own cache uses the same window).
const CHUNK_TTL: Duration = Duration::from_secs(10);
/// The largest `sum` a chunked event may claim. The header sizes an
/// allocation before any payload arrives, so a hostile or corrupt value would
/// have the allocator abort the process; no real event comes close.
const MAX_CHUNK_SUM: usize = 1024;
/// The one event this adapter subscribes to.
const EVENT_MESSAGE: &str = "im.message.receive_v1";

/// The `expire` of a token response — a number or a string — clamped to
/// `MIN_TTL_SECS..=MAX_TTL_SECS`.
fn token_ttl(v: &serde_json::Value) -> u64 {
    v["expire"]
        .as_u64()
        .or_else(|| v["expire"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(MIN_TTL_SECS, MAX_TTL_SECS)
}

/// In-flight chunked events: per `message_id`, when the first frame
/// landed and the slots seen so far (`None` = still missing).
type ChunkBufs = HashMap<String, (Instant, Vec<Option<Vec<u8>>>)>;

/// The API host for a region — an app registered on one domain does not
/// authenticate on the other.
fn domain(region: FeishuRegion) -> &'static str {
    match region {
        FeishuRegion::FeishuCn => "https://open.feishu.cn",
        FeishuRegion::LarkGlobal => "https://open.larksuite.com",
    }
}

/// Bot WS client + REST surface for one `{"kind":"feishu"}` channels.json
/// entry. The app secret lives in this struct for the process's life — it
/// never reaches the store, a ledger row, or a log line.
pub struct FeishuAdapter {
    http: reqwest::Client,
    base: &'static str,
    app_id: String,
    app_secret: String,
    /// `(tenant_access_token, expires_at)` — an `std` mutex so no guard is
    /// ever held across an await.
    token: std::sync::Mutex<Option<(String, Instant)>>,
}

impl FeishuAdapter {
    pub fn new(spec: &FeishuSpec) -> Result<Self> {
        anyhow::ensure!(
            !spec.app_id.trim().is_empty(),
            "feishu channel needs app_id"
        );
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            base: domain(spec.region),
            app_id: spec.app_id.trim().to_string(),
            app_secret: spec.app_secret()?,
            token: std::sync::Mutex::new(None),
        })
    }

    /// The cached token, if it is still good for `TOKEN_MARGIN_SECS`.
    fn cached_token(&self) -> Option<String> {
        let cache = self.token.lock().unwrap();
        cache.as_ref().and_then(|(token, expires)| {
            (Instant::now() + Duration::from_secs(TOKEN_MARGIN_SECS) < *expires)
                .then(|| token.clone())
        })
    }

    /// `tenant_access_token` — the only credential the REST calls carry.
    async fn tenant_token(&self) -> Result<String> {
        if let Some(token) = self.cached_token() {
            return Ok(token);
        }
        let (url, body) = token_request(self.base, &self.app_id, &self.app_secret);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("feishu tenant_access_token")?;
        let v: serde_json::Value = resp
            .json()
            .await
            .context("feishu tenant_access_token decode")?;
        let token = v["tenant_access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .with_context(|| {
                format!(
                    "feishu tenant_access_token: code={} msg={}",
                    v["code"].as_i64().unwrap_or(-1),
                    v["msg"].as_str().unwrap_or("-")
                )
            })?
            .to_string();
        let ttl = token_ttl(&v);
        *self.token.lock().unwrap() =
            Some((token.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(token)
    }

    /// One REST call. HTTP status and the Feishu business envelope are
    /// both error surfaces (`code != 0` arrives under HTTP 200); the
    /// `log_id` is the only handle support can work with, so it is always
    /// in the message.
    async fn api_call(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&serde_json::Value>,
        what: &str,
    ) -> Result<serde_json::Value> {
        let token = self.tenant_token().await?;
        let mut req = self.http.request(method, url).bearer_auth(token);
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req.send().await.with_context(|| format!("feishu {what}"))?;
        let status = resp.status();
        let v: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("feishu {what} decode"))?;
        let code = v["code"]
            .as_i64()
            .or_else(|| v["code"].as_str().and_then(|c| c.parse().ok()))
            .unwrap_or(0);
        if code != 0 || !status.is_success() {
            anyhow::bail!(
                "feishu {what}: http={status} code={code} msg={} log_id={}",
                v["msg"].as_str().unwrap_or("-"),
                v["error"]["log_id"]
                    .as_str()
                    .or_else(|| v["log_id"].as_str())
                    .unwrap_or("-")
            );
        }
        Ok(v)
    }

    /// The long connection's endpoint + ping interval. The URL is
    /// short-lived and bound to this app's connection slot (`device_id` /
    /// `service_id` ride its query string).
    async fn ws_endpoint(&self) -> Result<(String, Duration)> {
        let url = format!("{}/callback/ws/endpoint", self.base);
        let resp = self
            .http
            .post(&url)
            .header("locale", "zh")
            .json(&serde_json::json!({"AppID": self.app_id, "AppSecret": self.app_secret}))
            .send()
            .await
            .context("feishu ws endpoint")?;
        let v: serde_json::Value = resp.json().await.context("feishu ws endpoint decode")?;
        let code = v["code"].as_i64().unwrap_or(0);
        anyhow::ensure!(
            code == 0,
            "feishu ws endpoint: code={code} msg={}",
            v["msg"].as_str().unwrap_or("-")
        );
        let url = v["data"]["URL"]
            .as_str()
            .context("feishu ws endpoint: no URL")?
            .to_string();
        let secs = v["data"]["ClientConfig"]["PingInterval"]
            .as_u64()
            .unwrap_or(120)
            .clamp(5, 600);
        Ok((url, Duration::from_secs(secs)))
    }

    /// One long-connection lifetime. `Ok(())` means the gateway side is
    /// gone (shutdown); every other ending is an error the caller backs
    /// off from and reconnects.
    async fn ws_session(&self, tx: &mpsc::Sender<InboundMsg>) -> Result<()> {
        let (url, ping_every) = self.ws_endpoint().await?;
        let service = service_id(&url);
        let (ws, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .context("feishu ws connect")?;
        let (mut sink, mut stream) = ws.split();
        let mut tick = tokio::time::interval(ping_every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_rx = Instant::now();
        let mut chunks: ChunkBufs = HashMap::new();
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    // Health check: every ping is answered with a pong or a
                    // data frame, so silence past two intervals means the
                    // connection is a zombie even if the socket is open.
                    anyhow::ensure!(
                        last_rx.elapsed() < ping_every * 2,
                        "feishu ws silent for {:?}",
                        last_rx.elapsed()
                    );
                    sink.send(Message::binary(frame::ping(service)))
                        .await
                        .context("feishu ws ping")?;
                }
                msg = stream.next() => {
                    let Some(msg) = msg else {
                        anyhow::bail!("feishu ws closed");
                    };
                    let msg = msg.context("feishu ws read")?;
                    last_rx = Instant::now();
                    match msg {
                        Message::Binary(raw) => {
                            let Some(f) = frame::Frame::decode(&raw) else {
                                tracing::warn!("feishu ws: undecodable frame");
                                continue;
                            };
                            if f.method != frame::METHOD_DATA || f.header("type") != Some("event") {
                                continue; // ping/pong and non-event frames
                            }
                            let sum = f.header("sum").and_then(|s| s.parse().ok()).unwrap_or(1);
                            let seq = f.header("seq").and_then(|s| s.parse().ok()).unwrap_or(0);
                            let message_id = f.header("message_id").unwrap_or("").to_string();
                            let Some(payload) =
                                merge_chunk(&mut chunks, &message_id, sum, seq, &f.payload)
                            else {
                                continue; // chunked event, still incomplete
                            };
                            sink.send(Message::binary(frame::ack(&f, 0)))
                                .await
                                .context("feishu ws ack")?;
                            let event: serde_json::Value = match serde_json::from_slice(&payload) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("feishu ws: bad event payload: {e}");
                                    continue;
                                }
                            };
                            let Some(inbound) = extract_dm(&event, &self.app_id) else {
                                continue; // not a DM we carry
                            };
                            if tx.send(inbound).await.is_err() {
                                return Ok(());
                            }
                        }
                        Message::Close(_) => anyhow::bail!("feishu ws closed by peer"),
                        Message::Ping(p) => sink.send(Message::Pong(p)).await?,
                        _ => {}
                    }
                }
            }
        }
    }
}

/// `service_id` off the endpoint URL — the `service` field of every frame.
fn service_id(url: &str) -> i32 {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "service_id")
                .and_then(|(_, v)| v.parse().ok())
        })
        .unwrap_or(0)
}

/// Fold one data frame into its event's chunk buffer. Unchunked frames
/// (`sum <= 1`) pass straight through; a chunked event yields its joined
/// payload only once every slot has arrived. A `sum` past `MAX_CHUNK_SUM` is
/// refused before it can size an allocation.
fn merge_chunk(
    bufs: &mut ChunkBufs,
    message_id: &str,
    sum: usize,
    seq: usize,
    payload: &[u8],
) -> Option<Vec<u8>> {
    if sum <= 1 {
        return Some(payload.to_vec());
    }
    if message_id.is_empty() || seq >= sum || sum > MAX_CHUNK_SUM {
        return None;
    }
    bufs.retain(|_, (seen, _)| seen.elapsed() < CHUNK_TTL);
    {
        let entry = bufs
            .entry(message_id.to_string())
            .or_insert_with(|| (Instant::now(), vec![None; sum]));
        if entry.1.len() != sum {
            entry.1 = vec![None; sum];
        }
        entry.1[seq] = Some(payload.to_vec());
        if entry.1.iter().any(Option::is_none) {
            return None;
        }
    }
    let (_, slots) = bufs.remove(message_id)?;
    Some(slots.into_iter().flatten().flatten().collect())
}

/// Map one `im.message.receive_v1` envelope to an inbound DM. Anything
/// that is not a p2p text/post message — group traffic, media, another
/// app's message, our own echo, a different event — resolves to `None`.
fn extract_dm(payload: &serde_json::Value, app_id: &str) -> Option<InboundMsg> {
    if payload["header"]["event_type"].as_str() != Some(EVENT_MESSAGE) {
        return None;
    }
    let event = &payload["event"];
    let sender = &event["sender"];
    let message = &event["message"];
    if is_self_echo(sender, app_id) {
        return None;
    }
    if message["chat_type"].as_str() != Some("p2p") {
        return None;
    }
    let text = match message["message_type"].as_str()? {
        "text" => text_message(&message["content"])?,
        "post" => post_message(&message["content"])?,
        _ => return None,
    };
    let open_id = sender["sender_id"]["open_id"].as_str()?;
    let chat_id = message["chat_id"].as_str()?;
    if open_id.is_empty() || chat_id.is_empty() {
        return None;
    }
    Some(InboundMsg {
        source: ImSource {
            channel: "feishu".into(),
            // Reply target: im.message.create with receive_id_type=chat_id.
            chat_id: chat_id.to_string(),
            // Peer identity for authz and session attribution.
            sender_id: open_id.to_string(),
            // The contact API needs an extra tenant permission; the
            // reference falls back to the principal id when it fails, so
            // the id itself is what a reader gets here.
            sender_name: open_id.to_string(),
        },
        text,
    })
}

/// The app's own outbound message comes back over the same subscription.
/// Only this app's echo is dropped — another bot's message is a message.
fn is_self_echo(sender: &serde_json::Value, app_id: &str) -> bool {
    matches!(sender["sender_type"].as_str(), Some("bot" | "app"))
        && sender["sender_id"]["app_id"].as_str() == Some(app_id)
}

/// `message.content` is a JSON *string* on the wire; tolerate an already
/// parsed object so fixtures and future SDK-shaped callers both work.
fn parse_content(content: &serde_json::Value) -> Option<serde_json::Value> {
    match content {
        serde_json::Value::String(s) => serde_json::from_str(s).ok(),
        v if v.is_object() => Some(v.clone()),
        _ => None,
    }
}

/// A `text` message's body — empty text is not a message.
fn text_message(content: &serde_json::Value) -> Option<String> {
    let text = parse_content(content)?["text"].as_str()?.to_string();
    (!text.trim().is_empty()).then_some(text)
}

/// A `post` message's text. The reference renders one line per paragraph
/// and folds mention/image/media elements in; media elements are skipped
/// here because attachments are out of scope for this gateway.
fn post_message(content: &serde_json::Value) -> Option<String> {
    let content = parse_content(content)?;
    let locale = content
        .get("zh_cn")
        .or_else(|| content.get("en_us"))
        .or_else(|| {
            content
                .as_object()?
                .values()
                .find(|v| v.get("content").is_some_and(|c| c.is_array()))
        })?;
    let mut lines = Vec::new();
    for paragraph in locale["content"].as_array()? {
        let Some(items) = paragraph.as_array() else {
            continue;
        };
        let mut line = String::new();
        for item in items {
            match item["tag"].as_str().unwrap_or("") {
                "text" | "md" => line.push_str(item["text"].as_str().unwrap_or("")),
                "a" => line.push_str(
                    item["text"]
                        .as_str()
                        .or_else(|| item["href"].as_str())
                        .unwrap_or(""),
                ),
                "at" => {
                    let name = item["user_name"]
                        .as_str()
                        .or_else(|| item["name"].as_str())
                        .or_else(|| item["user_id"].as_str())
                        .or_else(|| item["open_id"].as_str())
                        .unwrap_or("unknown");
                    line.push('@');
                    line.push_str(name);
                }
                _ => {}
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// `POST /open-apis/auth/v3/tenant_access_token/internal` — App ID +
/// App Secret for the token every REST call carries.
fn token_request(base: &str, app_id: &str, app_secret: &str) -> (String, serde_json::Value) {
    let url = format!("{base}/open-apis/auth/v3/tenant_access_token/internal");
    let body = serde_json::json!({"app_id": app_id, "app_secret": app_secret});
    (url, body)
}

/// `POST /open-apis/im/v1/messages?receive_id_type=chat_id`.
fn send_request(base: &str, chat_id: &str, text: &str) -> (String, serde_json::Value) {
    let url = format!("{base}/open-apis/im/v1/messages?receive_id_type=chat_id");
    let body = serde_json::json!({
        "receive_id": chat_id,
        "msg_type": "post",
        "content": post_content(text),
    });
    (url, body)
}

/// `PATCH /open-apis/im/v1/messages/{message_id}` — the draft refresh.
fn edit_request(base: &str, message_id: &str, text: &str) -> (String, serde_json::Value) {
    let url = format!("{base}/open-apis/im/v1/messages/{message_id}");
    let body = serde_json::json!({"msg_type": "post", "content": post_content(text)});
    (url, body)
}

/// The `post` body: `{"zh_cn":{"content":[[{"tag":"md","text":<line>}],…]}}`
/// — one paragraph per input line, exactly the reference's rendering.
fn post_content(text: &str) -> String {
    let paragraphs: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::json!([{"tag": "md", "text": line}]))
        .collect();
    serde_json::json!({"zh_cn": {"content": paragraphs}}).to_string()
}

/// Split `text` into MSG_LIMIT-safe chunks — paragraph-first, then a hard
/// cut (same discipline as `telegram.rs`, different limit).
fn chunk(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > MSG_LIMIT {
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
impl ChannelAdapter for FeishuAdapter {
    fn channel(&self) -> &'static str {
        "feishu"
    }

    async fn poll(&self, tx: mpsc::Sender<InboundMsg>) {
        loop {
            match self.ws_session(&tx).await {
                Ok(()) => return, // gateway gone — shutting down
                Err(e) => {
                    tracing::warn!("feishu ws: {e:#}");
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
            let (url, body) = send_request(self.base, chat_id, &piece);
            let res = self
                .api_call(reqwest::Method::POST, &url, Some(&body), "message create")
                .await
                .map_err(|e| SendFailure::new(delivered, total, e))?;
            if first_id.is_none() {
                first_id = res["data"]["message_id"]
                    .as_str()
                    .or_else(|| res["message_id"].as_str())
                    .map(str::to_string);
            }
        }
        Ok(first_id)
    }

    async fn edit_text(&self, _chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        let (url, body) = edit_request(self.base, message_id, text);
        self.api_call(reqwest::Method::PATCH, &url, Some(&body), "message update")
            .await?;
        Ok(())
    }

    // send_typing: Feishu bots have no typing indicator — default no-op.
}

#[cfg(test)]
mod tests;
