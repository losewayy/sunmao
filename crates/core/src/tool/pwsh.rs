//! PowerShell 7 execution backend — selected by `SUNMAO_SHELL=pwsh` or a
//! project's `.sunmao/shell.txt`. `pwsh` is spawned with `-EncodedCommand`
//! (UTF-16LE base64) so the command string survives CreateProcess quoting
//! untouched — the same trick `powershell.exe` callers have used forever.
//!
//! Two spawn invariants that differ from the deno path:
//!  - `kill_on_drop(true)` + `CREATE_NO_WINDOW` — a dropped future must not
//!    orphan pwsh, and a GUI-subsystem host must not pop a console window.
//!  - stdout/stderr are drained on their own tasks — a pipe buffer fills
//!    (~64KB) and the child deadlocks if nobody reads while we wait().
//!
//! Both streams fan out to the job log and a bounded in-memory copy, the
//! same two-consumer shape the deno path uses; a pwsh job is stopped by
//! killing its process tree.

use anyhow::Context as _;
use base64::Engine as _;

use super::foreground::LocalShell;
use super::jobs;
use super::shell::ShellRun;
use crate::tool::ToolResult;

/// `pwsh -NoProfile -NonInteractive -EncodedCommand <utf16le-b64>`.
/// EncodedCommand instead of `-Command` because the latter round-trips the
/// script through two argv re-parses (ours + pwsh's) — braces, quotes and
/// `$` come through mangled on real commands.
fn encoded(command: &str) -> String {
    let utf16: Vec<u8> = command
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

fn pwsh_command(command: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("pwsh");
    c.arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-EncodedCommand")
        .arg(encoded(command))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // a dropped child future must take its pwsh with it — the default
        // leaks a live process when the caller selects cancel/timeout
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW — belt-and-suspenders next to the GUI's hidden
        // console (gui/main.rs hides the inherited one at startup); covers
        // any foreground spawn from a console-less host. tokio's Command
        // has creation_flags built in — no windows/CommandExt import needed.
        c.creation_flags(0x0800_0000);
    }
    c
}

/// `Bash` on the pwsh backend: register the run as a job, wait only as long
/// as the budget allows, then either render the result or move it to the
/// background.
pub(crate) async fn bash(
    command: &str,
    timeout: Option<u64>,
    background: bool,
    ctx: &crate::context::Context,
) -> anyhow::Result<ToolResult> {
    let timeout_secs = super::timeout::effective_timeout(timeout);
    if background {
        return spawn_background(command, ctx).await;
    }
    let run = local_shell(command, &ctx.cwd, timeout_secs, ctx)
        .await
        .map_err(anyhow::Error::msg)?;
    Ok(ToolResult {
        exit_code: run.exit_code(),
        output: run.render(),
        ok: run.ok(),
    })
}

