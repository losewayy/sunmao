//! 传输无关的 REST 面 — `HostHandle::request` 是 serve 路由表的协议内
//! 形式：同一份路由被 `serve::http`（axum fallback）与 Tauri 的
//! `sunmao` scheme 处理器共用。所有端点语义（白名单、CSP、版本链）
//! 住在这里与其 delegate（`artifacts.rs`），传输层只负责字节进出。

use std::sync::Arc;

use super::artifacts;
use super::host::{Host, HostHandle, Shared, display_path, log_path};
use crate::tui;

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
            ("GET" | "HEAD", ["tokens.css"]) => HostResponse::css(super::TOKENS_CSS),
            ("GET" | "HEAD", ["app.css"]) => HostResponse::css(super::APP_CSS),
            ("GET", ["sessions"]) => sessions_list(s).await,
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
            ("GET", ["artifacts", name]) => {
                artifacts::artifact_get(s, name, query_arg(query, "rev"), query_arg(query, "sess"))
                    .await
            }
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
            ("GET", ["models"]) => models_view(s, query_arg(query, "sess")).await,
            ("POST", ["models", "fetch"]) => models_fetch(s, query_arg(query, "sess"), body).await,
            ("PUT", ["models"]) => models_put(s, query_arg(query, "sess"), body).await,
            _ => HostResponse::err(404, "not found".into()),
        }
    }
}

/// `GET /sessions` — the rail = dormant logs on disk ∪ live hosts (a
/// session the host is running exists even when its log hasn't flushed a
/// fresh name yet). Scans every registered project's sessions dir; entries
/// are `{id, project}` — `project` is the display path the row groups by.
async fn sessions_list(s: &Arc<Shared>) -> HostResponse {
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

/// Rail metadata for one log: `title` = the first prompt the user typed
/// (folded hook/local-shell evidence skipped, first line, ≤ 80 chars;
/// `null` for a log with no prompt yet) and `mtime` in epoch ms. Reads
/// only up to the first user message — logs are append-only, so the
/// title never changes once it exists.
fn session_meta(path: &std::path::Path) -> serde_json::Value {
    use std::io::BufRead;
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    let mut title: Option<String> = None;
    if let Ok(f) = std::fs::File::open(path) {
        for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            // cheap pre-filter: only message lines can carry a prompt
            if !line.contains(r#""role":"user""#) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if v.pointer("/message/role").and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            let Some(c) = v.pointer("/message/content").and_then(|c| c.as_str()) else {
                continue;
            };
            if c.starts_with("[hook context]") || c.starts_with("<local-shell>") {
                continue;
            }
            let first = c.lines().map(str::trim).find(|l| !l.is_empty());
            if let Some(first) = first {
                title = Some(first.chars().take(80).collect());
                break;
            }
        }
    }
    serde_json::json!({ "title": title, "mtime": mtime })
}

/// `GET /session[?id=…]` — the viewed host's id + live set + the session's
/// own project dir (a session adopted from another project reports its
/// root, not the launch dir).
async fn session_info(s: &Arc<Shared>, id: Option<String>) -> HostResponse {
    let host = s.host(&id.unwrap_or_default());
    HostResponse::json(serde_json::json!({
        "id": host.as_ref().map(|h| h.id.clone()),
        "live": s.live_ids(),
        "cwd": host.as_ref()
            .map(|h| display_path(&h.agent.session_cwd()))
            .unwrap_or_else(|| display_path(&s.cwd)),
        "base_cwd": display_path(&s.cwd),
    }))
}

