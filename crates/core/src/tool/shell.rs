use crate::tool::*;
use anyhow::bail;
use serde::Deserialize;
use serde_json::{json, Value};

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
                })
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

        // declarative permission rules first (deny is hard refusal)
        match ctx.permissions.check("Bash", &a.command) {
            crate::permissions::Verdict::Deny => {
                return Ok(ToolResult {
                    output: "denied by permission rules".into(),
                    ok: false,
                })
            }
            crate::permissions::Verdict::Ask => {
                if !ctx
                    .approval
                    .approve("Bash", &a.command, "matched ask rule")
                    .await
                {
                    return Ok(ToolResult {
                        output: "denied at approval prompt".into(),
                        ok: false,
                    });
                }
            }
            crate::permissions::Verdict::PreApproved | crate::permissions::Verdict::Default => {}
        }
        if crate::permissions::Verdict::PreApproved != ctx.permissions.check("Bash", &a.command) {
            // approval gate for risky patterns — the audit seam's active half
            if let Some(why) = crate::approval::classify(&a.command) {
                let allowed = ctx.approval.approve("Bash", &a.command, why).await;
                if !allowed {
                    return Ok(ToolResult {
                        output: format!("denied by user approval gate ({why})"),
                        ok: false,
                    });
                }
            }
        }

        // deno_task_shell's internals are !Send (Rc<Cell> exit-code cells) —
        // every !Send value must be constructed *inside* the blocking closure.
        let cwd = ctx.cwd.clone();
        let env_vars: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
            std::env::vars_os().collect();
        let timeout_secs = a.timeout_secs.unwrap_or(120);

        let outcome =
            tokio::task::spawn_blocking(move || -> Result<(i32, String, String), String> {
                let state = deno_task_shell::ShellState::new(
                    env_vars,
                    cwd,
                    Default::default(),
                    Default::default(),
                );
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

        let (code, stdout, stderr) = match outcome {
            Ok(Ok(v)) => v,
            Ok(Err(msg)) => {
                return Ok(ToolResult {
                    output: msg,
                    ok: false,
                })
            }
            Err(e) => bail!("shell task panicked: {e}"),
        };

        // context-efficient output discipline: cap at ~8KB per side
        const CAP: usize = 8 * 1024;
        let trunc = |s: &str| {
            if s.len() > CAP {
                format!("{}…[{} bytes truncated]", &s[..CAP], s.len() - CAP)
            } else {
                s.to_string()
            }
        };
        let mut out = pre() + &trunc(stdout.trim_end());
        if !stderr.trim().is_empty() {
            out.push_str(&format!("\n[stderr]\n{}", trunc(stderr.trim())));
        }
        if code != 0 {
            out.push_str(&format!("\n[exit code {code}]"));
        }
        Ok(ToolResult {
            output: out,
            ok: code == 0,
        })
    }
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

    // Detach: drop the JoinHandle — the blocking thread outlives the call.
    let log_path_for_msg = log_path.clone();
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
