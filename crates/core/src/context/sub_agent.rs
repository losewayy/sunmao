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
    /// `detach_sub` on a child that already runs in the background.
    AlreadyBackground(String),
}

impl std::fmt::Display for SubSteerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuch(id) => write!(f, "no such sub-agent: {id}"),
            Self::Finished(id) => write!(f, "{id} finished, use resume"),
            Self::NoHandle(id) => write!(f, "{id} has no live handle"),
            Self::AlreadyBackground(id) => write!(f, "{id} already runs in the background"),
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
    /// The GUI's send-to-background latch for a FOREGROUND (awaited) spawn:
    /// tripping it tells the parent's `Task` call to stop waiting; the child
    /// keeps running and its result lands as a pushed `TaskDone` fact.
    /// Detached spawns never need it — they left the foreground already.
    pub(crate) detach: Option<crate::tool::KillSwitch>,
    /// Spawned already-detached (`run_in_background: true`) — the roster
    /// card tags these so a user-detached row can say "前台转后台" instead.
    pub background: bool,
    /// Flipped by the parent call that caught the detach latch — same UI
    /// tag source as the jobs panel's `detached` detail.
    pub detached: bool,
    /// Signalled once when `done` is written — `WaitFor` sleeps on this
    /// instead of polling the roster.
    pub(crate) done_notify: std::sync::Arc<tokio::sync::Notify>,
}

impl TaskEntry {
    /// A running row with no live handles — the shape tests and the
    /// restart-lineage path (`roster_reopen`'s fresh row) want; handles get
    /// filled in by whichever caller knows them.
    pub fn pending(id: impl Into<String>, lane: u16, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            lane,
            agent: None,
            prompt: prompt.into(),
            done: None,
            steer: None,
            cancel: None,
            detach: None,
            background: false,
            detached: false,
            done_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Can the GUI still send this row to the background? Only a running,
    /// not-yet-background spawn with a live latch qualifies — finished rows,
    /// `run_in_background` children and detached ones are already there, and
    /// a row whose completion feeds an internal consumer (the fusion
    /// sidekick) has nowhere to push.
    pub fn can_detach(&self) -> bool {
        self.done.is_none() && !self.background && !self.detached && self.detach.is_some()
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

    /// Trip a foreground sub-agent's detach latch — the GUI roster's
    /// send-to-background button. The child's turn keeps running untouched;
    /// only the parent's `Task` call stops waiting (its result still lands,
    /// as a pushed TaskDone). Refusals mirror `cancel_sub`: finished rows
    /// and already-detached rows can't be sent anywhere.
    pub(crate) fn detach_sub(&self, sub_id: &str) -> Result<(), SubSteerError> {
        let detach = {
            let mut tasks = self.live_tasks.lock_or_recover();
            let t = tasks
                .iter_mut()
                .find(|t| t.id == sub_id)
                .ok_or_else(|| SubSteerError::NoSuch(sub_id.to_string()))?;
            if t.done.is_some() {
                return Err(SubSteerError::Finished(sub_id.to_string()));
            }
            if t.background || t.detached {
                return Err(SubSteerError::AlreadyBackground(sub_id.to_string()));
            }
            t.detached = true;
            t.detach
                .clone()
                .ok_or_else(|| SubSteerError::NoHandle(sub_id.to_string()))?
        };
        detach.kill();
        Ok(())
    }
}
