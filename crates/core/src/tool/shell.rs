use crate::tool::*;
use anyhow::bail;
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
             command when it tells you one is doomed.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command line"},
                    "timeout_secs": {"type": "integer", "description": "Kill after N seconds (default 120)"},
                    "background": {"type": "boolean", "description": "Run detached; returns a job id readable via JobOutput"}
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
            if a.background {
                return pwsh::spawn_background(&a.command, ctx).await;
            }
            let run = pwsh::run_foreground(
                &a.command,
                ctx.cwd.clone(),
                a.timeout_secs.unwrap_or(120),
                Some(ctx.cancel_notify.clone()),
            )
            .await;
            return Ok(match run {
                Ok(r) => ToolResult {
                    output: render_run(&r),
                    ok: r.exit_code == 0,
                },
                Err(msg) => ToolResult {
                    output: msg,
                    ok: false,
                },
            });
        }

        // Parse once up front — a malformed command is reported before any
        // permission prompt, and the same AST feeds both preflight and exec.
        let list = match deno_task_shell::parser::parse(&a.command) {
            Ok(l) => l,
            Err(e) => {
                return Ok(ToolResult {
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

        if a.background {
            return spawn_background(&a.command, ctx, pre()).await;
        }

        // permission rules + approval gate live in the dispatch pipeline
        // (agent/turn.rs::gate_call) — the hook's permissionDecision can only
        // interpose there; the tool itself just executes.

        let mut run = match run_parsed(
            list,
            ctx.cwd.clone(),
            a.timeout_secs.unwrap_or(120),
            Some(ctx.cancel_notify.clone()),
        )
        .await
        {
            Ok(r) => r,
            Err(msg) => {
                return Ok(ToolResult {
                    output: msg,
                    ok: false,
                });
            }
        };
        run.preflight = notes.join("\n");
        let out = render_run(&run);
        Ok(ToolResult {
            output: out,
            ok: run.exit_code == 0,
        })
    }
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
    /// Timeout/cancel now keep their partial output (previously the `Err`
    /// path discarded it and leaked the children).
    pub ended: Option<String>,
}

/// Parse + preflight + execute `command` in `cwd`. `Err(String)` is a
/// legible failure (parse error, timeout, spawn panic), not an anyhow —
/// callers render it as output, same contract as `ToolResult{ok:false}`.
/// `cancel` wakes the run's kill path (the turn loop's `cancel_notify`).
pub async fn run_foreground(
    command: &str,
    cwd: std::path::PathBuf,
    timeout_secs: u64,
    shell: crate::tool::ShellBackend,
    cancel: Option<std::sync::Arc<tokio::sync::Notify>>,
) -> Result<ShellRun, String> {
    if shell == crate::tool::ShellBackend::Pwsh {
        return pwsh::run_foreground(command, cwd, timeout_secs, cancel).await;
    }
    let list = deno_task_shell::parser::parse(command)
        .map_err(|e| format!("cannot parse command: {e}"))?;
    let preflight = crate::preflight::advisories(&list, &cwd).join("\n");
    let mut run = run_parsed(list, cwd, timeout_secs, cancel).await?;
    run.preflight = preflight;
    Ok(run)
}

/// Execute an already-parsed command list. `deno_task_shell`'s internals are
/// `!Send` (`Rc<Cell>` exit-code cells) — every !Send value is constructed
/// *inside* the blocking closure, never moved in. The `KillSignal` is cloned
/// from `ShellState` before `execute_with_pipes` consumes it — timeout and
/// cancel both send SIGKILL (which cascades to tracked children) and then
/// *reap* the exec future, so neither path leaks a running pipeline.
async fn run_parsed(
    list: deno_task_shell::parser::SequentialList,
    cwd: std::path::PathBuf,
    timeout_secs: u64,
    cancel: Option<std::sync::Arc<tokio::sync::Notify>>,
) -> Result<ShellRun, String> {
    let env_vars: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
        std::env::vars_os().collect();

    let outcome = tokio::task::spawn_blocking(
        move || -> Result<(i32, String, String, Option<String>), String> {
            let state = deno_task_shell::ShellState::new(
                env_vars,
                cwd,
                Default::default(),
                Default::default(),
            );
            // clone before `execute_with_pipes` takes ownership — the signal
            // is the only handle that reaches deno's per-child KillSignals.
            let kill = state.kill_signal().clone();
            let (out_reader, out_writer) = deno_task_shell::pipe();
            let (err_reader, err_writer) = deno_task_shell::pipe();
            // empty stdin: tools must never block on the REPL's stdin
            let (stdin_reader, stdin_writer) = std::io::pipe().map_err(|e| e.to_string())?;
            drop(stdin_writer);
            let exec = deno_task_shell::execute_with_pipes(
                list,
                state,
                deno_task_shell::ShellPipeReader::from_raw(stdin_reader),
                out_writer,
                err_writer,
            );
            let rt = tokio::runtime::Handle::current();
            // Pinned so the abort arms can still await it — SIGKILL stops
            // the children but the future must resolve (aborted code) to
            // keep the pipe readers and JoinHandles drained.
            let mut exec = std::pin::pin!(exec);
            let cancel_fut = async {
                match &cancel {
                    Some(n) => n.notified().await,
                    None => std::future::pending::<()>().await,
                }
            };
            let (code, stdout, stderr, ended) = rt.block_on(async {
                // Readers drain *while* the pipeline runs — waiting for exec
                // to finish first deadlocks any child that fills the pipe
                // buffer (>64KB on Windows before anyone reads).
                let out_buf = SharedBuf::default();
                let err_buf = SharedBuf::default();
                let out_drain = {
                    let mut b = out_buf.clone();
                    tokio::task::spawn_blocking(move || {
                        out_reader.pipe_to(&mut b).ok();
                    })
                };
                let err_drain = {
                    let mut b = err_buf.clone();
                    tokio::task::spawn_blocking(move || {
                        err_reader.pipe_to(&mut b).ok();
                    })
                };
                enum End {
                    Natural(i32),
                    Timeout,
                    Cancelled,
                }
                let end = tokio::select! {
                    c = &mut exec => End::Natural(c),
                    () = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => End::Timeout,
                    () = cancel_fut => End::Cancelled,
                };
                let (code, ended) = match end {
                    End::Natural(c) => (c, None),
                    End::Timeout => {
                        kill.send(deno_task_shell::SignalKind::SIGKILL);
                        let c = (&mut exec).await;
                        (c, Some(format!("timed out after {timeout_secs}s — killed")))
                    }
                    End::Cancelled => {
                        kill.send(deno_task_shell::SignalKind::SIGKILL);
                        let c = (&mut exec).await;
                        (c, Some("cancelled by user — killed".to_string()))
                    }
                };
                // writer handles drop with exec → drains see EOF and
                // return — unless a detached grandchild still holds the
                // write end: bound the wait so the run can't hang past it.
                let (mut ended, truncated) = match tokio::time::timeout(
                    PIPE_DRAIN_TIMEOUT,
                    futures_util::future::join(out_drain, err_drain),
                )
                .await
                {
                    Ok(_) => (ended, false),
                    Err(_) => (ended, true),
                };
                if truncated {
                    let tag = format!(
                        "output truncated — pipes still held {}s after exit",
                        PIPE_DRAIN_TIMEOUT.as_secs()
                    );
                    ended = Some(match ended {
                        Some(e) => format!("{e}; {tag}"),
                        None => tag,
                    });
                }
                (code, out_buf.text(), err_buf.text(), ended)
            });
            Ok((code, stdout, stderr, ended))
        },
    )
    .await;

    match outcome {
        Ok(Ok((code, stdout, stderr, ended))) => Ok(ShellRun {
            exit_code: code,
            stdout,
            stderr,
            preflight: String::new(),
            ended,
        }),
        Ok(Err(msg)) => Err(msg),
        Err(e) => Err(format!("shell task panicked: {e}")),
    }
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

// ---------- background jobs (fastctx-style: filesystem is the state) ----------

/// A background job is `.sunmao/jobs/{id}/` containing output.log and,
/// once finished, exit.json. No in-memory registry — the log dir is truth.
/// `pub(crate)` so the pwsh backend writes the same layout.
pub(crate) fn jobs_dir(ctx: &crate::context::Context) -> std::path::PathBuf {
    ctx.cwd.join(".sunmao").join("jobs")
}

async fn spawn_background(
    command: &str,
    ctx: &crate::context::Context,
    preamble: String,
) -> anyhow::Result<ToolResult> {
    // `j-<ms>` alone collided when two `background:true` calls landed in the
    // same millisecond — the per-Context seq makes the id (and its log dir)
    // unique without a shared registry
    let seq = ctx
        .job_seq
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!(
        "j-{}-{seq}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let dir = jobs_dir(ctx).join(&id);
    std::fs::create_dir_all(&dir)?;
    let log_path = dir.join("output.log");
    let exit_path = dir.join("exit.json");

    let command = command.to_string();
    let cwd = ctx.cwd.clone();
    let env_vars: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
        std::env::vars_os().collect();
    // the jobs dir is pull-state; the sink gets a nudge at spawn and at
    // exit so watching frontends re-read it (mirrors tasks.changed)
    let sink = ctx.live_sink.get().cloned();
    if let Some(s) = &sink {
        s.on_event(&crate::agent::LiveEvent::Hook {
            event: "jobs.changed".into(),
            detail: format!("{id} started"),
        });
    }

    // Detach: drop the JoinHandle — the blocking thread outlives the call.
    let log_path_for_msg = log_path.clone();
    let job_id = id.clone();
    tokio::task::spawn_blocking(move || {
        let run = || -> anyhow::Result<i32> {
            let list = deno_task_shell::parser::parse(&command)?;
            let state = deno_task_shell::ShellState::new(
                env_vars,
                cwd,
                Default::default(),
                Default::default(),
            );
            let out_file = std::fs::File::create(&log_path)?;
            let err_file = out_file.try_clone()?;
            let (in_r, in_w) = std::io::pipe()?;
            drop(in_w);
            let exec = deno_task_shell::execute_with_pipes(
                list,
                state,
                deno_task_shell::ShellPipeReader::from_raw(in_r),
                deno_task_shell::ShellPipeWriter::from_std(out_file),
                deno_task_shell::ShellPipeWriter::from_std(err_file),
            );
            Ok(tokio::runtime::Handle::current().block_on(exec))
        };
        let code = run().unwrap_or(-1);
        let _ = std::fs::write(&exit_path, format!("{{\"exit_code\":{code}}}"));
        if let Some(s) = &sink {
            s.on_event(&crate::agent::LiveEvent::Hook {
                event: "jobs.changed".into(),
                detail: format!("{job_id} exit {code}"),
            });
        }
    });

    Ok(ToolResult {
        output: format!(
            "{preamble}job {id} started; log: {}",
            log_path_for_msg.display()
        ),
        ok: true,
    })
}

pub struct JobOutputTool;

#[async_trait::async_trait]
impl ToolImpl for JobOutputTool {
    fn name(&self) -> &'static str {
        "JobOutput"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "JobOutput",
            "Read incremental output of a background Bash job. Pass `offset` from a              previous call to get only new bytes (default 0). Returns status + chunk.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "job id, e.g. j-1234"},
                    "offset": {"type": "integer", "description": "byte offset to resume from"}
                },
                "required": ["id"]
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
            id: String,
            offset: Option<u64>,
        }
        let a: Args = serde_json::from_value(args)?;
        if !a.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("bad job id");
        }
        let dir = jobs_dir(ctx).join(&a.id);
        let offset = a.offset.unwrap_or(0);
        let data = tokio::fs::read(dir.join("output.log"))
            .await
            .with_context(|| format!("no such job: {}", a.id))?;
        const CAP: usize = 8 * 1024;
        let start = (offset as usize).min(data.len());
        let end = (start + CAP).min(data.len());
        let chunk = crate::console::console_text(&data[start..end]);
        let status = match std::fs::read_to_string(dir.join("exit.json")) {
            Ok(s) => s,
            Err(_) => "running".into(),
        };
        Ok(ToolResult {
            output: format!(
                "[{id} {status}] bytes {start}..{end}/{total}
{chunk}",
                id = a.id,
                total = data.len()
            ),
            ok: true,
        })
    }
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
