//! Sub-agent roster controls — the `steer_sub`/`cancel_sub` pair plus the
//! handles a parent's `TaskEntry` row holds into its child. They live off
//! `Context` but in their own file: the roster's lifecycle (register on
//! spawn, done on finish) is a different responsibility from the field
//! soup `mod.rs` assembles.

use super::{Context, MutexRecover};

/// A child's cancellation endpoints, held by the parent's roster row —
/// the two `Arc`s off its Context. `cancel()` is idempotent; a finished
/// child's notify just lands nowhere.
#[derive(Clone)]
pub(crate) struct SubCancel {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}

impl SubCancel {
    pub(crate) fn new(ctx: &Context) -> Self {
        Self {
            flag: ctx.cancelled.clone(),
            notify: ctx.cancel_notify.clone(),
        }
    }
    pub(crate) fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
        self.notify.notify_waiters();
    }
}

impl std::fmt::Debug for SubCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SubCancel(..)")
    }
}

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
    tasks: &std::sync::MutexGuard<'_, Vec<super::TaskEntry>>,
    sub_id: &str,
    pick: impl Fn(&super::TaskEntry) -> Option<T>,
) -> Result<T, SubSteerError> {
    match tasks.iter().find(|t| t.id == sub_id) {
        None => Err(SubSteerError::NoSuch(sub_id.to_string())),
        Some(t) if t.done.is_some() => Err(SubSteerError::Finished(sub_id.to_string())),
        Some(t) => pick(t).ok_or_else(|| SubSteerError::NoHandle(sub_id.to_string())),
    }
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
