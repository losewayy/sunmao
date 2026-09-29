use crate::tool::*;
use serde::Deserialize;
use serde_json::{json, Value};

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
