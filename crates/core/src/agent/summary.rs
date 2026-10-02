//! Tool-call digest helpers — the one-line `summary` a ToolStart/ToolDone
//! carries, plus the rule-match specifier the gate globs over.

/// One-line argument digest for `LiveEvent::ToolStart.summary`: the single
/// most interesting value per tool (the command for Bash, the path for file
/// tools, …), falling back to compact `k=v` pairs for unknown tools.
/// One-line digest of a tool call's interesting argument — the transcript
/// header string. Public so frontends replaying a session log render the
/// same headers a live turn would have produced.
pub fn call_summary(name: &str, args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let preferred: &[&str] = match name {
        "Bash" => &["command"],
        "Read" | "Write" | "Edit" => &["path"],
        "Glob" | "Grep" => &["pattern", "path"],
        "WebFetch" => &["url"],
        "Task" => &["prompt"],
        "JobOutput" => &["id"],
        "HtmlArtifact" => &["name"],
        _ => &[],
    };
    let mut out = String::new();
    for k in preferred {
        if let Some(v) = obj.get(*k).and_then(|v| v.as_str()) {
            out = v.to_string();
            break;
        }
    }
    if out.is_empty() {
        for (k, v) in obj.iter().take(3) {
            let vs = v
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| v.to_string());
            if !out.is_empty() {
                out.push_str("  ");
            }
            out.push_str(k);
            out.push('=');
            out.push_str(&vs);
        }
    }
    if name == "Bash" && obj.get("background").and_then(|v| v.as_bool()) == Some(true) {
        out.push_str("  &");
    }
    ellipsize(&out, 90)
}

/// Flatten whitespace and cap at `max` chars, adding `…` when cut.
fn ellipsize(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut it = flat.chars();
    let kept: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{kept}…")
    } else {
        kept
    }
}

/// Cap tool output carried in `LiveEvent::ToolDone` — frontends only need a
/// preview; the full text already lands in the session log.
pub(crate) fn truncate_output(s: &str) -> String {
    const MAX: usize = 8 * 1024;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…\n[truncated — {} bytes total]", &s[..end], s.len())
}

/// The string declarative rules glob over for a given tool: the command for
/// Bash, the path for file tools, the pattern for search — whatever a rule
/// like `Bash(npm *)` or `Read(./src/**)` is meant to match.
pub(crate) fn specifier_for(tool: &str, args: &serde_json::Value) -> String {
    let key = match tool {
        "Bash" => "command",
        "Read" | "Write" | "Edit" => "path",
        "Glob" | "Grep" => "pattern",
        "WebFetch" => "url",
        "Task" => "prompt",
        "HtmlArtifact" => "name",
        "JobOutput" => "id",
        _ => "",
    };
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Per-tool watchdog seconds from `ctx.tool_timeouts` — exact name first,
/// then the `mcp__` catch-all row for wire-named MCP tools (`mcp__x__y`
/// never matches literally). Bash and Task aren't listed on purpose:
/// Bash's `timeout_secs` arg is the finer control (it kills the process
/// tree, not just the wait), and long-running agents are Task's feature.
pub(crate) fn tool_timeout_for(ctx: &crate::context::Context, tool: &str) -> Option<u64> {
    ctx.tool_timeouts.get(tool).copied().or_else(|| {
        tool.starts_with("mcp__")
            .then(|| ctx.tool_timeouts.get("mcp__").copied())
            .flatten()
    })
}
