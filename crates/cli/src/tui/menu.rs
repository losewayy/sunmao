//! Completion popups — the slash-command menu's three kinds (command
//! names, `/model` selectors, `@` file mentions) and the path pool that
//! backs mention completion. State lives on `App`; this file owns the
//! *what completes and how a candidate is applied* half.

use super::app::{App, char_to_byte};
use super::slash;

/// What the completion popup is serving — drives its title, hint, and
/// the accept action (a path candidate rewrites the `@` fragment in
/// place; it never submits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// `/…` fragment — builtin + convention-dir commands
    Command,
    /// argument of a chosen builtin (today: `/model <selector>`)
    Args,
    /// `/resume <id>` / `/sessions` — session-file picker (newest first)
    Sessions,
    /// `@…` fragment — repo-relative file/dir mention (CC convention)
    Path,
}

/// Completion popup state. Open while the composer is exactly a `/…`
/// fragment with no whitespace; walks `slash::candidates`. In `Args`
/// mode it completes the *argument* of an already-chosen builtin (today
/// only `/model <selector>`) — the menu rows are selectors, not commands.
/// In `Path` mode the trigger is `@` and rows are repo-relative paths.
pub struct SlashMenu {
    /// name list filtered by the fragment after the trigger
    pub matches: Vec<String>,
    pub selected: usize,
    /// the fragment that produced `matches` (drives the Search row)
    pub fragment: String,
    pub kind: MenuKind,
    /// Sessions kind only: which builtin opened the picker — `resume`,
    /// `fork`, or `sessions` (alias of resume). Decides what Enter
    /// submits; the session row is the arg, not the command.
    pub cmd: Option<String>,
}

/// Session ids under `<cwd>/.sunmao/sessions`, newest first (mtime),
/// `.jsonl` stems only, capped at `limit`. Shared by the `/resume`
/// picker (menu.rs) and the bare `/resume` list (mod.rs) — one truth
/// for "what sessions exist".
pub(crate) fn recent_sessions(cwd: &std::path::Path, limit: usize) -> Vec<String> {
    let dir = cwd.join(".sunmao").join("sessions");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                        let stem = p.file_stem()?.to_string_lossy().to_string();
                        let m = e.metadata().ok()?.modified().ok()?;
                        Some((m, stem))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by_key(|b| std::cmp::Reverse(b.0));
    entries.into_iter().take(limit).map(|(_, s)| s).collect()
}

impl App {
    /// The composer is a slash fragment: `/cmd` (command completion) or
    /// `/model <arg>`/ `/resume <id>` (arg completion). Returns the arg
    /// kind + fragment. Bash mode owns the buffer, so no menu there.
    fn slash_fragment(&self) -> Option<(MenuKind, &str)> {
        if self.bash_mode {
            return None;
        }
        let rest = self.input.strip_prefix('/')?;
        if let Some(frag) = rest.strip_prefix("model ") {
            return Some((MenuKind::Args, frag));
        }
        for name in ["resume ", "sessions ", "fork "] {
            if let Some(frag) = rest.strip_prefix(name) {
                return Some((MenuKind::Sessions, frag));
            }
        }
        if rest.chars().any(char::is_whitespace) {
            return None;
        }
        Some((MenuKind::Command, rest))
    }

    /// The `@` mention fragment immediately before the cursor, if any:
    /// (start byte, end byte, fragment). Word-bounded on the left, no
    /// whitespace inside — `main.rs` and `src/` trigger, `a@b` doesn't.
    /// Bash mode owns the buffer — no completion there either.
    fn at_fragment(&self) -> Option<(usize, usize, &str)> {
        if self.bash_mode {
            return None;
        }
        let upto = char_to_byte(&self.input, self.cursor);
        let at = self.input[..upto].rfind('@')?;
        if at > 0 {
            let prev = self.input[..at].chars().last()?;
            if !prev.is_whitespace() {
                return None;
            }
        }
        let frag = &self.input[at + 1..upto];
        if frag.chars().any(char::is_whitespace) {
            return None;
        }
        Some((at + 1, upto, frag))
    }

