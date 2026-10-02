//! 传输无关的 REST 面 — `HostHandle::request` 是 serve 路由表的协议内
//! 形式：同一份路由被 `serve::http`（axum fallback）与 Tauri 的
//! `sunmao` scheme 处理器共用。所有端点语义（白名单、CSP、版本链）
//! 住在这里与其 delegate（`artifacts.rs`），传输层只负责字节进出。

use std::sync::Arc;

use super::artifacts;
use super::host::{HostHandle, Shared, display_path, log_path};

/// One answered request — status line, headers, body bytes. Header names
/// are already lowercase strings; the transports (axum, tauri scheme)
/// map them onto their own `http::Response`.
pub struct HostResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HostResponse {
    pub(crate) fn json(v: serde_json::Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: v.to_string().into_bytes(),
        }
    }

    pub(crate) fn html(s: &'static str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
            body: s.as_bytes().to_vec(),
        }
    }

    pub(crate) fn css(s: &'static str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/css; charset=utf-8".into())],
            body: s.as_bytes().to_vec(),
        }
    }

    pub(crate) fn js(s: &'static str) -> Self {
        Self {
            status: 200,
            headers: vec![(
                "content-type".into(),
                "text/javascript; charset=utf-8".into(),
            )],
            body: s.as_bytes().to_vec(),
        }
    }

    /// Plain-text error — mirrors axum's `(StatusCode, String)` shape.
    pub(crate) fn err(status: u16, text: String) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
            body: text.into_bytes(),
        }
    }

    pub(crate) fn bytes(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }
}

/// URL percent-decoding for path segments and query values — the serve
/// surface's inputs are artifact/session names (`[a-z0-9_-]`), so this
/// only needs correctness, not form-parsing completeness.
fn pct_decode(s: &str, plus_as_space: bool) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| -> Option<u8> {
                    match c {
                        b'0'..=b'9' => Some(c - b'0'),
                        b'a'..=b'f' => Some(c - b'a' + 10),
                        b'A'..=b'F' => Some(c - b'A' + 10),
                        _ => None,
                    }
                };
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h * 16 + l);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// First value of `key` in a `a=b&c=d` query string (`None` for missing).
fn query_arg(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if pct_decode(k, true) == key {
            return Some(pct_decode(v, true));
        }
    }
    None
}

/// Last-resort `"key": "value"` extraction for bodies that are almost-JSON
/// (unescaped Windows backslashes break strict parsing). Returns the raw
/// substring between quotes — escapes are *not* processed, which is the
/// point: `F:\x\y` keeps its backslashes.
fn raw_string_field(body: &[u8], key: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let needle = format!("\"{key}\"");
    let mut rest = text.split(&needle);
    rest.next()?;
    let tail = rest.next()?;
    let tail = tail.trim_start().strip_prefix(':')?.trim_start();
    let tail = tail.strip_prefix('"')?;
    tail.split('"').next().map(str::to_string)
}

