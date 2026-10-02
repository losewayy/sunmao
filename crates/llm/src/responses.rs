//! OpenAI Responses API dialect — `POST /responses`, `stream: true` only.
//!
//! The Responses API is OpenAI's stateful successor to Chat Completions
//! (GPT-5 / o-series; reasoning effort lives only on this surface). Wire
//! differences that matter here:
//!
//!   * system text travels as top-level `instructions`, not a message;
//!   * history is an `input` array of typed *items*: `message` (user/
//!     assistant), `function_call`, `function_call_output`;
//!   * tool defs flatten (`{"type":"function","name":...}` — no wrapper);
//!   * tool calls pair on `call_id` — we echo back whatever the stream
//!     gave us, `item.id` (`fc_…`) is server bookkeeping and stays unused;
//!   * streaming is `event:`-typed SSE (`response.output_text.delta`,
//!     `response.function_call_arguments.delta`, `response.completed`, …);
//!   * `previous_response_id` + `store` turn the API stateful: a request
//!     may send only the *new* items while the server replays the prefix.
//!
//! ## Chain caching (the "cache optimization")
//!
//! Two complementary mechanisms, both provider-side and free to use:
//!
//!   * `prompt_cache_key` — routes this adapter's requests into the same
//!     server prompt cache; a stable per-(base_url, model) key. Works even
//!     when the response chain is cold.
//!   * `previous_response_id` — after a completed response we remember the
//!     response id *and the exact item list we sent*. The next request
//!     sends only appended items when the previous send is a strict prefix
//!     of the new list. Any divergence — compaction, rewind, session
//!     switch (the adapter is shared via `ModelResolver`'s cache), a
//!     response that never completed — fails the prefix check or drops
//!     `prev_id`, and we fall back to full input. A rejected `prev_id`
//!     (stale/expired server-side) retries once full.
//!
//! Correctness never depends on the server: the chain is an optimization
//! layer, and every invalidation path collapses to "send everything".

use std::sync::{Arc, Mutex};

use anyhow::Context;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

use crate::oai::ChatRequest;
use crate::sse::{SseEvent, SseParser};
use crate::types::{Message, Role, Usage};
use crate::{DeltaStream, ProviderAdapter, StreamDelta, ToolCallFragment};

pub struct ResponsesClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    /// Server-side continuation chain — see module docs. Arc'd so the
    /// stream's commit step can outlive `stream()`'s `&self` borrow.
    chain: Arc<Mutex<Chain>>,
    cache_key: String,
}

/// State for `previous_response_id` continuation. `sent` is the *full*
/// item list the chain logically stands on — not just the last tail — so
/// the prefix check always compares against ground truth.
#[derive(Default)]
struct Chain {
    prev_id: Option<String>,
    sent: Vec<Value>,
}

/// What the next request actually sends.
struct Split {
    /// Wire `input` — the tail when chained, everything otherwise.
    input: Vec<Value>,
    /// Present only when `input` is a tail riding on a stored response.
    prev_id: Option<String>,
}

impl ResponsesClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let model = model.into();
        Self {
            // same socket policy as the other dialects — see OaiClient.
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .tcp_keepalive(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            cache_key: cache_key_for(&base_url, &model),
            base_url,
            api_key: api_key.into(),
            model,
            chain: Arc::new(Mutex::new(Chain::default())),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// One request establishment, with the same retry policy as OAI:
    /// 3 attempts, 300/600ms backoff, transport + 429/5xx only.
    async fn send(
        &self,
        req: &ChatRequest<'_>,
        instructions: &str,
        split: &Split,
    ) -> anyhow::Result<impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static>
    {
        let mut body = json!({
            "model": self.model,
            "input": split.input,
            "stream": true,
            "store": true,
            "prompt_cache_key": self.cache_key,
        });
        if !instructions.is_empty() {
            body["instructions"] = json!(instructions);
        }
        if let Some(prev) = &split.prev_id {
            body["previous_response_id"] = json!(prev);
        }
        if let Some(tools) = req.tools {
            // Responses flattens tool declarations: name/description/
            // parameters live at the item's top level — same JSON
            // verbatim, one level shallower than chat-completions.
            let flat: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.function.name,
                        "description": t.function.description,
                        "parameters": t.function.parameters,
                    })
                })
                .collect();
            body["tools"] = json!(flat);
        }
        if let Some(mt) = req.max_tokens {
            // Responses rejects max_output_tokens < 16.
            body["max_output_tokens"] = mt.max(16).into();
        }
        if let Some(t) = req.temperature {
            body["temperature"] = t.into();
        }

        let mut last_err = None;
        for attempt in 0..3 {
            match self
                .http
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => return Ok(resp.bytes_stream()),
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    let e = anyhow::anyhow!("provider {} {}: {}", status.as_u16(), status, text);
                    if retryable(&e) && attempt < 2 {
                        tracing::warn!(
                            "provider request failed (attempt {}): {e:#}; retrying",
                            attempt + 1
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(300 * (1 << attempt)))
                            .await;
                        last_err = Some(e);
                    } else {
                        return Err(e);
                    }
                }
                Err(e) => {
                    let e = anyhow::Error::new(e).context("provider request failed");
                    if attempt < 2 {
                        tracing::warn!(
                            "provider request failed (attempt {}): {e:#}; retrying",
                            attempt + 1
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(300 * (1 << attempt)))
                            .await;
                        last_err = Some(e);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        Err(last_err.unwrap())
    }
}

