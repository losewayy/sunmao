//! Wire types for chat-completions dialects. Kept deliberately flat: the
//! struct mirrors the JSON shape rather than a nicer internal model.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Arguments are a *string containing JSON* on the wire.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

/// One piece of a message's content — the multimodal shape providers map
/// onto their own dialect (`image_url` parts for OpenAI, `image` blocks for
/// Anthropic). `Image` carries the *path*, not the bytes: the session log
/// stays text-sized, and resolution to base64 happens once, at request
/// assembly (`resolve`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    /// An attached image — `path` is absolute (attachments land under
    /// `<project>/.sunmao/attachments/`). `mime` is guessed from the
    /// extension at construction; providers may re-derive it.
    Image {
        path: String,
        mime: String,
    },
}

/// Extensions a prompt can attach as image blocks — the mention parser
/// (`@file.png`) and the serve `/attachments` upload share this allowlist.
pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// Extension → MIME for image attachments. Unknown extensions return
/// `None` — callers decide whether the file qualifies as an image at all.
pub fn image_mime(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

impl Content {
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text { text: s.into() }
    }

    /// An image block for `path` — `None` when the extension isn't a known
    /// image type (callers attach those as a text path instead).
    pub fn image(path: impl Into<String>) -> Option<Self> {
        let path = path.into();
        Some(Content::Image {
            mime: image_mime(std::path::Path::new(&path))?.to_string(),
            path,
        })
    }
}

/// A content block resolved for the wire — `Image` has been read off disk
/// and base64'd (or degraded to a text marker when the file is gone).
#[derive(Debug)]
pub enum ResolvedBlock {
    Text(String),
    /// (mime, base64 data)
    Image {
        mime: String,
        data: String,
    },
}

impl Content {
    /// Read the block into wire-ready form. A missing/unreadable image
    /// degrades to a text marker — the turn still runs and the model sees
    /// *that* an attachment was dropped, rather than the request failing.
    pub async fn resolve(&self) -> ResolvedBlock {
        match self {
            Content::Text { text } => ResolvedBlock::Text(text.clone()),
            Content::Image { path, mime } => match tokio::fs::read(path).await {
                Ok(bytes) => {
                    use base64::Engine;
                    ResolvedBlock::Image {
                        mime: mime.clone(),
                        data: base64::engine::general_purpose::STANDARD.encode(bytes),
                    }
                }
                Err(_) => ResolvedBlock::Text(format!("[missing image: {path}]")),
            },
        }
    }
}

