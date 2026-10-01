//! Session-log helpers the frontends share — `<cwd>/.sunmao/sessions`
//! resolution, the recent-list the `/resume` picker and bare `/sessions`
//! render, and the copy-to-fresh-id that backs local `/fork`.

use std::path::{Path, PathBuf};

/// `<cwd>/.sunmao/sessions` — where every local frontend anchors session
/// lookups.
pub fn sessions_dir(cwd: &Path) -> PathBuf {
    cwd.join(".sunmao").join("sessions")
}

/// Session ids under `<cwd>/.sunmao/sessions`, newest first (mtime),
/// `.jsonl` stems only, capped at `limit`. One truth for "what sessions
/// exist" — the `/resume` pickers and the bare-command lists share it.
pub fn recent_sessions(cwd: &Path, limit: usize) -> Vec<String> {
    let dir = sessions_dir(cwd);
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

/// The bare `/sessions` + bare `/resume` body — recent logs phrased as
/// the command that would adopt them.
pub fn recent_sessions_text(cwd: &Path, limit: usize) -> String {
    let list = recent_sessions(cwd, limit)
        .iter()
        .map(|s| format!("  /resume {s}"))
        .collect::<Vec<_>>()
        .join("\n");
    if list.is_empty() {
        "[no sessions]".into()
    } else {
        format!("recent sessions:\n{list}")
    }
}

/// `/resume`/`/fork` target resolution — `id` may be a path that exists,
/// otherwise a bare session id resolved under `cwd`'s sessions dir.
pub fn resolve_log_path(cwd: &Path, id: &str) -> PathBuf {
    let p = PathBuf::from(id);
    if p.exists() {
        p
    } else {
        sessions_dir(cwd).join(format!("{id}.jsonl"))
    }
}

/// `/fork` half that never touches the session state: copy the resolved
/// log to a fresh `s-<ms>-fork` id under the project's sessions dir.
/// Returns `(new_id, dst)`.
pub fn fork_copy(cwd: &Path, id: &str) -> Result<(String, PathBuf), String> {
    let src = resolve_log_path(cwd, id);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let new_id = format!("s-{ms}-fork");
    let dst = sessions_dir(cwd).join(format!("{new_id}.jsonl"));
    std::fs::copy(&src, &dst).map_err(|e| format!("[fork {id} failed] {e}"))?;
    Ok((new_id, dst))
}
