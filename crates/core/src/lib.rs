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
