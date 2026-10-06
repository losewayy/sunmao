//! sunmao-llm — provider protocol adapters.
//!
//! Hand-rolled SSE framing + incremental tool_call reassembly are the point:
//! this crate is where sunmao owns the wire.

pub mod anthropic;
pub mod assemble;
pub mod oai;
pub mod responses;
pub mod sse;
pub mod types;

pub use anthropic::AnthropicClient;
pub use oai::{ChatRequest, OaiClient};
pub use responses::ResponsesClient;
pub use sse::{SseEvent, SseParser};
pub use types::*;

use futures_util::Stream;

/// One normalized piece of a streaming model response.
/// Provider dialects all reduce to this vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    /// Visible assistant text.
    Content(String),
    /// Reasoning/thinking channel (e.g. DeepSeek `reasoning_content`).
    Reasoning(String),
    /// Incremental tool call fragments keyed by `index`.
    ToolCalls(Vec<ToolCallFragment>),
    /// Terminal event of the stream.
    Finish {
        reason: Option<String>,
        usage: Option<Usage>,
    },
}

/// A fragment of a streamed `tool_calls` entry. `arguments` arrives as an
/// arbitrary JSON string prefix/suffix stream — reassembly lives in
/// [`assemble::ToolCallAssembler`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCallFragment {
    pub index: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

pub type DeltaStream = std::pin::Pin<Box<dyn Stream<Item = anyhow::Result<StreamDelta>> + Send>>;

#[async_trait::async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn stream(&self, req: ChatRequest<'_>) -> anyhow::Result<DeltaStream>;
}

/// The base a dialect appends its own path to.
///
/// The configured `base_url` is a base — `https://api.example.com/v1` — but a
/// pasted full endpoint is the same endpoint, and appending to it would ask
/// for `/v1/chat/completions/chat/completions`. The path this dialect is about
/// to add is stripped when it is already there, so both spellings work.
pub(crate) const DIALECT_ENDPOINTS: [&str; 3] = ["/chat/completions", "/responses", "/messages"];

/// The base every dialect and every catalog fetch should start from.
///
/// The configured `base_url` is used verbatim — a version segment (`/v1`,
/// `/v2`), a gateway prefix (`/openai/v1`), or nothing at all are all the
/// user's call, and guessing one would break the others. The only rewrite is
/// removing an endpoint path the user already wrote out: pasting
/// `https://host/v1/chat/completions` must not ask for
/// `/v1/chat/completions/chat/completions`.
pub fn canonical_base(base: &str) -> String {
    let mut b = base.trim_end_matches('/');
    for e in DIALECT_ENDPOINTS {
        if let Some(stripped) = b.strip_suffix(e) {
            b = stripped.trim_end_matches('/');
        }
    }
    b.to_string()
}

#[cfg(test)]
mod endpoint_base_tests {

    #[test]
    fn the_version_segment_is_the_users_call() {
        use crate::canonical_base;
        assert_eq!(
            canonical_base("https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            canonical_base("https://host:4000/v2"),
            "https://host:4000/v2"
        );
        assert_eq!(
            canonical_base("https://api.deepseek.com"),
            "https://api.deepseek.com"
        );
        assert_eq!(
            canonical_base("http://host:4000/openai/v1/"),
            "http://host:4000/openai/v1"
        );
    }

    #[test]
    fn any_pasted_endpoint_is_stripped_once() {
        use crate::canonical_base;
        assert_eq!(
            canonical_base("https://x/v1/chat/completions"),
            "https://x/v1"
        );
        assert_eq!(canonical_base("https://x/v1/responses/"), "https://x/v1");
        assert_eq!(canonical_base("https://x/v1/messages"), "https://x/v1");
    }

    #[test]
    fn pasted_endpoint_tolerates_extra_trailing_slashes() {
        use crate::canonical_base;
        assert_eq!(
            canonical_base("https://x/v1/chat/completions/"),
            "https://x/v1"
        );
        assert_eq!(canonical_base("https://x/v1/responses///"), "https://x/v1");
        assert_eq!(
            canonical_base("https://api.openai.com/v1/"),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn empty_and_root_base_are_preserved() {
        use crate::canonical_base;
        assert_eq!(canonical_base(""), "");
        assert_eq!(canonical_base("/"), "");
        assert_eq!(canonical_base("///"), "");
    }

    #[test]
    fn canonical_base_is_idempotent() {
        use crate::canonical_base;
        let inputs = [
            "https://x/v1",
            "https://x/v1/chat/completions",
            "http://host:4000/openai/v1/",
        ];
        for input in inputs {
            let once = canonical_base(input);
            assert_eq!(canonical_base(&once), once);
        }
    }
}
