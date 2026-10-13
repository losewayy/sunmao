//! The model-facing job tools — read, list, stop.
//!
//! All three are thin: the truth is the job's log on disk plus the owning
//! context's [`crate::tool::jobs::JobTable`]. `JobStop` deliberately has no
//! permission logic of its own — it is classified as a mutation by the
//! dispatch gate (`agent/mode.rs`), which is where the approval stance
//! lives.

use crate::tool::jobs;
use crate::tool::*;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct JobOutputTool;

#[async_trait::async_trait]
impl ToolImpl for JobOutputTool {
    fn name(&self) -> &'static str {
        "JobOutput"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "JobOutput",
            "Read a background job's output.log. Never blocks and never waits \
             for the job: it returns a snapshot of what the log holds right \
             now. Without `offset` it returns the TAIL (~8KB), which is what \
             you want for a long-running job; pass `offset` from a previous \
             call to resume where it left off. For the whole log, Read the \
             `log:` path it prints.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "job id, e.g. j-1234"},
                    "offset": {"type": "integer", "description": "byte offset to resume from; omit for the tail"}
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
            return Ok(ToolResult {
                exit_code: None,
                output: "bad job id".into(),
                ok: false,
            });
        }
        let log_path = jobs::jobs_dir(ctx).join(&a.id).join("output.log");
        let data = match tokio::fs::read(&log_path).await {
            Ok(d) => d,
            Err(_) => {
                return Ok(ToolResult {
                    exit_code: None,
                    output: format!("no such job: {}", a.id),
                    ok: false,
                });
            }
        };
        const CAP: usize = 8 * 1024;
        let start = match a.offset {
            Some(o) => (o as usize).min(data.len()),
            None => data.len().saturating_sub(CAP),
        };
        let end = (start + CAP).min(data.len());
        let chunk = crate::console::console_text(&data[start..end]);
        let status = match std::fs::read_to_string(log_path.with_file_name("exit.json")) {
            Ok(s) => s,
            Err(_) => "running".into(),
        };
        Ok(ToolResult {
            exit_code: None,
            output: format!(
                "[{id} {status}] bytes {start}..{end}/{total}
log: {path}
{chunk}",
                id = a.id,
                total = data.len(),
                path = log_path.display(),
            ),
            ok: true,
        })
    }
}

pub struct JobListTool;

#[async_trait::async_trait]
impl ToolImpl for JobListTool {
    fn name(&self) -> &'static str {
        "JobList"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "JobList",
            "List this session's background jobs with their status — running, \
             moved to the background, or exited with a code. Newest first; the \
             newest 20 unless `limit` says otherwise. Read-only; use JobOutput \
             or Read for a job's output.",
            json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "rows to return (default 20, max 100)"}
                }
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
            limit: Option<usize>,
        }
        /// A session can register hundreds of jobs; the list is a working set,
        /// so it pages like one rather than printing the whole roster.
        const DEFAULT_LIMIT: usize = 20;
        const MAX_LIMIT: usize = 100;
        let a: Args = serde_json::from_value(args)?;
        let limit = a.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let entries = jobs::snapshot(&ctx.jobs);
        if entries.is_empty() {
            return Ok(ToolResult {
                exit_code: None,
                output: "no background jobs in this session".into(),
                ok: true,
            });
        }
        let now = jobs::now_ms();
        let shown = entries.len().min(limit);
        let mut out = if shown < entries.len() {
            format!(
                "{shown} newest of {} job(s), newest first:\n",
                entries.len()
            )
        } else {
            format!("{} job(s), newest first:\n", entries.len())
        };
        for e in entries.iter().take(limit) {
            let pid = e.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
            let origin = if e.foreground {
                "foreground"
            } else {
                "background"
            };
            out.push_str(&format!(
                "{} | {} | pid {} | {} | {}s ago | {} | {}\n",
                e.id,
                e.status.label(),
                pid,
                origin,
                now.saturating_sub(e.started_at) / 1000,
                one_line(&e.command, 80),
                e.output_path.display(),
            ));
        }
        Ok(ToolResult {
            exit_code: None,
            output: out,
            ok: true,
        })
    }
}

