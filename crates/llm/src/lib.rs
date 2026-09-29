//! sunmao-llm — provider protocol adapters.
//!
//! Hand-rolled SSE framing + incremental tool_call reassembly are the point:
//! this crate is where sunmao owns the wire.

pub mod assemble;
pub mod oai;
pub mod sse;
pub mod types;

pub use oai::{ChatRequest, OaiClient};
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
