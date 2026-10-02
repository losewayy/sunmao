//! Project registry + session-dir resolution — split out of `host.rs`
//! under the 600-line shape budget. The registry (`.sunmao/projects.json`)
//! is how sessions adopted from other projects stay findable; the dir
//! helpers here are what `log_path`'s containment check runs against.

use std::sync::Mutex;

use sunmao_core::context::MutexRecover;

use super::Shared;
use super::display_path;

/// The known-project registry: `.sunmao/projects.json` under the launch
/// dir — a JSON array of project paths. `register_project` appends on
/// adopt so `GET /projects` and session listing see every project a
/// session has ever run in during this host's life.
pub(crate) fn projects(s: &Shared) -> Vec<std::path::PathBuf> {
    let path = s.cwd.join(".sunmao/projects.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
        .map(|v| v.into_iter().map(std::path::PathBuf::from).collect())
        .unwrap_or_default()
}

pub(crate) fn register_project(launch_cwd: &std::path::Path, project: &std::path::Path) {
    if project == launch_cwd {
        return; // launch dir is implicit — always listed
    }
    // read-modify-write races under concurrent adopts — serialize the
    // whole cycle on a process-wide lock.
    static REG: Mutex<()> = Mutex::new(());
    let _g = REG.lock_or_recover();
    let path = launch_cwd.join(".sunmao/projects.json");
    let mut list: Vec<String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let disp = display_path(project);
    if list.iter().any(|p| p == &disp) {
        return;
    }
    list.push(disp);
    if let Ok(t) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(&path, t);
    }
}

/// Every `<project>/.sunmao/sessions` dir the host knows: launch cwd first,
/// then the project registry's — sessions adopted from another project
/// stay findable after their dir registers.
pub(crate) fn session_dirs(s: &Shared) -> Vec<std::path::PathBuf> {
    let mut out = vec![s.cwd.join(".sunmao/sessions")];
    for p in projects(s) {
        let d = p.join(".sunmao/sessions");
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// The project a session log belongs to — `<project>/.sunmao/sessions/`
/// `<id>.jsonl` implies `<project>`; anything else (bare path args, odd
/// layouts) falls back to the launch dir.
pub(crate) fn session_project(s: &Shared, log: &std::path::Path) -> std::path::PathBuf {
    let mut a = log.ancestors();
    let is_sessions_layout = matches!(
        (a.nth(1), a.next()),
        (Some(parent), Some(grand))
            if parent.file_name().map(|f| f == "sessions").unwrap_or(false)
                && grand.file_name().map(|f| f == ".sunmao").unwrap_or(false)
    );
    if is_sessions_layout && let Some(project) = a.next() {
        return project.to_path_buf();
    }
    s.cwd.clone()
}
