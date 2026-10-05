//! The in-memory roster — what a job row is, the context-owned table, and the
//! bound that keeps `JobList` a working set rather than a ledger.
//!
//! Split out of `jobs/mod.rs` by responsibility: the rest of the module owns
//! identity, spawn and completion delivery; this owns the volatile row list
//! that the model's `JobList`/`JobStop` and the serve surface read.

use super::KillSwitch;
use crate::context::MutexRecover;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The registry a context owns — `Context::jobs`. Same seam shape as
/// `live_tasks`: a `std::sync::Mutex` around a `Vec`, `Arc` so a detached
/// completion task can still reach it.
pub type JobTable = Arc<std::sync::Mutex<Vec<JobEntry>>>;

pub fn new_table() -> JobTable {
    Arc::new(std::sync::Mutex::new(Vec::new()))
}

/// Where a job stands. `Detached` is running-but-unwatched — the model was
/// told it will be notified, so nothing is waiting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Detached,
    Exited(i32),
}

impl JobStatus {
    pub fn label(self) -> String {
        match self {
            Self::Running => "running".into(),
            Self::Detached => "running (moved to background)".into(),
            Self::Exited(code) => format!("exit {code}"),
        }
    }

    pub fn is_running(self) -> bool {
        !matches!(self, Self::Exited(_))
    }
}

/// One registered job. Cloned out of the table — never held across an await.
#[derive(Clone)]
pub struct JobEntry {
    pub id: String,
    /// The child's pid when the backend knows it (pwsh); the deno engine
    /// tracks its children behind a kill handle and exposes no pid.
    pub pid: Option<u32>,
    pub command: String,
    /// unix milliseconds
    pub started_at: u64,
    pub output_path: PathBuf,
    /// Started as a foreground `Bash` call rather than `background: true`.
    pub foreground: bool,
    pub status: JobStatus,
    /// The backend's kill action, armed once the process exists.
    pub stop: KillSwitch,
}

impl std::fmt::Debug for JobEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobEntry")
            .field("id", &self.id)
            .field("pid", &self.pid)
            .field("command", &self.command)
            .field("status", &self.status)
            .field("foreground", &self.foreground)
            .finish_non_exhaustive()
    }
}

// ---------- origin marker ----------

/// The origin marker the serve surface filters on: `foreground` is true while
/// the run belongs to an inline `Bash` call, and false from the hand-off (or
/// the `background: true` spawn) onward. Deliberately not part of the job's
/// durable truth — losing it only means "treat the row as a background job".
pub(super) fn meta_path(dir: &Path) -> PathBuf {
    dir.join("job.json")
}

pub(super) fn write_foreground(dir: &Path, foreground: bool) {
    if dir.as_os_str().is_empty() {
        return;
    }
    let meta = serde_json::json!({"foreground": foreground});
    let _ = std::fs::write(meta_path(dir), meta.to_string());
}

// ---------- registry ----------

/// The roster's ceiling. `JobList` is a working set, not a ledger: every
/// foreground call registers here, so without a bound a long session's list
/// grows with every command it ever ran. A running job is never evicted —
/// its kill latch is the only handle `JobStop` has.
pub const MAX_JOBS: usize = 64;

pub fn register(table: &JobTable, entry: JobEntry) {
    let mut v = table.lock_or_recover();
    v.push(entry);
    prune(&mut v);
}

/// Keep every running entry, then the newest finished ones up to the ceiling.
fn prune(v: &mut Vec<JobEntry>) {
    if v.len() <= MAX_JOBS {
        return;
    }
    let running = v.iter().filter(|e| e.status.is_running()).count();
    let budget = MAX_JOBS.saturating_sub(running);
    let mut finished: Vec<(u64, String)> = v
        .iter()
        .filter(|e| !e.status.is_running())
        .map(|e| (e.started_at, e.id.clone()))
        .collect();
    finished.sort_by_key(|(started_at, _)| std::cmp::Reverse(*started_at));
    let keep: std::collections::HashSet<String> = finished
        .into_iter()
        .take(budget)
        .map(|(_, id)| id)
        .collect();
    v.retain(|e| e.status.is_running() || keep.contains(&e.id));
}

/// Drop one row. An inline run's whole life happened inside a tool call, so
/// nothing outside that call needs to see it again.
pub fn forget(table: &JobTable, id: &str) {
    table.lock_or_recover().retain(|e| e.id != id);
}

/// Retire a finished inline run: the registry row goes away (its output is
/// already the tool result) and so does its scratch dir. The disk stays the
/// truth for jobs that outlive their call, not for every command that ran.
pub fn retire(table: &JobTable, id: &str, dir: &Path) {
    forget(table, id);
    if !dir.as_os_str().is_empty() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

pub fn set_status(table: &JobTable, id: &str, status: JobStatus) {
    if let Some(e) = table.lock_or_recover().iter_mut().find(|e| e.id == id) {
        e.status = status;
    }
}

/// Newest first — the order both `JobList` and a human want.
pub fn snapshot(table: &JobTable) -> Vec<JobEntry> {
    let mut out = table.lock_or_recover().clone();
    out.sort_by_key(|e| std::cmp::Reverse(e.started_at));
    out
}

/// The stop wire, cloned out from under the lock (never held across await).
pub fn stop_wire(table: &JobTable, id: &str) -> Option<JobEntry> {
    table.lock_or_recover().iter().find(|e| e.id == id).cloned()
}
