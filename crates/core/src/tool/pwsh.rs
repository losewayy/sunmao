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

use std::sync::Arc;

use anyhow::Context as _;
use base64::Engine as _;

use super::shell::ShellRun;
use crate::context::MutexRecover;

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

/// Foreground pwsh run — same contract as the deno path: partial output is
/// kept on timeout/cancel, `ended` names how it stopped.
pub async fn run_foreground(
    command: &str,
    cwd: std::path::PathBuf,
    timeout_secs: u64,
    cancel: Option<Arc<tokio::sync::Notify>>,
) -> Result<ShellRun, String> {
    let mut child = pwsh_command(command)
        .current_dir(&cwd)
        .spawn()
        .map_err(|e| format!("cannot spawn pwsh: {e}"))?;

    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    // the buffers live outside the drain tasks: a detached grandchild
    // keeps the pipe's write end open past exit, and `read_to_end` would
    // hang forever — the timeout below keeps the partial bytes instead.
    let out_buf = super::shell::SharedBuf::default();
    let err_buf = super::shell::SharedBuf::default();
    let out_task = {
        let buf = out_buf.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            loop {
                match tokio::io::AsyncReadExt::read(&mut out_pipe, &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.0.lock_or_recover().extend_from_slice(&chunk[..n]),
                }
            }
        })
    };
    let err_task = {
        let buf = err_buf.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            loop {
                match tokio::io::AsyncReadExt::read(&mut err_pipe, &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.0.lock_or_recover().extend_from_slice(&chunk[..n]),
                }
            }
        })
    };

    let cancel_fut = async {
        match &cancel {
            Some(n) => n.notified().await,
            None => std::future::pending::<()>().await,
        }
    };

    enum End {
        Natural(i32),
        Timeout,
        Cancelled,
    }
    let end = tokio::select! {
        r = child.wait() => match r {
            Ok(s) => End::Natural(s.code().unwrap_or(-1)),
            Err(e) => return Err(format!("pwsh wait failed: {e}")),
        },
        () = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => End::Timeout,
        () = cancel_fut => End::Cancelled,
    };

    let (code, ended) = match end {
        End::Natural(c) => (c, None),
        End::Timeout => {
            // start_kill, then still wait — the pipes must drain and the
            // process must reap; `kill_on_drop` alone can't give us output.
            let _ = child.start_kill();
            let c = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
            (c, Some(format!("timed out after {timeout_secs}s — killed")))
        }
        End::Cancelled => {
            let _ = child.start_kill();
            let c = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
            (c, Some("cancelled by user — killed".to_string()))
        }
    };

    // pwsh writes pipes in [Console]::OutputEncoding (the OEM codepage
    // unless the box opted into UTF-8) — console_text covers both.
    // The join is bounded: a detached grandchild holding a pipe's write
    // end would otherwise hang the run past pwsh's own exit.
    let truncated = tokio::time::timeout(
        super::shell::PIPE_DRAIN_TIMEOUT,
        futures_util::future::join(out_task, err_task),
    )
    .await
    .is_err();
    let mut ended = ended;
    if truncated {
        let tag = format!(
            "output truncated — pipes still held {}s after exit",
            super::shell::PIPE_DRAIN_TIMEOUT.as_secs()
        );
        ended = Some(match ended {
            Some(e) => format!("{e}; {tag}"),
            None => tag,
        });
    }
    let stdout = out_buf.text();
    let stderr = err_buf.text();

    Ok(ShellRun {
        exit_code: code,
        stdout,
        stderr,
        preflight: String::new(),
        ended,
    })
}

/// Background pwsh job — same `.sunmao/jobs/{id}/` shape as the deno path
/// so `JobOutput` reads it identically. Output goes to `output.log`; exit
/// lands in `exit.json`. The `Notify` handle is the cancel wire for the
/// job — a stopped turn kills a background pwsh too (POSIX jobs are
/// intentionally detached and unaffected).
pub async fn spawn_background(
    command: &str,
    ctx: &crate::context::Context,
) -> anyhow::Result<super::ToolResult> {
    // ms + the per-Context seq — two spawns in the same millisecond
    // used to collide on `j-<ms>` and share one output.log.
    let seq = ctx
        .job_seq
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!(
        "j-{}-{seq}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    );
    let dir = super::shell::jobs_dir(ctx).join(&id);
    std::fs::create_dir_all(&dir)?;
    let log_path = dir.join("output.log");
    let exit_path = dir.join("exit.json");

    let sink = ctx.live_sink.get().cloned();
    if let Some(s) = &sink {
        s.on_event(&crate::agent::LiveEvent::Hook {
            event: "jobs.changed".into(),
            detail: format!("{id} started"),
        });
    }

    let mut child = pwsh_command(command)
        .current_dir(&ctx.cwd)
        .spawn()
        .with_context(|| "cannot spawn pwsh (background)")?;
    // background jobs stream their own output.log — a chatty long-runner
    // would grow an in-memory Vec unboundedly (and JobOutput reads nothing
    // until exit) if we buffered; drain both pipes straight to disk instead.
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let log_path2 = log_path.clone();
    let err_path = dir.join("stderr.log");
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let write_err = async {
            if let Ok(mut f) = tokio::fs::File::create(&err_path).await {
                let _ = tokio::io::copy(&mut err_pipe, &mut f).await;
                let _ = f.flush().await;
            }
        };
        let write_out = async {
            if let Ok(mut f) = tokio::fs::File::create(&log_path2).await {
                let _ = tokio::io::copy(&mut out_pipe, &mut f).await;
                let _ = f.flush().await;
            }
        };
        tokio::join!(write_out, write_err);
    });

    let job_id = id.clone();
    let sink2 = sink.clone();
    tokio::spawn(async move {
        // no timeout, no cancel wire — a background job is meant to outlive
        // the turn; kill it by deleting its process tree externally.
        let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = std::fs::write(&exit_path, format!("{{\"exit_code\":{code}}}"));
        if let Some(s) = &sink2 {
            s.on_event(&crate::agent::LiveEvent::Hook {
                event: "jobs.changed".into(),
                detail: format!("{job_id} exit {code}"),
            });
        }
    });

    Ok(super::ToolResult {
        output: format!("job {id} started; log: {}", log_path.display()),
        ok: true,
    })
}
