//! ui.json + shell pin + custom wallpaper surface — the GUI settings page
//! persists appearance state to `<project>/.sunmao/ui.json`, the shell
//! backend pick to `.sunmao/shell.txt`, and an uploaded wallpaper to
//! `.sunmao/wallpapers/`, through GET/PUT `/ui`, `/shell`, `/wallpaper`.
//! Same write→broadcast shape as `models.rs`: the file is the source of
//! truth, `ui_changed`/`shell_changed`/`wallpaper_changed` nudge every tab
//! to re-pull.

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
    s.newest_live_id().and_then(|id| s.host(&id))
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
    let local = sunmao_core::tool::ShellBackend::resolve_local_with(&cwd);
    let name = |b| match b {
        sunmao_core::tool::ShellBackend::Pwsh => "pwsh",
        sunmao_core::tool::ShellBackend::Posix => "posix",
    };
    HostResponse::json(serde_json::json!({
        "backend": name(res.backend),
        // the `!` local shell resolves the same pins but auto defaults to
        // the operator's dialect — pwsh on Windows — while the agent's
        // `Bash` auto is Posix
        "local_backend": name(local.backend),
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

// ── custom wallpaper (`.sunmao/wallpapers/`) ──

/// Decoded-image cap — a 2560px jpeg lands at 2–4 MiB; 16 MiB leaves room
/// for png/webp uploads without turning the settings page into a disk eater.
pub(super) const WALL_MAX: usize = 16 * 1024 * 1024;

/// Magic-byte sniff → stored extension. Declared MIME/dataURL typing is a
/// hint only; the whitelist is on the bytes themselves (jpeg/png/webp).
fn sniff_image(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("png")
    } else if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// The wallpaper store — one `custom.{ext}` per project; an upload rotates
/// the old file out rather than accumulating (the same semantics
/// `attachments.rs` documents for its own dir).
fn wallpaper_dir(s: &Arc<Shared>, sess: Option<String>) -> std::path::PathBuf {
    project_dir(s, sess).join(".sunmao").join("wallpapers")
}

/// `PUT /wallpaper?sess=` — body is either a `data:<mime>;base64,…` URL
/// (what `canvas.toDataURL` hands the frontend) or the raw image bytes
/// themselves. Decoded bytes must sniff jpeg/png/webp; the file lands as
/// `custom.{ext}` and any previous custom file is removed.
pub(super) fn wallpaper_put(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    use base64::Engine;
    let bytes = match std::str::from_utf8(body) {
        Ok(t) if t.starts_with("data:") => {
            let (meta, data) = match t.split_once(',') {
                Some(pair) => pair,
                None => return HostResponse::err(400, "bad data URL".into()),
            };
            if !meta.contains(";base64") {
                return HostResponse::err(400, "data URL must be base64".into());
            }
            match base64::engine::general_purpose::STANDARD.decode(data.trim()) {
                Ok(b) => b,
                Err(e) => return HostResponse::err(400, format!("bad data URL: {e}")),
            }
        }
        _ => body.to_vec(),
    };
    if bytes.len() > WALL_MAX {
        return HostResponse::err(413, "wallpaper too large (max 16 MiB)".into());
    }
    let Some(ext) = sniff_image(&bytes) else {
        return HostResponse::err(400, "not a jpeg/png/webp image".into());
    };
    let dir = wallpaper_dir(s, sess.clone());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    // rotate: drop the previous custom file whatever its extension was
    for old in ["jpg", "jpeg", "png", "webp"] {
        if old != ext {
            let _ = std::fs::remove_file(dir.join(format!("custom.{old}")));
        }
    }
    if let Err(e) = std::fs::write(dir.join(format!("custom.{ext}")), &bytes) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    s.emit(serde_json::json!({"type": "wallpaper_changed"}));
    HostResponse::json(serde_json::json!({"ok": true, "mime": format!("image/{ext}")}))
}

/// `GET /wallpaper?sess=` — the stored custom wallpaper with its real
/// Content-Type (extension ↔ sniffed format), 404 when the project has
/// none. `no-cache` so a re-upload shows up on the next pull.
pub(super) fn wallpaper_view(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let dir = wallpaper_dir(s, sess);
    for ext in ["jpg", "jpeg", "png", "webp"] {
        let p = dir.join(format!("custom.{ext}"));
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let mime = sunmao_llm::types::image_mime(&p).unwrap_or("application/octet-stream");
        return HostResponse::bytes(
            200,
            vec![
                ("content-type".into(), mime.to_string()),
                ("cache-control".into(), "no-cache".into()),
            ],
            bytes,
        );
    }
    HostResponse::err(404, "no custom wallpaper".into())
}

/// `DELETE /wallpaper?sess=` — drop `custom.{ext}` whatever its format;
/// idempotent (nothing stored is not an error), broadcast so the deleting
/// tab's siblings drop their stale custom thumb too.
pub(super) fn wallpaper_delete(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let dir = wallpaper_dir(s, sess);
    let mut hit = false;
    for ext in ["jpg", "jpeg", "png", "webp"] {
        hit |= std::fs::remove_file(dir.join(format!("custom.{ext}"))).is_ok();
    }
    s.emit(serde_json::json!({"type": "wallpaper_changed"}));
    HostResponse::json(serde_json::json!({"ok": true, "removed": hit}))
}
