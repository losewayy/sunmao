//! Slash-command vocabulary — the one place `/name` lines are understood.
//! The REPL, the TUI composer, and the serve driver all funnel a `/<name>
//! [args]` line through `parse` here and get a `Command` back; what a
//! command *does* stays frontend-side (`&AgentLoop` for REPL/TUI, the
//! mgmt lane + broadcast frames for serve). Also shared: the file-command
//! resolution (`command_body`/`expand_command`), the artifact helpers
//! behind `/artifacts` and `/annotate`, the `@`-mention path pool, and the
//! note-text builders so all frontends phrase identical answers.

use std::path::{Path, PathBuf};

use sunmao_core::agent::ApprovalMode;
use sunmao_core::context::TaskEntry;
use sunmao_core::tool::TodoItem;

/// Builtin command names — the `/` menus offer these alongside the
/// file-backed `.md` commands found by `candidates`. Frontend-local
/// entries (`clear`, `multiline`, `quit`) parse to their `Command`
/// variant like everything else; a frontend that can't run one treats it
/// as unknown.
const BUILTINS: &[&str] = &[
    "annotate",
    "artifacts",
    "clear",
    "compact",
    "help",
    "mode",
    "model",
    "multiline",
    "quit",
    "resume",
    "rewind",
    "sessions",
    "fork",
    "tasks",
    "todos",
];

/// The backend-semantic command tail every frontend's `/help` shares;
/// frontend-local names prepend via `help_text`'s `local` argument.
const HELP_COMMANDS: &str = "/compact · /model [sel] · /mode [stance] · /resume [id] · /sessions · /fork <id> · /rewind [n] [session|code|both] · /tasks · /todos · /artifacts · /annotate <name> <note> · /help";

/// The `/help` commands line — `local` inserts frontend-only names
/// (`"/multiline · /clear · "` for the TUI, `""` elsewhere) ahead of the
/// shared tail.
pub fn help_text(local: &str) -> String {
    format!(
        "commands — {local}{HELP_COMMANDS} · /quit · + every *.md in .sunmao/commands, .claude/commands, plugins/*/commands"
    )
}

/// A parsed `/…` line — command vocabulary only; execution is the
/// frontend's (`&AgentLoop` locally, `s.mgmt` oneshots for serve).
#[derive(Debug, Clone)]
pub enum Command {
    /// /quit | /exit | /q — frontend-local
    Quit,
    /// /compact — fold the transcript into the session log
    Compact,
    /// /model [selector] — bare lists choices, arg swaps the adapter
    Model(Option<String>),
    /// /mode [stance] — bare lists stances, arg switches approval mode
    Mode(Option<String>),
    /// /resume [id|path] — bare lists recent sessions
    Resume(Option<String>),
    /// /fork <id|path> — copy the log to a fresh id, resume the copy
    Fork(String),
    /// /rewind [n] [session|code|both] — bare lists turn boundaries
    Rewind(Option<RewindSpec>),
    /// /sessions [id] — the resume picker alias: an arg resumes in the
    /// TUI, bare lists recent sessions everywhere
    Sessions(Option<String>),
    /// /tasks — the live sub-agent roster
    Tasks,
    /// /todos — the model's session task list
    Todos,
    /// /artifacts — the .sunmao/artifacts listing
    Artifacts,
    /// /annotate <name> <note> — human notes into artifact state.json
    Annotate(String, String),
    /// /help | /h | /?
    Help,
    /// /clear — frontend-local (transcript reset)
    Clear,
    /// /multiline | /ml — frontend-local (composer mode)
    Multiline,
    /// a recognized builtin with bad/missing args — the note to show
    Note(String),
    /// not a builtin — resolve as a file command, else unknown
    Other,
}

/// `/rewind <n> [mode]` fully parsed — the turn ordinal plus which
/// surfaces it touches.
#[derive(Debug, Clone, Copy)]
pub struct RewindSpec {
    /// 1-based turn ordinal — rewind to just before its boundary
    pub turn: u64,
    pub mode: crate::rewind::Mode,
}

/// `/rewind n [session|code|both]` → the spec, or the error note the
/// frontend shows. Same grammar `rewind::run` used to parse by itself.
fn rewind_spec(rest: &str) -> Result<RewindSpec, String> {
    let mut it = rest.split_whitespace();
    let n: u64 = it
        .next()
        .and_then(|t| t.parse().ok())
        .filter(|&n| n >= 1)
        .ok_or_else(|| "[usage: /rewind <n> [session|code|both]]".to_string())?;
    let mode = crate::rewind::Mode::parse(it.next())
        .ok_or_else(|| "[unknown mode — session|code|both]".to_string())?;
    if it.next().is_some() {
        return Err("[usage: /rewind <n> [session|code|both]]".to_string());
    }
    Ok(RewindSpec { turn: n, mode })
}

