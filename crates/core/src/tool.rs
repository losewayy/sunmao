//! Tool registry + the four builtin capabilities.
//!
//! Three execution classes (see SPEC §4.3):
//!   native       — in-process Rust (Read/Write/Edit): structured, auditable
//!   managed      — spawned binaries without a shell (not yet wired)
//!   shell        — `Bash`: model writes a command string; routed through
//!                  deno_task_shell so bash syntax is identical on Windows.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context as _};
use serde::Deserialize;
use serde_json::{json, Value};
use sunmao_llm::types::Tool;

pub struct ToolResult {
    pub output: String,
    /// Failed tool calls are still delivered to the model as output —
    /// models recover from errors better when the error is legible.
    pub ok: bool,
}

#[async_trait::async_trait]
pub trait ToolImpl: Send + Sync {
    fn name(&self) -> &'static str;
    fn decl(&self) -> Tool;
    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult>;
}

pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn ToolImpl>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, tool: impl ToolImpl + 'static) {
        self.tools.insert(tool.name().to_string(), Box::new(tool));
    }

    pub fn register_boxed(&mut self, tool: Box<dyn ToolImpl>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn declarations(&self) -> Vec<Tool> {
        self.tools.values().map(|t| t.decl()).collect()
    }

    pub async fn call(
        &self,
        name: &str,
        args_json: &str,
        ctx: &crate::context::Context,
    ) -> ToolResult {
        let Some(tool) = self.tools.get(name) else {
            return ToolResult {
                output: format!("unknown tool: {name}"),
                ok: false,
            };
        };
        let args: Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolResult {
                    output: format!("invalid arguments for {name}: {e}"),
                    ok: false,
                }
            }
        };
        match tool.call(args, ctx).await {
            Ok(r) => r,
            Err(e) => ToolResult {
                output: format!("{name} failed: {e:#}"),
                ok: false,
            },
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical builtin set for v0.1.
pub fn builtin_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(ReadTool);
    r.register(WriteTool);
    r.register(EditTool);
    r.register(BashTool);
    r.register(GlobTool);
    r.register(GrepTool);
    r.register(JobOutputTool);
    r.register(HtmlArtifactTool);
    r.register(crate::task::TaskTool);
    r
}

// ---------- Read ----------

pub struct ReadTool;

#[async_trait::async_trait]
impl ToolImpl for ReadTool {
    fn name(&self) -> &'static str {
        "Read"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "Read",
            "Read a text file. Returns numbered lines (up to `limit`, default 400).",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute or cwd-relative path"},
                    "offset": {"type": "integer", "description": "1-based first line to read"},
                    "limit": {"type": "integer", "description": "Max lines (default 400)"}
                },
                "required": ["path"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: PathBuf,
            offset: Option<usize>,
            limit: Option<usize>,
        }
        let a: Args = serde_json::from_value(args)?;
        let path = ctx.cwd.join(&a.path);
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("cannot read {}", path.display()))?;
        ctx.mark_read(&path);
        let offset = a.offset.unwrap_or(1).max(1);
        let limit = a.limit.unwrap_or(400);
        let body: Vec<String> = text
            .lines()
            .skip(offset - 1)
            .take(limit)
            .enumerate()
            .map(|(i, l)| format!("{}\t{}", offset + i, l))
            .collect();
        let mut out = body.join("\n");
        let total = text.lines().count();
        if offset - 1 + limit < total {
            out.push_str(&format!(
                "\n[truncated: {} more lines, total {}]",
                total - (offset - 1 + limit),
                total
            ));
        }
        Ok(ToolResult {
            output: out,
            ok: true,
        })
    }
}

// ---------- Write ----------

pub struct WriteTool;

#[async_trait::async_trait]
impl ToolImpl for WriteTool {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "Write",
            "Write a file (creates or overwrites). Parent dirs must exist.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: PathBuf,
            content: String,
        }
        let a: Args = serde_json::from_value(args)?;
        let path = ctx.cwd.join(&a.path);
        // Read-before-Write gate: overwriting a file the model hasn't read
        // is how blind edits happen. New files are exempt.
        if path.exists() && !ctx.has_read(&path) {
            return Ok(ToolResult {
                output: format!(
                    "refused: {} exists but was not Read this session. Read it first.",
                    path.display()
                ),
                ok: false,
            });
        }
        tokio::fs::write(&path, &a.content)
            .await
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(ToolResult {
            output: format!("wrote {} bytes to {}", a.content.len(), path.display()),
            ok: true,
        })
    }
}

// ---------- Edit ----------

pub struct EditTool;

