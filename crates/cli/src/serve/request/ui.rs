//! ui.json + shell pin surface — the GUI settings page persists appearance
//! state to `<project>/.sunmao/ui.json` and the shell backend pick to
//! `.sunmao/shell.txt`, both through GET/PUT `/ui` and GET/PUT `/shell`.
//! Same write→broadcast shape as `models.rs`: the file is the source of
//! truth, `ui_changed`/`shell_changed` nudge every tab to re-pull.

use std::sync::Arc;

use super::super::host::{Host, Shared};
use super::HostResponse;

/// The project dir a project-scoped write belongs to — `?sess=` picks a
/// live session's root; absent it, the newest live host; no live host → the
/// launch dir (settings still work on the bare files).
fn host(s: &Arc<Shared>, sess: Option<String>) -> Option<Arc<Host>> {
    if let Some(id) = sess
        && let Some(h) = s.host(&id)
    {
        return Some(h);
    }
    s.live_ids().first().and_then(|id| s.host(id))
}

fn project_dir(s: &Arc<Shared>, sess: Option<String>) -> std::path::PathBuf {
    host(s, sess)
        .map(|h| h.agent.session_cwd())
        .unwrap_or_else(|| s.cwd.clone())
}

// ── appearance state (`.sunmao/ui.json`) ──

/// `GET /ui?sess=` — the persisted appearance object verbatim (`{}` when no
/// file exists yet). The frontend owns defaults; the file only ever carries
/// keys the user actually changed, so a missing file is indistinguishable
/// from "all defaults".
pub(super) async fn view(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let path = project_dir(s, sess).join(".sunmao").join("ui.json");
    let ui = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    HostResponse::json(serde_json::json!({"ui": ui}))
}

/// `PUT /ui?sess=` — replace the project's `.sunmao/ui.json` wholesale with
/// the posted object, then broadcast `ui_changed` so sibling tabs re-pull.
pub(super) async fn put(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    let v: serde_json::Value = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(v) if v.is_object() => v,
        Ok(_) => return HostResponse::err(400, "ui body must be a JSON object".into()),
        Err(e) => return HostResponse::err(400, format!("bad json: {e}")),
    };
    let dir = project_dir(s, sess.clone()).join(".sunmao");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    let pretty = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
    if let Err(e) = std::fs::write(dir.join("ui.json"), pretty) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    s.emit(serde_json::json!({"type": "ui_changed"}));
    view(s, sess).await
}

// ── shell backend pin (`.sunmao/shell.txt`) ──

/// `GET /shell?sess=` — the kernel's `ShellResolution` for the viewed
/// project (which layer chose the backend, whether a `pwsh` ask fell back,
/// and whether the `pwsh` binary is on PATH at all).
pub(super) fn shell_view(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let cwd = project_dir(s, sess);
    let res = sunmao_core::tool::ShellBackend::resolve_with(&cwd);
    HostResponse::json(serde_json::json!({
        "backend": match res.backend {
            sunmao_core::tool::ShellBackend::Pwsh => "pwsh",
            sunmao_core::tool::ShellBackend::Posix => "posix",
        },
        "source": res.source.label(),
        "pwsh_on_path": sunmao_core::tool::ShellBackend::pwsh_on_path(),
        "pwsh_requested_but_missing": res.pwsh_requested_but_missing,
        "unrecognized": res.unrecognized_value,
    }))
}

/// `PUT /shell?sess= {"backend":"auto|pwsh|posix"}` — write the project pin
/// `.sunmao/shell.txt`. Cold-plug semantics: the file is read at Context
/// build, so this only affects sessions started afterwards; live hosts
/// keep their backend. `auto` is written literally — it forces detection
/// past a user-level pin rather than deleting the file (which would let
/// `~/.sunmao/shell.txt` take over, not auto-detect).
pub(super) fn shell_put(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    let backend = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["backend"].as_str().map(str::to_string))
        .unwrap_or_default();
    match backend.as_str() {
        "auto" | "pwsh" | "posix" => {}
        _ => {
            return HostResponse::err(400, "backend must be auto|pwsh|posix".into());
        }
    }
    let dir = project_dir(s, sess.clone()).join(".sunmao");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    if let Err(e) = std::fs::write(dir.join("shell.txt"), format!("{backend}\n")) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    s.emit(serde_json::json!({"type": "shell_changed"}));
    shell_view(s, sess)
}
