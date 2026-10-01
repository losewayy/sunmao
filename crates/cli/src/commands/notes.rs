//! Note-text builders — the one phrasing every frontend's answer shares.
//! `/name` outputs are built here so the REPL, TUI and serve's note frames
//! render identical text; the frontend only chooses where it lands.

use std::path::Path;

use sunmao_core::agent::{ApprovalMode, SessionStatus};
use sunmao_core::context::TaskEntry;
use sunmao_core::mcp::McpServerStatus;
use sunmao_core::tool::TodoItem;

/// The backend-semantic command tail every frontend's `/help` shares;
/// frontend-local names prepend via `help_text`'s `local` argument.
const HELP_COMMANDS: &str = "/compact · /model [sel] · /mode [stance] · /resume [id] · /sessions · /search <q> · /fork <id> · /rewind [n] [session|code|both] · /tasks · /todos · /mcp · /status · /artifacts · /annotate <name> <note> · /help";

/// The `/help` commands line — `local` inserts frontend-only names
/// (`"/multiline · /clear · "` for the TUI, `""` elsewhere) ahead of the
/// shared tail.
pub fn help_text(local: &str) -> String {
    format!(
        "commands — {local}{HELP_COMMANDS} · /quit · + every *.md in .sunmao/commands, .claude/commands, plugins/*/commands"
    )
}

/// `agent.compact` → the note every frontend shows.
pub fn compact_note(res: anyhow::Result<String>) -> String {
    match res {
        Ok(s) if s.is_empty() => "[compacted: nothing to fold]".to_string(),
        Ok(s) => format!("[compacted]\n{s}"),
        Err(e) => format!("[compact failed] {e:#}"),
    }
}

/// Bare `/model` — the model list, or the models.json missing note.
pub fn models_text(choices: &[String]) -> String {
    if choices.is_empty() {
        "[no models.json — session model only]".to_string()
    } else {
        format!("available models:\n{}", choices.join("\n"))
    }
}

/// Rejected `/model <sel>` selector.
pub fn model_unknown(sel: &str) -> String {
    format!("[unknown selector: {sel} — try /model for the list]")
}

