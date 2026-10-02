use crate::tool::*;
use serde::Deserialize;
use serde_json::{Value, json};

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

    async fn call(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult> {
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
        // §4.10 annotation回流 — Reading an artifact surfaces its human
        // margin notes with it; the state file is the same doc's margin,
        // not a separate read the model must remember to make
        if path.extension().map(|x| x == "html").unwrap_or(false)
            && path.parent().is_some_and(|p| p.ends_with("artifacts"))
        {
            let state = path.with_extension("state.json");
            if let Ok(notes) = std::fs::read_to_string(&state)
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&notes)
            {
                let open: Vec<&serde_json::Value> = v
                    .get("annotations")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .filter(|n| n.get("resolved") != Some(&serde_json::json!(true)))
                            .collect()
                    })
                    .unwrap_or_default();
                if !open.is_empty() {
                    out.push_str("\n\n[annotations — unresolved reviewer notes:");
                    for n in &open {
                        let note = n.get("note").and_then(|x| x.as_str()).unwrap_or("");
                        let at = n.get("at").and_then(|x| x.as_str()).unwrap_or("");
                        let sec = n.get("section").and_then(|x| x.as_str()).unwrap_or("");
                        out.push_str(&format!("\n  ({at}) {note}"));
                        if !sec.is_empty() {
                            out.push_str(&format!(" §{sec}"));
                        }
                    }
                    out.push_str("\n  fold into the next revision;");
                    out.push_str(" mark each \"resolved\": true]");
                }
            }
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

    async fn call(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult> {
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
        // checkpoint BEFORE bytes change — the ledger keeps what this
        // write is about to destroy (no-op for files already preserved)
        ctx.checkpoint_file(&path).await?;
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

    async fn call(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: PathBuf,
            old_string: String,
            new_string: String,
        }
        let a: Args = serde_json::from_value(args)?;
        let path = ctx.cwd.join(&a.path);

        // empty old_string = create file — a file that already exists is
        // an overwrite, not a create; route it through the read gate the
        // same as a real edit so bytes can't be destroyed sight-unseen.
        if a.old_string.is_empty() {
            if path.exists() && !ctx.has_read(&path) {
                return Ok(ToolResult {
                    output: format!(
                        "refused: {} exists and was not Read this session. Read it first (or use Write for a new file).",
                        path.display()
                    ),
                    ok: false,
                });
            }
            ctx.checkpoint_file(&path).await?;
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
        ctx.checkpoint_file(&path).await?;
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
    // candidate windows start at position 0 and after every whitespace
    // char — line starts AND mid-line boundaries (a needle beginning
    // mid-line can still normalize against whitespace drift)
    let mut found: Option<(usize, usize)> = None;
    let starts = std::iter::once(0).chain(
        haystack
            .char_indices()
            .filter(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8()),
    );
    for start in starts {
        // limit candidate window to needle length * 4 + margin — the cap
        // can land mid-codepoint; step back to a char boundary or the
        // slice panics on multibyte haystacks.
        let mut window_end = (start + needle.len() * 4 + 64).min(haystack.len());
        while !haystack.is_char_boundary(window_end) {
            window_end -= 1;
        }
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
    found
}

fn line_ends(s: &str) -> Vec<usize> {
    let mut v: Vec<usize> = s.match_indices('\n').map(|(i, _)| i).collect();
    v.push(s.len());
    v
}
