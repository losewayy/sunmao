//! Slash commands — Markdown files under the convention dirs, plus the
//! builtin names the frontends handle themselves. Shared by the REPL, the
//! TUI driver, and the TUI's `/` popup.

use std::path::Path;

/// Builtin commands handled locally (not file-backed). Shown in the menu
/// alongside file commands.
const BUILTINS: &[&str] = &["clear", "compact", "help", "multiline", "quit"];

/// Names the `/` menu should offer: builtins + every `<name>.md` found in
/// the convention dirs under `cwd`.
pub fn candidates(cwd: &Path) -> Vec<String> {
    let mut names: Vec<String> = BUILTINS.iter().map(|s| s.to_string()).collect();
    for dir in command_dirs(cwd) {
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
pub fn command_body(cwd: &Path, name: &str) -> Option<String> {
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    for dir in command_dirs(cwd) {
        let p = dir.join(format!("{name}.md"));
        if let Ok(t) = std::fs::read_to_string(&p) {
            return Some(t);
        }
    }
    None
}

/// All dirs slash commands may live in — project, claude-compat, plugin dirs.
fn command_dirs(cwd: &Path) -> Vec<std::path::PathBuf> {
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
    dirs
}
