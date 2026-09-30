//! sunmao-core — the kernel: event-sourced sessions, tool registry, agent loop.
//!
//! Design contract: `docs/SPEC.md`. The seam-first rule is "earn your seam" —
//! an interface appears only once a second real implementation demands it.

pub mod agent;
pub mod agents;
pub mod approval;
pub mod audit;
pub mod context;
pub mod ext;
pub mod hooks;
pub mod mcp;
pub mod models;
pub mod permissions;
pub mod plugin;
pub mod preflight;
pub mod presets;
pub mod prompt;
pub mod session;
pub mod task;
pub mod tool;
pub mod web;

pub use agent::{AgentLoop, TurnOutcome};
pub use context::Context;
pub use session::{SessionEvent, SessionLog};
pub use tool::{ToolRegistry, ToolResult};

/// Directory enumeration in **sorted path order** — `read_dir` order is
/// filesystem-dependent, and every scan that feeds the serialized request
/// (skills index, agent defs, hook/plugin/extension manifests, command
/// files) would otherwise churn the prompt/tools prefix and defeat
/// provider-side prompt caching. Deterministic enumeration is a
/// cache-hit invariant: apply it anywhere the order reaches the wire.
pub(crate) fn sorted_entries(dir: &std::path::Path) -> Vec<std::fs::DirEntry> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default();
    v.sort_by_key(|e| e.path());
    v
}

/// pid-keyed temp dirs recycle (Windows PIDs wrap fast) — a second test run
/// landing on a recycled pid inherited leftover files and flaked. Nanos makes
/// each caller's scratch dir unique. Test-only; production code keeps its own
/// naming.
#[cfg(test)]
pub(crate) fn fresh_test_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("sunmao-test-{tag}-{}-{nanos}", std::process::id()))
}
