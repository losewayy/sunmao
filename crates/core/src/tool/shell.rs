use crate::tool::jobs;
use crate::tool::*;
use serde::Deserialize;
use serde_json::{Value, json};

// ---------- Bash ----------

pub struct BashTool;

#[async_trait::async_trait]
impl ToolImpl for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "Bash",
            "Run a shell command (cross-platform; works identically on Windows). \
             Use for builds, tests, git, and anything without a dedicated tool. \
             Commands are preflighted before they run: a '[preflight]' block in \
             the result predicts spawn failures and mangled argv — fix the \
             command when it tells you one is doomed. A foreground command that \
             reaches its timeout is moved to the background rather than killed: \
             you get a job id and an automatic notification when it finishes, \
             so never poll for it.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command line"},
                    "timeout_secs": {"type": "integer", "description": "Foreground budget in seconds (default 120, max 600). Reaching it moves the command to the background instead of killing it."},
                    "background": {"type": "boolean", "description": "Run detached from the start; returns a job id immediately. Read it with JobOutput, list with JobList, stop with JobStop."}
                },
                "required": ["command"]
            }),
        )
    }

    async fn call(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            command: String,
            timeout_secs: Option<u64>,
            #[serde(default)]
            background: bool,
        }
        let a: Args = serde_json::from_value(args)?;

        // pwsh backend: no POSIX parse/preflight — the command runs as real
        // PowerShell text. The dispatch gate already classified it via the
        // pwsh mutates/segments path.
        if ctx.shell == crate::tool::ShellBackend::Pwsh {
            return pwsh::bash(&a.command, a.timeout_secs, a.background, ctx).await;
        }

        // Parse once up front — a malformed command is reported before any
        // permission prompt, and the same AST feeds both preflight and exec.
        let list = match deno_task_shell::parser::parse(&a.command) {
            Ok(l) => l,
            Err(e) => {
                return Ok(ToolResult {
                    exit_code: None,
                    output: format!("cannot parse command: {e}"),
                    ok: false,
                });
            }
        };
        // shell/preflight: spawnfate models the which-resolve + CreateProcess
        // path deno_task_shell actually takes; advisories ride in the result
        // so the model can self-correct. Advisory only — never blocks.
        let notes = crate::preflight::advisories(&list, &ctx.cwd);
        let pre = || {
            if notes.is_empty() {
                String::new()
            } else {
                format!("[preflight — advisory only]\n{}\n\n", notes.join("\n"))
            }
        };
        let timeout_secs = super::timeout::effective_timeout(a.timeout_secs);

        if a.background {
            return spawn_background(&a.command, list, ctx, pre()).await;
        }

        // permission rules + approval gate live in the dispatch pipeline
        // (agent/turn.rs::gate_call) — the hook's permissionDecision can only
        // interpose there; the tool itself just executes.
        foreground(list, &a.command, timeout_secs, notes, ctx).await
    }
}

/// The `Bash` tool's foreground call: [`super::foreground::job_run`] plus the
/// model-facing rendering of whichever way it ended.
async fn foreground(
    list: deno_task_shell::parser::SequentialList,
    command: &str,
    timeout_secs: u64,
    notes: Vec<String>,
    ctx: &Arc<crate::context::Context>,
) -> anyhow::Result<ToolResult> {
    let run = super::foreground::job_run(list, command, ctx.cwd.clone(), timeout_secs, notes, ctx)
        .await?;
    Ok(ToolResult {
        exit_code: run.exit_code(),
        output: run.render(),
        ok: run.ok(),
    })
}

/// One foreground shell run — the shared execution path behind the `Bash`
/// tool and the TUI's `!` local mode. Returns legible text the same way the
/// tool does: preflight advisory + truncated stdout/stderr + exit marker.
pub struct ShellRun {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// spawnfate advisories ("" when clean) — surfaced, never blocking.
    pub preflight: String,
    /// How the run ended when it didn't exit on its own —
    /// `"cancelled by user"` / `"timed out"`; None on a natural exit.
    pub ended: Option<String>,
}

