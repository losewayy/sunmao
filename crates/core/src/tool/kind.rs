//! Which shell executes `Bash` — the embedded POSIX interpreter (identical
//! syntax on Windows and Unix via deno_task_shell) or PowerShell 7 (`pwsh`).
//! One enum because the choice propagates to places that can't all
//! re-resolve it: the executor (shell.rs / pwsh.rs), the approval
//! classifier (agent/mode.rs), the segment splitter (preflight.rs), and the
//! system prompt's dialect section (prompt.rs).
//!
//! Two resolutions share the same pin layers: [`ShellBackend::resolve`]
//! answers for the agent's `Bash` tool, [`ShellBackend::resolve_local`]
//! for the user's `!` local shell. They differ only in the auto-detect
//! default — the agent pays a fresh `pwsh` spawn per call and models write
//! bash more reliably than PowerShell, so auto picks Posix everywhere;
//! the interactive shell keeps `pwsh` on Windows because that's the
//! dialect a Windows operator actually types.
//!
//! Resolution order, first hit wins: `SUNMAO_SHELL` → the project pin
//! `.sunmao/shell.txt` → the user-level pin `~/.sunmao/shell.txt` →
//! auto-detect. Values are `pwsh`/`powershell` (require the `pwsh` binary on
//! PATH — `powershell.exe` is Windows PowerShell 5 and does not count),
//! `posix`/`bash`/`deno` for the embedded interpreter, and `auto` to force
//! detection over a lower layer's pin. An explicit pin applies to BOTH
//! resolutions — asking for pwsh means asking for it everywhere; only the
//! unpinned default differs between agent and operator.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellBackend {
    /// Embedded POSIX backend — auto-selected off Windows and used as pwsh fallback.
    Posix,
    /// `pwsh -NoProfile -Command <text>` — real PowerShell 7 on this box.
    Pwsh,
}

/// Which layer decided the shell backend — reported by `sunmao doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSource {
    /// `SUNMAO_SHELL` environment variable.
    EnvVar,
    /// `<cwd>/.sunmao/shell.txt`.
    ProjectFile,
    /// `~/.sunmao/shell.txt`.
    UserFile,
    /// No explicit value — platform detection picked the backend.
    Auto,
}

impl ShellSource {
    /// Human-readable layer name for doctor-style reports.
    pub fn label(self) -> &'static str {
        match self {
            Self::EnvVar => "SUNMAO_SHELL",
            Self::ProjectFile => ".sunmao/shell.txt",
            Self::UserFile => "~/.sunmao/shell.txt",
            Self::Auto => "auto-detect",
        }
    }
}

/// `resolve` plus the diagnostics a front-end needs: where the answer came
/// from, and whether the layers produced signals the backend choice alone
/// doesn't show — the fallbacks are silent at runtime by design, so doctor
/// is the place that surfaces them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellResolution {
    pub backend: ShellBackend,
    pub source: ShellSource,
    /// A layer asked for `pwsh` but no `pwsh` binary is on PATH.
    pub pwsh_requested_but_missing: bool,
    /// The last layer value that matched no known word — almost always a
    /// typo the operator meant to do something.
    pub unrecognized_value: Option<String>,
}

impl ShellBackend {
    /// The backend only — the executor-facing entry point.
    pub fn resolve(cwd: &Path) -> Self {
        Self::resolve_with(cwd).backend
    }

    /// Backend + provenance for the agent's `Bash` tool. `SUNMAO_SHELL`
    /// beats the project pin, which beats the user-level pin; explicit
    /// values always win over auto-detection.
    pub fn resolve_with(cwd: &Path) -> ShellResolution {
        Self::pick(Self::layers(cwd), cfg!(windows), Self::pwsh_on_path(), false)
    }

    /// The `!` local shell — same pin layers, but the auto default follows
    /// the operator's platform habit, not the model's: `pwsh` on Windows
    /// when the binary exists, Posix elsewhere. `run_local_shell` callers
    /// pass `ctx.local_shell` so the operator's dialect survives the
    /// agent-facing default flip.
    pub fn resolve_local(cwd: &Path) -> Self {
        Self::resolve_local_with(cwd).backend
    }

