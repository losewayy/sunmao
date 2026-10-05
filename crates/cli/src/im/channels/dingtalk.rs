//! DingTalk adapter — the enterprise internal bot's Stream mode. The
//! connection is *reverse*: `POST /gateway/connections/open` hands back a
//! one-shot `endpoint` + `ticket`, the bot dials that WebSocket, and the
//! platform pushes callbacks down it. Every callback is answered with an ACK
//! envelope; replies ride REST (`/robot/oToMessages/batchSend`).
//!
//! Non-goals here: group traffic (`conversationType != "1"` is dropped —
//! routing and authz are DM-only, see `authz.rs`), media, message edits
//! (robot messages have no edit API, so the progress draft is re-posted),
//! and the custom-robot webhook flow (this is the Stream connector only).

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::{ChannelAdapter, InboundMsg};
use crate::im::config::DingtalkSpec;
use crate::im::redact;

mod api;
mod protocol;

/// Refresh the access token this long before it expires.
const TOKEN_MARGIN_SECS: u64 = 60;
/// Reconnect delay — fixed, like the reference: the platform closes a
/// connection when it wants the bot to re-register, and the next
/// registration is the retry.
const RECONNECT_SECS: u64 = 5;

/// Bot client for one `{"kind":"dingtalk"}` channels.json entry. The client
/// secret and the access token live here for the process's life — never in
/// the store, a ledger row, or a log line.
pub struct DingtalkAdapter {
    http: reqwest::Client,
    api_base: String,
    corp_id: String,
    client_id: String,
    client_secret: String,
    robot_code: String,
    /// `(access_token, expires_at)` — an `std` mutex so no guard is ever
    /// held across an await.
    token: std::sync::Mutex<Option<(String, Instant)>>,
}