/// A forced foreground shell run with no job identity: it keeps the plain
/// kill-on-timeout contract. This is for callers that need a verdict *now* —
/// the fusion verifier's own check, the cancel tests — not for a frontend the
/// user is waiting at: [`super::foreground::run_local_shell`] is the `!` path,
/// and it backgrounds on timeout like the `Bash` tool.
pub async fn run_foreground(
    command: &str,
    cwd: std::path::PathBuf,
    timeout_secs: u64,
    shell: crate::tool::ShellBackend,
    cancel: Option<crate::context::CancelSignal>,
) -> Result<ShellRun, String> {
    if shell == crate::tool::ShellBackend::Pwsh {
        return pwsh::run_foreground(command, cwd, timeout_secs, cancel).await;
    }
    let list = deno_task_shell::parser::parse(command)
        .map_err(|e| format!("cannot parse command: {e}"))?;
    let preflight = crate::preflight::advisories(&list, &cwd).join("\n");
    let mut run = spawn_run(list, cwd, None).map_err(|e| format!("{e:#}"))?;
    let fg = jobs::wait_foreground(&mut run, timeout_secs, false, cancel).await;
    let label = fg.label(timeout_secs);
    let end = fg.end().unwrap_or_else(jobs::RunEnd::lost);
    Ok(ShellRun {
        exit_code: end.code,
        stdout: run.out.text(),
        stderr: run.err.text(),
        preflight,
        ended: jobs::ended_note(end.ended, label),
    })
}

/// The in-memory half of the fan-out. `deno_task_shell`'s internals are
/// `!Send` (`Rc<Cell>` exit-code cells) — the pipes and state are built
/// *inside* the blocking closure, never moved in.
struct TeeWrite {
    file: Option<std::fs::File>,
    mem: jobs::CappedBuf,
}

impl std::io::Write for TeeWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // two consumers per reader: the job log (durable, always) and the
        // bounded in-memory copy the tool result renders from. A log write
        // failure must not kill the run — the job's output still exists.
        if let Some(f) = &mut self.file {
            let _ = std::io::Write::write_all(f, buf);
        }
        self.mem.push(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = &mut self.file {
            let _ = std::io::Write::flush(f);
        }
        Ok(())
    }
}

