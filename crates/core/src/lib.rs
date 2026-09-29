//! sunmao-core — the kernel: event-sourced sessions, tool registry, agent loop.
//!
//! Design contract: `docs/SPEC.md`. The seam-first rule is "earn your seam" —
//! an interface appears only once a second real implementation demands it.

pub mod agent;
pub mod approval;
pub mod audit;
pub mod context;
pub mod hooks;
pub mod mcp;
pub mod session;
pub mod task;
pub mod tool;
pub mod web;

pub use agent::{AgentLoop, TurnOutcome};
pub use context::Context;
pub use session::{SessionEvent, SessionLog};
pub use tool::{ToolRegistry, ToolResult};