    /// [`resolve_local`] with provenance, for doctor/settings surfaces.
    pub fn resolve_local_with(cwd: &Path) -> ShellResolution {
        Self::pick(Self::layers(cwd), cfg!(windows), Self::pwsh_on_path(), true)
    }

    fn layers(cwd: &Path) -> [(ShellSource, Option<String>); 3] {
        [
            (ShellSource::EnvVar, std::env::var("SUNMAO_SHELL").ok()),
            (
                ShellSource::ProjectFile,
                read_pin(&cwd.join(".sunmao").join("shell.txt")),
            ),
            (
                ShellSource::UserFile,
                read_pin(&crate::prompt::user_layer_dir().join("shell.txt")),
            ),
        ]
    }

    /// The layered match, pure so tests can drive every combination without
    /// mutating process env. Higher layers win; an explicit `pwsh` on a box
    /// without the binary is recorded and skipped, not an error. When no
    /// layer speaks, `auto_prefers_pwsh` selects the interactive default
    /// (pwsh on Windows) vs the agent default (Posix).
    fn pick(
        layers: [(ShellSource, Option<String>); 3],
        windows: bool,
        pwsh_on_path: bool,
        auto_prefers_pwsh: bool,
    ) -> ShellResolution {
        let mut pwsh_requested_but_missing = false;
        let mut unrecognized_value = None;
        for (source, raw) in layers {
            let Some(raw) = raw else { continue };
            match raw.trim().to_ascii_lowercase().as_str() {
                "pwsh" | "powershell" if pwsh_on_path => {
                    return ShellResolution {
                        backend: Self::Pwsh,
                        source,
                        pwsh_requested_but_missing,
                        unrecognized_value,
                    };
                }
                "pwsh" | "powershell" => pwsh_requested_but_missing = true,
                "posix" | "bash" | "deno" => {
                    return ShellResolution {
                        backend: Self::Posix,
                        source,
                        pwsh_requested_but_missing,
                        unrecognized_value,
                    };
                }
                "auto" => break,
                _ => unrecognized_value = Some(raw.trim().to_string()),
            }
        }
        ShellResolution {
            backend: if windows && pwsh_on_path && auto_prefers_pwsh {
                Self::Pwsh
            } else {
                Self::Posix
            },
            source: ShellSource::Auto,
            pwsh_requested_but_missing,
            unrecognized_value,
        }
    }

    /// `pwsh` must exist before we promise it — PATH lookup, no spawn.
    /// `pwsh` is the PowerShell 7+ binary name; Windows PowerShell 5 ships
    /// as `powershell.exe` and never satisfies this check. Public because
    /// frontends (GUI settings, doctor) report availability separately
    /// from the resolved backend.
    pub fn pwsh_on_path() -> bool {
        let Some(path_var) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path_var)
            .any(|dir| dir.join("pwsh.exe").is_file() || dir.join("pwsh").is_file())
    }
}