pub struct JobStopTool;

#[async_trait::async_trait]
impl ToolImpl for JobStopTool {
    fn name(&self) -> &'static str {
        "JobStop"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "JobStop",
            "Stop a background job by killing its process tree. Use it when a \
             job is no longer wanted or is wedged; you do not need it just to \
             learn a job's result — completion is delivered automatically.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "job id from JobList"}
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
        }
        let a: Args = serde_json::from_value(args)?;
        let Some(entry) = jobs::stop_wire(&ctx.jobs, &a.id) else {
            return Ok(ToolResult {
                exit_code: None,
                output: format!("no such job: {}", a.id),
                ok: false,
            });
        };
        if !entry.status.is_running() {
            return Ok(ToolResult {
                exit_code: None,
                output: format!("job {} already finished ({})", a.id, entry.status.label()),
                ok: false,
            });
        }
        // kill out from under the lock — the registry guard is already gone
        entry.stop.kill();
        let pid = entry.pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
        Ok(ToolResult {
            exit_code: None,
            output: format!(
                "stopping job {}{pid}; its completion will still be reported",
                a.id
            ),
            ok: true,
        })
    }
}

pub struct WaitForTool;

#[async_trait::async_trait]
impl ToolImpl for WaitForTool {
    fn name(&self) -> &'static str {
        "WaitFor"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "WaitFor",
            "Wait for a background job (j-…) or detached sub-agent (sub-…) to \
             finish, up to `timeout` seconds. Returns the exit code and log \
             tail for jobs, or the final message for sub-agents; if the task \
             is still running when the budget lapses it returns 'still \
             running' — call again rather than polling in a loop. Use this \
             instead of sleeping when you expect a short finish; completions \
             also arrive unprompted as tagged messages, so never block on a \
             long task.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "job id (j-…) from Bash/Jobs, or sub-agent id (sub-…) from Task"},
                    "timeout": {"type": "integer", "description": "seconds to wait (default 30, max 90)"}
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
            timeout: Option<u64>,
        }
        let a: Args = serde_json::from_value(args)?;
        const MAX_WAIT: u64 = 90;
        let timeout = a.timeout.unwrap_or(30).clamp(1, MAX_WAIT);
        if a.id.starts_with("sub-") {
            wait_task(&a.id, timeout, ctx).await
        } else {
            wait_job(&a.id, timeout, ctx).await
        }
    }
}

/// The wait's deadline, shared by both id kinds.
fn deadline(secs: u64) -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(secs)
}

