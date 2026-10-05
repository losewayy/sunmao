//! Background jobs — identity, registry, and completion delivery.
//!
//! A job's durable half is `.sunmao/jobs/{id}/` (`output.log`, and
//! `exit.json` once it ended) — the layout `JobOutput` and the GUI have
//! always read, so the disk stays the truth for anyone who arrives late.
//! The volatile half is the owning context's [`JobTable`]: pid, command,
//! start time, foreground flag and the stop wire — none of which the disk
//! can answer while the job is still running.
//!
//! Foreground `Bash` calls register here too, and their stream is read by
//! ONE reader per pipe that fans every chunk to two consumers: the job log
//! (always) and a bounded in-memory copy for the tool result. That is why
//! moving a foreground command to the background needs no pipe takeover —
//! it is a flag flip plus "stop waiting", and the log has been complete
//! since the first byte.
//!
//! But an inline call is not a *job* to the user, and both the panel and the
//! session roster used to treat it as one. Two rules keep that honest:
//! `job.json` carries `foreground` (true until a hand-off rewrites it to
//! false) so the serve surface can hide a run that never detached, and a
//! foreground run that ends where it started is [`retire`]d — the registry
//! row and the scratch dir both go away, because its result already reached
//! the model as a tool result.

use crate::context::MutexRecover;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod registry;
pub use registry::*;

/// A one-way "kill this job" request.
///
/// Deliberately a latch rather than a stored closure: `deno_task_shell`'s
/// `KillSignal` is `!Send` (`Rc` inside), so the handle that can actually
/// signal a POSIX pipeline can never be moved into a cross-thread registry.
/// Instead the runner *watches* this latch — from the thread that owns the
/// kill signal — and everyone else (the foreground timeout, `JobStop`, the
/// longer background tier) just asks.
///
/// The latch is checked before the waiter is armed, so a request that
/// landed while the run was still being set up is not lost.
#[derive(Clone, Default)]
pub struct KillSwitch(Arc<KillState>);

#[derive(Default)]
struct KillState {
    fired: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl KillSwitch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn kill(&self) {
        self.0
            .fired
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.0.notify.notify_one();
    }

    pub fn is_fired(&self) -> bool {
        self.0.fired.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolve once a kill has been requested — immediately if one already
    /// has been.
    pub async fn wait(&self) {
        if self.is_fired() {
            return;
        }
        self.0.notify.notified().await;
    }
}

impl std::fmt::Debug for KillSwitch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KillSwitch")
            .field("fired", &self.is_fired())
            .finish()
    }
}

