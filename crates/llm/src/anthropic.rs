//! Anthropic Messages API dialect — the ProviderAdapter seam's second
//! implementation, and the proof it's a real abstraction rather than an
//! OAI-shaped wrapper.
//!
//! Wire shape:
//!   POST {base}/v1/messages
//!   headers: x-api-key, anthropic-version
//!   body: {model, max_tokens, system, messages[{role, content:[blocks]}],
//!          tools:[{name, description, input_schema}], stream: true}
//!
//! SSE events: message_start / content_block_start (text|tool_use) /
//! content_block_delta (text_delta|thinking_delta|input_json_delta) /
//! message_delta (stop_reason) / message_stop.
//!
//! Mapping onto the shared vocabulary: text_delta→Content,
//! thinking_delta→Reasoning, tool_use start+deltas→ToolCallFragment
//! (index = content block index; arguments accumulate as partial_json).

use anyhow::{Context as _, bail};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::sse::{SseEvent, SseParser};
use crate::types::{Content, Message, ResolvedBlock, Role, Usage};
use crate::{DeltaStream, ProviderAdapter, StreamDelta, ToolCallFragment};

pub struct AnthropicClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    /// Anthropic requires max_tokens — callers may omit it, we can't.
    default_max_tokens: u32,
}

impl AnthropicClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            // SSE streams can outlive any whole-request timeout — bound the
            // handshake + keepalive instead so a wedged socket still dies.
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .tcp_keepalive(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url: crate::canonical_base(&base_url.into()),
            api_key: api_key.into(),
            model: model.into(),
            default_max_tokens: 8_192,
        }
    }

    /// Our `Message` uses OAI shape (flat content + tool_calls +
    /// tool_call_id); Anthropic needs content-block arrays and tool results
    /// nested inside user turns. `async` because image blocks read their
    /// bytes off disk (`Content::resolve` — a missing file degrades to a
    /// `[missing image]` text block rather than failing the request).
    async fn map_messages(&self, messages: &[Message]) -> (Option<String>, Vec<Value>) {
        let mut system = None;
        let mut out: Vec<Value> = Vec::new();
        for m in messages {
            match m.role {
                Role::System => {
                    system = Some(match (system.take(), m.content_text()) {
                        (Some(prev), Some(c)) => format!("{prev}\n\n{c}"),
                        (None, c) => c.unwrap_or_default(),
                        (p, None) => p.unwrap_or_default(),
                    });
                }
                Role::User => {
                    let mut content = Vec::new();
                    for b in m.content.iter().flatten() {
                        match b.resolve().await {
                            ResolvedBlock::Text(text) => {
                                content.push(json!({"type": "text", "text": text}));
                            }
                            ResolvedBlock::Image { mime, data } => {
                                content.push(json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": mime,
                                        "data": data,
                                    },
                                }));
                            }
                        }
                    }
                    out.push(json!({"role": "user", "content": content}));
                }
                Role::Assistant => {
                    let mut content = Vec::new();
                    for b in m.content.iter().flatten() {
                        if let Content::Text { text } = b
                            && !text.is_empty()
                        {
                            content.push(json!({"type": "text", "text": text}));
                        }
                    }
                    for tc in m.tool_calls.clone().unwrap_or_default() {
                        let input: Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or(json!({}));
                        content.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": input,
                        }));
                    }
                    if content.is_empty() {
                        content.push(json!({"type": "text", "text": ""}));
                    }
                    out.push(json!({"role": "assistant", "content": content}));
                }
                Role::Tool => {
                    // tool_result blocks fold into a user turn — and a
                    // parallel tool_calls batch emits N consecutive results,
                    // which Anthropic requires in ONE user message: merge
                    // onto the previous turn if it already carries results.
                    let block = json!({
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                        "content": m.content_text().unwrap_or_default(),
                    });
                    let merged = out.last_mut().and_then(|p| {
                        if p["role"] == "user"
                            && p["content"]
                                .as_array()
                                .map(|c| c.iter().all(|b| b["type"] == "tool_result"))
                                .unwrap_or(false)
                        {
                            p["content"].as_array_mut().map(|c| c.push(block.clone()))
                        } else {
                            None
                        }
                    });
                    if merged.is_none() {
                        out.push(json!({"role": "user", "content": [block]}));
                    }
                }
            }
        }
        // cache breakpoints on the last two messages' tail blocks: the
        // newest marks the write position for the next request, the
        // second-newest is the read anchor — its prefix is what the
        // previous request wrote. (Writes happen only at breakpoints; a
        // lone tail breakpoint would make the previous write findable
        // only via the 20-block lookback, which a fat tool-result turn
        // can blow past.)
        let len = out.len();
        for i in [len.wrapping_sub(2), len.wrapping_sub(1)] {
            if i < len
                && let Some(blocks) = out[i]
                    .get_mut("content")
                    .and_then(|c| c.as_array_mut())
                    .and_then(|a| a.last_mut())
            {
                blocks["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        (system, out)
    }

    /// The wire body for one request — assembled separately from `send` so
    /// the dialect's field spellings stay unit-testable.
    fn request_body(
        &self,
        req: &crate::oai::ChatRequest<'_>,
        system: Option<String>,
        messages: Vec<Value>,
    ) -> Value {
        let mut body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens.unwrap_or(self.default_max_tokens),
            "messages": messages,
            "stream": true,
        });
        if let Some(s) = system {
            // breakpoint 1 — system is the biggest stable prefix. Structured
            // form so cache_control lands on its block.
            body["system"] = json!([{
                "type": "text",
                "text": s,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        if let Some(tools) = req.tools {
            let mut decls: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.function.name,
                        "description": t.function.description,
                        "input_schema": t.function.parameters,
                    })
                })
                .collect();
            // breakpoint 2 — the tool list sits between system and messages;
            // marking its tail caches tools+system as one prefix. (Anthropic
            // caches ≥1024 tokens per breakpoint — small requests just don't
            // mark rather than erroring.)
            if let Some(last) = decls.last_mut() {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
            body["tools"] = Value::Array(decls);
        }
        if let Some(e) = req.reasoning_effort {
            // Anthropic spells effort `output_config.effort` (GA — adaptive
            // thinking, no beta header); the level passes through verbatim.
            body["output_config"] = json!({ "effort": e });
        }
        body
    }

    async fn stream_inner(&self, req: &crate::oai::ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        let (system, messages) = self.map_messages(req.messages).await;
        let body = self.request_body(req, system, messages);

        let resp = self
            .http
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("provider request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("provider {} {}: {}", status.as_u16(), status, text);
        }
        Ok(Box::pin(fold_events(resp.bytes_stream())))
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for AnthropicClient {
    async fn stream(&self, req: crate::oai::ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        let mut last_err = None;
        for attempt in 0..3 {
            match self.stream_inner(&req).await {
                Ok(s) => return Ok(s),
                Err(e) if crate::oai::retryable(&e) && attempt < 2 => {
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

// ---- wire deserialization ----

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Ev {
    /// input + cache counters only appear here; output lives in
    /// message_delta — the fold merges them into one Usage.
    #[serde(rename = "message_start")]
    MessageStart { message: MsgStart },
    #[serde(rename = "content_block_start")]
    BlockStart {
        index: u32,
        content_block: BlockStart,
    },
    #[serde(rename = "content_block_delta")]
    BlockDelta { index: u32, delta: BlockDelta },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MsgDelta,
        usage: Option<Usage>,
    },
    /// Anthropic reports mid-stream failures as `{"type":"error",…}` — the
    /// old `Other` catch-all swallowed it and the turn looked like a clean
    /// finish. Surface it as an error so the loop reports, not silently
    /// accepts, a truncated stream.
    #[serde(rename = "error")]
    Error { error: StreamError },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct StreamError {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct MsgStart {
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct BlockStart {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct BlockDelta {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}

#[derive(Deserialize)]
struct MsgDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

impl Ev {
    fn deltas(self) -> Vec<anyhow::Result<StreamDelta>> {
        match self {
            Ev::BlockStart {
                index,
                content_block,
            } if content_block.kind == "tool_use" => {
                vec![Ok(StreamDelta::ToolCalls(vec![ToolCallFragment {
                    index,
                    id: content_block.id,
                    name: content_block.name,
                    arguments: None,
                }]))]
            }
            Ev::BlockDelta { index, delta } => match delta.kind.as_str() {
                "text_delta" => vec![Ok(StreamDelta::Content(delta.text.unwrap_or_default()))],
                "thinking_delta" => {
                    vec![Ok(StreamDelta::Reasoning(
                        delta.thinking.unwrap_or_default(),
                    ))]
                }
                "input_json_delta" => vec![Ok(StreamDelta::ToolCalls(vec![ToolCallFragment {
                    index,
                    arguments: delta.partial_json,
                    ..Default::default()
                }]))],
                _ => vec![],
            },
            Ev::MessageDelta { delta, usage } => {
                vec![Ok(StreamDelta::Finish {
                    reason: delta.stop_reason,
                    usage,
                })]
            }
            Ev::Error { error } => vec![Err(anyhow::anyhow!(
                "anthropic stream error {}: {}",
                error.kind,
                error.message
            ))],
            _ => vec![],
        }
    }
}

fn fold_events(
    bytes: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = anyhow::Result<StreamDelta>> + Send {
    let bytes = Box::pin(bytes);
    futures_util::stream::unfold(
        (
            bytes,
            SseParser::new(),
            Vec::new().into_iter(),
            None::<Usage>,
        ),
        |(mut bytes, mut parser, mut pending, mut start_usage)| async move {
            loop {
                if let Some(d) = pending.next() {
                    return Some((d, (bytes, parser, pending, start_usage)));
                }
                let chunk = match bytes.next().await {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => {
                        return Some((Err(e.into()), (bytes, parser, pending, start_usage)));
                    }
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
                            match serde_json::from_str::<Ev>(&data) {
                                // message_start's usage carries input/cache
                                // counters — stash it; message_delta's Finish
                                // merges both halves before surfacing.
                                Ok(Ev::MessageStart { message }) => start_usage = message.usage,
                                Ok(Ev::MessageDelta { delta, mut usage }) => {
                                    if let (Some(u), Some(s)) = (&mut usage, &start_usage) {
                                        u.prompt_tokens = s.prompt_tokens;
                                        u.total_tokens = s.prompt_tokens + u.completion_tokens;
                                        u.cache_read_input_tokens = s.cache_read_input_tokens;
                                        u.cache_creation_input_tokens =
                                            s.cache_creation_input_tokens;
                                    }
                                    deltas.extend(Ev::MessageDelta { delta, usage }.deltas());
                                }
                                Ok(ev) => deltas.extend(ev.deltas()),
                                Err(e) => {
                                    deltas.push(Err(anyhow::anyhow!("bad event: {e}: {data}")))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, ToolCall};

    #[tokio::test]
    async fn maps_tool_calls_and_results_into_block_arrays() {
        let c = AnthropicClient::new("http://x", "k", "m");
        let msgs = vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant(
                Some("ok".into()),
                vec![ToolCall {
                    id: "t1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "Bash".into(),
                        arguments: "{\"command\":\"ls\"}".into(),
                    },
                }],
            ),
            Message::tool_result("t1", "file.rs"),
        ];
        let (sys, mapped) = c.map_messages(&msgs).await;
        assert_eq!(sys.as_deref(), Some("sys"));
        assert_eq!(mapped.len(), 3);
        // assistant message has tool_use block
        let content = &mapped[1]["content"];
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["name"], "Bash");
        // tool result folded into a user turn
        assert_eq!(mapped[2]["role"], "user");
        assert_eq!(mapped[2]["content"][0]["type"], "tool_result");
        assert_eq!(mapped[2]["content"][0]["tool_use_id"], "t1");
    }

    /// A parallel tool_calls batch emits N consecutive `Role::Tool` results;
    /// Anthropic requires them merged into ONE user message — separate user
    /// turns per result are a 400 on the wire.
    #[tokio::test]
    async fn parallel_tool_results_merge_into_one_user_turn() {
        let c = AnthropicClient::new("http://x", "k", "m");
        let call = |id: &str| ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "Bash".into(),
                arguments: "{}".into(),
            },
        };
        let msgs = vec![
            Message::assistant(None, vec![call("t1"), call("t2")]),
            Message::tool_result("t1", "out1"),
            Message::tool_result("t2", "out2"),
            Message::user("next"),
        ];
        let (_sys, mapped) = c.map_messages(&msgs).await;
        assert_eq!(mapped.len(), 3);
        let content = &mapped[1]["content"];
        assert_eq!(content.as_array().unwrap().len(), 2);
        assert_eq!(content[0]["tool_use_id"], "t1");
        assert_eq!(content[1]["tool_use_id"], "t2");
        // a real user turn after the batch must NOT absorb further results
        // nor be absorbed — it stays its own message
        assert_eq!(mapped[2]["content"][0]["type"], "text");
    }

    /// Prompt-cache breakpoints: the last TWO messages carry
    /// `cache_control` — the tail marks the write position, the
    /// second-to-last is the read anchor whose prefix the previous request
    /// already wrote. A lone tail breakpoint would leave the prior write
    /// findable only through the 20-block lookback, which a fat tool batch
    /// can exceed.
    #[tokio::test]
    async fn cache_breakpoints_mark_last_two_messages() {
        let c = AnthropicClient::new("http://x", "k", "m");
        let msgs = vec![
            Message::system("sys"),
            Message::user("q1"),
            Message::assistant(Some("a1".into()), vec![]),
            Message::user("q2"),
        ];
        let (_sys, mapped) = c.map_messages(&msgs).await;
        let marked: Vec<usize> = mapped
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m["content"]
                    .as_array()
                    .and_then(|a| a.last())
                    .map(|b| b["cache_control"]["type"] == "ephemeral")
                    .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect();
        assert_eq!(marked, vec![1, 2], "exactly the last two messages marked");
    }

    /// `reasoning_effort` maps to `output_config.effort` — Anthropic's
    /// effort spelling — verbatim, and stays absent when unset.
    #[tokio::test]
    async fn effort_lands_on_output_config() {
        let c = AnthropicClient::new("http://x", "k", "m");
        let msgs = [Message::user("hi")];
        let body = c.request_body(
            &crate::oai::ChatRequest {
                messages: &msgs,
                tools: None,
                max_tokens: None,
                temperature: None,
                reasoning_effort: Some("low"),
            },
            None,
            vec![],
        );
        assert_eq!(body["output_config"]["effort"], "low");
        let body = c.request_body(
            &crate::oai::ChatRequest {
                messages: &msgs,
                tools: None,
                max_tokens: None,
                temperature: None,
                reasoning_effort: None,
            },
            None,
            vec![],
        );
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn parses_streaming_events() {
        let ev: Ev = serde_json::from_str(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        )
        .unwrap();
        let deltas = ev.deltas();
        assert_eq!(deltas.len(), 1);
        let ev: Ev = serde_json::from_str(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t","name":"Bash"}}"#,
        )
        .unwrap();
        match &ev.deltas()[0].as_ref().unwrap() {
            StreamDelta::ToolCalls(f) => {
                assert_eq!(f[0].index, 1);
                assert_eq!(f[0].name.as_deref(), Some("Bash"));
            }
            other => panic!("{other:?}"),
        }
        let ev: Ev = serde_json::from_str(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
        )
        .unwrap();
        match &ev.deltas()[0].as_ref().unwrap() {
            StreamDelta::ToolCalls(f) => assert_eq!(f[0].arguments.as_deref(), Some("{\"a\":")),
            other => panic!("{other:?}"),
        }
    }
}
