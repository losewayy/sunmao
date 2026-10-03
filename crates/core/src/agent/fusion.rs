//! Fusion mode — Lead/Sidekick delegation for `TurnMode::Fusion`.
//!
//! The session's context becomes the Lead: read-only at the gate
//! (`ctx.read_only`), a write-tool-stripped declaration surface, and a
//! `fusion-lead` tail-of-request prompt. Work reaches the Sidekick — a
//! fresh-context sub-agent — through the `FusionExecute` call, which this
//! module implements.
//!
//! Delegation state lives on the Lead's Context (`ctx.fusion`) rather than
//! in a task-local: the Sidekick survives across turn iterations (rework
//! steers the SAME session), so the handle must outlive any one call.

use std::path::PathBuf;
use std::sync::Arc;

use crate::context::Context;

/// One Sidekick's live handle + the delegation's accounting.
// the fields are wired up by the FusionExecute dispatch — the state
// struct lands first so the Context field and gate arm compile on their
// own commit
#[allow(dead_code)]
///
/// `whitelist` is the file set the Lead granted write access to —
/// canonicalized absolute paths, stored on the LEAD's context and mirrored
/// onto the Sidekick's `fusion.whitelist` each time it grows. Add-only by
/// construction: no code path shrinks it, and every growth appends a
/// `fusion.whitelist` audit fact.
#[derive(Default)]
pub(crate) struct FusionState {
    /// Running/finished Sidekick context — reused across `steer` rework so
    /// the child keeps its transcript, whitelist and read ledger.
    pub(crate) sidekick: Option<Arc<Context>>,
    /// The child's `sub-…-lN` session id — doubles as the audit link.
    pub(crate) sidekick_id: Option<String>,
    /// Delegation-spec counter — `FusionSpec.seq`.
    pub(crate) spec_seq: u64,
    /// Files the Sidekick may Write/Edit (canonicalized). Add-only.
    pub(crate) whitelist: Vec<PathBuf>,
    /// Verify commands from the latest spec — rework steers re-run them.
    pub(crate) verify: Vec<String>,
    /// Consecutive failed delegations — the escalation streak.
    pub(crate) verify_fails: u32,
    /// The Lead unlocked for the rest of this turn (read_only disarmed) —
    /// the turn loop restores the flag at turn end.
    pub(crate) escalated: bool,
}

/// Delegations before the Lead escalates and finishes the job itself.
#[allow(dead_code)]
pub(crate) const ESCALATE_AFTER: u32 = 2;
