//! The foreground-run lifecycle shared by the `Bash` tool and the frontends'
//! `!` local shell.
//!
//! Split out of `shell.rs` by responsibility: that file owns the tool
//! declaration, the deno execution engine and rendering; this one owns how a
//! run somebody waited on reports back. Both audiences get the same
//! semantics — registered as a job from its first byte, and a budget that
//! MOVES the run to the background instead of killing it.

use super::shell::{ShellRun, render_run, spawn_run};
use crate::tool::jobs;
use std::sync::Arc;

/// How a foreground run that a frontend (or the `Bash` tool) waited on ended.
/// The two shapes are the two ways a wait can end, so every caller renders
/// them the same way instead of inventing its own wording.
pub enum LocalShell {
    /// Ended inside its budget — or was cancelled / killed at it.
    Done(ShellRun),
    /// Still running as a job: the budget lapsed and the run was moved to the
    /// background, where its completion is pushed as a `JobDone` fact.
    Detached {
        id: String,
        pid: Option<u32>,
        log_path: std::path::PathBuf,
        output: String,
    },
}

impl LocalShell {
    /// The text to show: the finished run, or the detach note the model's
    /// `Bash` result also carries (one sentence for every audience).
    pub fn render(&self) -> String {
        match self {
            Self::Done(run) => render_run(run),
            Self::Detached {
                id,
                pid,
                log_path,
                output,
            } => jobs::detached_result(id, *pid, log_path, output),
        }
    }

    /// Did the command succeed? A run that is still going is not a failure.
    pub fn ok(&self) -> bool {
        match self {
            Self::Done(run) => run.exit_code == 0,
            Self::Detached { .. } => true,
        }
    }

    /// What a frontend records as the durable `LocalShell` exit code. A run
    /// the budget moved to the background has no exit yet, so it records the
    /// same `-1` the spawn-error path already uses; the real code arrives
    /// later in the job's `JobDone` fact.
    pub fn record_code(&self) -> i32 {
        match self {
            Self::Done(run) => run.exit_code,
            Self::Detached { .. } => -1,
        }
    }
}

/// One foreground run, registered as a job from its first byte. The only
/// difference from `background: true` is that this caller waits — and that
/// it stops waiting, rather than killing, when the budget lapses.
pub(super) async fn job_run(
    list: deno_task_shell::parser::SequentialList,
    command: &str,
    cwd: std::path::PathBuf,
    timeout_secs: u64,
    notes: Vec<String>,
    ctx: &Arc<crate::context::Context>,
) -> anyhow::Result<LocalShell> {
    let paths = jobs::JobPaths::create(ctx, jobs::next_job_id())?;
    let notifier = jobs::JobNotifier::from_ctx(ctx).await;
    let mut run = match spawn_run(list, cwd, Some(&paths)) {
        Ok(r) => r,
        Err(e) => {
            paths.discard(); // a dir with no run reads as "still running"
            return Err(e);
        }
    };
    run.register(&ctx.jobs, command, true);

    let fg = jobs::wait_foreground(
        &mut run,
        timeout_secs,
        super::timeout::AUTO_BACKGROUND_ON_TIMEOUT,
        Some(ctx.cancel_signal()),
    )
    .await;

    if matches!(fg, jobs::Foreground::Detached) {
        // The log already holds everything the command produced; the job
        // keeps writing it, and completion arrives as a pushed fact.
        let output = run.out.text();
        let (id, log_path, pid) = (run.id.clone(), run.log_path.clone(), run.pid);
        jobs::hand_off(
            run,
            notifier,
            ctx.jobs.clone(),
            super::timeout::BACKGROUND_TIMEOUT_SECS,
        );
        return Ok(LocalShell::Detached {
            id,
            pid,
            log_path,
            output,
        });
    }

    let label = fg.label(timeout_secs);
    let end = fg.end().unwrap_or_else(jobs::RunEnd::lost);
    let run_out = ShellRun {
        exit_code: end.code,
        stdout: run.out.text(),
        stderr: run.err.text(),
        preflight: notes.join("\n"),
        ended: jobs::ended_note(end.ended, label),
    };
    jobs::conclude(&notifier, &ctx.jobs, &run.id, &run.dir, end.code, false).await;
    // an inline run leaves nothing behind: its result is the tool result, and
    // both the jobs panel and the session roster read the disk — a plain `ls`
    // must not become a row in either
    jobs::retire(&ctx.jobs, &run.id, &run.dir);
    Ok(LocalShell::Done(run_out))
}

/// A frontend's `!` local shell (TUI, REPL, serve client). The same
/// job-aware run the `Bash` tool makes: registered in `ctx.jobs` from its
/// first byte, so a command that reaches its budget is MOVED TO THE
/// BACKGROUND instead of being killed — a long build the user started stays
/// legal work, it just stops parking the prompt. `Err(String)` is a legible
/// failure (parse error, spawn failure); callers render it as output.
pub async fn run_local_shell(
    command: &str,
    cwd: std::path::PathBuf,
    timeout_secs: u64,
    shell: crate::tool::ShellBackend,
    ctx: &Arc<crate::context::Context>,
) -> Result<LocalShell, String> {
    if shell == crate::tool::ShellBackend::Pwsh {
        return super::pwsh::local_shell(command, &cwd, timeout_secs, ctx).await;
    }
    let list = deno_task_shell::parser::parse(command)
        .map_err(|e| format!("cannot parse command: {e}"))?;
    let notes = crate::preflight::advisories(&list, &cwd);
    job_run(list, command, cwd, timeout_secs, notes, ctx)
        .await
        .map_err(|e| format!("{e:#}"))
}
