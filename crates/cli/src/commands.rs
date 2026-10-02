//! Slash-command vocabulary — the one place `/name` lines are understood.
//! The REPL, the TUI composer, and the serve driver all funnel a `/<name>
//! [args]` line through `parse` here and get a `Command` back; what a
//! command *does* stays frontend-side (`&AgentLoop` for REPL/TUI, the
//! mgmt lane + broadcast frames for serve). Also shared: the file-command
//! resolution (`command_body`/`expand_command`), the artifact helpers
//! behind `/artifacts` and `/annotate`, the `@`-mention path pool, and the
//! note-text builders so all frontends phrase identical answers.

use std::path::{Path, PathBuf};

/// Note-text builders (`/tasks`, `/mcp`, `/status`, `/artifacts`, …) —
/// one phrasing across the three frontends, kept in a sibling file so the
/// vocabulary here stays under the god-file budget.
#[path = "commands/notes.rs"]
pub mod notes;
pub use notes::*;

/// `/export-md` — session log → shareable markdown transcript.
#[path = "commands/export.rs"]
pub mod export;
pub use export::*;

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
    "mcp",
    "mode",
    "model",
    "multiline",
    "quit",
    "export-md",
    "export-zip",
    "stop",
    "resume",
    "rewind",
    "search",
    "sessions",
    "fork",
    "goal",
    "status",
    "tasks",
    "todos",
];

/// Builtin one-line descriptions (`assets/slash-descs.txt`, cold-plug rule
/// 6) — slash menu, command palette and the TUI popup all read this.
/// `name\t一行动说明` per line; unknown/file commands get `""`.
pub fn desc(name: &str) -> &'static str {
    static TABLE: std::sync::OnceLock<Vec<(&'static str, &'static str)>> =
        std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| {
            include_str!("../assets/slash-descs.txt")
                .lines()
                .filter_map(|l| l.split_once('\t'))
                .collect()
        })
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, d)| *d)
        .unwrap_or("")
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
    /// /export-zip — debug bundle: transcript + raw session log zipped
    ExportZip,
    /// /export-md — write the session transcript as markdown
    Export,
    /// /sessions [id] — the resume picker alias: an arg resumes in the
    /// TUI, bare lists recent sessions everywhere
    Sessions(Option<String>),
    /// /search <q> — cross-session grep over every known sessions dir;
    /// frontends render `sessions::search_text` (or the REST rows)
    Search(String),
    /// /tasks — the live sub-agent roster
    Tasks,
    /// /stop <sub-…-lN> — cancel one running sub-agent; the roster's kill
    /// switch surfaced as a builtin (the `task_cancel` ws frame is its
    /// served equivalent — frontends share `cancel_sub` underneath)
    Stop(String),
    /// /todos — the model's session task list
    Todos,
    /// /goal [objective] — bare shows the standing goal; text sets it and
    /// kicks the continuation loop; `clear` abandons it
    Goal(Option<String>),
    /// /goal clear — explicit human stop (distinct parse so `/goal clear
    /// skies` can still be an objective)
    GoalClear,
    /// /mcp — the connected MCP server roster
    Mcp,
    /// /status — session vitals (model/provider/cwd/id/mode/tokens)
    Status,
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
        "export-md" => Command::Export,
        "export-zip" => Command::ExportZip,
        "stop" => match arg() {
            Some(id) => Command::Stop(id),
            None => Command::Note("[usage: /stop <sub-…-lN>]".into()),
        },
        "sessions" => Command::Sessions(arg()),
        "search" => match arg() {
            Some(q) => Command::Search(q),
            None => Command::Note("[usage: /search <query>]".into()),
        },
        "tasks" => Command::Tasks,
        "todos" => Command::Todos,
        "goal" => match arg().as_deref() {
            Some("clear") | Some("off") => Command::GoalClear,
            other => Command::Goal(other.map(str::to_string)),
        },
        "mcp" => Command::Mcp,
        "status" => Command::Status,
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
        assert!(matches!(parse("export-md"), Command::Export));
        assert!(matches!(parse("export-zip"), Command::ExportZip));
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
        assert!(matches!(parse("search"), Command::Note(_)));
        assert!(matches!(
            parse("search helo wrld"),
            Command::Search(ref q) if q == "helo wrld"
        ));
        assert!(matches!(parse("mcp"), Command::Mcp));
        assert!(matches!(parse("status"), Command::Status));
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
