//! 传输无关的 REST 面 — `HostHandle::request` 是 serve 路由表的协议内
//! 形式：同一份路由被 `serve::http`（axum fallback）与 Tauri 的
//! `sunmao` scheme 处理器共用。所有端点语义（白名单、CSP、版本链）
//! 住在这里与其 delegate（`artifacts.rs`），传输层只负责字节进出。

use std::sync::Arc;

use super::artifacts;
use super::host::{HostHandle, Shared, display_path, log_path};
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
            ("POST", ["session", "new"]) => match super::host::new_session(s).await {
                Ok(v) => HostResponse::json(v),
                Err(e) => HostResponse::err(500, format!("{e:#}")),
            },
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
                artifacts::artifact_get(s, name, query_arg(query, "rev")).await
            }
            ("GET", ["artifacts", name, "revs"]) => artifacts::artifact_revs(s, name).await,
            ("GET", ["artifacts", name, "ui"]) => artifacts::artifact_ui(s, name).await,
            ("GET", ["artifacts", name, "notes"]) => artifacts::artifact_notes(s, name).await,
            ("POST", ["artifacts", name, "annotate"]) => {
                artifacts::artifact_annotate(s, name, body).await
            }
            ("GET", ["dataflow"]) => dataflow_current(s, query_arg(query, "sess")).await,
            ("GET", ["dataflow", id]) => dataflow_by_id(s, id).await,
            _ => HostResponse::err(404, "not found".into()),
        }
    }
}

/// `GET /sessions` — the rail = dormant logs on disk ∪ live hosts (a
/// session the host is running exists even when its log hasn't flushed a
/// fresh name yet).
async fn sessions_list(s: &Arc<Shared>) -> HostResponse {
    let mut ids = tui::menu::recent_sessions(&s.cwd, 50);
    for id in s.live_ids() {
        if !ids.contains(&id) {
            ids.insert(0, id);
        }
    }
    let dir = s.cwd.join(".sunmao/sessions");
    let meta: serde_json::Map<String, serde_json::Value> = ids
        .iter()
        .map(|id| (id.clone(), session_meta(&dir.join(format!("{id}.jsonl")))))
        .collect();
    HostResponse::json(serde_json::json!({
        "sessions": ids,
        "live": s.live_ids(),
        "meta": meta,
    }))
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

/// `GET /session[?id=…]` — the viewed host's id + live set + cwd.
async fn session_info(s: &Arc<Shared>, id: Option<String>) -> HostResponse {
    let host = s.host(&id.unwrap_or_default());
    HostResponse::json(serde_json::json!({
        "id": host.as_ref().map(|h| h.id.clone()),
        "live": s.live_ids(),
        "cwd": display_path(&s.cwd),
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
