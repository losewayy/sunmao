//! `/session/{id}/…` 的日志操作 — rename/delete/events/md 都只碰磁盘
//! 上的 jsonl（rename 走 SessionMeta 追加，live 宿主拒绝删除）。拆出
//! request.rs 守 600 行预算。

use std::sync::Arc;

use super::super::host::{Shared, log_path};
use super::{HostResponse, raw_string_field};

/// `POST /session/new` — `cwd` may arrive as a Windows path with unescaped
/// backslashes (F:\x\y) — strict JSON rejects `\p`, so a failed parse
/// falls back to a raw-string extraction.
pub(super) async fn session_new(s: &Arc<Shared>, body: &[u8]) -> HostResponse {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let cwd = parsed
        .as_ref()
        .and_then(|v| v["cwd"].as_str().map(std::path::PathBuf::from))
        .or_else(|| raw_string_field(body, "cwd").map(std::path::PathBuf::from));
    // `loop` names the turn's driver for this session (cold-plug — the
    // choice freezes at creation): absent = --loop flag, else manifest scan
    let loop_drv = match parsed
        .as_ref()
        .and_then(|v| v["loop"].as_str())
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        Some(name) => match sunmao_core::agent::LoopDriver::parse(name) {
            Ok(d) => Some(d),
            Err(e) => return HostResponse::err(400, format!("{e:#}")),
        },
        None => None,
    };
    match super::super::host::new_session(s, cwd, loop_drv).await {
        Ok(v) => HostResponse::json(v),
        Err(e) => HostResponse::err(500, format!("{e:#}")),
    }
}

/// Rail metadata for one log: `title` = the last `session_meta` rename,
/// else the first prompt the user typed (hook/local-shell evidence skipped,
/// first line, ≤ 80 chars; `null` for a log with neither) and `mtime` in
/// epoch ms — `crate::sessions::log_title` owns the scan.
pub(super) fn session_meta(path: &std::path::Path) -> serde_json::Value {
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    let title = crate::sessions::log_title(path);
    serde_json::json!({ "title": title, "mtime": mtime })
}

/// `POST /session/{id}/rename {"title"}` — append a `session_meta` fact to
/// the log. Works on dormant logs (append-only is safe next to the open
/// live handle) and live ones alike; the rail re-reads on the
/// `sessions_changed` frame.
pub(super) async fn session_rename(s: &Arc<Shared>, id: &str, body: &[u8]) -> HostResponse {
    let title = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["title"].as_str().map(str::to_string))
        .or_else(|| raw_string_field(body, "title"))
        .unwrap_or_default();
    let title = title.trim().to_string();
    if title.is_empty() {
        return HostResponse::err(400, "title must be non-empty".into());
    }
    let Some(p) = log_path(s, id) else {
        return HostResponse::err(404, "no such session".into());
    };
    // open_path also seals a crash-stranded partial tail before appending
    let log = sunmao_core::SessionLog::open_path(&p).await;
    match log {
        Ok(mut log) => match log
            .append(&sunmao_core::SessionEvent::SessionMeta { title })
            .await
        {
            Ok(()) => {
                s.emit(serde_json::json!({"type":"sessions_changed"}));
                HostResponse::json(serde_json::json!({"ok": true}))
            }
            Err(e) => HostResponse::err(500, format!("{e:#}")),
        },
        Err(e) => HostResponse::err(500, format!("{e:#}")),
    }
}

/// `DELETE /session/{id}` — remove the log file. A live session refuses
/// outright (idle or busy): there's no graceful host teardown today, and a
/// driver still appending to an unlinked log would keep mutating a session
/// the UI already forgot — restartable confusion, not data safety.
pub(super) async fn session_delete(s: &Arc<Shared>, id: &str) -> HostResponse {
    if s.host(id).is_some() {
        return HostResponse::err(409, "session is live — close it before deleting".into());
    }
    let Some(p) = log_path(s, id) else {
        return HostResponse::err(404, "no such session".into());
    };
    match std::fs::remove_file(&p) {
        Ok(()) => {
            s.emit(serde_json::json!({"type":"sessions_changed"}));
            HostResponse::json(serde_json::json!({"ok": true}))
        }
        Err(e) => HostResponse::err(500, format!("delete {}: {e}", p.display())),
    }
}

/// `GET /session/{id}/events` — the raw durable event list (`{events:[]}`),
/// for dormant logs that never got a host. Frontend exports (markdown
/// download) read this instead of re-deriving the fold.
pub(super) fn session_events(s: &Arc<Shared>, id: &str) -> HostResponse {
    let Some(p) = log_path(s, id) else {
        return HostResponse::err(404, "no such session".into());
    };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return HostResponse::err(500, "unreadable log".into());
    };
    let events: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    HostResponse::json(serde_json::json!({"events": events}))
}

/// `GET /session/{id}/md` — the session transcript as markdown. The wire
/// shape (raw `Content-Type: text/markdown` body) rides the download it
/// was built for; the renderer is `commands::export::markdown` — the same
/// fold `/export-md` (REPL), `/export-zip` (debug bundle), and TUI use —
/// so the four exports are the same document.
pub(super) fn session_markdown(s: &Arc<Shared>, id: &str) -> HostResponse {
    let Some(p) = log_path(s, id) else {
        return HostResponse::err(404, "no such session".into());
    };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return HostResponse::err(500, "unreadable log".into());
    };
    let events: Vec<sunmao_core::session::SessionEvent> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let md = crate::commands::export::markdown(id, &s.cwd, &events);
    HostResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/markdown; charset=utf-8".into())],
        body: md.into_bytes(),
    }
}