/// Retryable establishment failures mirror `oai::retryable`.
fn retryable(e: &anyhow::Error) -> bool {
    crate::oai::retryable(e)
}

/// Decide the incremental send against the remembered chain. A chained
/// send requires BOTH: a stored `prev_id` and our previously sent items
/// forming a strict prefix of the new list. Anything else is a full send
/// that re-establishes the chain from scratch.
fn split_input(chain: &Chain, items: &[Value]) -> Split {
    if let Some(prev) = &chain.prev_id
        && items.len() > chain.sent.len()
        && items[..chain.sent.len()] == chain.sent[..]
    {
        return Split {
            input: items[chain.sent.len()..].to_vec(),
            prev_id: Some(prev.clone()),
        };
    }
    Split {
        input: items.to_vec(),
        prev_id: None,
    }
}

/// Short stable cache key for this adapter — the provider hashes it
/// anyway; all we need is same-client-same-key across requests.
fn cache_key_for(base_url: &str, model: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    base_url.hash(&mut h);
    model.hash(&mut h);
    format!("sunmao-{:016x}", h.finish())
}

/// Rejected `previous_response_id` → the one case where a full-input
/// retry is semantics-preserving (nothing reached the model yet).
fn stale_chain(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("previous_response_id") || msg.contains("previous response")
}

#[async_trait::async_trait]
impl ProviderAdapter for ResponsesClient {
    async fn stream(&self, req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        // Resolve items once — `send` and the chain record need the same
        // serialization, and image blocks hit disk (don't read twice).
        let (instructions, items) = map_items(req.messages).await;
        let split = {
            let mut chain = self.chain.lock().unwrap_or_else(|p| p.into_inner());
            let split = split_input(&chain, &items);
            if split.prev_id.is_none() {
                // a full send abandons whatever chain existed — a new one
                // only stands if this request completes.
                chain.prev_id = None;
                chain.sent.clear();
            }
            split
        };
        match self.send(&req, &instructions, &split).await {
            Ok(bytes) => Ok(Box::pin(parse_stream(
                bytes,
                Some((self.chain.clone(), items)),
            ))),
            Err(e) if split.prev_id.is_some() && stale_chain(&e) => {
                tracing::warn!("previous_response_id rejected; resending full input: {e:#}");
                let split = Split {
                    input: items.clone(),
                    prev_id: None,
                };
                let bytes = self.send(&req, &instructions, &split).await?;
                Ok(Box::pin(parse_stream(
                    bytes,
                    Some((self.chain.clone(), items)),
                )))
            }
            Err(e) => Err(e),
        }
    }
}

// ---- message → items mapping ----------------------------------------------

/// `Message` list → `(instructions, items)`. System text collects into
/// `instructions` (top-level field, not an item); everything else becomes
/// typed input items in order.
async fn map_items(messages: &[Message]) -> (String, Vec<Value>) {
    use crate::types::ResolvedBlock;
    let mut instructions = String::new();
    let mut items = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            Role::System => {
                if let Some(text) = m.content_text() {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(&text);
                }
            }
            Role::User => {
                let mut parts = Vec::new();
                for b in m.content.iter().flatten() {
                    match b.resolve().await {
                        ResolvedBlock::Text(text) => {
                            parts.push(json!({"type": "input_text", "text": text}));
                        }
                        ResolvedBlock::Image { mime, data } => {
                            parts.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{mime};base64,{data}"),
                            }));
                        }
                    }
                }
                items.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": parts,
                }));
            }
            Role::Assistant => {
                // An assistant turn decomposes: optional output message,
                // then one `function_call` item per tool call (order
                // preserved — the server replays this shape on `prev_id`).
                if let Some(text) = m.content_text()
                    && !text.is_empty()
                {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for call in m.tool_calls.iter().flatten() {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.function.name,
                        "arguments": call.function.arguments,
                    }));
                }
            }
            Role::Tool => {
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": m.tool_call_id.clone().unwrap_or_default(),
                    "output": m.content_text().unwrap_or_default(),
                }));
            }
        }
    }
    (instructions, items)
}

// ---- SSE event → StreamDelta ----------------------------------------------