/// A job (`j-…`): the disk is the truth — `exit.json` when the run ends,
/// `job_lost` when the runner process died without writing one. Polls on a
/// short beat; the registry can't help here because a restarted session
/// has the dir but no table row.
async fn wait_job(
    id: &str,
    secs: u64,
    ctx: &Arc<crate::context::Context>,
) -> anyhow::Result<ToolResult> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Ok(ToolResult {
            exit_code: None,
            output: "bad job id".into(),
            ok: false,
        });
    }
    let dir = jobs::jobs_dir(ctx).join(id);
    if !dir.is_dir() {
        return Ok(ToolResult {
            exit_code: None,
            output: format!("no such job: {id}"),
            ok: false,
        });
    }
    let end_at = deadline(secs);
    loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("exit.json")) {
            let tail = tail_of(&dir.join("output.log"), 8 * 1024);
            return Ok(ToolResult {
                exit_code: None,
                output: format!(
                    "[{id} finished {s}]\nlog: {}\n{tail}",
                    dir.join("output.log").display()
                ),
                ok: true,
            });
        }
        if jobs::job_lost(&dir, id) {
            let tail = tail_of(&dir.join("output.log"), 8 * 1024);
            return Ok(ToolResult {
                exit_code: None,
                output: format!(
                    "[{id} lost — the process that ran it is gone; no exit code was ever written]\nlog: {}\n{tail}",
                    dir.join("output.log").display()
                ),
                ok: false,
            });
        }
        if tokio::time::Instant::now() >= end_at {
            return Ok(ToolResult {
                exit_code: None,
                output: format!(
                    "[{id} still running after {secs}s]\nlog: {}",
                    dir.join("output.log").display()
                ),
                ok: true,
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// A detached sub-agent (`sub-…`): sleeps on the roster's done_notify, then
/// reads the child's final assistant message off its own log — the same
/// text a pushed TaskDone would carry, without waiting for the push.
async fn wait_task(
    id: &str,
    secs: u64,
    ctx: &Arc<crate::context::Context>,
) -> anyhow::Result<ToolResult> {
    let (done, notify) = {
        let tasks = ctx.live_tasks.lock_or_recover();
        match tasks.iter().find(|t| t.id == id) {
            None => (None, None),
            Some(t) => (Some(t.done), Some(t.done_notify.clone())),
        }
    };
    let Some(notify) = notify else {
        // the roster is process-local — a finished-elsewhere child still
        // has its log; an id neither knows is a real miss
        let path = crate::session::session_log_path(&ctx.cwd, id);
        return if path.exists() {
            match last_assistant_text(&path) {
                Some(text) => Ok(ToolResult {
                    exit_code: None,
                    output: format!("[{id} finished — roster is empty after a restart]\n\n{text}"),
                    ok: true,
                }),
                None => Ok(ToolResult {
                    exit_code: None,
                    output: format!(
                        "{id} has a log but no finished run this process knows — resume it to continue"
                    ),
                    ok: false,
                }),
            }
        } else {
            Ok(ToolResult {
                exit_code: None,
                output: format!("no such sub-agent: {id}"),
                ok: false,
            })
        };
    };
    let end_at = deadline(secs);
    let mut done = done.flatten();
    while done.is_none() {
        let n = notify.notified();
        tokio::pin!(n);
        n.as_mut().enable();
        // re-check after arming — a finish between the roster read and here
        // would otherwise be a permit nobody consumed
        done = ctx
            .live_tasks
            .lock_or_recover()
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.done);
        if done.is_some() {
            break;
        }
        if tokio::time::timeout_at(end_at, n).await.is_err() {
            return Ok(ToolResult {
                exit_code: None,
                output: format!("[{id} still running after {secs}s]"),
                ok: true,
            });
        }
    }
    let ok = done.unwrap_or(false);
    let path = crate::session::session_log_path(&ctx.cwd, id);
    let body = last_assistant_text(&path)
        .unwrap_or_else(|| "(child wrote no final message — see its log)".into());
    Ok(ToolResult {
        exit_code: None,
        output: format!(
            "[{id} {}]\nlog: {}\n\n{body}",
            if ok { "finished" } else { "failed" },
            path.display()
        ),
        ok,
    })
}

/// The last assistant message's text in a session log — a finished
/// sub-agent's result. Reads the tail only; a mid-write partial line is
/// skipped by the same newline boundary rule the log's own parser uses.
fn last_assistant_text(path: &std::path::Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let tail = &data[data.len().saturating_sub(2 * 1024 * 1024)..];
    let mut found = None;
    for line in tail.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(crate::session::SessionEvent::Message { message }) =
            serde_json::from_slice::<crate::session::SessionEvent>(line)
            && message.role == sunmao_llm::types::Role::Assistant
            && let Some(t) = message.content_text()
            && !t.trim().is_empty()
        {
            found = Some(t);
        }
    }
    found.map(|t| crate::agent::truncate_output(&t))
}

fn tail_of(path: &std::path::Path, n: usize) -> String {
    let Ok(data) = std::fs::read(path) else {
        return String::new();
    };
    let start = data.len().saturating_sub(n);
    crate::console::console_text(&data[start..])
}

/// First `max` chars, one line — commands are multi-line and long by nature.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{cut}…")
}