/// pwsh's half of the shared foreground run: registered in `ctx.jobs` from
/// its first byte, so reaching the budget MOVES THE COMMAND TO THE BACKGROUND
/// instead of killing it. The deno twin is `shell::foreground_run`; both feed
/// the tool result and the frontends' `!` local shell.
pub(crate) async fn local_shell(
    command: &str,
    cwd: &std::path::Path,
    timeout_secs: u64,
    ctx: &crate::context::Context,
) -> Result<LocalShell, String> {
    let paths = jobs::JobPaths::create(ctx, jobs::next_job_id()).map_err(|e| format!("{e:#}"))?;
    let notifier = jobs::JobNotifier::from_ctx(ctx).await;
    let mut run = match spawn_run(command, cwd, Some(&paths)) {
        Ok(r) => r,
        Err(e) => {
            paths.discard(); // a dir with no run reads as "still running"
            return Err(format!("{e:#}"));
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
        stderr: declixml(&run.err.text()),
        preflight: String::new(),
        ended: jobs::ended_note(end.ended, label),
    };
    jobs::conclude(&notifier, &ctx.jobs, &run.id, &run.dir, end.code, false).await;
    // same rule as the deno path: an inline run retires itself, dir and all
    jobs::retire(&ctx.jobs, &run.id, &run.dir);
    Ok(LocalShell::Done(run_out))
}

/// Foreground pwsh run — same contract as the deno path: partial output is
/// kept on timeout/cancel, `ended` names how it stopped. No job identity,
/// so it keeps the plain kill-on-timeout behavior.
pub async fn run_foreground(
    command: &str,
    cwd: std::path::PathBuf,
    timeout_secs: u64,
    cancel: Option<crate::context::CancelSignal>,
) -> Result<ShellRun, String> {
    let mut run = spawn_run(command, &cwd, None).map_err(|e| format!("{e:#}"))?;
    let fg = jobs::wait_foreground(&mut run, timeout_secs, false, cancel).await;
    let label = fg.label(timeout_secs);
    let end = fg.end().unwrap_or_else(jobs::RunEnd::lost);
    Ok(ShellRun {
        exit_code: end.code,
        stdout: run.out.text(),
        stderr: declixml(&run.err.text()),
        preflight: String::new(),
        ended: jobs::ended_note(end.ended, label),
    })
}

/// Redirected pwsh serializes stderr as CLIXML — `#< CLIXML` + `<S S="Error">`
/// elements whose payload is `_xNNNN_`-escaped (ESC becomes `_x001B_`, so
/// ANSI codes arrive double-encoded). A model can't read that; lift the
/// `<S>` payloads, undo the escapes, and strip the ANSI sequences they hide.
/// Non-CLIXML stderr passes through untouched.
fn declixml(stderr: &str) -> String {
    if !stderr.trim_start().starts_with("#< CLIXML") {
        return stderr.to_string();
    }
    let mut out = String::new();
    let mut rest = stderr;
    while let Some(i) = rest.find("<S ") {
        rest = &rest[i..];
        let (Some(gt), Some(end)) = (rest.find('>'), rest.find("</S>")) else {
            break;
        };
        if gt < end {
            out.push_str(&rest[gt + 1..end]);
        }
        rest = &rest[end + 4..];
    }
    strip_ansi(&unescape_clixml(&out))
}

/// `_xHHHH_` → the char it encodes (CR/LF/ESC/whatever pwsh hid).
fn unescape_clixml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '_' && it.peek() == Some(&'x') {
            let tail: String = it.by_ref().take(6).collect();
            // tail is "xHHHH_" — 1 + 4 hex + '_'
            if tail.len() == 6
                && tail.starts_with('x')
                && tail.ends_with('_')
                && let Some(v) = u32::from_str_radix(&tail[1..5], 16)
                    .ok()
                    .and_then(char::from_u32)
            {
                out.push(v);
                continue;
            }
            out.push('_');
            out.push_str(&tail);
        } else {
            out.push(c);
        }
    }
    out
}