/// A shell pin file is one word; a missing or unreadable file is no pin.
fn read_pin(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers(
        env: Option<&str>,
        project: Option<&str>,
        user: Option<&str>,
    ) -> [(ShellSource, Option<String>); 3] {
        [
            (ShellSource::EnvVar, env.map(str::to_string)),
            (ShellSource::ProjectFile, project.map(str::to_string)),
            (ShellSource::UserFile, user.map(str::to_string)),
        ]
    }

    #[test]
    fn windows_auto_detects_pwsh() {
        // the interactive default only — `!` follows the operator's habit
        let r = ShellBackend::pick(layers(None, None, None), true, true, true);
        assert_eq!(r.backend, ShellBackend::Pwsh);
        assert_eq!(r.source, ShellSource::Auto);
    }

    #[test]
    fn agent_auto_detect_is_posix_even_on_windows() {
        // the flip this whole split exists for: the model gets the
        // in-process POSIX shell (no per-call spawn, the dialect LLMs
        // write most reliably) unless a pin explicitly asks for pwsh
        let r = ShellBackend::pick(layers(None, None, None), true, true, false);
        assert_eq!(r.backend, ShellBackend::Posix);
        assert_eq!(r.source, ShellSource::Auto);
        // an explicit pwsh pin still reaches the agent — asking for it
        // means asking for it everywhere
        let r = ShellBackend::pick(layers(Some("pwsh"), None, None), true, true, false);
        assert_eq!(r.backend, ShellBackend::Pwsh);
        assert_eq!(r.source, ShellSource::EnvVar);
    }

    #[test]
    fn auto_detect_never_leaves_posix_off_windows() {
        // No config + no pwsh on PATH, or non-Windows entirely → Posix.
        for (windows, on_path) in [(true, false), (false, true), (false, false)] {
            let r = ShellBackend::pick(layers(None, None, None), windows, on_path, true);
            assert_eq!(r.backend, ShellBackend::Posix);
            assert_eq!(r.source, ShellSource::Auto);
        }
    }

    #[test]
    fn explicit_posix_beats_autodetect() {
        for (env, project, user) in [
            (Some("posix"), None, None),
            (None, Some("bash"), None),
            (None, None, Some("deno")),
        ] {
            let r = ShellBackend::pick(layers(env, project, user), true, true, true);
            assert_eq!(r.backend, ShellBackend::Posix);
            assert_ne!(r.source, ShellSource::Auto);
        }
    }

    #[test]
    fn env_wins_over_project_file() {
        let r = ShellBackend::pick(layers(Some("posix"), Some("pwsh"), None), true, true, true);
        assert_eq!(r.backend, ShellBackend::Posix);
        assert_eq!(r.source, ShellSource::EnvVar);
    }

    #[test]
    fn project_file_wins_over_user_file() {
        let r = ShellBackend::pick(layers(None, Some("posix"), Some("pwsh")), true, true, true);
        assert_eq!(r.backend, ShellBackend::Posix);
        assert_eq!(r.source, ShellSource::ProjectFile);
    }

    #[test]
    fn missing_pwsh_falls_through_and_is_flagged() {
        let r = ShellBackend::pick(layers(Some("pwsh"), Some("posix"), None), true, false, true);
        assert_eq!(r.backend, ShellBackend::Posix);
        assert_eq!(r.source, ShellSource::ProjectFile);
        assert!(r.pwsh_requested_but_missing);
    }

    #[test]
    fn auto_overrides_lower_pin() {
        let r = ShellBackend::pick(layers(Some("auto"), Some("posix"), None), true, true, true);
        assert_eq!(r.backend, ShellBackend::Pwsh);
        assert_eq!(r.source, ShellSource::Auto);
        // …and on a box without pwsh the same pin still lands Posix.
        let r = ShellBackend::pick(layers(Some("auto"), Some("pwsh"), None), true, false, true);
        assert_eq!(r.backend, ShellBackend::Posix);
        assert_eq!(r.source, ShellSource::Auto);
    }

    #[test]
    fn garbage_values_are_skipped() {
        let r = ShellBackend::pick(layers(Some("fish"), Some("posix"), None), true, true, true);
        assert_eq!(r.backend, ShellBackend::Posix);
        assert_eq!(r.source, ShellSource::ProjectFile);
        assert_eq!(r.unrecognized_value.as_deref(), Some("fish"));
    }

    #[test]
    fn project_pin_file_drives_resolve() {
        if std::env::var_os("SUNMAO_SHELL").is_some() {
            return; // env beats every layer — nothing left to assert
        }
        let dir = crate::fresh_test_dir("shell-pin");
        std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
        std::fs::write(dir.join(".sunmao/shell.txt"), "posix\n").unwrap();
        assert_eq!(ShellBackend::resolve(&dir), ShellBackend::Posix);
    }
}