/// Fold the SSE byte stream into normalized [`StreamDelta`]s. `commit`,
/// when present, is the chain handle + the full logical item list this
/// request stands on — written on `response.completed` (and only then:
/// a failed/cancelled response is not a valid chain link).
fn parse_stream(
    bytes: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    commit: Option<(Arc<Mutex<Chain>>, Vec<Value>)>,
) -> impl Stream<Item = anyhow::Result<StreamDelta>> + Send {
    let bytes = Box::pin(bytes);
    futures_util::stream::unfold(
        State {
            bytes,
            parser: SseParser::new(),
            pending: Vec::new().into_iter(),
            commit,
            last_id: None,
        },
        |mut st| async move {
            loop {
                if let Some(d) = st.pending.next() {
                    return Some((d, st));
                }
                let chunk = match st.bytes.next().await {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => return Some((Err(e.into()), st)),
                    None => return None,
                };
                let mut deltas = Vec::new();
                for ev in st.parser.feed(&chunk) {
                    match ev {
                        SseEvent::Comment => continue,
                        SseEvent::Message { data } | SseEvent::Event { data, .. } => {
                            if data.trim() == "[DONE]" {
                                continue;
                            }
                            match map_event(&data, &mut st.last_id) {
                                Ok(mapped) => {
                                    for d in mapped {
                                        // chain commit rides the Finish
                                        // delta — only on a clean
                                        // `completed` terminal.
                                        if matches!(
                                            d,
                                            StreamDelta::Finish {
                                                reason: Some(ref r),
                                                ..
                                            } if r == "completed"
                                        ) && let Some((chain, items)) = st.commit.take()
                                        {
                                            let mut c =
                                                chain.lock().unwrap_or_else(|p| p.into_inner());
                                            c.sent = items;
                                            c.prev_id = st.last_id.clone();
                                        }
                                        deltas.push(Ok(d));
                                    }
                                }
                                Err(e) => deltas.push(Err(e)),
                            }
                        }
                    }
                }
                st.pending = deltas.into_iter();
            }
        },
    )
}

struct State<S> {
    bytes: S,
    parser: SseParser,
    pending: std::vec::IntoIter<anyhow::Result<StreamDelta>>,
    commit: Option<(Arc<Mutex<Chain>>, Vec<Value>)>,
    /// `response.id` seen on `response.created`/`completed` — committed to
    /// the chain only when the terminal status is `completed`.
    last_id: Option<String>,
}

/// One SSE `data:` payload → zero or more deltas. The event's `type` lives
/// inside the JSON as well as on the `event:` line — reading it from the
/// payload tolerates proxies that drop the SSE field.
fn map_event(data: &str, last_id: &mut Option<String>) -> anyhow::Result<Vec<StreamDelta>> {
    let v: Value = serde_json::from_str(data).with_context(|| format!("bad chunk: {data}"))?;
    let ty = v["type"].as_str().unwrap_or_default();
    let mut out = Vec::new();
    match ty {
        "response.output_text.delta" | "response.refusal.delta" => {
            if let Some(d) = v["delta"].as_str() {
                out.push(StreamDelta::Content(d.to_string()));
            }
        }
        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
            if let Some(d) = v["delta"].as_str() {
                out.push(StreamDelta::Reasoning(d.to_string()));
            }
        }
        "response.output_item.added" => {
            let item = &v["item"];
            if item["type"].as_str() == Some("function_call") {
                out.push(StreamDelta::ToolCalls(vec![ToolCallFragment {
                    index: v["output_index"].as_u64().unwrap_or(0) as u32,
                    // `call_id` is the pairing key — function_call_output
                    // items and server replay both reference it.
                    id: item["call_id"]
                        .as_str()
                        .or_else(|| item["id"].as_str())
                        .map(String::from),
                    name: item["name"].as_str().map(String::from),
                    arguments: None,
                }]));
            }
        }
        "response.function_call_arguments.delta" => {
            out.push(StreamDelta::ToolCalls(vec![ToolCallFragment {
                index: v["output_index"].as_u64().unwrap_or(0) as u32,
                id: None,
                name: None,
                arguments: v["delta"].as_str().map(String::from),
            }]));
        }
        "response.created" => {
            if let Some(id) = v["response"]["id"].as_str() {
                *last_id = Some(id.to_string());
            }
        }
        "response.completed" | "response.incomplete" | "response.failed" => {
            let resp = &v["response"];
            if let Some(id) = resp["id"].as_str() {
                *last_id = Some(id.to_string());
            }
            let usage: Option<Usage> = serde_json::from_value(resp["usage"].clone()).ok();
            let reason = match ty {
                "response.completed" => "completed".to_string(),
                "response.failed" => format!(
                    "failed:{}",
                    resp["error"]["message"].as_str().unwrap_or("unknown")
                ),
                _ => format!(
                    "incomplete:{}",
                    resp["incomplete_details"]["reason"]
                        .as_str()
                        .or_else(|| resp["status"].as_str())
                        .unwrap_or("unknown")
                ),
            };
            out.push(StreamDelta::Finish {
                reason: Some(reason),
                usage,
            });
        }
        "error" => {
            let msg = v["message"].as_str().unwrap_or("provider error");
            out.push(StreamDelta::Finish {
                reason: Some(format!("error:{msg}")),
                usage: None,
            });
        }
        _ => {
            // progress/annotation/item-done events carry nothing the delta
            // vocabulary needs — skipped by design, not overlooked.
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