/// Bare `/mode` — the current stance plus the full list, `→` marking the
/// active one.
pub fn mode_list_text(cur: ApprovalMode) -> String {
    let list = ApprovalMode::ALL
        .iter()
        .map(|m| {
            let mark = if *m == cur { "→" } else { " " };
            format!("  {mark} {}", m.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("approval mode: {}\n{list}", cur.as_str())
}

/// Rejected `/mode <name>` stance.
pub fn mode_unknown(name: &str) -> String {
    format!("[unknown mode: {name} — always_ask · auto · read_only · full_access]")
}

/// `/tasks` — the live sub-agent roster.
pub fn tasks_text(tasks: &[TaskEntry]) -> String {
    if tasks.is_empty() {
        "[no sub-agents this session]".to_string()
    } else {
        let rows = tasks
            .iter()
            .map(|t| {
                let status = match t.done {
                    None => "running",
                    Some(true) => "done",
                    Some(false) => "failed",
                };
                let agent = t
                    .agent
                    .as_deref()
                    .map(|a| format!(" @{a}"))
                    .unwrap_or_default();
                format!("  {status:<7} {}{} — {}", t.id, agent, t.prompt)
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("sub-agents:\n{rows}")
    }
}

/// `/todos` — the model's session task list.
pub fn todos_text(items: &[TodoItem]) -> String {
    if items.is_empty() {
        "[no task list — TodoWrite creates it]".to_string()
    } else {
        format!("task list:\n{}", sunmao_core::tool::render_todos(items))
    }
}

/// `/mcp` — the configured MCP server roster: name, transport, how many
/// tools/prompts/resources it advertised, and whether the connection is
/// still alive. Servers that failed at startup never connected — they're
/// the startup warnings, not roster rows.
pub fn mcp_text(servers: &[McpServerStatus]) -> String {
    if servers.is_empty() {
        return "[no MCP servers connected — .sunmao/mcp.json or plugin manifests]".to_string();
    }
    let rows = servers
        .iter()
        .map(|s| {
            format!(
                "  {}  {} · {} tools · {} prompts · {} resources · {}",
                s.name,
                s.transport,
                s.tools,
                s.prompts,
                s.resources,
                if s.connected {
                    "connected"
                } else {
                    "disconnected"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("mcp servers ({}):\n{rows}", servers.len())
}

/// `/status` — the session's vitals, folded by `AgentLoop::status`.
/// `grants` lists the `Approval::Session` ledger verbatim — a grant covers
/// the identical call only, so the rows are the audit surface (revocation
/// is deliberately absent this round: read-only).
pub fn status_text(s: &SessionStatus) -> String {
    let t = &s.tokens;
    let grants = if s.grants.is_empty() {
        String::new()
    } else {
        format!(
            "\n  grants   {} session grant{}",
            s.grants.len(),
            if s.grants.len() == 1 { "" } else { "s" }
        ) + &s
            .grants
            .iter()
            .map(|g| format!("\n           · {g}"))
            .collect::<String>()
    };
    format!(
        "session {id}\n  model    {model} ({provider})\n  cwd      {cwd}\n  mode     {mode}\n  tokens   {total} total — {prompt} prompt + {completion} completion · cache {cache_read} read / {cache_write} write{grants}",
        id = s.session_id,
        model = s.model,
        provider = s.provider,
        cwd = s.cwd.display(),
        mode = s.approval_mode.as_str(),
        total = t.prompt + t.completion,
        prompt = t.prompt,
        completion = t.completion,
        cache_read = t.cache_read,
        cache_write = t.cache_write,
    )
}

/// `.sunmao/artifacts` listing for `/artifacts` — one row per HtmlArtifact
/// output, `state.json` sidecars flagged (unresolved human notes live
/// there). Shared by every frontend's artifacts view.
pub fn artifacts_text(cwd: &Path) -> String {
    let dir = cwd.join(".sunmao").join("artifacts");
    let mut rows: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().map(|x| x == "html").unwrap_or(false) {
                        let name = p.file_stem()?.to_string_lossy().to_string();
                        // `{name}.v{N}.html` files are archived revisions,
                        // not artifacts — the rev chain hangs off the live
                        // `{name}.html` row.
                        if let Some((_, suffix)) = name.rsplit_once(".v")
                            && suffix.parse::<usize>().is_ok()
                        {
                            return None;
                        }
                        let bytes = e.metadata().ok()?.len();
                        let notes = p.with_extension("state.json").exists();
                        let revs = sunmao_core::tool::artifact_rev(&dir, &name);
                        Some(format!(
                            "  {name:<24} {bytes:>7} B{}{}",
                            if notes { "  +notes" } else { "" },
                            if revs > 1 {
                                format!("  ·{revs} revs")
                            } else {
                                String::new()
                            },
                        ))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort();
    if rows.is_empty() {
        "[no artifacts — HtmlArtifact writes .sunmao/artifacts/*.html]".to_string()
    } else {
        format!(
            "artifacts ({}):\n{}\n  dir: {}",
            rows.len(),
            rows.join("\n"),
            dir.display().to_string().replace("\\\\?\\", "")
        )
    }
}

/// `/annotate <name> <note>` — append a human note to the artifact's
/// `state.json` sidecar (SPEC §4.10 interaction回流): the note becomes
/// agent input on the next Read. `section` may be empty — it's just the
/// margin the note points at.
pub fn annotate(cwd: &Path, name: &str, note: &str) -> String {
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return format!("[invalid artifact name: {name} — [a-z0-9_-]]");
    }
    if note.trim().is_empty() {
        return "[usage: /annotate <name> <note>]".into();
    }
    let dir = cwd.join(".sunmao").join("artifacts");
    if !dir.join(format!("{name}.html")).exists() {
        return format!("[no artifact '{name}' — see /artifacts]");
    }
    let state = dir.join(format!("{name}.state.json"));
    let mut doc: serde_json::Value = std::fs::read_to_string(&state)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({"annotations": []}));
    if !doc.is_object() {
        return format!("[{name}.state.json is not a JSON object — fix by hand]");
    }
    let arr = doc
        .as_object_mut()
        .unwrap()
        .entry("annotations")
        .or_insert_with(|| serde_json::json!([]));
    if !arr.is_array() {
        return format!("[{name}.state.json: 'annotations' is not an array]");
    }
    arr.as_array_mut().unwrap().push(serde_json::json!({
        "section": "",
        "note": note,
        "at": today(),
    }));
    match std::fs::write(&state, serde_json::to_string_pretty(&doc).unwrap()) {
        Ok(()) => format!("[annotated {name} — the agent sees it on next Read]"),
        Err(e) => format!("[write failed: {e}]"),
    }
}

/// Local date as YYYY-MM-DD — civil-from-days, no chrono needed for a
/// timestamp that only ever labels human notes.
fn today() -> String {
    let days = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's civil_from_days — days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
