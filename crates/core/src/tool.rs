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
                    "timeout_secs": {"type": "integer", "description": "Kill after N seconds (default 120)"}
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
        }
        let a: Args = serde_json::from_value(args)?;

        // deno_task_shell's internals are !Send (Rc<Cell> exit-code cells) —
        // every !Send value must be constructed *inside* the blocking closure.
        let command = a.command.clone();
        let cwd = ctx.cwd.clone();
        let env_vars: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
            std::env::vars_os().collect();
        let timeout_secs = a.timeout_secs.unwrap_or(120);

        let outcome = tokio::task::spawn_blocking(move || -> Result<(i32, String, String), String> {
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
            let (stdin_reader, stdin_writer) =
                std::io::pipe().map_err(|e| e.to_string())?;
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