impl DingtalkAdapter {
    pub fn new(spec: &DingtalkSpec) -> Result<Self> {
        anyhow::ensure!(
            !spec.corp_id.trim().is_empty(),
            "dingtalk channel needs corp_id"
        );
        anyhow::ensure!(
            !spec.client_id.trim().is_empty(),
            "dingtalk channel needs client_id"
        );
        anyhow::ensure!(
            !spec.robot_code.trim().is_empty(),
            "dingtalk channel needs robot_code"
        );
        anyhow::ensure!(
            !spec.api_base_url.trim().is_empty(),
            "dingtalk channel needs api_base_url"
        );
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            api_base: spec.api_base_url.trim().to_string(),
            corp_id: spec.corp_id.trim().to_string(),
            client_id: spec.client_id.trim().to_string(),
            client_secret: spec.client_secret()?,
            robot_code: spec.robot_code.trim().to_string(),
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

    /// The corp-scoped `access_token`, refreshed a minute early.
    async fn access_token(&self) -> Result<String> {
        if let Some(token) = self.cached_token() {
            return Ok(token);
        }
        let (url, body) = api::token_request(
            &self.api_base,
            &self.corp_id,
            &self.client_id,
            &self.client_secret,
        );
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("dingtalk token")?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.context("dingtalk token decode")?;
        api::check_response("token", status, &v)?;
        let (token, ttl) = api::access_token(&v)?;
        *self.token.lock().unwrap() =
            Some((token.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(token)
    }

    /// One robot REST call — the token rides `x-acs-dingtalk-access-token`,
    /// and the business envelope is checked on top of the HTTP status.
    async fn robot_call(
        &self,
        url: &str,
        body: &serde_json::Value,
        stage: &str,
    ) -> Result<serde_json::Value> {
        let token = self.access_token().await?;
        let resp = self
            .http
            .post(url)
            .header("x-acs-dingtalk-access-token", token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("dingtalk {stage}"))?;
        let status = resp.status();
        let v: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("dingtalk {stage} decode"))?;
        api::check_response(stage, status, &v)?;
        Ok(v)
    }

    /// The Stream registration: the endpoint to dial and its ticket.
    async fn open_stream(&self) -> Result<(String, String)> {
        let body = protocol::open_body(&self.client_id, &self.client_secret);
        let resp = self
            .http
            .post(api::open_url(&self.api_base))
            .json(&body)
            .send()
            .await
            .context("dingtalk stream open")?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.context("dingtalk stream open decode")?;
        api::check_response("stream_open", status, &v)?;
        api::stream_registration(&v)
    }

    /// One connection lifetime. `Ok(())` means the gateway side is gone
    /// (shutdown); every other ending is an error the caller backs off from
    /// and reconnects.
    async fn ws_session(&self, tx: &mpsc::Sender<InboundMsg>) -> Result<()> {
        let (endpoint, ticket) = self.open_stream().await?;
        let url = api::append_ticket(&endpoint, &ticket)?;
        // The ticket is one-shot material and it rides the URL, so both go
        // into the mask list: a failed handshake is logged with `{e:#}`, and
        // whatever it echoes back must not be the credential.
        let secrets = [self.client_secret.as_str(), ticket.as_str(), url.as_str()];
        let (ws, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|e| redact::masked_error(e.into(), &secrets))
            .context("dingtalk ws connect")?;
        let (mut sink, mut stream) = ws.split();
        while let Some(raw) = stream.next().await {
            let text = match raw.context("dingtalk ws read")? {
                Message::Text(text) => text.as_str().to_string(),
                Message::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                    Ok(text) => text,
                    Err(_) => continue,
                },
                Message::Close(_) => anyhow::bail!("dingtalk ws closed by peer"),
                Message::Ping(p) => {
                    sink.send(Message::Pong(p)).await?;
                    continue;
                }
                _ => continue,
            };
            let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&text) else {
                tracing::warn!("dingtalk ws: undecodable frame");
                continue;
            };
            if protocol::is_disconnect(&envelope) {
                // The platform asked for this connection to end: close it the
                // polite way and let `poll` register a new one.
                sink.close().await.context("dingtalk ws close")?;
                anyhow::bail!("dingtalk stream: server asked to disconnect");
            }
            if !protocol::is_callback(&envelope) {
                continue; // handshake/other system frames
            }
            // ACK before parsing: an unanswered callback is redelivered, and
            // a payload we drop is still a payload we received.
            if let Some(id) = protocol::message_id(&envelope) {
                let ack = protocol::ack(id).to_string();
                sink.send(Message::text(ack))
                    .await
                    .context("dingtalk ws ack")?;
            }
            let Some(payload) = protocol::callback_data(&envelope) else {
                continue;
            };
            if let Some(inbound) = protocol::extract_dm(&payload, &self.robot_code)
                && tx.send(inbound).await.is_err()
            {
                return Ok(()); // gateway gone — the process is shutting down
            }
        }
        anyhow::bail!("dingtalk ws closed")
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for DingtalkAdapter {
    fn channel(&self) -> &'static str {
        "dingtalk"
    }

    async fn poll(&self, tx: mpsc::Sender<InboundMsg>) {
        loop {
            match self.ws_session(&tx).await {
                Ok(()) => return, // gateway gone — shutting down
                Err(e) => {
                    tracing::warn!("dingtalk ws: {e:#}");
                    tokio::time::sleep(Duration::from_secs(RECONNECT_SECS)).await;
                }
            }
        }
    }

    async fn send_text(&self, chat_id: &str, text: &str) -> Result<Option<String>> {
        let url = api::dm_send_url(&self.api_base);
        let pieces = protocol::chunk(text);
        let total = pieces.len();
        for (index, piece) in pieces.iter().enumerate() {
            let body = protocol::send_body(&self.robot_code, chat_id, piece, index, total);
            self.robot_call(&url, &body, "send").await?;
        }
        // batchSend answers with a process query key, not a message id, and
        // robot messages cannot be edited — there is nothing for the
        // progress lane to hold on to.
        Ok(None)
    }

    // edit_text: no edit API for enterprise robot messages — the progress
    // draft is re-posted, and the default no-op is the honest behavior.
    // send_typing: no typing indicator on this channel.
}

#[cfg(test)]
mod tests;
