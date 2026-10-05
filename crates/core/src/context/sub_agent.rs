//! Sub-agent roster controls — the `steer_sub`/`cancel_sub` pair plus the
//! handles a parent's `TaskEntry` row holds into its child. They live off
//! `Context` but in their own file: the roster's lifecycle (register on
//! spawn, done on finish) is a different responsibility from the field
//! soup `mod.rs` assembles.

use super::{CancelSignal, Context, MutexRecover};

/// Why a `steer_sub`/`cancel_sub`/`Task{steer}` push was refused.
#[derive(Debug)]
pub enum SubSteerError {
    /// No roster entry under that id — wrong id, or the child predates this
    /// process (restart loses the roster; `resume` is the way back in).
    NoSuch(String),
    /// The child already finished — a dead child can't take a steer; it can
    /// only be resumed.
    Finished(String),
    /// The roster entry exists but carries no live handle (a spawn that
    /// never registered one — pre-feature entries can't appear in practice).
    NoHandle(String),
}

impl std::fmt::Display for SubSteerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuch(id) => write!(f, "no such sub-agent: {id}"),
            Self::Finished(id) => write!(f, "{id} finished, use resume"),
            Self::NoHandle(id) => write!(f, "{id} has no live handle"),
        }
    }
}

impl std::error::Error for SubSteerError {}

/// Shared roster lookup: steer and cancel take identical happy paths —
/// find the entry, refuse a finished child, clone the caller's handle out.
fn live_handle<T: Clone>(
    tasks: &std::sync::MutexGuard<'_, Vec<TaskEntry>>,
    sub_id: &str,
    pick: impl Fn(&TaskEntry) -> Option<T>,
) -> Result<T, SubSteerError> {
    match tasks.iter().find(|t| t.id == sub_id) {
        None => Err(SubSteerError::NoSuch(sub_id.to_string())),
        Some(t) if t.done.is_some() => Err(SubSteerError::Finished(sub_id.to_string())),
        Some(t) => pick(t).ok_or_else(|| SubSteerError::NoHandle(sub_id.to_string())),
    }
}

/// One detached sub-agent in the roster.
#[derive(Debug, Clone)]
pub struct TaskEntry {
    /// The `sub-…-l<lane>` id — doubles as the child log's file stem.
    pub id: String,
    /// The lane this spawn claimed — unique across the spawn tree; lets a
    /// frontend (or a test) prove distinctness without racing live events.
    pub lane: u16,
    /// Agent def name, or None for a generic spawn.
    pub agent: Option<String>,
    /// One-line digest of the prompt it was given.
    pub prompt: String,
    /// None while running; Some(ok) once TaskDone landed.
    pub done: Option<bool>,
    /// Steer-queue handle into the child's context — a clone of its
    /// `Context.steer`. `Task{steer:id, message}` and `steer_sub` push
    /// through here; None for entries registered before the handle was
    /// threaded (legacy roster rows can't be steered).
    pub(crate) steer: Option<crate::context::SteerQueue>,
    /// Cancel handle into the child's context — `AgentLoop::cancel`
    /// cascades through it so killing a turn also kills its running
    /// sub-agents (otherwise a foreground Task keeps churning after the
    /// user hit stop). None on legacy rows.
    pub(crate) cancel: Option<CancelSignal>,
}

impl Context {
    /// Steer a live sub-agent by id — roster lookup, then push the text
    /// onto the child's steer queue. The child's turn loop drains it at
    /// the next request boundary like any other steer. `Finished` means
    /// the child can only be resumed; `NoSuch` means the roster never knew
    /// this id (a pre-restart child lives on disk only — resume it).
    /// `AgentLoop::steer_sub` is this helper's public wrapper.
    pub(crate) fn steer_sub(&self, sub_id: &str, text: String) -> Result<(), SubSteerError> {
        let steer = {
            let tasks = self.live_tasks.lock_or_recover();
            live_handle(&tasks, sub_id, |t| t.steer.clone())?
        };
        steer.lock_or_recover().push_back((0, text));
        Ok(())
    }

    /// Cancel one live sub-agent by id — the same cascade primitive
    /// `AgentLoop::cancel` uses, but pointed at a single roster row. The
    /// child's own finish path records `done`/`err` in the roster; this
    /// only trips its flag+notify so the next boundary exits.
    pub(crate) fn cancel_sub(&self, sub_id: &str) -> Result<(), SubSteerError> {
        let cancel = {
            let tasks = self.live_tasks.lock_or_recover();
            live_handle(&tasks, sub_id, |t| t.cancel.clone())?
        };
        cancel.cancel();
        Ok(())
    }
}
