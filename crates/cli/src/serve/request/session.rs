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
/// Rail metadata for one log — the (mtime, len) pair is a complete identity
/// for an append-only file, so the title scan runs once per change, not
/// once per /sessions fetch.
pub(super) fn session_meta(path: &std::path::Path) -> serde_json::Value {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<std::path::PathBuf, (u64, u64, Option<String>)>>> =
        OnceLock::new();
    let meta = std::fs::metadata(path).ok();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let title = {
        let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
        match c.get(path) {
            Some(&(mt, ln, ref t)) if mt == mtime.unwrap_or(0) && ln == len => t.clone(),
            _ => {
                let t = crate::sessions::log_title(path);
                // bound it — a project with thousands of logs shouldn't grow
                // a cache to match; evict the whole thing rather than order
                if c.len() >= 512 {
                    c.clear();
                }
                c.insert(path.to_path_buf(), (mtime.unwrap_or(0), len, t.clone()));
                t
            }
        }
    };
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
/// outright (idle or busy) — except an EMPTY session, which never ran a
/// turn: its driver is parked on the queue notify, so flagging `shutdown`
/// exits it and the last Arc drops the log writer. A content-bearing live
/// session still refuses: the driver appending to an unlinked log would
/// keep mutating a session the UI already forgot.
pub(super) async fn session_delete(s: &Arc<Shared>, id: &str) -> HostResponse {
    let Some(p) = log_path(s, id) else {
        return HostResponse::err(404, "no such session".into());
    };
    let live = s.host(id);
    if live.is_some() && !log_is_empty(&p) {
        return HostResponse::err(409, "session is live — close it before deleting".into());
    }
    if let Some(host) = live {
        use std::sync::atomic::Ordering;
        host.shutdown.store(true, Ordering::Relaxed);
        host.queue_notify.notify_one();
        use sunmao_core::context::MutexRecover;
        s.sessions.lock_or_recover().remove(id);
        // our own Arc keeps the host (and its log writer) alive — drop it
        // before waiting on the driver's exit or the count never reaches 0
        drop(host);
        // the driver's pop check wakes on the notify and exits — the file
        // stays locked until that Arc drops, so retry briefly instead of
        // racing the first remove_file
        for _ in 0..20 {
            match std::fs::remove_file(&p) {
                Ok(()) => {
                    s.emit(serde_json::json!({"type":"sessions_changed"}));
                    return HostResponse::json(serde_json::json!({"ok": true}));
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => {
                    return HostResponse::err(500, format!("delete {}: {e}", p.display()));
                }
            }
        }
        return HostResponse::err(409, "session is live — close it before deleting".into());
    }
    match std::fs::remove_file(&p) {
        Ok(()) => {
            s.emit(serde_json::json!({"type":"sessions_changed"}));
            HostResponse::json(serde_json::json!({"ok": true}))
        }
        Err(e) => HostResponse::err(500, format!("delete {}: {e}", p.display())),
    }
}

/// The log holds only setup events — no user content ever landed. Config
/// writes (fusion role picks, mode changes) don't count as content: a
/// session that got configured but never prompted is still deletable.
/// Neither does the seeded system `message` — every fresh log carries it.
fn log_is_empty(p: &std::path::Path) -> bool {
    const CONTENT: &[&str] = &[
        "prompt",
        "tool_call",
        "tool_result",
        "turn_end",
        "usage",
        "local_shell",
    ];
    let Ok(text) = std::fs::read_to_string(p) else {
        return false;
    };
    !text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|e| {
            let t = e.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if t == "message" {
                return e
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|r| r.as_str())
                    .is_some_and(|r| r != "system");
            }
            CONTENT.contains(&t)
        })
}