/// Parse a `/<name> [rest]` line (leading `/` already stripped). File
/// commands and unknown names come back as `Other`.
pub fn parse(cmd_line: &str) -> Command {
    let name = cmd_line.split_whitespace().next().unwrap_or("");
    let rest = cmd_line[name.len()..].trim();
    let arg = || (!rest.is_empty()).then(|| rest.to_string());
    match name {
        "quit" | "exit" | "q" => Command::Quit,
        "compact" => Command::Compact,
        "model" => Command::Model(arg()),
        "mode" => Command::Mode(arg()),
        "resume" => Command::Resume(arg()),
        "fork" => match arg() {
            Some(id) => Command::Fork(id),
            None => Command::Note("[usage: /fork <id>]".into()),
        },
        "rewind" => match arg() {
            None => Command::Rewind(None),
            Some(r) => match rewind_spec(&r) {
                Ok(spec) => Command::Rewind(Some(spec)),
                Err(note) => Command::Note(note),
            },
        },
        "sessions" => Command::Sessions(arg()),
        "tasks" => Command::Tasks,
        "todos" => Command::Todos,
        "artifacts" => Command::Artifacts,
        "annotate" => {
            let mut it = rest.splitn(2, char::is_whitespace);
            match (it.next(), it.next().map(str::trim)) {
                (Some(n), Some(t)) if !n.is_empty() && !t.is_empty() => {
                    Command::Annotate(n.to_string(), t.to_string())
                }
                _ => Command::Note("[usage: /annotate <name> <note>]".into()),
            }
        }
        "help" | "h" | "?" => Command::Help,
        "clear" => Command::Clear,
        "multiline" | "ml" => Command::Multiline,
        _ => Command::Other,
    }
}

/// Names the `/` menu should offer: builtins + every `<name>.md` found in
/// the convention dirs under `cwd` plus the enabled preset roots.
pub fn candidates(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = BUILTINS.iter().map(|s| s.to_string()).collect();
    for dir in command_dirs(cwd, extra_roots) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "md").unwrap_or(false)
                    && let Some(stem) = p.file_stem()
                {
                    names.push(stem.to_string_lossy().to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// `/review` → `.sunmao/commands/review.md` or `.claude/commands/review.md`
/// (same convention, both dirs scanned). Returns the file body.
pub fn command_body(cwd: &Path, extra_roots: &[PathBuf], name: &str) -> Option<String> {
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    for dir in command_dirs(cwd, extra_roots) {
        let p = dir.join(format!("{name}.md"));
        if let Ok(t) = std::fs::read_to_string(&p) {
            return Some(t);
        }
    }
    None
}

/// Expand a command body with the typed tail — the ecosystem's
/// `$ARGUMENTS` / `$1`..$9 substitution (Claude Code convention). A body
/// mentioning none of the placeholders gets the args appended after a
/// blank line, same as before — zero-config commands just work.
pub fn expand_command(body: &str, args: &str) -> String {
    if args.is_empty() {
        return body.to_string();
    }
    let mut out = body.replace("$ARGUMENTS", args);
    for (i, word) in args.split_whitespace().enumerate().take(9) {
        out = out.replace(&format!("${}", i + 1), word);
    }
    if out == *body {
        format!("{body}\n\n{args}")
    } else {
        // substituted at least once; any leftover $N for missing words
        // collapses to the empty string rather than leaking the marker
        for i in 1..=9 {
            out = out.replace(&format!("${i}"), "");
        }
        out
    }
}

/// All dirs slash commands may live in — project, claude-compat, plugin
/// dirs, then enabled presets (appended last in layering order; lookup is
/// first-hit so a preset command only fills a name nobody else claims).
fn command_dirs(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = vec![
        cwd.join(".sunmao/commands"),
        cwd.join(".claude/commands"),
        cwd.join(".sunmao/plugin/commands"),
    ];
    for base in [cwd.join(".sunmao/plugins"), cwd.join(".claude/plugins")] {
        // deterministic enumeration — same rule as core's sorted_entries:
        // first-hit resolution must not depend on filesystem order
        let mut plugins: Vec<_> = std::fs::read_dir(&base)
            .map(|rd| rd.flatten().map(|p| p.path()).collect())
            .unwrap_or_default();
        plugins.sort();
        for p in plugins {
            dirs.push(p.join("commands"));
        }
    }
    for root in extra_roots {
        dirs.push(root.join("commands"));
    }
    dirs
}

/// Repo-relative path pool for `@` mention completion (`GET /paths` and
/// the TUI's Path menu share this) — depth-bounded walk that skips
/// VCS/build/dependency dirs (they'd drown the menu in generated paths;
/// the model can still reach them by name). Directories carry a `/`
/// suffix: the descent marker and how a frontend knows not to terminate.
pub fn scan_files(root: &Path) -> Vec<String> {
    const SKIP: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        "__pycache__",
        ".venv",
        "dist",
        "build",
        ".dart_tool",
        ".idea",
        ".vscode",
    ];
    const MAX_DEPTH: usize = 6;
    const MAX_ENTRIES: usize = 3000;
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize, String::new())];
    while let Some((dir, depth, prefix)) = stack.pop() {
        if out.len() >= MAX_ENTRIES {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if out.len() >= MAX_ENTRIES {
                break;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let rel = format!("{prefix}{name}");
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if SKIP.contains(&name.as_str()) {
                    continue;
                }
                out.push(format!("{rel}/"));
                if depth < MAX_DEPTH {
                    stack.push((e.path(), depth + 1, format!("{rel}/")));
                }
            } else {
                out.push(rel);
            }
        }
    }
    out.sort();
    out
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

