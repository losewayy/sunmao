use super::*;
use crate::session::SessionLog;
use crate::tool::builtin_registry;
use futures_util::stream;
use sunmao_llm::types::Usage;
use sunmao_llm::{ChatRequest, DeltaStream, ProviderAdapter, StreamDelta, ToolCallFragment};

mod approvals;
mod hooks;
mod models;
mod subagents;
mod turns;

/// Scripted provider: each queued response is a Vec of deltas replayed in
/// order. The seam being a trait is what makes the whole loop testable.
struct MockProvider {
    responses: std::sync::Mutex<std::collections::VecDeque<Vec<StreamDelta>>>,
    /// how many times stream() was invoked
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ProviderAdapter for MockProvider {
    async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let deltas = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                vec![
                    StreamDelta::Content("done".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: Some(Usage::default()),
                    },
                ]
            });
        Ok(Box::pin(stream::iter(deltas.into_iter().map(Ok))))
    }
}

struct NullObserver;
impl Observer for NullObserver {
    fn on_event(&self, _ev: &LiveEvent) {}
}

/// Records every LiveEvent — used to assert TurnEnd fires on the error
/// path (frontends unwind busy/spinner state from it; a missing TurnEnd
/// leaves the TUI stuck).
struct RecObserver(std::sync::Mutex<Vec<String>>);
impl Observer for RecObserver {
    fn on_event(&self, ev: &LiveEvent) {
        let tag = match ev {
            LiveEvent::TurnEnd { outcome } => format!("TurnEnd:{outcome:?}"),
            LiveEvent::Content(_) => "Content".into(),
            LiveEvent::Reasoning(_) => "Reasoning".into(),
            LiveEvent::ToolStart { .. } => "ToolStart".into(),
            LiveEvent::ToolDone { .. } => "ToolDone".into(),
            LiveEvent::Hook { .. } => "Hook".into(),
            LiveEvent::Usage(_) => "Usage".into(),
        };
        self.0.lock().unwrap().push(tag);
    }
}