impl HostHandle {
    /// The whole REST surface as one route table — `path` may carry its
    /// `?query`. `method`/`path` mirror axum's router exactly; unknown
    /// pairs answer 404.
    pub async fn request(&self, method: &str, path: &str, body: &[u8]) -> HostResponse {
        let (path, query) = match path.split_once('?') {
            Some((p, q)) => (p, q),
            None => (path, ""),
        };
        let segs: Vec<String> = path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| pct_decode(s, false))
            .collect();
        let segs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        let s = &self.s;
        match (method, segs.as_slice()) {
            ("GET" | "HEAD", []) => HostResponse::html(super::INDEX),
            ("GET" | "HEAD", [name]) if name.ends_with(".css") => match css_asset(name) {
                Some(s) => HostResponse::css(s),
                None => HostResponse::err(404, "not found".into()),
            },
            ("GET" | "HEAD", [name]) if name.ends_with(".js") => match js_asset(name) {
                Some(s) => HostResponse::js(s),
                None => HostResponse::err(404, "not found".into()),
            },
            ("GET", ["sessions"]) => sessions_list(s, query_arg(query, "q")).await,
            ("GET", ["session"]) => session_info(s, query_arg(query, "id")).await,
            ("GET", ["projects"]) => projects_list(s).await,
            ("POST", ["session", "new"]) => {
                // `cwd` may arrive as a Windows path with unescaped
                // backslashes (F:\x\y) — strict JSON rejects `\p`, so a
                // failed parse falls back to a raw-string extraction.
                let cwd = serde_json::from_slice::<serde_json::Value>(body)
                    .ok()
                    .and_then(|v| v["cwd"].as_str().map(std::path::PathBuf::from))
                    .or_else(|| raw_string_field(body, "cwd").map(std::path::PathBuf::from));
                match super::host::new_session(s, cwd).await {
                    Ok(v) => HostResponse::json(v),
                    Err(e) => HostResponse::err(500, format!("{e:#}")),
                }
            }
            ("POST", ["session", id, "resume"]) => {
                match super::host::fork_or_resume(s, id, false).await {
                    Ok(v) => HostResponse::json(v),
                    Err(e) => HostResponse::err(400, format!("{e:#}")),
                }
            }
            ("POST", ["session", id, "fork"]) => {
                match super::host::fork_or_resume(s, id, true).await {
                    Ok(v) => HostResponse::json(v),
                    Err(e) => HostResponse::err(400, format!("{e:#}")),
                }
            }
            ("POST", ["session", id, "rename"]) => session_rename(s, id, body).await,
            ("DELETE", ["session", id]) => session_delete(s, id).await,
            ("GET", ["session", id, "events"]) => session_events(s, id),
            // `GET /session/{id}/md` — the SAME renderer `export_md`/`export_zip`
            // ride, served over HTTP so the download matches REPL/TUI output
            // byte-for-byte. Frontend never re-implements the fold.
            ("GET", ["session", id, "md"]) => session_markdown(s, id),
            ("GET", ["session", id, "turns"]) => {
                // the /rewind picker's data — user-turn boundaries on the
                // log, numbered and previewed exactly like the TUI list
                match log_path(s, id) {
                    Some(p) => {
                        let turns: Vec<serde_json::Value> =
                            sunmao_core::checkpoints::turn_boundaries(&p)
                                .iter()
                                .map(|b| serde_json::json!({"n": b.n, "preview": b.preview}))
                                .collect();
                        HostResponse::json(serde_json::json!({"turns": turns}))
                    }
                    None => HostResponse::err(404, "no such session".into()),
                }
            }
            ("POST", ["session", id, "rewind"]) => {
                let v: serde_json::Value = match serde_json::from_slice(body) {
                    Ok(v) => v,
                    Err(e) => return HostResponse::err(400, format!("bad json: {e}")),
                };
                let n = v["turn"].as_u64().unwrap_or(0);
                let mode = match v["mode"].as_str().unwrap_or("both") {
                    "session" => super::host::RewindMode::Session,
                    "code" => super::host::RewindMode::Code,
                    "both" => super::host::RewindMode::Both,
                    m => return HostResponse::err(400, format!("unknown mode: {m}")),
                };
                if n == 0 {
                    return HostResponse::err(400, "missing turn".into());
                }
                match super::host::rewind_session(s, id, n, mode).await {
                    Ok(v) => HostResponse::json(v),
                    Err(e) => HostResponse::err(400, format!("{e:#}")),
                }
            }
            ("GET", ["artifacts", name]) => {
                artifacts::artifact_get(s, name, query_arg(query, "rev"), query_arg(query, "sess"))
                    .await
            }
            // image attachments — uploads land in the viewed session's
            // `.sunmao/attachments/` (content-hashed name), reads serve the
            // same dir so transcripts can render <img> from a stored block
            ("POST", ["attachments"]) => {
                attachments::put(s, query_arg(query, "sess"), query_arg(query, "ext"), body).await
            }
            ("GET", ["attachments", name]) => attachments::get(s, name, query_arg(query, "sess")),
            ("GET", ["artifacts", name, "revs"]) => {
                artifacts::artifact_revs(s, name, query_arg(query, "sess")).await
            }
            ("GET", ["artifacts", name, "ui"]) => {
                artifacts::artifact_ui(s, name, query_arg(query, "sess")).await
            }
            ("GET", ["artifacts", name, "notes"]) => {
                artifacts::artifact_notes(s, name, query_arg(query, "sess")).await
            }
            ("POST", ["artifacts", name, "annotate"]) => {
                artifacts::artifact_annotate(s, name, body, query_arg(query, "sess")).await
            }
            ("GET", ["dataflow"]) => dataflow_current(s, query_arg(query, "sess")).await,
            ("GET", ["dataflow", id]) => dataflow_by_id(s, id).await,
            ("GET", ["tasks"]) => ops::tasks_list(s, query_arg(query, "sess")),
            ("GET", ["hooks"]) => ops::hooks_list(s, query_arg(query, "sess")),
            ("GET", ["jobs"]) => ops::jobs_list(s, query_arg(query, "sess")),
            ("GET", ["jobs", id, "output"]) => {
                ops::job_output(s, id, query_arg(query, "sess"), query)
            }
            ("GET", ["paths"]) => paths_list(s, query_arg(query, "sess")).await,
            ("GET", ["models"]) => models::view(s, query_arg(query, "sess")).await,
            ("POST", ["models", "fetch"]) => models::fetch(s, query_arg(query, "sess"), body).await,
            ("PUT", ["models"]) => models::put(s, query_arg(query, "sess"), body).await,
            ("GET", ["ui"]) => ui::view(s, query_arg(query, "sess")).await,
            ("PUT", ["ui"]) => ui::put(s, query_arg(query, "sess"), body).await,
            ("GET", ["shell"]) => ui::shell_view(s, query_arg(query, "sess")),
            ("PUT", ["shell"]) => ui::shell_put(s, query_arg(query, "sess"), body),
            _ => HostResponse::err(404, "not found".into()),
        }
    }
}

