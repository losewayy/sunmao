//! Hook subprocess runner — the embedded shell + drain/kill discipline.
//!
//! Split out of `hooks.rs` when the dispatcher file hit the 600-line shape
//! budget. Two correctness rules live here and nowhere else:
//!   1. stdout/stderr drain *concurrently* with exec — a child that fills
//!      the OS pipe buffer before finishing deadlocks a post-exec drain.
//!   2. a timed-out hook is SIGKILLed through the ShellState kill signal,
//!      never orphaned by dropping the exec future.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

/// A hook process that outlives this budget is abandoned — hooks advise
/// the loop, they must never be able to hang it.
const HOOK_TIMEOUT_SECS: u64 = 60;

/// Hook commands get the payload on stdin and run under the embedded shell,
/// same as the Bash tool — one execution model for all shell surfaces.
/// `timeout_secs` overrides the global budget (cursor per-entry `timeout`).
pub(super) async fn run_hook_command(
    command: &str,
    payload: &Value,
    cwd: &Path,
    timeout_secs: Option<u64>,
) -> anyhow::Result<(i32, String, String)> {
    let command = command.to_string();
    let payload = serde_json::to_string(payload)?;
    let cwd = cwd.to_path_buf();
    let budget = timeout_secs.unwrap_or(HOOK_TIMEOUT_SECS);
    tokio::task::spawn_blocking(move || -> anyhow::Result<(i32, String, String)> {
        let list = deno_task_shell::parser::parse(&command)
            .map_err(|e| anyhow::anyhow!("bad hook command: {e}"))?;
        let env_vars: HashMap<std::ffi::OsString, std::ffi::OsString> =
            std::env::vars_os().collect();
        let state =
            deno_task_shell::ShellState::new(env_vars, cwd, Default::default(), Default::default());
        let kill = state.kill_signal().clone();
        let (out_r, out_w) = deno_task_shell::pipe();
        let (err_r, err_w) = deno_task_shell::pipe();
        // stdin carries the JSON payload. A writer thread guards against
        // pipe-buffer backpressure on large PostToolUse payloads; join it
        // after exec — when write_all returns, in_w drops → child sees EOF.
        // (Detaching without join is what the old code did; on a slow
        // scheduler the EOF could arrive late, stalling stdin-blocking hooks.)
        let (in_r, mut in_w) = std::io::pipe()?;
        let feed_thread = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = in_w.write_all(payload.as_bytes());
        });
        let mut exec = std::pin::pin!(deno_task_shell::execute_with_pipes(
            list,
            state,
            deno_task_shell::ShellPipeReader::from_raw(in_r),
            out_w,
            err_w,
        ));
        let rt = tokio::runtime::Handle::current();
        // stdout/stderr drain concurrently — a hook that fills the pipe
        // buffer before exec resolves deadlocks otherwise (same shape the
        // Bash tool had). Timeout SIGKILLs rather than orphaning the child.
        let (code, out, err) = rt.block_on(async {
            let out_drain = tokio::task::spawn_blocking(move || {
                let mut b = Vec::new();
                out_r.pipe_to(&mut b).ok();
                crate::console::console_text(&b)
            });
            let err_drain = tokio::task::spawn_blocking(move || {
                let mut b = Vec::new();
                err_r.pipe_to(&mut b).ok();
                crate::console::console_text(&b)
            });
            enum End {
                Natural(i32),
                Timeout,
            }
            let end = tokio::select! {
                c = &mut exec => End::Natural(c),
                () = tokio::time::sleep(std::time::Duration::from_secs(budget)) => End::Timeout,
            };
            let code = match end {
                End::Natural(c) => Ok(c),
                End::Timeout => {
                    kill.send(deno_task_shell::SignalKind::SIGKILL);
                    let _ = (&mut exec).await; // reap — killed children exit
                    Err(())
                }
            };
            let out = out_drain.await.unwrap_or_default();
            let err = err_drain.await.unwrap_or_default();
            (code, out, err)
        });
        let code = match code {
            Ok(c) => c,
            Err(()) => {
                // a hung hook must not stall the agent. The feed thread is
                // deliberately NOT joined — joining a still-writing stdin
                // would re-create the stall we're escaping.
                anyhow::bail!("hook timed out after {budget}s");
            }
        };
        let _ = feed_thread.join();
        Ok((code, out, err))
    })
    .await?
}
