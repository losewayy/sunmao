use super::*;
use crate::context::MutexRecover;
use crate::session::SessionLog;
use crate::tool::builtin_registry;
use futures_util::stream;
use sunmao_llm::types::Usage;
use sunmao_llm::{ChatRequest, DeltaStream, ProviderAdapter, StreamDelta, ToolCallFragment};

mod approval_modes;
mod approvals;
mod cancel;
mod compact;
mod driver;
mod effort;
mod fusion;
mod goal;
mod hooks;
mod lazy;
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

/// The `sunmao serve` ws protocol ships `LiveEvent` verbatim (GUI.md §7) —
/// a variant that can't serialize under the tagged-enum shape would arrive
/// as `null` at the browser. Lock the wire contract: every variant
/// serializes to an object carrying its snake_case `type`.
#[test]
fn live_event_wire_shape_is_stable() {
    let evs = vec![
        LiveEvent::Content { text: "hi".into() },
        LiveEvent::Reasoning { text: "th".into() },
        LiveEvent::ToolStart {
            name: "Bash".into(),
            summary: "ls".into(),
            depth: 1,
            lane: 2,
            call_id: Some("call_1".into()),
            args: serde_json::json!({"command": "ls"}),
        },
        LiveEvent::ToolDone {
            name: "Bash".into(),
            ok: true,
            output: "out".into(),
            depth: 0,
            lane: 0,
            call_id: Some("call_1".into()),
            elapsed_ms: 12,
        },
        LiveEvent::Hook {
            event: "SessionStart".into(),
            detail: "startup".into(),
        },
        LiveEvent::Artifact {
            name: "plan".into(),
            path: ".sunmao/artifacts/plan.html".into(),
            bytes: 42,
            rev: 1,
        },
        LiveEvent::Usage(Usage::default()),
        LiveEvent::Goal {
            goal: crate::tool::GoalState::new("ship it".into()),
        },
        LiveEvent::TurnEnd {
            outcome: TurnOutcome::Completed,
        },
    ];
    for ev in &evs {
        let v = serde_json::to_value(ev).unwrap();
        assert!(
            v.get("type").and_then(|t| t.as_str()).is_some(),
            "every LiveEvent variant needs a `type` tag on the wire: {v}"
        );
    }
    let v = serde_json::to_value(&evs[0]).unwrap();
    assert_eq!(v["type"], "content");
    assert_eq!(v["text"], "hi");
    // ToolStart carries the call's parsed args — the GUI's diff cards
    // consume them; `null` marks synthetic events (compact, local shell).
    let v = serde_json::to_value(&evs[2]).unwrap();
    assert_eq!(v["type"], "tool_start");
    assert_eq!(v["args"]["command"], "ls");
}

/// Records every LiveEvent — used to assert TurnEnd fires on the error
/// path (frontends unwind busy/spinner state from it; a missing TurnEnd
/// leaves the TUI stuck).
struct RecObserver(std::sync::Mutex<Vec<String>>);
impl Observer for RecObserver {
    fn on_event(&self, ev: &LiveEvent) {
        let tag = match ev {
            LiveEvent::TurnEnd { outcome } => format!("TurnEnd:{outcome:?}"),
            LiveEvent::Content { .. } => "Content".into(),
            LiveEvent::Reasoning { .. } => "Reasoning".into(),
            LiveEvent::ToolStart { .. } => "ToolStart".into(),
            LiveEvent::ToolDone { .. } => "ToolDone".into(),
            LiveEvent::Hook { .. } => "Hook".into(),
            LiveEvent::Artifact { .. } => "Artifact".into(),
            LiveEvent::Usage(_) => "Usage".into(),
            LiveEvent::Compacted { .. } => "Compacted".into(),
            LiveEvent::Todos { .. } => "Todos".into(),
            LiveEvent::Goal { .. } => "Goal".into(),
            LiveEvent::TaskDone { .. } => "TaskDone".into(),
            LiveEvent::JobDone { .. } => "JobDone".into(),
            LiveEvent::UserMessage { .. } => "UserMessage".into(),
            LiveEvent::TurnBoundary { .. } => "TurnBoundary".into(),
        };
        self.0.lock_or_recover().push(tag);
    }
}
