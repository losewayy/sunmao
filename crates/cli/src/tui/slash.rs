//! Slash commands — Markdown files under the convention dirs, plus the
//! builtin names the frontends handle themselves. Shared by the REPL, the
//! TUI driver, and the TUI's `/` popup.

use std::path::{Path, PathBuf};

/// Builtin commands handled locally (not file-backed). Shown in the menu
/// alongside file commands.
const BUILTINS: &[&str] = &[
    "artifacts",
    "clear",
    "compact",
    "help",
    "model",
    "multiline",
    "quit",
    "resume",
    "sessions",
    "tasks",
];

/// Names the `/` menu should offer: builtins + every `<name>.md` found in
/// the convention dirs under `cwd` plus the enabled preset roots.
pub fn candidates(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = BUILTINS.iter().map(|s| s.to_string()).collect();
    for dir in command_dirs(cwd, extra_roots) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "md").unwrap_or(false) {
                    if let Some(stem) = p.file_stem() {
                        names.push(stem.to_string_lossy().to_string());
                    }
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
        if let Ok(plugins) = std::fs::read_dir(&base) {
            for p in plugins.flatten() {
                dirs.push(p.path().join("commands"));
            }
        }
    }
    for root in extra_roots {
        dirs.push(root.join("commands"));
    }
    dirs
}

/// `.sunmao/artifacts` listing for `/artifacts` — one row per HtmlArtifact
/// output, `state.json` sidecars flagged (unresolved human notes live
/// there). Shared by the REPL and the TUI driver.
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