/// `GET /dataflow[?sess=…]` — `sess` picks a live host's log; without it
/// the report reads the newest session log on disk (a dormant-but-just-
/// finished session is still reportable).
async fn dataflow_current(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let p = match sess.as_deref().and_then(|id| s.host(id)) {
        Some(h) => h.agent.session_path().await,
        None => match s.live_ids().first().and_then(|id| s.host(id)) {
            Some(h) => h.agent.session_path().await,
            None => match tui::menu::recent_sessions(&s.cwd, 1).first() {
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

// ── provider/model surface (GUI settings page + composer picker) ──

/// The host to answer models/config questions for — `?sess=` picks a live
/// session's project; absent it, the newest live host; no live host → the
/// launch dir (settings still work on the bare file).
fn models_host(s: &Arc<Shared>, sess: Option<String>) -> Option<Arc<Host>> {
    if let Some(id) = sess
        && let Some(h) = s.host(&id)
    {
        return Some(h);
    }
    s.live_ids().first().and_then(|id| s.host(id))
}

/// `GET /models?sess=` — providers (keys redacted) + routes + completable
/// selectors, everything the settings page and the composer picker need
/// in one shot. Anchored to the viewed session's project.
async fn models_view(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let host = models_host(s, sess);
    let resolver = host.as_ref().and_then(|h| h.agent.models_resolver());
    let file = match resolver.as_ref() {
        Some(m) => m.file(),
        None => {
            // no live host — the bare file still answers (the session's
            // own `default` row is honestly absent: nothing is running)
            let text =
                std::fs::read_to_string(s.cwd.join(".sunmao/models.json")).unwrap_or_default();
            serde_json::from_str::<sunmao_core::models::ModelsFile>(&text).unwrap_or_default()
        }
    };
    HostResponse::json(serde_json::json!({
        "providers": providers_view(&file),
        "routes": file.routes,
        "selectors": resolver.as_ref().map(|m| m.selectors()).unwrap_or_default(),
        "default_provider": resolver.as_ref().map(|m| m.default_provider()).unwrap_or_else(|| "default".into()),
    }))
}

/// Serialize the provider table for the GUI — keys are redacted to a
/// `api_key_set` boolean; the settings editor writes keys, it never
/// reads them back.
fn providers_view(file: &sunmao_core::models::ModelsFile) -> serde_json::Value {
    file.providers
        .iter()
        .map(|(name, p)| {
            (
                name.clone(),
                serde_json::json!({
                    "base_url": p.base_url,
                    "dialect": p.dialect,
                    "api_key_env": p.api_key_env,
                    "api_key_set": p.api_key_env.is_some() || p.api_key.is_some(),
                    "catalog": p.catalog,
                }),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into()
}

/// `POST /models/fetch?sess= {provider}` — proxy the provider's own
/// `/models` listing. Body may instead carry an inline
/// `{"base_url","api_key",…}` for a provider the user is still typing
/// (not yet saved).
async fn models_fetch(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HostResponse::err(400, format!("bad json: {e}")),
    };
    let def: sunmao_core::models::ProviderDef = if let Some(name) = v["provider"].as_str() {
        let resolver = models_host(s, sess).and_then(|h| h.agent.models_resolver());
        let file = resolver.as_ref().map(|m| m.file()).unwrap_or_else(|| {
            let text =
                std::fs::read_to_string(s.cwd.join(".sunmao/models.json")).unwrap_or_default();
            serde_json::from_str(&text).unwrap_or_default()
        });
        match file.providers.get(name) {
            Some(p) => p.clone(),
            None => return HostResponse::err(404, format!("no such provider: {name}")),
        }
    } else {
        match serde_json::from_value(v) {
            Ok(p) => p,
            Err(e) => {
                return HostResponse::err(400, format!("provider object or provider name: {e}"));
            }
        }
    };
    match sunmao_core::models::fetch_catalog(&def).await {
        Ok(catalog) => HostResponse::json(serde_json::json!({ "catalog": catalog })),
        Err(e) => HostResponse::err(502, format!("{e:#}")),
    }
}

/// `PUT /models?sess=` — replace `<session's project>/.sunmao/models.json`
/// wholesale, then reload every live host's resolver so the
/// picker/settings see it at once. The file shape is `ModelsFile`.
async fn models_put(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    let file: sunmao_core::models::ModelsFile = match serde_json::from_slice(body) {
        Ok(f) => f,
        Err(e) => return HostResponse::err(400, format!("bad models.json: {e}")),
    };
    let cwd = models_host(s, sess.clone())
        .map(|h| h.agent.session_cwd())
        .unwrap_or_else(|| s.cwd.clone());
    let dir = cwd.join(".sunmao");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    let pretty = match serde_json::to_string_pretty(&file) {
        Ok(t) => t,
        Err(e) => return HostResponse::err(400, format!("{e:#}")),
    };
    if let Err(e) = std::fs::write(dir.join("models.json"), pretty) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    for id in s.live_ids() {
        if let Some(h) = s.host(&id) {
            h.agent.reload_models();
        }
    }
    s.emit(serde_json::json!({"type": "models_changed"}));
    models_view(s, sess).await
}

#[cfg(test)]
mod tests {
    use super::session_meta;
    use crate::serve::host::display_path;

    #[test]
    fn title_is_first_typed_prompt() {
        let dir = std::env::temp_dir().join(format!("sunmao-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s-1.jsonl");
        let log = [
            r#"{"type":"started","model":"m","cwd":"x"}"#,
            r#"{"type":"message","message":{"role":"system","content":"identity"}}"#,
            r#"{"type":"message","message":{"role":"user","content":"[hook context] injected"}}"#,
            r#"{"type":"message","message":{"role":"user","content":"\n  fix the drag bug  \nsecond line"}}"#,
            r#"{"type":"message","message":{"role":"user","content":"later prompt"}}"#,
        ];
        std::fs::write(&p, log.join("\n")).unwrap();
        let m = session_meta(&p);
        assert_eq!(m["title"], "fix the drag bug");
        assert!(m["mtime"].as_u64().is_some());

        std::fs::write(&p, log[..3].join("\n")).unwrap();
        assert!(session_meta(&p)["title"].is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn display_path_strips_verbatim_prefix() {
        let p = |s: &str| display_path(std::path::Path::new(s));
        assert_eq!(p(r"\\?\C:\work\x"), r"C:\work\x");
        assert_eq!(p(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert_eq!(p("/home/u/x"), "/home/u/x");
    }
}
