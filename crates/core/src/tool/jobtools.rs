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

/// First `max` chars, one line — commands are multi-line and long by nature.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{cut}…")
}