/// CSI/OSC-free text: drop `ESC[…final` sequences (colors, cursor moves).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            // consume params/intermediates until the final byte @..~
            while let Some(&n) = it.peek() {
                it.next();
                if ('@'..='~').contains(&n) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Start a pwsh run as a job. The child is owned by its own wait task, so
/// the caller can simply stop waiting; stdout and stderr drain straight to
/// the job log and to the bounded in-memory copy.
fn spawn_run(
    command: &str,
    cwd: &std::path::Path,
    job: Option<&jobs::JobPaths>,
) -> anyhow::Result<jobs::JobRun> {
    let mut child = pwsh_command(command)
        .current_dir(cwd)
        .spawn()
        .with_context(|| "cannot spawn pwsh")?;
    let pid = child.id();
    let log_path = job.map(|j| j.log());
    let (id, dir) = match job {
        Some(j) => (j.id.clone(), j.dir.clone()),
        None => (String::new(), std::path::PathBuf::new()),
    };

    let out_mem = jobs::CappedBuf::default();
    let err_mem = jobs::CappedBuf::default();
    // stdout and stderr merge into one log — stderr the model can't see is
    // a tool that fails silently, and `JobOutput` reads a single file.
    let out_task = spawn_drain(
        child.stdout.take().expect("piped stdout"),
        log_path.clone(),
        out_mem.clone(),
    );
    let err_task = spawn_drain(
        child.stderr.take().expect("piped stderr"),
        log_path.clone(),
        err_mem.clone(),
    );

    let kill = jobs::KillSwitch::new();
    let kill_watch = kill.clone();

    let (end_tx, release) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // no clock and no cancel wire of our own — a background job is meant
        // to outlive the turn; whoever is waiting enforces its own budget
        // through the kill switch (see timeout.rs). The watch races the
        // child's own exit, so a late stop request can never hit a pid the
        // OS has already recycled.
        let wait = child.wait();
        tokio::pin!(wait);
        let code = match pid {
            Some(p) => tokio::select! {
                r = &mut wait => r.ok().and_then(|s| s.code()).unwrap_or(-1),
                () = kill_watch.wait() => {
                    jobs::kill_tree(p);
                    wait.await.ok().and_then(|s| s.code()).unwrap_or(-1)
                }
            },
            None => wait.await.ok().and_then(|s| s.code()).unwrap_or(-1),
        };
        // a detached grandchild holding a pipe's write end would otherwise
        // hang this wait past pwsh's own exit — bound it and keep the bytes
        let truncated = tokio::time::timeout(
            super::PIPE_DRAIN_TIMEOUT,
            futures_util::future::join(out_task, err_task),
        )
        .await
        .is_err();
        let ended = truncated.then(|| {
            format!(
                "output truncated — pipes still held {}s after exit",
                super::PIPE_DRAIN_TIMEOUT.as_secs()
            )
        });
        let _ = end_tx.send(jobs::RunEnd { code, ended });
    });

    Ok(jobs::JobRun {
        id,
        dir,
        log_path: log_path.unwrap_or_default(),
        out: out_mem,
        err: err_mem,
        pid,
        started_at: jobs::now_ms(),
        kill,
        release: Some(release),
    })
}

/// A `background: true` spawn — an explicit job, so no clock of its own.
async fn spawn_background(
    command: &str,
    ctx: &crate::context::Context,
) -> anyhow::Result<ToolResult> {
    let paths = jobs::JobPaths::create(ctx, jobs::next_job_id())?;
    let notifier = jobs::JobNotifier::from_ctx(ctx).await;
    let run = match spawn_run(command, &ctx.cwd, Some(&paths)) {
        Ok(r) => r,
        Err(e) => {
            paths.discard();
            return Err(e);
        }
    };
    run.register(&ctx.jobs, command, false);
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
        output: format!("job {} started; log: {}", paths.id, log_path.display()),
        ok: true,
    })
}

