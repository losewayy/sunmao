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
            "Run a bash command (cross-platform; works identically on Windows). \
             Use for builds, tests, git, and anything without a dedicated tool. \
             Commands are preflighted before they run: a '[preflight]' block in \
             the result predicts spawn failures and mangled argv — fix the \
             command when it tells you one is doomed.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Bash command line"},
                    "timeout_secs": {"type": "integer", "description": "Kill after N seconds (default 120)"},
                    "background": {"type": "boolean", "description": "Run detached; returns a job id readable via JobOutput"}
                },
                "required": ["command"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            command: String,
            timeout_secs: Option<u64>,
            #[serde(default)]
            background: bool,
        }
        let a: Args = serde_json::from_value(args)?;

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

        let run = match run_parsed(list, ctx.cwd.clone(), a.timeout_secs.unwrap_or(120)).await {
            Ok(r) => r,
            Err(msg) => {
                return Ok(ToolResult {
                    output: msg,
                    ok: false,
                });
            }
        };

        // context-efficient output discipline: cap at ~8KB per side
        let mut out = pre() + &trunc(run.stdout.trim_end());
        if !run.stderr.trim().is_empty() {
            out.push_str(&format!("\n[stderr]\n{}", trunc(run.stderr.trim())));
        }
        if run.exit_code != 0 {
            out.push_str(&format!("\n[exit code {}]", run.exit_code));
        }
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
}

/// Parse + preflight + execute `command` in `cwd`. `Err(String)` is a
/// legible failure (parse error, timeout, spawn panic), not an anyhow —
/// callers render it as output, same contract as `ToolResult{ok:false}`.
pub async fn run_foreground(
    command: &str,
    cwd: std::path::PathBuf,
    timeout_secs: u64,
) -> Result<ShellRun, String> {
    let list = deno_task_shell::parser::parse(command)
        .map_err(|e| format!("cannot parse command: {e}"))?;
    let preflight = crate::preflight::advisories(&list, &cwd).join("\n");
    let mut run = run_parsed(list, cwd, timeout_secs).await?;
    run.preflight = preflight;
    Ok(run)
}

/// Execute an already-parsed command list. `deno_task_shell`'s internals are
/// `!Send` (`Rc<Cell>` exit-code cells) — every !Send value is constructed
/// *inside* the blocking closure, never moved in.
async fn run_parsed(
    list: deno_task_shell::parser::SequentialList,
    cwd: std::path::PathBuf,
    timeout_secs: u64,
) -> Result<ShellRun, String> {
    let env_vars: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
        std::env::vars_os().collect();

    let outcome = tokio::task::spawn_blocking(move || -> Result<(i32, String, String), String> {
        let state =
            deno_task_shell::ShellState::new(env_vars, cwd, Default::default(), Default::default());
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
        let code = rt
            .block_on(tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                exec,
            ))
            .map_err(|_| format!("command timed out after {timeout_secs}s"))?;
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        out_reader.pipe_to(&mut out_buf).ok();
        err_reader.pipe_to(&mut err_buf).ok();
        Ok((
            code,
            String::from_utf8_lossy(&out_buf).into_owned(),
            String::from_utf8_lossy(&err_buf).into_owned(),
        ))
    })
    .await;

    match outcome {
        Ok(Ok((code, stdout, stderr))) => Ok(ShellRun {
            exit_code: code,
            stdout,
            stderr,
            preflight: String::new(),
        }),
        Ok(Err(msg)) => Err(msg),
        Err(e) => Err(format!("shell task panicked: {e}")),
    }
}

/// Cap one side's output at ~8KB — the same discipline the tool result uses.
fn trunc(s: &str) -> String {
    const CAP: usize = 8 * 1024;
    if s.len() > CAP {
        format!("{}…[{} bytes truncated]", &s[..CAP], s.len() - CAP)
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
    if r.exit_code != 0 {
        out.push_str(&format!("\n[exit code {}]", r.exit_code));
    }
    out
}

// ---------- background jobs (fastctx-style: filesystem is the state) ----------

/// A background job is `.sunmao/jobs/{id}/` containing output.log and,
/// once finished, exit.json. No in-memory registry — the log dir is truth.
fn jobs_dir(ctx: &crate::context::Context) -> std::path::PathBuf {
    ctx.cwd.join(".sunmao").join("jobs")
}

async fn spawn_background(
    command: &str,
    ctx: &crate::context::Context,
    preamble: String,
) -> anyhow::Result<ToolResult> {
    let id = format!(
        "j-{}",
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

    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult> {
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
        let chunk = String::from_utf8_lossy(&data[start..end]);
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