#[async_trait::async_trait]
impl ToolImpl for EditTool {
    fn name(&self) -> &'static str {
        "Edit"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "Edit",
            "Replace `old_string` with `new_string` in a file. `old_string` must match \
             uniquely (whitespace-normalized matching is applied). Empty `old_string` \
             creates the file with `new_string` as its content.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: PathBuf,
            old_string: String,
            new_string: String,
        }
        let a: Args = serde_json::from_value(args)?;
        let path = ctx.cwd.join(&a.path);

        // empty old_string = create file
        if a.old_string.is_empty() {
            tokio::fs::write(&path, &a.new_string).await?;
            return Ok(ToolResult {
                output: format!("created {}", path.display()),
                ok: true,
            });
        }

        if !ctx.has_read(&path) {
            return Ok(ToolResult {
                output: format!(
                    "refused: {} was not Read this session. Read it first.",
                    path.display()
                ),
                ok: false,
            });
        }

        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("cannot read {}", path.display()))?;

        // exact match first, then whitespace-normalized match
        let (start, end) = match find_unique(&text, &a.old_string) {
            Some(span) => span,
            None => find_normalized(&text, &a.old_string)
                .ok_or_else(|| anyhow::anyhow!("old_string not found (or not unique)"))?,
        };
        let mut new_text = String::with_capacity(text.len());
        new_text.push_str(&text[..start]);
        new_text.push_str(&a.new_string);
        new_text.push_str(&text[end..]);
        tokio::fs::write(&path, &new_text).await?;
        Ok(ToolResult {
            output: format!("edited {}", path.display()),
            ok: true,
        })
    }
}

/// Exact match that must occur exactly once.
fn find_unique(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    let mut it = haystack.match_indices(needle);
    let (i, _) = it.next()?;
    if it.next().is_some() {
        return None; // not unique
    }
    Some((i, i + needle.len()))
}

/// Whitespace-normalized match: compare with runs of whitespace collapsed and
/// leading/trailing whitespace trimmed. Returns a span in the original text.
fn find_normalized(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    fn norm(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    let target = norm(needle);
    if target.is_empty() {
        return None;
    }
    // slide over candidate windows starting at each whitespace boundary
    let mut found: Option<(usize, usize)> = None;
    for (i, _) in haystack.match_indices('\n') {
        let start = i + 1;
        // limit candidate window to needle length * 4 + margin
        let window_end = (start + needle.len() * 4 + 64).min(haystack.len());
        let window = &haystack[start..window_end];
        for end_off in line_ends(window) {
            let cand = &window[..end_off];
            if norm(cand) == target {
                let span = (start, start + end_off);
                if found.is_some() {
                    return None; // ambiguous
                }
                found = Some(span);
            }
        }
    }
    // also try from position 0 if file doesn't start with newline
    if haystack.starts_with(needle.trim_start()) {
        return None; // already covered by exact match if truly exact
    }
    found
}

fn line_ends(s: &str) -> Vec<usize> {
    let mut v: Vec<usize> = s.match_indices('\n').map(|(i, _)| i).collect();
    v.push(s.len());
    v
}

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
             Use for builds, tests, git, and anything without a dedicated tool.",
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

        if a.background {
            return spawn_background(&a.command, ctx).await;
        }

        // deno_task_shell's internals are !Send (Rc<Cell> exit-code cells) —
        // every !Send value must be constructed *inside* the blocking closure.
        let command = a.command.clone();
        let cwd = ctx.cwd.clone();
        let env_vars: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
            std::env::vars_os().collect();
        let timeout_secs = a.timeout_secs.unwrap_or(120);

        let outcome =
            tokio::task::spawn_blocking(move || -> Result<(i32, String, String), String> {
                let list = deno_task_shell::parser::parse(&command)
                    .map_err(|e| format!("cannot parse command: {e}"))?;
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
        let mut out = trunc(stdout.trim_end());
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

// ---------- Glob ----------

pub struct GlobTool;

#[async_trait::async_trait]
impl ToolImpl for GlobTool {
    fn name(&self) -> &'static str {
        "Glob"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "Glob",
            "Find files by glob pattern relative to the working directory. \
             Returns matching paths (max 200), most recent first is not guaranteed.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "e.g. **/*.rs, src/**/*.toml"}
                },
                "required": ["pattern"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            pattern: String,
        }
        let a: Args = serde_json::from_value(args)?;
        let full = ctx.cwd.join(&a.pattern);
        let pat = full.to_string_lossy().replace('\\', "/");
        let mut hits = Vec::new();
        for entry in glob::glob(&pat).with_context(|| format!("bad pattern: {}", a.pattern))? {
            if let Ok(p) = entry {
                let rel = p
                    .strip_prefix(&ctx.cwd)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| p.display().to_string());
                hits.push(rel);
            }
            if hits.len() >= 200 {
                break;
            }
        }
        let mut out = hits.join("\n");
        if hits.len() >= 200 {
            out.push_str("\n[truncated at 200]");
        }
        if hits.is_empty() {
            out = "[no matches]".into();
        }
        Ok(ToolResult {
            output: out,
            ok: true,
        })
    }
}

// ---------- Grep (managed subprocess: rg, no shell) ----------

pub struct GrepTool;