    /// Apply the highlighted Path candidate: rewrite the `@` fragment in
    /// place, keep directories open for descent (`dir/` → menu reopens),
    /// land a trailing space on files. Returns false if the fragment
    /// moved under us — caller falls back to closing the menu.
    pub fn accept_path_candidate(&mut self, name: &str) -> bool {
        let Some((start, end, _)) = self.at_fragment() else {
            return false;
        };
        let is_dir = name.ends_with('/');
        // files get a trailing space — except when real text already
        // follows the fragment (mid-sentence would double-space)
        let fill = if is_dir {
            name.to_string()
        } else {
            match self.input[end..].chars().next() {
                None => format!("{name} "),
                Some(c) if !c.is_whitespace() => format!("{name} "),
                _ => name.to_string(),
            }
        };
        self.input.replace_range(start..end, &fill);
        self.cursor = self.input[..start].chars().count() + fill.chars().count();
        if is_dir {
            self.refresh_slash_menu();
        } else {
            self.slash_menu = None;
        }
        true
    }

    /// Repo-relative path pool for `@` completion — depth-bounded walk
    /// that skips VCS/build/dependency dirs (they'd drown the menu in
    /// generated paths; the model can still reach them by name).
    /// Directories carry a `/` suffix: that's both the descent marker
    /// and how `accept_path_candidate` knows not to terminate.
    fn scan_files(&self) -> Vec<String> {
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
        let mut stack = vec![(self.cwd.clone(), 0usize, String::new())];
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

    /// Session ids for `/resume` completion — rescanned when the menu
    /// opens so sessions the model spawned mid-turn show up too.
    fn scan_sessions(&self) -> Vec<String> {
        recent_sessions(&self.cwd, 50)
    }

    /// Re-evaluate which completion popup (if any) the composer shows —
    /// `@` mentions preempt the slash menu (`/x @y` completes y).
    pub fn refresh_slash_menu(&mut self) {
        if let Some((_, _, frag)) = self.at_fragment() {
            let frag = frag.to_string();
            // opening the menu rebuilds the pool; while it stays open the
            // pool is stable so filtering is per-keystroke cheap.
            if self
                .slash_menu
                .as_ref()
                .is_none_or(|m| m.kind != MenuKind::Path)
            {
                self.file_pool = self.scan_files();
            }
            let matches: Vec<String> = self
                .file_pool
                .iter()
                .filter(|c| c.starts_with(&frag) || c.contains(&frag))
                .take(50)
                .cloned()
                .collect();
            if matches.is_empty() {
                self.slash_menu = None;
            } else {
                let sel = self
                    .slash_menu
                    .as_ref()
                    .map(|m| m.selected.min(matches.len() - 1))
                    .unwrap_or(0);
                self.slash_menu = Some(SlashMenu {
                    matches,
                    selected: sel,
                    fragment: frag,
                    kind: MenuKind::Path,
                    cmd: None,
                });
            }
            return;
        }
        match self.slash_fragment() {
            Some((MenuKind::Sessions, frag)) => {
                let frag = frag.to_string();
                // which builtin opened the picker — sessions aliases resume
                let cmd = self
                    .input
                    .strip_prefix('/')
                    .and_then(|r| r.split_whitespace().next())
                    .map(|c| if c == "sessions" { "resume" } else { c })
                    .unwrap_or("resume")
                    .to_string();
                if self
                    .slash_menu
                    .as_ref()
                    .is_none_or(|m| m.kind != MenuKind::Sessions)
                {
                    self.session_ids = self.scan_sessions();
                }
                let matches: Vec<String> = self
                    .session_ids
                    .iter()
                    .filter(|c| c.starts_with(&frag) || c.contains(&frag))
                    .cloned()
                    .collect();
                if matches.is_empty() {
                    self.slash_menu = None;
                } else {
                    let sel = self
                        .slash_menu
                        .as_ref()
                        .map(|m| m.selected.min(matches.len() - 1))
                        .unwrap_or(0);
                    self.slash_menu = Some(SlashMenu {
                        matches,
                        selected: sel,
                        fragment: frag,
                        kind: MenuKind::Sessions,
                        cmd: Some(cmd),
                    });
                }
            }
            Some((kind, frag)) => {
                let pool = match kind {
                    MenuKind::Args => self.model_selectors.clone(),
                    _ => slash::candidates(&self.cwd, &self.extra_roots),
                };
                let matches: Vec<String> = pool
                    .into_iter()
                    .filter(|c| c.starts_with(frag) || c.contains(frag))
                    .collect();
                if matches.is_empty() {
                    self.slash_menu = None;
                } else {
                    let sel = self
                        .slash_menu
                        .as_ref()
                        .map(|m| m.selected.min(matches.len() - 1))
                        .unwrap_or(0);
                    self.slash_menu = Some(SlashMenu {
                        matches,
                        selected: sel,
                        fragment: frag.to_string(),
                        kind,
                        cmd: None,
                    });
                }
            }
            None => self.slash_menu = None,
        }
    }
}