/// `GET /{name}.css` — the page's split stylesheet bundle, served off the
/// same route table the Tauri scheme rides. New asset stylesheets need
/// both the include_str! const in serve.rs and an arm here.
fn css_asset(name: &str) -> Option<&'static str> {
    Some(match name {
        "tokens.css" => super::TOKENS_CSS,
        "app.css" => super::APP_CSS,
        "transcript.css" => super::TRANSCRIPT_CSS,
        "composer.css" => super::COMPOSER_CSS,
        "dock.css" => super::DOCK_CSS,
        "settings.css" => super::SETTINGS_CSS,
        "overlay.css" => super::OVERLAY_CSS,
        _ => return None,
    })
}

/// `GET /{name}.js` — the page's split script bundle, served off the same
/// route table the Tauri scheme rides (GUI.md §8). New asset scripts need
/// both the include_str! const in serve.rs and an arm here.
fn js_asset(name: &str) -> Option<&'static str> {
    Some(match name {
        "state.js" => super::STATE_JS,
        "wallpaper.js" => super::WALLPAPER_JS,
        "settings.js" => super::SETTINGS_JS,
        "diff.js" => super::DIFF_JS,
        "md.js" => super::MD_JS,
        "transcript.js" => super::TRANSCRIPT_JS,
        "islands.js" => super::ISLANDS_JS,
        "connection.js" => super::CONNECTION_JS,
        "composer.js" => super::COMPOSER_JS,
        "palette.js" => super::PALETTE_JS,
        "find.js" => super::FIND_JS,
        "menus.js" => super::MENUS_JS,
        "roster.js" => super::ROSTER_JS,
        "jobs.js" => super::JOBS_JS,
        "boot.js" => super::BOOT_JS,
        _ => return None,
    })
}

/// `GET /sessions[?q=…]` — the rail = dormant logs on disk ∪ live hosts (a
/// session the host is running exists even when its log hasn't flushed a
/// fresh name yet). Scans every registered project's sessions dir; entries
/// are `{id, project}` — `project` is the display path the row groups by.
/// `?q` switches to cross-session content search instead: `{id, title,
/// hits[]}` rows from `crate::sessions::search_sessions`, capped at 20.
async fn sessions_list(s: &Arc<Shared>, q: Option<String>) -> HostResponse {
    if let Some(q) = q.filter(|q| !q.trim().is_empty()) {
        let hits = crate::sessions::search_sessions(&super::host::session_dirs(s), &q);
        return HostResponse::json(serde_json::json!({
            "sessions": hits.iter().map(|h| serde_json::json!({
                "id": h.id, "title": h.title, "hits": h.hits,
            })).collect::<Vec<_>>(),
        }));
    }
    use std::collections::BTreeSet;
    let mut rows: Vec<(std::time::SystemTime, String, String)> = Vec::new();
    let mut seen = BTreeSet::new();
    for dir in super::host::session_dirs(s) {
        let project = dir.ancestors().nth(2).map(display_path).unwrap_or_default();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "jsonl").unwrap_or(false)
                    && let (Some(stem), Ok(md)) = (
                        p.file_stem().map(|s| s.to_string_lossy().to_string()),
                        e.metadata(),
                    )
                    && seen.insert(stem.clone())
                {
                    rows.push((
                        md.modified().unwrap_or(std::time::UNIX_EPOCH),
                        stem,
                        project.clone(),
                    ));
                }
            }
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    let mut sessions: Vec<serde_json::Value> = rows
        .into_iter()
        .take(50)
        .map(|(_, id, project)| serde_json::json!({"id": id, "project": project}))
        .collect();
    for id in s.live_ids() {
        if !sessions.iter().any(|r| r["id"].as_str() == Some(&id))
            && let Some(h) = s.host(&id)
        {
            let project = display_path(&h.agent.session_cwd());
            sessions.insert(0, serde_json::json!({"id": id, "project": project}));
        }
    }
    let meta: serde_json::Map<String, serde_json::Value> = sessions
        .iter()
        .filter_map(|r| {
            let id = r["id"].as_str()?;
            log_path(s, id).map(|p| (id.to_string(), session_meta(&p)))
        })
        .collect();
    HostResponse::json(serde_json::json!({
        "sessions": sessions,
        "live": s.live_ids(),
        "meta": meta,
    }))
}