/// One pipe reader, two consumers: the job log (durable) and the bounded
/// in-memory copy the foreground result renders from.
fn spawn_drain<R>(
    mut pipe: R,
    log: Option<std::path::PathBuf>,
    mem: jobs::CappedBuf,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // append handles, so concurrent writes concatenate rather than
        // overwrite each other's offsets
        let mut file = match &log {
            Some(p) => tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .await
                .ok(),
            None => None,
        };
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(f) = &mut file {
                        let _ = f.write_all(&chunk[..n]).await;
                    }
                    mem.push(&chunk[..n]);
                }
            }
        }
        if let Some(f) = &mut file {
            let _ = f.flush().await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::bash;

    struct StubLlm;

    #[async_trait::async_trait]
    impl sunmao_llm::ProviderAdapter for StubLlm {
        async fn stream(
            &self,
            _req: sunmao_llm::ChatRequest<'_>,
        ) -> anyhow::Result<sunmao_llm::DeltaStream> {
            Ok(Box::pin(futures_util::stream::empty()))
        }
    }

    /// A background job's stderr must land in output.log — JobOutput only
    /// reads that file; a separate stderr.log made the model blind to why
    /// the job failed (deno merges both into output.log via try_clone).
    #[cfg(windows)]
    #[tokio::test]
    async fn background_stderr_lands_in_output_log() {
        if std::process::Command::new("pwsh")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no pwsh on this box — the path is untestable here
        }
        let dir = crate::fresh_test_dir("pwsh-bg");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = std::sync::Arc::new(crate::context::Context::new(
            std::sync::Arc::new(StubLlm),
            crate::session::SessionLog::ephemeral(),
            crate::tool::builtin_registry(),
            dir.clone(),
        ));
        bash(
            "[Console]::Error.WriteLine('pwsh-err-marker'); 'pwsh-out-marker'",
            None,
            true,
            &ctx,
        )
        .await
        .unwrap();

        let jobs = dir.join(".sunmao").join("jobs");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let log = loop {
            let done = std::fs::read_dir(&jobs).ok().and_then(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .find(|p| p.join("exit.json").exists())
            });
            if let Some(d) = done {
                break d.join("output.log");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job never wrote exit.json"
            );
            // async sleep — a blocking one would starve the spawned copy
            // and wait tasks on this single-threaded test runtime
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        };
        // pipe-copy tasks race the exit write — give output.log a beat
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let out = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(out.contains("pwsh-out-marker"), "stdout: {out}");
        assert!(out.contains("pwsh-err-marker"), "stderr must merge: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A pwsh FOREGROUND command that overruns its budget is moved to the
    /// background, not killed: the child keeps running and logging, and the
    /// tool result hands over the job (id + log) instead of a corpse. This is
    /// the pwsh half of the deno case in `jobs::tests`; the old kill-on-
    /// timeout behaviour would show up here as a dead `Start-Sleep`.
    #[cfg(windows)]
    #[tokio::test]
    async fn foreground_timeout_moves_a_pwsh_command_to_the_background() {
        if std::process::Command::new("pwsh")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no pwsh on this box — the path is untestable here
        }
        let dir = crate::fresh_test_dir("pwsh-fg-timeout");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = std::sync::Arc::new(crate::context::Context::new(
            std::sync::Arc::new(StubLlm),
            crate::session::SessionLog::ephemeral(),
            crate::tool::builtin_registry(),
            dir.clone(),
        ));

        // two 4s sleeps, a 1s budget: the first pair must land in the log
        // AFTER the tool has already returned
        let res = bash(
            "'pwsh-first'; Start-Sleep -Seconds 4; 'pwsh-second'; Start-Sleep -Seconds 4; 'pwsh-third'",
            Some(1),
            false,
            &ctx,
        )
        .await
        .unwrap();
        assert!(
            !res.output.contains("killed"),
            "a pwsh timeout must not kill the command: {}",
            res.output
        );
        assert!(
            res.output.contains("moved to the background"),
            "the tool result must hand over the job: {}",
            res.output
        );
        assert!(res.output.contains("log:"), "{}", res.output);

        let jobs = dir.join(".sunmao").join("jobs");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let log = loop {
            let later = std::fs::read_dir(&jobs).ok().and_then(|rd| {
                rd.flatten().map(|e| e.path().join("output.log")).find(|p| {
                    std::fs::read_to_string(p)
                        .unwrap_or_default()
                        .contains("pwsh-second")
                })
            });
            if let Some(l) = later {
                break l;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the backgrounded pwsh command never produced its later output"
            );
            // async sleep — a blocking one would starve the drain and wait
            // tasks on this single-threaded test runtime
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        };
        // it is a background job now: the panel marker flipped, and it still
        // settles its own exit.json
        let meta = std::fs::read_to_string(log.with_file_name("job.json")).unwrap_or_default();
        assert!(
            meta.contains("\"foreground\":false"),
            "a detached job must be visible to the jobs panel: {meta}"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !log.with_file_name("exit.json").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the detached pwsh command never settled its exit.json"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        let out = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(out.contains("pwsh-third"), "the run must finish: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod declixml_tests {
    use super::*;

    #[test]
    fn declixml_lifts_error_text_and_strips_escapes() {
        let raw = "#< CLIXML\r\n<Objs Version=\"1.1.0.1\"><S S=\"Error\">_x001B_[31;1mbadcmd : \u{672f}\u{8bed} _x001B_[0m_x000D__x000A_</S></Objs>";
        let out = declixml(raw);
        assert!(out.contains("badcmd :"), "{out}");
        assert!(out.contains('\u{672f}'), "{out}");
        assert!(!out.contains("_x001B_"), "{out}");
        assert!(!out.contains('\x1b'), "{out}");
        assert!(!out.contains("<S"), "{out}");
        // CRLF escapes decode to real newlines
        assert!(out.contains('\n'), "{out}");
    }

    #[test]
    fn declixml_passes_plain_stderr_through() {
        let raw = "rm : cannot remove\nplain error";
        assert_eq!(declixml(raw), raw);
    }
}
