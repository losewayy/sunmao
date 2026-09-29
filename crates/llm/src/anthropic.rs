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

use anyhow::{bail, Context as _};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::sse::{SseEvent, SseParser};
use crate::types::{Message, Role, Usage};
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
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            default_max_tokens: 8_192,
        }
    }

    /// Our `Message` uses OAI shape (flat content + tool_calls +
    /// tool_call_id); Anthropic needs content-block arrays and tool results
    /// nested inside user turns.
    fn map_messages(&self, messages: &[Message]) -> (Option<String>, Vec<Value>) {
        let mut system = None;
        let mut out: Vec<Value> = Vec::new();
        for m in messages {
            match m.role {
                Role::System => {
                    system = Some(match (system.take(), m.content.clone()) {
                        (Some(prev), Some(c)) => format!("{prev}\n\n{c}"),
                        (None, c) => c.unwrap_or_default(),
                        (p, None) => p.unwrap_or_default(),
                    });
                }
                Role::User => {
                    // tool_result messages fold into a user turn
                    if let Some(id) = &m.tool_call_id {
                        out.push(json!({
                            "role": "user",
                            "content": [{
                                "type": "tool_result",
                                "tool_use_id": id,
                                "content": m.content.clone().unwrap_or_default(),
                            }],
                        }));
                    } else {
                        out.push(json!({
                            "role": "user",
                            "content": [{ "type": "text", "text": m.content.clone().unwrap_or_default() }],
                        }));
                    }
                }
                Role::Assistant => {
                    let mut content = Vec::new();
                    if let Some(c) = &m.content {
                        if !c.is_empty() {
                            content.push(json!({"type": "text", "text": c}));
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
                    // OAI `tool` role — already covered by the User arm via
                    // tool_call_id, but keep an explicit path for safety
                    out.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                            "content": m.content.clone().unwrap_or_default(),
                        }],
                    }));
                }
            }
        }
        (system, out)
    }

    async fn stream_inner(&self, req: &crate::oai::ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        let (system, messages) = self.map_messages(req.messages);
        let mut body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens.unwrap_or(self.default_max_tokens),
            "messages": messages,
            "stream": true,
        });
        if let Some(s) = system {
            body["system"] = s.into();
        }
        if let Some(tools) = req.tools {
            body["tools"] = serde_json::to_value(
                tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.function.name,
                            "description": t.function.description,
                            "input_schema": t.function.parameters,
                        })
                    })
                    .collect::<Vec<_>>(),
            )?;
        }

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
    #[serde(other)]
    Other,
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
            _ => vec![],
        }
    }
}

fn fold_events(
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
                            match serde_json::from_str::<Ev>(&data) {
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

    #[test]
    fn maps_tool_calls_and_results_into_block_arrays() {
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
        let (sys, mapped) = c.map_messages(&msgs);
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