/// `content` on the wire may be a bare string (every log written before
/// content blocks existed, plus any OAI-shaped producer) or a block array —
/// this fold accepts both so old logs keep replaying.
fn de_content<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<Content>>, D::Error> {
    match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(vec![Content::Text { text: s }])),
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// Outgoing message. `content` is `None` when an assistant message carries
/// only tool calls; otherwise a block list (a lone text block serializes
/// as a one-element array — the array shape is canonical on write, strings
/// stay readable on the way in).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(
        default,
        deserialize_with = "de_content",
        skip_serializing_if = "Option::is_none"
    )]
    pub content: Option<Vec<Content>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(vec![Content::text(content)]),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(vec![Content::text(content)]),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// A user turn carrying more than text — attachments ride as their own
    /// blocks after the prompt text. `extra` blocks (non-image attachments)
    /// arrive already shaped by the caller.
    pub fn user_blocks(text: impl Into<String>, mut blocks: Vec<Content>) -> Self {
        blocks.insert(0, Content::text(text));
        Self {
            role: Role::User,
            content: Some(blocks),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.map(|c| vec![Content::text(c)]),
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(vec![Content::text(content)]),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    /// The message's text surface — Text blocks joined on newlines, image
    /// blocks rendered as `[image: path]` placeholders. `None` only when
    /// there is no content at all. Every consumer that printed or searched
    /// the old `Option<String>` should read through this.
    pub fn content_text(&self) -> Option<String> {
        let parts: Vec<String> = self
            .content
            .as_ref()?
            .iter()
            .map(|c| match c {
                Content::Text { text } => text.clone(),
                Content::Image { path, .. } => format!("[image: {path}]"),
            })
            .collect();
        Some(parts.join("\n"))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: &'static str, // "function"
    pub function: FunctionDecl,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDecl {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl Tool {
    pub fn function(name: &str, description: &str, parameters: serde_json::Value) -> Self {
        Self {
            kind: "function",
            function: FunctionDecl {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// Token accounting for one request. Providers disagree on where cache
/// counters live (flat `prompt_cache_hit_tokens`, nested
/// `prompt_tokens_details.cached_tokens`, Anthropic's
/// `cache_*_input_tokens`) — the custom Deserialize normalizes them all
/// into the same two fields so frontends and the session log see one shape.
/// Invariant after normalization: `prompt_tokens` is the *total input*
/// (OpenAI's field already includes cached tokens; Anthropic's
/// `input_tokens` excludes them, so cached+creation are folded in) — the
/// cache-hit dial can simply divide `cache_read / prompt_tokens` on every
/// dialect.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Usage {
    /// total input tokens — cached reads included
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// tokens served from the provider's prompt cache (the saving)
    pub cache_read_input_tokens: u64,
    /// tokens written into the cache this request (the cost)
    pub cache_creation_input_tokens: u64,
}

impl<'de> Deserialize<'de> for Usage {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        let num = |keys: &[&str]| -> u64 {
            for k in keys {
                if let Some(n) = v.get(*k).and_then(|x| x.as_u64()) {
                    return n;
                }
            }
            0
        };
        let nested = |outer: &str, key: &str| -> u64 {
            v.get(outer)
                .and_then(|o| o.get(key))
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
        };
        let cache_read = num(&["cache_read_input_tokens", "prompt_cache_hit_tokens"])
            + nested("prompt_tokens_details", "cached_tokens")
            + nested("input_tokens_details", "cached_tokens");
        let cache_write = num(&["cache_creation_input_tokens", "prompt_cache_miss_tokens"]);
        // normalize to one shape: `prompt_tokens` is the TOTAL input,
        // cached reads included. Anthropic reports input_tokens *without*
        // the cached half; OpenAI (chat + Responses) reports prompt_tokens /
        // input_tokens *with* it — folding cache_read+cache_creation into
        // the former aligns both so the hit ratio is cache_read /
        // prompt_tokens on every dialect. Anthropic is told apart by its
        // flat cache_* fields: Responses uses `input_tokens_details` instead
        // and must not be double-folded.
        let anthropic_flat = v.get("input_tokens").is_some()
            && (v.get("cache_read_input_tokens").is_some()
                || v.get("cache_creation_input_tokens").is_some());
        let prompt = num(&["prompt_tokens", "input_tokens"])
            + if anthropic_flat {
                cache_read + cache_write
            } else {
                0
            };
        Ok(Usage {
            prompt_tokens: prompt,
            completion_tokens: num(&["completion_tokens", "output_tokens"]),
            total_tokens: num(&["total_tokens"]),
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_write,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every provider's usage dialect must land in the same Usage shape —
    /// Anthropic's cache_*_input_tokens, DeepSeek/DashScope's flat
    /// prompt_cache_*, OpenAI's nested prompt_tokens_details — with the one
    /// invariant `prompt_tokens` = total input including the cached share.
    #[test]
    fn usage_deserializes_every_dialect() {
        let anthropic: Usage = serde_json::from_str(
            r#"{"input_tokens":1000,"output_tokens":50,"cache_read_input_tokens":800,"cache_creation_input_tokens":100}"#,
        )
        .unwrap();
        // Anthropic's input_tokens excludes the cached 900 — normalized to
        // the full input so hit% = cache_read/prompt_tokens on every dialect
        assert_eq!(anthropic.prompt_tokens, 1900);
        assert_eq!(anthropic.cache_read_input_tokens, 800);
        assert_eq!(anthropic.cache_creation_input_tokens, 100);

        let deepseek: Usage = serde_json::from_str(
            r#"{"prompt_tokens":500,"completion_tokens":20,"total_tokens":520,"prompt_cache_hit_tokens":300,"prompt_cache_miss_tokens":200}"#,
        )
        .unwrap();
        // DeepSeek's prompt_tokens already includes the cached share — no
        // adjustment; hit% = 300/500, not the old 300/700
        assert_eq!(deepseek.prompt_tokens, 500);
        assert_eq!(deepseek.cache_read_input_tokens, 300);
        assert_eq!(deepseek.cache_creation_input_tokens, 200);

        let openai: Usage = serde_json::from_str(
            r#"{"prompt_tokens":2048,"completion_tokens":64,"total_tokens":2112,"prompt_tokens_details":{"cached_tokens":1024}}"#,
        )
        .unwrap();
        assert_eq!(openai.prompt_tokens, 2048);
        assert_eq!(openai.cache_read_input_tokens, 1024);

        // a bare usage still parses — absent cache fields are zeros
        let plain: Usage =
            serde_json::from_str(r#"{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}"#)
                .unwrap();
        assert_eq!(plain.cache_read_input_tokens, 0);
    }
}
