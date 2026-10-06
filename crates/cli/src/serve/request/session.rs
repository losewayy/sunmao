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
    match validate_open_click(&text, &s.cwd, &raw) {
        Err(msg) => HostResponse::err(403, msg),
        Ok(None) => HostResponse::err(404, "file no longer exists".into()),
        Ok(Some(want)) => match open::that(&want) {
            Ok(()) => HostResponse::json(serde_json::json!({"ok": true})),
            Err(e) => HostResponse::err(500, format!("open failed: {e}")),
        },
    }
}

/// Resolve a click's raw path against the session's surfaced set.
/// `Err` = forbidden (not in the log / not a path); `Ok(None)` = surfaced
/// but gone from disk; `Ok(Some)` = safe to hand to the OS handler.
fn validate_open_click(
    text: &str,
    fallback_cwd: &std::path::Path,
    raw: &str,
) -> Result<Option<std::path::PathBuf>, String> {
    let (cwd, exposed) = exposed_paths(text, fallback_cwd);
    let Some(want) = normalize_click(&cwd, raw) else {
        return Err("not a filesystem path".into());
    };
    if !exposed.contains(&path_key(&want)) {
        return Err("path wasn't surfaced by this session".into());
    }
    Ok(if want.exists() { Some(want) } else { None })
}

/// The session's surfaced set: normalized keys for every path a tool call
/// or its output put in front of the user, plus the session root.
fn exposed_paths(
    text: &str,
    fallback_cwd: &std::path::Path,
) -> (std::path::PathBuf, std::collections::HashSet<String>) {
    let mut cwd = fallback_cwd.to_path_buf();
    // the Started event's cwd is the normalization base — a resumed
    // foreign project's session resolves relative paths against ITS root
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["type"].as_str() == Some("started") {
            if let Some(c) = v["cwd"].as_str() {
                cwd = std::path::PathBuf::from(c);
            }
            break;
        }
    }
    let mut set = std::collections::HashSet::new();
    let mut push = |raw: &str| {
        if let Some(p) = normalize_click(&cwd, raw) {
            set.insert(path_key(&p));
        }
    };
    push(&cwd.to_string_lossy());
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v["type"].as_str().unwrap_or("") {
            "tool_call" => {
                let name = v["call"]["function"]["name"].as_str().unwrap_or("");
                if !matches!(name, "Read" | "Write" | "Edit" | "Glob" | "Grep") {
                    continue;
                }
                let args = v["call"]["function"]["arguments"].as_str().unwrap_or("{}");
                let Ok(a) = serde_json::from_str::<serde_json::Value>(args) else {
                    continue;
                };
                if let Some(p) = a["path"].as_str() {
                    push(p);
                }
            }
            "tool_result" => {
                let name = v["name"].as_str().unwrap_or("");
                if name != "Glob" && name != "Grep" {
                    continue;
                }
                for line in v["output"].as_str().unwrap_or("").lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('[') {
                        continue;
                    }
                    if name == "Grep" {
                        // rg prints `path:line:match` — scan colon
                        // positions for the first `:digits:` split so a
                        // drive-letter colon inside the path can't eat it
                        for (i, _) in line.match_indices(':') {
                            let rest = &line[i + 1..];
                            let Some(end) = rest.find(':') else { break };
                            if end > 0 && rest[..end].chars().all(|c| c.is_ascii_digit()) {
                                push(&line[..i]);
                                break;
                            }
                        }
                    } else {
                        push(line);
                    }
                }
            }
            "artifact" => {
                if let Some(p) = v["path"].as_str() {
                    push(p);
                }
            }
            _ => {}
        }
    }
    (cwd, set)
}

/// Lexical normalize a clicked path: `file://` stripped, other schemes
/// refused, relative joins the session cwd, `.`/`..` folded without
/// touching the filesystem (the file may already be gone).
fn normalize_click(cwd: &std::path::Path, raw: &str) -> Option<std::path::PathBuf> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("file://").unwrap_or(raw);
    if raw.is_empty() || raw.contains("://") {
        return None;
    }
    let p = std::path::PathBuf::from(raw);
    let joined = if p.is_absolute() { p } else { cwd.join(&p) };
    let mut out = std::path::PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// Set-key for a path: slash-normalized, and case-folded on Windows where
/// the filesystem can't tell `Src/Foo.rs` from `src/foo.rs`.
fn path_key(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    return s.to_lowercase();
    #[cfg(not(windows))]
    s
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
    use super::{log_is_empty, validate_open_click};

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

    fn open_fixture() -> String {
        concat!(
            r#"{"type":"started","model":"m","cwd":"/proj","driver":"full"}"#,
            "\n",
            r#"{"type":"tool_call","call":{"function":{"name":"Read","arguments":"{\"path\":\"src/foo.rs\"}"}}}"#,
            "\n",
            r#"{"type":"tool_call","call":{"function":{"name":"Bash","arguments":"{\"command\":\"cat /etc/passwd\"}"}}}"#,
            "\n",
            r#"{"type":"tool_result","name":"Glob","ok":true,"output":"src/a.rs\nsrc/dir/b.rs\n[truncated at 200]"}"#,
            "\n",
            r#"{"type":"tool_result","name":"Grep","ok":true,"output":"src/c.rs:12:match text"}"#,
            "\n",
            r#"{"type":"artifact","name":"x","path":"/proj/.sunmao/artifacts/x.html","bytes":1}"#,
            "\n",
        )
        .to_string()
    }

    #[test]
    fn open_click_accepts_surfaced_paths_only() {
        let text = open_fixture();
        let cwd = std::path::Path::new("/fallback");
        // tool-arg, glob line, grep path, artifact, and the session root
        for ok in [
            "src/foo.rs",
            "src/a.rs",
            "src/dir/b.rs",
            "src/c.rs",
            "/proj/.sunmao/artifacts/x.html",
            "/proj",
            "./src/foo.rs",
            "src/../src/foo.rs",
        ] {
            assert!(validate_open_click(&text, cwd, ok).is_ok(), "{ok}");
        }
        // forged: never surfaced, bash-command string, scheme, traversal
        for bad in [
            "src/secret.rs",
            "/etc/passwd",
            "https://evil/x",
            "../outside.rs",
        ] {
            assert!(validate_open_click(&text, cwd, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn open_click_reports_gone_files_distinctly() {
        let text = open_fixture();
        // surfaced + missing on disk → Ok(None), the 404 branch
        assert!(matches!(
            validate_open_click(&text, std::path::Path::new("/x"), "src/foo.rs"),
            Ok(None)
        ));
    }

    #[test]
    fn open_click_resolves_a_real_surfaced_file() {
        let dir = std::env::temp_dir().join("sm-open-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let real = dir.join("src").join("real.txt");
        std::fs::write(&real, "x").unwrap();
        let cwd_json = serde_json::to_string(&dir.to_string_lossy().to_string()).unwrap();
        let started =
            format!(r#"{{"type":"started","model":"m","cwd":{cwd_json},"driver":"full"}}"#);
        let text = format!(
            "{started}\n{}\n",
            r#"{"type":"tool_call","call":{"function":{"name":"Read","arguments":"{\"path\":\"src/real.txt\"}"}}}"#
        );
        match validate_open_click(&text, std::path::Path::new("/fb"), "src/real.txt") {
            Ok(Some(p)) => assert!(p.ends_with("real.txt")),
            other => panic!("expected Ok(Some), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