#[async_trait::async_trait]
impl ToolImpl for GrepTool {
    fn name(&self) -> &'static str {
        "Grep"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "Grep",
            "Search file contents with ripgrep (regex). Returns file:line:match lines, capped at 8KB.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regex pattern"},
                    "path": {"type": "string", "description": "File/dir to search (default: cwd)"},
                    "glob": {"type": "string", "description": "e.g. *.rs to filter files"}
                },
                "required": ["pattern"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            pattern: String,
            path: Option<String>,
            glob: Option<String>,
        }
        let a: Args = serde_json::from_value(args)?;

        let mut cmd = tokio::process::Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg("--color=never")
            .arg("--max-columns=400")
            .current_dir(&ctx.cwd)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        if let Some(g) = &a.glob {
            cmd.arg("--glob").arg(g);
        }
        cmd.arg(&a.pattern);
        if let Some(p) = &a.path {
            cmd.arg(p);
        }
        let out = match cmd.output().await {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolResult {
                    output: "ripgrep (rg) not found on PATH".into(),
                    ok: false,
                })
            }
            Err(e) => return Err(e.into()),
        };
        const CAP: usize = 8 * 1024;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut res = if text.len() > CAP {
            format!("{}…[truncated {} bytes]", &text[..CAP], text.len() - CAP)
        } else {
            text.into_owned()
        };
        if res.is_empty() {
            res = "[no matches]".into();
        }
        // rg exit 1 = no matches (fine); >=2 = real error
        Ok(ToolResult {
            output: res,
            ok: out.status.code().unwrap_or(2) <= 1,
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
        output: format!("job {id} started; log: {}", log_path_for_msg.display()),
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

// ---------- HtmlArtifact (SPEC §4.10) ----------

pub struct HtmlArtifactTool;

#[async_trait::async_trait]
impl ToolImpl for HtmlArtifactTool {
    fn name(&self) -> &'static str {
        "HtmlArtifact"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "HtmlArtifact",
            "Produce an HTML artifact — plans, reports, dashboards, anything meant for a              human to *look at*. Written to .sunmao/artifacts/{name}.html and registered              in the session log. Use this instead of Markdown for rich deliverables.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "artifact slug, e.g. migration-plan"},
                    "html": {"type": "string", "description": "complete HTML document"}
                },
                "required": ["name", "html"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
            html: String,
        }
        let a: Args = serde_json::from_value(args)?;
        if !a
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("artifact name must be [a-z0-9_-]");
        }
        let dir = ctx.cwd.join(".sunmao").join("artifacts");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.html", a.name));
        let bytes = a.html.len();
        tokio::fs::write(&path, &a.html).await?;
        ctx.sessions
            .lock()
            .await
            .append(&crate::session::SessionEvent::Artifact {
                name: a.name.clone(),
                path: path.display().to_string(),
                bytes,
            })
            .await?;
        Ok(ToolResult {
            output: format!(
                "artifact '{}' → {} ({} bytes)",
                a.name,
                path.display(),
                bytes
            ),
            ok: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use crate::session::SessionLog;
    use std::sync::Arc;
    use sunmao_llm::ProviderAdapter;

    struct StubLlm;

    #[async_trait::async_trait]
    impl ProviderAdapter for StubLlm {
        async fn stream(
            &self,
            _req: sunmao_llm::ChatRequest<'_>,
        ) -> anyhow::Result<sunmao_llm::DeltaStream> {
            Ok(Box::pin(futures_util::stream::empty()))
        }
    }

    fn test_ctx(dir: &std::path::Path) -> Arc<Context> {
        Arc::new(Context::new(
            Arc::new(StubLlm),
            SessionLog::ephemeral(),
            builtin_registry(),
            dir.to_path_buf(),
        ))
    }

    #[tokio::test]
    async fn edit_refuses_unread_file() {
        let dir = std::env::temp_dir().join(format!("sunmao-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.txt"), "hello world").unwrap();
        let ctx = test_ctx(&dir);
        let tool = EditTool;
        let res = tool
            .call(
                json!({"path": "t.txt", "old_string": "hello", "new_string": "bye"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!res.ok);
        assert!(res.output.contains("not Read this session"));
        // after a Read, the same edit must succeed
        let read = ReadTool;
        read.call(json!({"path": "t.txt"}), &ctx).await.unwrap();
        let res = tool
            .call(
                json!({"path": "t.txt", "old_string": "hello", "new_string": "bye"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.ok);
        assert_eq!(
            std::fs::read_to_string(dir.join("t.txt")).unwrap(),
            "bye world"
        );
    }

    #[tokio::test]
    async fn write_allows_new_file_without_read() {
        let dir = std::env::temp_dir().join(format!("sunmao-test-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_ctx(&dir);
        let res = WriteTool
            .call(json!({"path": "n.txt", "content": "x"}), &ctx)
            .await
            .unwrap();
        assert!(res.ok);
    }
}