/// Kill a whole process tree by pid — the pwsh backend's stop action, and
/// the fallback for anything that outlived its handle.
pub fn kill_tree(pid: u32) {
    let mut c = std::process::Command::new(if cfg!(windows) { "taskkill" } else { "kill" });
    if cfg!(windows) {
        c.args(["/PID", &pid.to_string(), "/T", "/F"]);
    } else {
        c.args(["-9", &pid.to_string()]);
    }
    let _ = c
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Jobs live under the session dir, next to sessions/ and checkpoints/.
pub fn jobs_dir(ctx: &crate::context::Context) -> PathBuf {
    ctx.cwd.join(".sunmao").join("jobs")
}

/// `j-<ms>-<pid:x>-<seq>` — a per-Context counter collided when two
/// contexts (main + sub-agent, or two sessions in one cwd) spawned in the
/// same millisecond: each started at seq 0 and shared one output.log. The
/// seq is process-global now, and pid covers two sunmao processes running
/// the same project.
pub fn next_job_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "j-{}-{:x}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// A job's identity and directory. Created before the spawn so the runner
/// can open its log, and removed again if the spawn fails — an empty job
/// dir reads to every consumer as "still running".
#[derive(Clone, Debug)]
pub struct JobPaths {
    pub id: String,
    pub dir: PathBuf,
}

impl JobPaths {
    pub fn create(ctx: &crate::context::Context, id: String) -> anyhow::Result<Self> {
        let dir = jobs_dir(ctx).join(&id);
        std::fs::create_dir_all(&dir)?;
        Ok(Self { id, dir })
    }

    pub fn log(&self) -> PathBuf {
        self.dir.join("output.log")
    }

    /// Undo a directory whose spawn never happened.
    pub fn discard(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The in-memory half of the fan-out: bounded, tail-keeping. What falls off
/// the front is still in the job log, which is the consumer that never
/// stops writing.
#[derive(Clone, Default)]
pub struct CappedBuf(Arc<std::sync::Mutex<Vec<u8>>>);

/// One MiB per stream — far more than a tool result can carry (8 KiB after
/// rendering), and bounded, so a chatty long-runner can't grow the kernel's
/// heap.
const MEM_CAP: usize = 1024 * 1024;

impl CappedBuf {
    pub fn push(&self, chunk: &[u8]) {
        let mut b = self.0.lock_or_recover();
        b.extend_from_slice(chunk);
        if b.len() > MEM_CAP {
            let cut = b.len() - MEM_CAP;
            b.drain(..cut);
        }
    }

    pub fn text(&self) -> String {
        crate::console::console_text(&self.0.lock_or_recover())
    }
}

impl std::io::Write for CappedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.push(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How a run ended, as the backend saw it. The *political* label
/// ("cancelled by user", "timed out") belongs to the caller that chose to
/// wait or not — this carries only what the engine observed.
#[derive(Debug, Clone)]
pub struct RunEnd {
    pub code: i32,
    pub ended: Option<String>,
}

impl RunEnd {
    /// The sender never reported — the run's thread or task died.
    pub fn lost() -> Self {
        Self {
            code: -1,
            ended: None,
        }
    }
}

/// A started shell run. It is *always* internally detached: `release` fires
/// when the process is done and its pipes are drained, whoever is (or is
/// no longer) listening.
pub struct JobRun {
    pub id: String,
    pub dir: PathBuf,
    pub log_path: PathBuf,
    pub out: CappedBuf,
    pub err: CappedBuf,
    pub pid: Option<u32>,
    pub started_at: u64,
    pub kill: KillSwitch,
    /// Taken by whoever waits — the foreground caller first, or the
    /// background watcher after a hand-off.
    pub release: Option<tokio::sync::oneshot::Receiver<RunEnd>>,
}

impl JobRun {
    /// Register this run before anyone waits on it — a job that finishes
    /// before it was ever listed would be invisible to `JobList`/`JobStop`.
    pub fn register(&self, table: &JobTable, command: &str, foreground: bool) {
        write_foreground(&self.dir, foreground);
        register(
            table,
            JobEntry {
                id: self.id.clone(),
                pid: self.pid,
                command: command.to_string(),
                started_at: self.started_at,
                output_path: self.log_path.clone(),
                foreground,
                status: JobStatus::Running,
                stop: self.kill.clone(),
            },
        );
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------- completion delivery ----------

/// Where a job's completion is delivered: the log that spawned it (forked
/// so a later `swap_session` can't re-attribute it), the spawning context's
/// turn fence, and its live sink.
#[derive(Clone)]
pub struct JobNotifier {
    log: Option<Arc<tokio::sync::Mutex<crate::session::SessionLog>>>,
    fence: Option<Arc<tokio::sync::Mutex<()>>>,
    sink: Option<Arc<dyn crate::agent::Observer>>,
}

impl JobNotifier {
    /// Pin the current session log + turn fence. Same discipline as the
    /// detached `Task` path: the append must land on the log that spawned
    /// the job, and it must take the parent's fence — a `JobDone` message
    /// injected between a ToolCall and its ToolResult would break provider
    /// pairing and hard-fail the next request.
    pub async fn from_ctx(ctx: &crate::context::Context) -> Self {
        let log = match ctx.sessions.lock().await.fork_writer().await {
            Ok(w) => Some(Arc::new(tokio::sync::Mutex::new(w))),
            Err(e) => {
                tracing::warn!(
                    "job log fork failed ({e}) — JobDone attribution may follow a later swap"
                );
                Some(ctx.sessions.clone())
            }
        };
        Self {
            log,
            fence: Some(ctx.turn_lock.clone()),
            sink: ctx.live_sink.get().cloned(),
        }
    }
}

/// The durable end of a job: `exit.json`, the registry flip, the GUI nudge,
/// and — only when the result was NOT already delivered as a tool result —
/// the `JobDone` fact and its live mirror. Delivery is idempotent by
/// construction: this runs once, at the single point where the run ends.
pub async fn conclude(
    notifier: &JobNotifier,
    table: &JobTable,
    id: &str,
    dir: &Path,
    code: i32,
    notify: bool,
) {
    let bytes = std::fs::metadata(dir.join("output.log"))
        .map(|m| m.len())
        .unwrap_or(0);
    let _ = std::fs::write(dir.join("exit.json"), format!("{{\"exit_code\":{code}}}"));
    set_status(table, id, JobStatus::Exited(code));
    if let Some(s) = &notifier.sink {
        s.on_event(&crate::agent::LiveEvent::Hook {
            event: "jobs.changed".into(),
            detail: format!("{id} exit {code}"),
        });
    }
    if notify {
        push_done(notifier, id, code, bytes, dir).await;
    }
}

/// Push the completion into the conversation + the live wire. Split out
/// because the two are different audiences with one payload.
async fn push_done(notifier: &JobNotifier, id: &str, code: i32, bytes: u64, dir: &Path) {
    let output_path = dir.join("output.log").display().to_string();
    let ok = code == 0;
    if let (Some(fence), Some(log)) = (&notifier.fence, &notifier.log) {
        let event = crate::session::SessionEvent::JobDone {
            id: id.to_string(),
            ok,
            exit_code: code,
            output_path: output_path.clone(),
            bytes,
        };
        let _fence_permit = fence.lock().await;
        let mut l = log.lock().await;
        l.append_audit(&event).await;
    }
    if let Some(s) = &notifier.sink {
        s.on_event(&crate::agent::LiveEvent::JobDone {
            id: id.to_string(),
            ok,
            exit_code: code,
            output_path,
            bytes,
        });
    }
}

/// Stop waiting. From here on the run belongs to the background: a watcher
/// awaits its release — arming the longer background tier when the policy
/// sets one — then concludes it and pushes the notification. This is the
/// whole "move to background" step; there is no pipe to hand over.
pub fn hand_off(run: JobRun, notifier: JobNotifier, table: JobTable, bg_timeout: Option<u64>) {
    // from here on this is a background job like any other — the panel must
    // show it, and the marker flip is what lets it through
    write_foreground(&run.dir, false);
    // the card's "moved to the background" tag is a live-only fact: the marker
    // says the job is visible now, not that it used to be inline, so the nudge
    // carries that half
    if let Some(s) = &notifier.sink {
        s.on_event(&crate::agent::LiveEvent::Hook {
            event: "jobs.changed".into(),
            detail: format!("{} detached", run.id),
        });
    }
    set_status(&table, &run.id, JobStatus::Detached);
    tokio::spawn(async move {
        let JobRun {
            id,
            dir,
            kill,
            release,
            ..
        } = run;
        let code = match release {
            Some(mut rx) => {
                let first = match bg_timeout {
                    Some(secs) => tokio::select! {
                        e = &mut rx => Some(e.ok()),
                        () = tokio::time::sleep(std::time::Duration::from_secs(secs)) => None,
                    },
                    None => Some((&mut rx).await.ok()),
                };
                match first {
                    Some(Some(end)) => end.code,
                    Some(None) => RunEnd::lost().code,
                    None => {
                        // the longer tier lapsed — this job is not going to
                        // finish on its own, so it gets the kill the
                        // foreground would have given it, and still reports
                        // through the normal path
                        kill.kill();
                        rx.await
                            .map(|e| e.code)
                            .unwrap_or_else(|_| RunEnd::lost().code)
                    }
                }
            }
            None => RunEnd::lost().code,
        };
        conclude(&notifier, &table, &id, &dir, code, true).await;
    });
}

/// How a foreground wait ended.
pub enum Foreground {
    /// Ran to completion inside its budget.
    Done(RunEnd),
    /// Hit the budget and was moved to the background — still running.
    Detached,
    /// Hit the budget with auto-background disabled, and was killed.
    TimedOut(RunEnd),
    /// The turn's cancel wire fired; the run was killed.
    Cancelled(RunEnd),
}

impl Foreground {
    /// The label the tool result shows when the run didn't end on its own.
    pub fn label(&self, timeout_secs: u64) -> Option<String> {
        match self {
            Self::Done(_) | Self::Detached => None,
            Self::TimedOut(_) => Some(format!("timed out after {timeout_secs}s — killed")),
            Self::Cancelled(_) => Some("cancelled by user — killed".to_string()),
        }
    }

    pub fn end(self) -> Option<RunEnd> {
        match self {
            Self::Done(e) | Self::TimedOut(e) | Self::Cancelled(e) => Some(e),
            Self::Detached => None,
        }
    }
}

/// Wait on a run for at most `timeout_secs`. On timeout the caller either
/// takes the run over ([`Foreground::Detached`], the default) or kills it.
pub async fn wait_foreground(
    run: &mut JobRun,
    timeout_secs: u64,
    detach_on_timeout: bool,
    cancel: Option<crate::context::CancelSignal>,
) -> Foreground {
    enum First {
        End(Option<RunEnd>),
        Timeout,
        Cancel,
    }
    let cancel_fut = async move {
        match cancel {
            Some(c) => c.wait().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(cancel_fut);
    let Some(mut release) = run.release.take() else {
        return Foreground::Done(RunEnd::lost());
    };
    let first = tokio::select! {
        e = &mut release => First::End(e.ok()),
        () = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => First::Timeout,
        () = &mut cancel_fut => First::Cancel,
    };
    match first {
        First::End(end) => Foreground::Done(end.unwrap_or_else(RunEnd::lost)),
        // the caller stops waiting — hand the completion wire back so the
        // background watcher can pick the run up
        First::Timeout if detach_on_timeout => {
            run.release = Some(release);
            Foreground::Detached
        }
        First::Timeout => {
            run.kill.kill();
            Foreground::TimedOut(release.await.ok().unwrap_or_else(RunEnd::lost))
        }
        First::Cancel => {
            run.kill.kill();
            Foreground::Cancelled(release.await.ok().unwrap_or_else(RunEnd::lost))
        }
    }
}

/// Combine what the engine observed with the caller's label — the truncation
/// note must survive alongside "timed out".
pub fn ended_note(end: Option<String>, label: Option<String>) -> Option<String> {
    match (end, label) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    }
}

/// The model-facing text for a command that just moved to the background.
/// Kept here (not in the tool) so both shell backends and the future detach
/// path speak one sentence.
pub fn detached_result(id: &str, pid: Option<u32>, log_path: &Path, output: &str) -> String {
    let pid = pid.map(|p| format!("\npid: {p}")).unwrap_or_default();
    let head = format!(
        "command moved to the background after its timeout ({id}) — still running.\n\
         job: {id}{pid}\n\
         log: {}\n\
         you will be notified when it finishes; do not wait or poll.",
        log_path.display()
    );
    if output.trim().is_empty() {
        head
    } else {
        format!("{head}\n\noutput so far:\n{}", output.trim_end())
    }
}

#[cfg(test)]
mod tests;
