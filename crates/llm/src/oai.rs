//! OpenAI-compatible Chat Completions dialect, `stream: true` only.
//!
//! Wire shape per SSE data line:
//!   {"choices":[{"delta":{"content"|"reasoning_content"|"tool_calls":[...]},
//!               "finish_reason":...}], "usage":{...}}
//! terminated by `data: [DONE]`.

use anyhow::{Context, bail};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;

use crate::sse::{SseEvent, SseParser};
use crate::types::{Message, Tool, Usage};
use crate::{DeltaStream, ProviderAdapter, StreamDelta, ToolCallFragment};

pub struct OaiClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

pub struct ChatRequest<'a> {
    pub messages: &'a [Message],
    pub tools: Option<&'a [Tool]>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

impl OaiClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Streaming chat completion over a live SSE byte stream.
    async fn stream_inner(&self, req: &ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        let messages = map_messages(req.messages).await;
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if let Some(tools) = req.tools {
            body["tools"] = serde_json::to_value(tools)?;
        }
        if let Some(mt) = req.max_tokens {
            body["max_tokens"] = mt.into();
        }
        if let Some(t) = req.temperature {
            body["temperature"] = t.into();
        }

        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("provider request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("provider {} {}: {}", status.as_u16(), status, text);
        }

        let byte_stream = resp.bytes_stream();
        let s = parse_stream(byte_stream);
        Ok(Box::pin(s))
    }
}

/// Message → wire JSON. Pure text stays a bare string (the shape every
/// OAI-compatible server accepts); a message carrying image blocks becomes
/// the parts array (`text` + `image_url` data-url parts). Images are read
/// off disk here — the async is the file read, not anything provider-bound.
async fn map_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    use crate::types::ResolvedBlock;
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        let role = serde_json::to_value(&m.role).unwrap_or_default();
        let mut v = serde_json::json!({ "role": role });
        if let Some(blocks) = &m.content {
            let has_image = blocks
                .iter()
                .any(|b| matches!(b, crate::types::Content::Image { .. }));
            if !has_image {
                v["content"] = serde_json::json!(m.content_text().unwrap_or_default());
            } else {
                let mut parts = Vec::with_capacity(blocks.len());
                for b in blocks {
                    match b.resolve().await {
                        ResolvedBlock::Text(text) => {
                            parts.push(serde_json::json!({"type": "text", "text": text}));
                        }
                        ResolvedBlock::Image { mime, data } => {
                            parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {"url": format!("data:{mime};base64,{data}")},
                            }));
                        }
                    }
                }
                v["content"] = serde_json::Value::Array(parts);
            }
        }
        if let Some(calls) = &m.tool_calls {
            v["tool_calls"] = serde_json::to_value(calls).unwrap_or_default();
        }
        if let Some(id) = &m.tool_call_id {
            v["tool_call_id"] = serde_json::json!(id);
        }
        out.push(v);
    }
    out
}

/// Retryable provider failures: transport errors (connect/TLS/timeout) and
/// 429/5xx status. Errors once deltas have flowed are fatal — we can't replay.
pub(crate) fn retryable(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("provider request failed")
        || msg.starts_with("provider 429")
        || (msg.starts_with("provider 5") && msg.len() > "provider 5xx".len())
}

#[async_trait::async_trait]
impl ProviderAdapter for OaiClient {
    async fn stream(&self, req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        let mut last_err = None;
        for attempt in 0..3 {
            match self.stream_inner(&req).await {
                Ok(s) => return Ok(s),
                Err(e) if retryable(&e) && attempt < 2 => {
                    tracing::warn!(
                        "provider request failed (attempt {}): {e:#}; retrying",
                        attempt + 1
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(300 * (1 << attempt)))
                        .await;
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap())
    }
}

/// Fold the SSE byte stream into normalized [`StreamDelta`]s.
pub fn parse_stream(
    bytes: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = anyhow::Result<StreamDelta>> + Send {
    async_stream(bytes).boxed()
}

fn async_stream(
    bytes: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = anyhow::Result<StreamDelta>> + Send {
    let bytes = Box::pin(bytes);
    futures_util::stream::unfold(
        (bytes, SseParser::new(), Vec::new().into_iter()),
        |(mut bytes, mut parser, mut pending)| async move {
            loop {
                if let Some(d) = pending.next() {
                    return Some((d, (bytes, parser, pending)));
                }
                let chunk = match bytes.next().await {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => return Some((Err(e.into()), (bytes, parser, pending))),
                    None => return None,
                };
                let mut deltas = Vec::new();
                for ev in parser.feed(&chunk) {
                    match ev {
                        SseEvent::Comment => continue,
                        SseEvent::Message { data } | SseEvent::Event { data, .. } => {
                            if data.trim() == "[DONE]" {
                                continue;
                            }
                            match serde_json::from_str::<Chunk>(&data) {
                                Ok(chunk) => deltas.extend(chunk.into_deltas()),
                                Err(e) => {
                                    deltas.push(Err(anyhow::anyhow!("bad chunk: {e}: {data}")))
                                }
                            }
                        }
                    }
                }
                pending = deltas.into_iter();
            }
        },
    )
}

// ---- wire deserialization (chat.completion.chunk) ----

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Deserialize)]
struct ChunkToolCall {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChunkFunction>,
}

#[derive(Deserialize)]
struct ChunkFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

impl Chunk {
    fn into_deltas(self) -> Vec<anyhow::Result<StreamDelta>> {
        let mut out = Vec::new();
        for choice in self.choices {
            if let Some(c) = choice.delta.content
                && !c.is_empty()
            {
                out.push(Ok(StreamDelta::Content(c)));
            }
            if let Some(r) = choice.delta.reasoning_content
                && !r.is_empty()
            {
                out.push(Ok(StreamDelta::Reasoning(r)));
            }
            if let Some(calls) = choice.delta.tool_calls {
                let frags = calls
                    .into_iter()
                    .map(|c| ToolCallFragment {
                        index: c.index,
                        id: c.id,
                        name: c.function.as_ref().and_then(|f| f.name.clone()),
                        arguments: c.function.and_then(|f| f.arguments),
                    })
                    .collect();
                out.push(Ok(StreamDelta::ToolCalls(frags)));
            }
            if let Some(reason) = choice.finish_reason {
                out.push(Ok(StreamDelta::Finish {
                    reason: Some(reason),
                    usage: self.usage.clone(),
                }));
            }
        }
        // some providers send usage on the [DONE]-adjacent empty-choices chunk
        if out.is_empty()
            && let Some(usage) = self.usage
        {
            out.push(Ok(StreamDelta::Finish {
                reason: None,
                usage: Some(usage),
            }));
        }
        out
    }
}