// ── note text — one phrasing across the three surfaces ─────────────────

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

#[cfg(test)]
mod tests {
    use super::{Command, expand_command, parse};

    /// `$ARGUMENTS` substitutes inline where the command author put it —
    /// not appended at the end. The ecosystem contract (Claude Code
    /// command files) sunmao claims compatibility with.
    #[test]
    fn arguments_placeholder_substitutes_inline() {
        let body = "Summarize $ARGUMENTS in three bullets.";
        assert_eq!(
            expand_command(body, "the diff below"),
            "Summarize the diff below in three bullets."
        );
    }

    /// Positional $1..$9 resolve per whitespace-split word; missing
    /// positions collapse to empty rather than leaking the marker.
    #[test]
    fn positional_placeholders_fill_then_clear() {
        assert_eq!(
            expand_command("compare $1 with $2", "foo bar"),
            "compare foo with bar"
        );
        assert_eq!(expand_command("hello $1 you $2", "foo"), "hello foo you ");
    }

    /// A body with no placeholder still gets the args appended — the
    /// zero-config convention that predates the placeholders.
    #[test]
    fn no_placeholder_appends_args() {
        assert_eq!(
            expand_command("Review the most recent commit.", "extra notes"),
            "Review the most recent commit.\n\nextra notes"
        );
        // empty args never append
        assert_eq!(expand_command("body", ""), "body");
    }

    /// Builtin names parse to their variants; aliases collapse.
    #[test]
    fn builtin_parse() {
        assert!(matches!(parse("quit"), Command::Quit));
        assert!(matches!(parse("compact"), Command::Compact));
        assert!(matches!(parse("model"), Command::Model(None)));
        assert!(matches!(
            parse("model default/qwen-flash"),
            Command::Model(Some(ref s)) if s == "default/qwen-flash"
        ));
        assert!(matches!(
            parse("resume s-1"),
            Command::Resume(Some(ref s)) if s == "s-1"
        ));
        assert!(matches!(parse("fork s-1"), Command::Fork(ref s) if s == "s-1"));
        assert!(matches!(parse("fork"), Command::Note(_)));
        assert!(matches!(parse("sessions"), Command::Sessions(None)));
        assert!(matches!(parse("mode auto"), Command::Mode(Some(_))));
        assert!(matches!(parse("h"), Command::Help));
        assert!(matches!(parse("rewind"), Command::Rewind(None)));
        assert!(matches!(
            parse("rewind 2 code"),
            Command::Rewind(Some(spec)) if spec.turn == 2 && spec.mode == crate::rewind::Mode::Code
        ));
        assert!(matches!(parse("rewind x"), Command::Note(_)));
        assert!(matches!(parse("annotate x"), Command::Note(_)));
        assert!(matches!(
            parse("annotate x a note"),
            Command::Annotate(ref n, ref t) if n == "x" && t == "a note"
        ));
        assert!(matches!(parse("review stuff"), Command::Other));
    }
}