/// Start a POSIX run. The runner owns its blocking thread and reports
/// through `release`, so nobody has to be waiting on it: the foreground
/// caller waits only as long as its budget allows.
pub(super) fn spawn_run(
    list: deno_task_shell::parser::SequentialList,
    cwd: std::path::PathBuf,
    job: Option<&jobs::JobPaths>,
) -> anyhow::Result<jobs::JobRun> {
    let env_vars: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
        std::env::vars_os().collect();
    let out = jobs::CappedBuf::default();
    let err = jobs::CappedBuf::default();
    let kill = jobs::KillSwitch::new();
    let (end_tx, release) = tokio::sync::oneshot::channel();
    let log_path = job.map(|j| j.log());
    let (id, dir) = match job {
        Some(j) => (j.id.clone(), j.dir.clone()),
        None => (String::new(), std::path::PathBuf::new()),
    };
    // one append handle per stream, sharing the file cursor (`try_clone`) so
    // the two pipes concatenate instead of overwriting each other's offsets;
    // stdout and stderr merge into one log, the same shape the pwsh backend
    // writes and `JobOutput` reads.
    let files = match &log_path {
        Some(p) => {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            Some((f.try_clone()?, f))
        }
        None => None,
    };
    let run = jobs::JobRun {
        id,
        dir,
        log_path: log_path.unwrap_or_default(),
        out: out.clone(),
        err: err.clone(),
        pid: None,
        started_at: jobs::now_ms(),
        kill: kill.clone(),
        release: Some(release),
    };
    let (out_mem, err_mem) = (out, err);
    tokio::task::spawn_blocking(move || {
        let state =
            deno_task_shell::ShellState::new(env_vars, cwd, Default::default(), Default::default());
        // clone before `execute_with_pipes` takes ownership — the signal is
        // the only handle that reaches deno's per-child KillSignals, and it
        // must stay on THIS thread: `KillSignal` is `!Send`. Everyone else
        // asks through the latch, and the select below turns asking into a
        // signal.
        let signal = state.kill_signal().clone();
        let kill_fut = kill.wait();
        tokio::pin!(kill_fut);
        let (out_reader, out_writer) = deno_task_shell::pipe();
        let (err_reader, err_writer) = deno_task_shell::pipe();
        // empty stdin: tools must never block on the REPL's stdin
        let (stdin_reader, stdin_writer) = match std::io::pipe() {
            Ok(p) => p,
            Err(e) => {
                let _ = end_tx.send(jobs::RunEnd {
                    code: -1,
                    ended: Some(format!("stdin pipe failed: {e}")),
                });
                return;
            }
        };
        drop(stdin_writer);
        let exec = deno_task_shell::execute_with_pipes(
            list,
            state,
            deno_task_shell::ShellPipeReader::from_raw(stdin_reader),
            out_writer,
            err_writer,
        );
        let rt = tokio::runtime::Handle::current();
        // Pinned so the future can be awaited to completion here; readers
        // drain *while* the pipeline runs (a child filling the ~64KB pipe
        // buffer deadlocks if nobody reads).
        let mut exec = std::pin::pin!(exec);
        let (code, ended) = rt.block_on(async {
            let (out_file, err_file) = match files {
                Some((a, b)) => (Some(a), Some(b)),
                None => (None, None),
            };
            let out_drain = {
                let mut w = TeeWrite {
                    file: out_file,
                    mem: out_mem,
                };
                tokio::task::spawn_blocking(move || {
                    out_reader.pipe_to(&mut w).ok();
                })
            };
            let err_drain = {
                let mut w = TeeWrite {
                    file: err_file,
                    mem: err_mem,
                };
                tokio::task::spawn_blocking(move || {
                    err_reader.pipe_to(&mut w).ok();
                })
            };
            // Deliberately no timeout arm: the budget belongs to whoever is
            // waiting, and a run that outlives its foreground caller must
            // not be disarmed by a later cancel — a detached job is a
            // background job. A kill request is honored here, where the
            // `!Send` signal lives.
            let code = tokio::select! {
                c = &mut exec => c,
                () = &mut kill_fut => {
                    signal.send(deno_task_shell::SignalKind::SIGKILL);
                    (&mut exec).await
                }
            };
            // writer handles drop with exec → drains see EOF and return —
            // unless a detached grandchild still holds the write end: bound
            // the wait so the run can't hang past it.
            let truncated = tokio::time::timeout(
                PIPE_DRAIN_TIMEOUT,
                futures_util::future::join(out_drain, err_drain),
            )
            .await
            .is_err();
            let ended = truncated.then(|| {
                format!(
                    "output truncated — pipes still held {}s after exit",
                    PIPE_DRAIN_TIMEOUT.as_secs()
                )
            });
            (code, ended)
        });
        let _ = end_tx.send(jobs::RunEnd { code, ended });
    });
    Ok(run)
}

/// A `background: true` spawn — an explicit job, so it gets no clock of its
/// own (it is meant to outlive the turn) and no cancel wire; stop it with
/// `JobStop` or the frontend.
async fn spawn_background(
    command: &str,
    list: deno_task_shell::parser::SequentialList,
    ctx: &crate::context::Context,
    preamble: String,
) -> anyhow::Result<ToolResult> {
    let paths = jobs::JobPaths::create(ctx, jobs::next_job_id())?;
    let notifier = jobs::JobNotifier::from_ctx(ctx).await;
    let run = match spawn_run(list, ctx.cwd.clone(), Some(&paths)) {
        Ok(r) => r,
        Err(e) => {
            paths.discard();
            return Err(e);
        }
    };
    run.register(&ctx.jobs, command, false);
    // the jobs dir is pull-state; the sink gets a nudge at spawn and at
    // exit so watching frontends re-read it (mirrors tasks.changed)
    if let Some(s) = ctx.live_sink.get() {
        s.on_event(&crate::agent::LiveEvent::Hook {
            event: "jobs.changed".into(),
            detail: format!("{} started", paths.id),
        });
    }
    let log_path = run.log_path.clone();
    jobs::hand_off(run, notifier, ctx.jobs.clone(), None);
    Ok(ToolResult {
        exit_code: None,
        output: format!(
            "{preamble}job {} started; log: {}",
            paths.id,
            log_path.display()
        ),
        ok: true,
    })
}

