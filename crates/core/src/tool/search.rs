use crate::tool::*;
use serde::Deserialize;
use serde_json::{Value, json};

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

    async fn call(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            pattern: String,
        }
        let a: Args = serde_json::from_value(args)?;
        // the cwd is a literal path, not pattern syntax — a project dir named
        // `repo[1]` would otherwise be parsed as a char class and silently
        // match nothing (or error). Escape it; the user pattern keeps its
        // metachars.
        // verbatim `\\?\` cwd breaks every glob — `//?/F:/…` matches
        // nothing; display_path strips it before the pattern is spliced.
        // A U+FFFD in the cwd means a non-UTF-8 path — error honestly
        // instead of silently returning `[no matches]`.
        let cwd_disp = crate::paths::display_path_fwd(&ctx.cwd);
        anyhow::ensure!(
            !cwd_disp.contains('\u{FFFD}'),
            "cwd contains non-UTF-8 characters and cannot be globbed"
        );
        let cwd = glob::Pattern::escape(&cwd_disp);
        let pat = if std::path::Path::new(&a.pattern).is_absolute() {
            // join() semantics: an absolute pattern ignores the cwd
            a.pattern.replace('\\', "/")
        } else {
            format!("{cwd}/{}", a.pattern)
        };
        let mut hits = Vec::new();
        for entry in glob::glob(&pat).with_context(|| format!("bad pattern: {}", a.pattern))? {
            if let Ok(p) = entry {
                // entries inherit the pattern's spelling — strip the plain
                // (non-verbatim) cwd and normalize separators so hits come
                // back as clean `dir/file` relative paths
                let rel = p
                    .strip_prefix(&cwd_disp)
                    .unwrap_or(&p)
                    .display()
                    .to_string()
                    .replace('\\', "/");
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
            exit_code: None,
            output: out,
            ok: true,
        })
    }
}

// ---------- Grep (managed subprocess: rg, no shell) ----------

/// A hung rg (fifo, wedged FS, pathological tree) must not park the tool
/// call forever — the child is killed on drop when the timeout fires.
const RG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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

    async fn call(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult> {
        self.run(args, ctx, RG_TIMEOUT).await
    }
}

impl GrepTool {
    async fn run(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<ToolResult> {
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
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(g) = &a.glob {
            cmd.arg("--glob").arg(g);
        }
        // `--` before positionals — a pattern or path starting with `-`
        // (model-emitted `-foo` search) would parse as an rg flag, not an
        // operand, and the tool would silently search the wrong thing
        cmd.arg("--").arg(&a.pattern);
        if let Some(p) = &a.path {
            cmd.arg(p);
        }
        let out = match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolResult {
                    exit_code: None,
                    output: "ripgrep (rg) not found on PATH".into(),
                    ok: false,
                });
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                return Ok(ToolResult {
                    exit_code: None,
                    output: format!("ripgrep timed out after {}s", timeout.as_secs()),
                    ok: false,
                });
            }
        };
        const CAP: usize = 8 * 1024;
        let text = crate::console::console_text(&out.stdout);
        let mut res = if text.len() > CAP {
            // CAP isn't char-aligned for CJK/emoji output — step back or
            // the slice panics mid-codepoint.
            let mut end = CAP;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…[truncated {} bytes]", &text[..end], text.len() - end)
        } else {
            text
        };
        // rg exit 1 = no matches (fine); >=2 = real error — surface its
        // stderr so a syntax/flag error doesn't read as "[no matches]"
        let ok = out.status.code().unwrap_or(2) <= 1;
        if !ok {
            let err = crate::console::console_text(&out.stderr);
            let err = err.trim();
            res = if err.is_empty() {
                format!("ripgrep failed (status {})", out.status)
            } else {
                format!("ripgrep failed (status {}): {err}", out.status)
            };
        } else if res.is_empty() {
            res = "[no matches]".into();
        }
        Ok(ToolResult {
            exit_code: None,
            output: res,
            ok,
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
            crate::tool::builtin_registry(),
            dir.to_path_buf(),
        ))
    }

    /// A project dir named like `work[v2]` used to be spliced into the glob
    /// pattern raw — `[v2]` parsed as a char class and every Glob missed.
    /// The cwd must be escaped before the user pattern is appended.
    #[tokio::test]
    async fn glob_escapes_cwd_metachars() {
        let base = crate::fresh_test_dir("glob-meta");
        let dir = base.join("work[v2]");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let ctx = test_ctx(&dir);
        let res = GlobTool
            .call(json!({"pattern": "*.txt"}), &ctx)
            .await
            .unwrap();
        assert!(res.ok);
        assert!(res.output.contains("a.txt"), "{}", res.output);
        std::fs::remove_dir_all(&base).ok();
    }

    /// A verbatim `\\?\`-prefixed cwd (what `canonicalize`/`current_dir`
    /// yield on Windows) must not poison relative glob patterns — spliced
    /// raw it produced `//?/C:/…` which silently matched nothing.
    #[cfg(windows)]
    #[tokio::test]
    async fn glob_relative_pattern_with_verbatim_cwd() {
        let dir = crate::fresh_test_dir("glob-verbatim");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let verbatim = std::path::PathBuf::from(format!(r"\\?\{}", dir.display()));
        let ctx = test_ctx(&verbatim);
        let res = GlobTool
            .call(json!({"pattern": "*.txt"}), &ctx)
            .await
            .unwrap();
        assert!(res.ok);
        assert!(res.output.contains("a.txt"), "{}", res.output);
        assert!(!res.output.contains(r"\\?\"), "{}", res.output);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// rg exit >= 2 is a real error — stderr must reach the model instead of
    /// a misleading "[no matches]", and a hung child must die at the timeout.
    /// One test holds both so the SUNMAO_RG env swap never races itself.
    #[tokio::test]
    async fn grep_surfaces_stderr_and_times_out() {
        let Some(fake) = crate::compile_fixture("fake_rg.rs", "sunmao-fake-rg") else {
            return; // no rustc on this box — live-path skip
        };
        let dir = crate::fresh_test_dir("grep-fake");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_ctx(&dir);
        unsafe {
            std::env::set_var("SUNMAO_RG", &fake);
        }

        let res = GrepTool
            .run(
                json!({"pattern": "BOOM"}),
                &ctx,
                std::time::Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(!res.ok);
        assert!(res.output.contains("regex parse error"), "{}", res.output);

        let res = GrepTool
            .run(
                json!({"pattern": "SLEEP"}),
                &ctx,
                std::time::Duration::from_millis(300),
            )
            .await
            .unwrap();
        assert!(!res.ok);
        assert!(res.output.contains("timed out"), "{}", res.output);

        unsafe {
            std::env::remove_var("SUNMAO_RG");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
