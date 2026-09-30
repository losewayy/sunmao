use crate::tool::*;
use serde::Deserialize;
use serde_json::{json, Value};

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

        // managed binary — an injected path wins over PATH so a bundled
        // ripgrep works where none is installed (RG_BIN_PATH convention)
        let rg = std::env::var("SUNMAO_RG")
            .or_else(|_| std::env::var("RG_BIN_PATH"))
            .unwrap_or_else(|_| "rg".into());
        let mut cmd = tokio::process::Command::new(rg);
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