/// Cap one side's output at ~8KB — the same discipline the tool result uses.
/// CAP isn't char-aligned for CJK/emoji output; step back to a boundary.
fn trunc(s: &str) -> String {
    const CAP: usize = 8 * 1024;
    if s.len() > CAP {
        let mut end = CAP;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…[{} bytes truncated]", &s[..end], s.len() - end)
    } else {
        s.to_string()
    }
}

/// Render a `ShellRun` as one display string (transcript + session log share
/// this shape so replay shows what the user saw).
pub fn render_run(r: &ShellRun) -> String {
    let mut out = if r.preflight.is_empty() {
        trunc(r.stdout.trim_end())
    } else {
        format!(
            "[preflight — advisory only]\n{}\n\n{}",
            r.preflight,
            trunc(r.stdout.trim_end())
        )
    };
    if !r.stderr.trim().is_empty() {
        out.push_str(&format!("\n[stderr]\n{}", trunc(r.stderr.trim())));
    }
    if let Some(e) = &r.ended {
        out.push_str(&format!("\n[{e}]"));
    }
    if r.exit_code != 0 {
        out.push_str(&format!("\n[exit code {}]", r.exit_code));
    }
    // Windows-shaped failures (file locks, busy files) read like Unix
    // permission errors — a hint line teaches the right reflex. Advisory,
    // never a verdict: the stderr itself is still shown verbatim above.
    #[cfg(windows)]
    if let Some(hint) = stderr_hint(&r.stderr) {
        out.push_str(&format!("\n[hint] {hint}"));
    }
    out
}

/// stderr substring → model-facing hint. `assets/stderr-hints.txt`, same
/// `pattern | reason` grammar as risky-patterns.txt — substring match,
/// case-insensitive, first match wins.
#[cfg(windows)]
fn stderr_hint(stderr: &str) -> Option<String> {
    static TABLE: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        crate::approval::parse_table(include_str!("../../assets/stderr-hints.txt"))
    });
    crate::approval::classify(stderr, table).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code: i32, stdout: &str, stderr: &str) -> ShellRun {
        ShellRun {
            exit_code: code,
            stdout: stdout.into(),
            stderr: stderr.into(),
            preflight: String::new(),
            ended: None,
        }
    }

    #[test]
    fn render_run_keeps_result_shape() {
        let out = render_run(&run(1, "partial", "boom"));
        assert!(out.contains("partial"));
        assert!(out.contains("[stderr]\nboom"));
        assert!(out.contains("[exit code 1]"));
    }

    /// Windows-only: stderr that smells like a file lock gets a `[hint]`
    /// line appended after the exit code; unrelated stderr stays quiet.
    #[cfg(windows)]
    #[test]
    fn windows_stderr_hints_surface() {
        let out = render_run(&run(
            1,
            "",
            "rm: cannot remove 'x': os error 32: The process cannot access \
             the file because it is being used by another process",
        ));
        assert!(out.contains("[hint]"), "{out}");
        assert!(out.contains("file lock"), "{out}");
        let quiet = render_run(&run(1, "", "compile error: expected ;"));
        assert!(!quiet.contains("[hint]"), "{quiet}");
    }

    /// A `start /b` grandchild inherits the stdout pipe's write end — cmd
    /// exits but the pipe never EOFs. The run must still return with the
    /// bytes it got (bounded drain), not hang the tool call forever.
    /// Without the drain deadline `run_foreground` only returns when
    /// ping's own ~20s budget lapses; with it the wait is ~5s. (The
    /// abandoned drain thread still pins the test runtime until ping
    /// exits — the wall time is the grandchild's, not the run's.)
    #[cfg(windows)]
    #[tokio::test]
    async fn detached_grandchild_pipe_does_not_hang_the_run() {
        let cwd = std::env::current_dir().unwrap();
        let t0 = std::time::Instant::now();
        let run = run_foreground(
            "cmd /c start /b ping -n 20 127.0.0.1 >nul",
            cwd,
            120,
            crate::tool::ShellBackend::Posix,
            None,
        )
        .await
        .unwrap();
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(12),
            "drain must be bounded: {:?}",
            t0.elapsed()
        );
        let ended = run.ended.clone().unwrap_or_default();
        assert!(ended.contains("truncated"), "ended: {:?}", run.ended);
    }
}