/// `GET /projects` — the launch dir plus every project the registry has
/// seen a session run in.
async fn projects_list(s: &Arc<Shared>) -> HostResponse {
    let mut out = vec![display_path(&s.cwd)];
    for p in super::host::projects(s) {
        let d = display_path(&p);
        if !out.contains(&d) {
            out.push(d);
        }
    }
    HostResponse::json(serde_json::json!({ "projects": out }))
}

/// Rail metadata for one log: `title` = the last `session_meta` rename,
/// else the first prompt the user typed (hook/local-shell evidence skipped,
/// first line, ≤ 80 chars; `null` for a log with neither) and `mtime` in
/// epoch ms — `crate::sessions::log_title` owns the scan.
fn session_meta(path: &std::path::Path) -> serde_json::Value {
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
async fn session_rename(s: &Arc<Shared>, id: &str, body: &[u8]) -> HostResponse {
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
async fn session_delete(s: &Arc<Shared>, id: &str) -> HostResponse {
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
fn session_events(s: &Arc<Shared>, id: &str) -> HostResponse {
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
fn session_markdown(s: &Arc<Shared>, id: &str) -> HostResponse {
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

/// `GET /session[?id=…]` — the viewed host's id + live set + the session's
/// own project dir (a session adopted from another project reports its
/// root, not the launch dir) + its session-grant ledger.
async fn session_info(s: &Arc<Shared>, id: Option<String>) -> HostResponse {
    let host = s.host(&id.unwrap_or_default());
    HostResponse::json(serde_json::json!({
        "id": host.as_ref().map(|h| h.id.clone()),
        "live": s.live_ids(),
        "cwd": host.as_ref()
            .map(|h| display_path(&h.agent.session_cwd()))
            .unwrap_or_else(|| display_path(&s.cwd)),
        "base_cwd": display_path(&s.cwd),
        "grants": host.as_ref().map(|h| h.agent.session_grants()).unwrap_or_default(),
    }))
}

/// `GET /paths?sess=…` — the `@` mention picker's path pool, delegated to
/// `commands::scan_files` (the same walk the TUI's Path menu runs).
async fn paths_list(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let root = sess
        .as_deref()
        .and_then(|id| s.host(id))
        .map(|h| h.agent.session_cwd())
        .unwrap_or_else(|| s.cwd.clone());
    HostResponse::json(serde_json::json!({"paths": crate::commands::scan_files(&root)}))
}

/// `GET /dataflow[?sess=…]` — `sess` picks a live host's log; without it
/// the report reads the newest session log on disk (a dormant-but-just-
/// finished session is still reportable).
async fn dataflow_current(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let p = match sess.as_deref().and_then(|id| s.host(id)) {
        Some(h) => h.agent.session_path().await,
        None => match s.live_ids().first().and_then(|id| s.host(id)) {
            Some(h) => h.agent.session_path().await,
            None => match crate::sessions::recent_sessions(&s.cwd, 1).first() {
                Some(id) => match log_path(s, id) {
                    Some(p) => p,
                    None => return HostResponse::err(404, "no sessions".into()),
                },
                None => return HostResponse::err(404, "no sessions".into()),
            },
        },
    };
    match crate::dataflow::report(&p).await {
        Ok(v) => HostResponse::json(v),
        Err(e) => HostResponse::err(500, format!("{e:#}")),
    }
}

async fn dataflow_by_id(s: &Arc<Shared>, id: &str) -> HostResponse {
    match log_path(s, id) {
        Some(p) => match crate::dataflow::report(&p).await {
            Ok(v) => HostResponse::json(v),
            Err(e) => HostResponse::err(500, format!("{e:#}")),
        },
        None => HostResponse::err(404, "no such session".into()),
    }
}

#[path = "request/attachments.rs"]
mod attachments;
#[path = "request/models.rs"]
mod models;
#[path = "request/ops.rs"]
mod ops;
#[path = "request/ui.rs"]
mod ui;

#[cfg(test)]
#[path = "request/tests.rs"]
mod tests;
