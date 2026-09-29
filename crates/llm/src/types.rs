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

/// Outgoing message. `content` is `null` when an assistant message carries
/// only tool calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
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
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Usage {
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
        Ok(Usage {
            prompt_tokens: num(&["prompt_tokens", "input_tokens"]),
            completion_tokens: num(&["completion_tokens", "output_tokens"]),
            total_tokens: num(&["total_tokens"]),
            cache_read_input_tokens: num(&["cache_read_input_tokens", "prompt_cache_hit_tokens"])
                + nested("prompt_tokens_details", "cached_tokens")
                + nested("input_tokens_details", "cached_tokens"),
            cache_creation_input_tokens: num(&[
                "cache_creation_input_tokens",
                "prompt_cache_miss_tokens",
            ]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every provider's usage dialect must land in the same Usage shape —
    /// Anthropic's cache_*_input_tokens, DeepSeek/DashScope's flat
    /// prompt_cache_*, OpenAI's nested prompt_tokens_details.
    #[test]
    fn usage_deserializes_every_dialect() {
        let anthropic: Usage = serde_json::from_str(
            r#"{"input_tokens":1000,"output_tokens":50,"cache_read_input_tokens":800,"cache_creation_input_tokens":100}"#,
        )
        .unwrap();
        assert_eq!(anthropic.prompt_tokens, 1000);
        assert_eq!(anthropic.cache_read_input_tokens, 800);
        assert_eq!(anthropic.cache_creation_input_tokens, 100);

        let deepseek: Usage = serde_json::from_str(
            r#"{"prompt_tokens":500,"completion_tokens":20,"total_tokens":520,"prompt_cache_hit_tokens":300,"prompt_cache_miss_tokens":200}"#,
        )
        .unwrap();
        assert_eq!(deepseek.cache_read_input_tokens, 300);
        assert_eq!(deepseek.cache_creation_input_tokens, 200);

        let openai: Usage = serde_json::from_str(
            r#"{"prompt_tokens":2048,"completion_tokens":64,"total_tokens":2112,"prompt_tokens_details":{"cached_tokens":1024}}"#,
        )
        .unwrap();
        assert_eq!(openai.cache_read_input_tokens, 1024);

        // a bare usage still parses — absent cache fields are zeros
        let plain: Usage =
            serde_json::from_str(r#"{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}"#)
                .unwrap();
        assert_eq!(plain.cache_read_input_tokens, 0);
    }
}
