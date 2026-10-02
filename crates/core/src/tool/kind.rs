//! Which shell executes `Bash` — the embedded POSIX interpreter (the
//! default; identical syntax on Windows and Unix via deno_task_shell) or
//! PowerShell 7 (`pwsh`) when the site asks for it. One enum because the
//! choice propagates to places that can't all re-resolve it: the executor
//! (shell.rs / pwsh.rs), the approval classifier (agent/mode.rs), the
//! segment splitter (preflight.rs), and the system prompt's dialect
//! section (prompt.rs).
//!
//! Resolution order is flag-free for now: `SUNMAO_SHELL` env var wins over
//! a project's `.sunmao/shell.txt` (one word: `pwsh`/`powershell`/`posix`/
//! `bash`), Posix otherwise. `pwsh` resolves only when the binary is
//! actually on PATH — a machine without it silently falls back rather than
//! breaking every Bash call at spawn.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellBackend {
    /// deno_task_shell in-process interpreter — the product default.
    Posix,
    /// `pwsh -NoProfile -Command <text>` — real PowerShell 7 on this box.
    Pwsh,
}

impl ShellBackend {
    /// Site-level choice: `SUNMAO_SHELL` (the user's machine default) beats
    /// the project pin `.sunmao/shell.txt`; both absent → Posix.
    pub fn resolve(cwd: &Path) -> Self {
        let from_env = std::env::var("SUNMAO_SHELL").ok();
        let from_file = std::fs::read_to_string(cwd.join(".sunmao").join("shell.txt")).ok();
        for raw in [from_env, from_file].into_iter().flatten() {
            match raw.trim().to_ascii_lowercase().as_str() {
                "pwsh" | "powershell" if Self::pwsh_on_path() => return Self::Pwsh,
                "posix" | "bash" | "deno" => return Self::Posix,
                _ => {}
            }
        }
        Self::Posix
    }

    /// `pwsh` must exist before we promise it — PATH lookup, no spawn.
    fn pwsh_on_path() -> bool {
        let Some(path_var) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path_var)
            .any(|dir| dir.join("pwsh.exe").is_file() || dir.join("pwsh").is_file())
    }
}
