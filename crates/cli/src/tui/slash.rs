//! Slash commands — Markdown files under the convention dirs, plus the
//! builtin names the frontends handle themselves. Shared by the REPL, the
//! TUI driver, and the TUI's `/` popup.

use std::path::{Path, PathBuf};

/// Builtin commands handled locally (not file-backed). Shown in the menu
/// alongside file commands.
const BUILTINS: &[&str] = &[
    "annotate",
    "artifacts",
    "clear",
    "compact",
    "help",
    "model",
    "multiline",
    "quit",
    "resume",
    "sessions",
    "fork",
    "tasks",
    "todos",
];

/// Names the `/` menu should offer: builtins + every `<name>.md` found in
/// the convention dirs under `cwd` plus the enabled preset roots.
pub fn candidates(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = BUILTINS.iter().map(|s| s.to_string()).collect();
    for dir in command_dirs(cwd, extra_roots) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "md").unwrap_or(false)
                    && let Some(stem) = p.file_stem() {
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

/// `.sunmao/artifacts` listing for `/artifacts` — one row per HtmlArtifact
/// output, `state.json` sidecars flagged (unresolved human notes live/// there). Shared by the REPL and the TUI driver.
pub fn artifacts_text(cwd: &Path) -> String {
    let dir = cwd.join(".sunmao").join("artifacts");
    let mut rows: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().map(|x| x == "html").unwrap_or(false) {
                        let name = p.file_stem()?.to_string_lossy().to_string();
                        let bytes = e.metadata().ok()?.len();
                        let notes = p.with_extension("state.json").exists();
                        Some(format!(
                            "  {name:<24} {bytes:>7} B{}",
                            if notes { "  +notes" } else { "" }
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

#[cfg(test)]
mod tests {
    use super::expand_command;

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
}