/// `POST /session/{id}/open {path}` — click-to-open for files a tool call
/// actually surfaced in THIS session. The acceptable path set is rebuilt
/// from the log on every click: file-tool `path` args, Glob/Grep result
/// lines, artifact records, and the session root itself. Anything else is
/// a webview-invented string and gets 403 — the page can only open what
/// the transcript already showed it, never an arbitrary filesystem path.
/// No caching or snapshotting: a file the agent rewrote opens at its
/// current bytes; a deleted one answers with a plain 404.
pub(super) fn session_open(s: &Arc<Shared>, id: &str, body: &[u8]) -> HostResponse {
    let raw = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["path"].as_str().map(str::to_string))
        .unwrap_or_default();
    if raw.trim().is_empty() {
        return HostResponse::err(400, "missing path".into());
    }
    let Some(log) = log_path(s, id) else {
        return HostResponse::err(404, "no such session".into());
    };
    let Ok(text) = std::fs::read_to_string(&log) else {
        return HostResponse::err(500, "unreadable log".into());
    };
    match super::fopen::validate_open_click(&text, &s.cwd, &raw, log.parent()) {
        Err(msg) => HostResponse::err(403, msg),
        Ok(None) => HostResponse::err(404, "file no longer exists".into()),
        Ok(Some(want)) => match open::that(&want) {
            Ok(()) => HostResponse::json(serde_json::json!({"ok": true})),
            Err(e) => HostResponse::err(500, format!("open failed: {e}")),
        },
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

/// `GET /session/{id}/zip` — the `/export-zip` debug bundle served for
/// the GUI's download path: the slash command leaves the file in
/// `.sunmao/exports/` where a browser can't reach it, so this runs the
/// same builder and streams the bytes back as an attachment. The exports
/// dir keeps its copy either way — same side effect the slash path has.
pub(super) fn session_zip(s: &Arc<Shared>, id: &str) -> HostResponse {
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
    // exports land in the SESSION's project, not the serve root — the
    // bundle's manifest also names that dir
    let cwd = events
        .iter()
        .find_map(|e| match e {
            sunmao_core::session::SessionEvent::Started { cwd, .. } => {
                Some(std::path::PathBuf::from(cwd))
            }
            _ => None,
        })
        .unwrap_or_else(|| s.cwd.clone());
    match crate::commands::export_zip(&cwd, id, &events, &p) {
        Ok(path) => match std::fs::read(&path) {
            Ok(bytes) => HostResponse {
                status: 200,
                headers: vec![
                    ("content-type".into(), "application/zip".into()),
                    (
                        "content-disposition".into(),
                        format!("attachment; filename=\"sunmao-{id}.zip\""),
                    ),
                ],
                body: bytes,
            },
            Err(e) => HostResponse::err(500, format!("read bundle: {e}")),
        },
        Err(e) => HostResponse::err(500, format!("export failed: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::log_is_empty;

    fn log_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("sunmao-sessdel-{tag}-{}", std::process::id()))
    }

    #[test]
    fn empty_log_tolerates_setup_events() {
        let p = log_path("empty");
        std::fs::write(
            &p,
            concat!(
                r#"{"type":"started","model":"m","cwd":".","driver":"full"}"#,
                "\n",
                r#"{"type":"message","message":{"role":"system","content":[]}}"#,
                "\n",
                r#"{"type":"turn_mode_change","mode":"fusion"}"#,
                "\n",
                r#"{"type":"fusion_models_change","lead":"a","sidekick":"b"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert!(log_is_empty(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn user_content_makes_the_log_live() {
        let p = log_path("live");
        for line in [
            r#"{"type":"message","message":{"role":"user","content":[]}}"#,
            r#"{"type":"message","message":{"role":"assistant","content":[]}}"#,
            r#"{"type":"tool_call","call":{}}"#,
            r#"{"type":"turn_end"}"#,
            r#"{"type":"usage"}"#,
        ] {
            std::fs::write(
                &p,
                format!("{}\n{line}\n", r#"{"type":"started","driver":"full"}"#),
            )
            .unwrap();
            assert!(!log_is_empty(&p), "{line} should count as content");
        }
        let _ = std::fs::remove_file(&p);
    }
}
