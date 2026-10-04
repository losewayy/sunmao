//! `sunmao serve` 的 artifact 面 — versioned read (`?rev=`)、revs、
//! notes、annotate、MCP Apps sidecar (`{name}.ui.json`)。白名单 `safe_name`
//! 与 /annotate 同源；路径全部锚定在 `.sunmao/artifacts` 下。
//! 处理器返回 `HostResponse`（传输无关）—— axum 与 Tauri scheme 两个
//! 适配层共用同一套逻辑。

use std::sync::Arc;

use super::host::{Shared, safe_name};
use super::request::HostResponse;

fn artifact_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    cwd.join(".sunmao").join("artifacts")
}

/// The session's own project dir — artifacts live under the project the
/// session runs in, which is not necessarily the host's launch dir.
fn sess_dir(s: &Shared, sess: Option<&str>) -> std::path::PathBuf {
    sess.and_then(|id| s.host(id))
        .map(|h| h.agent.session_cwd())
        .or_else(|| {
            s.live_ids()
                .first()
                .and_then(|id| s.host(id))
                .map(|h| h.agent.session_cwd())
        })
        .unwrap_or_else(|| s.cwd.clone())
}

/// `GET /artifacts/{name}[?rev=k]` — the versioned read (GUI.md §3).
/// Latest is always `{name}.html`; history lives at `{name}.v{rev-1}.html`.
pub(super) async fn artifact_get(
    s: &Arc<Shared>,
    name: &str,
    rev: Option<String>,
    sess: Option<String>,
) -> HostResponse {
    if !safe_name(name) {
        return HostResponse::err(400, "bad artifact name".into());
    }
    let dir = artifact_dir(&sess_dir(s, sess.as_deref()));
    let latest = sunmao_core::tool::artifact_rev(&dir, name);
    if latest == 0 {
        return HostResponse::err(404, "no such artifact".into());
    }
    let rev = rev.and_then(|r| r.parse::<usize>().ok());
    let p = match rev.unwrap_or(latest) {
        r if r == 0 || r == latest => dir.join(format!("{name}.html")),
        r if r < latest => dir.join(format!("{name}.v{r}.html")),
        _ => {
            return HostResponse::err(404, format!("no rev — latest is {latest}"));
        }
    };
    match tokio::fs::read(&p).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            // declaration-based CSP (GUI.md §3): artifacts are untrusted —
            // default-deny everything; a frontmatter `csp.*:` list may
            // whitelist external resource/connect/frame domains.
            HostResponse::bytes(
                200,
                vec![
                    ("content-type".into(), "text/html; charset=utf-8".into()),
                    ("content-security-policy".into(), artifact_csp(&text)),
                ],
                bytes,
            )
        }
        Err(_) => HostResponse::err(404, "no such revision".into()),
    }
}

/// Frontmatter CSP declaration — `---`-delimited header at file top,
/// dot-keys with JSON-array values:
///   ---
///   csp.resourceDomains: ["https://fonts.googleapis.com"]
///   csp.connectDomains: ["https://api.example.com"]
///   ---
/// Same domain vocabulary as MCP Apps `_meta.ui.csp` (resource/connect/
/// frameDomains). Undeclared → fully locked CSP (scripts can never be
/// declared — the island sandbox already strips them; the header is the
/// second wall, not the first).
fn artifact_csp(html: &str) -> String {
    let mut resource = Vec::new();
    let mut connect = Vec::new();
    let mut frame = Vec::new();
    if let Some(rest) = html.trim_start().strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
        for line in rest[..end].lines() {
            let line = line.trim();
            let Some((key, val)) = line.split_once(':') else {
                continue;
            };
            let Ok(domains) = serde_json::from_str::<Vec<String>>(val.trim()) else {
                continue;
            };
            // origin-shaped values only — a bare `*` or scheme-less
            // token would defeat the whitelist's point
            let domains: Vec<String> = domains
                .into_iter()
                .filter(|d| d.starts_with("https://") || d.starts_with("http://127.0.0.1"))
                .collect();
            match key.trim() {
                "csp.resourceDomains" => resource = domains,
                "csp.connectDomains" => connect = domains,
                "csp.frameDomains" => frame = domains,
                _ => {}
            }
        }
    }
    let rd = if resource.is_empty() {
        String::new()
    } else {
        format!(" {}", resource.join(" "))
    };
    let cd = if connect.is_empty() {
        "'none'".into()
    } else {
        connect.join(" ")
    };
    let fd = if frame.is_empty() {
        "'none'".into()
    } else {
        frame.join(" ")
    };
    format!(
        "default-src 'none'; script-src 'none'; style-src 'self' 'unsafe-inline'{rd}; \
         img-src 'self' data:{rd}; font-src 'self' data:{rd}; \
         connect-src {cd}; frame-src {fd}; base-uri 'none'; form-action 'none'"
    )
}

/// `GET /artifacts/{name}/revs` → `{"rev": N}` — the island's ◀ ▶ nav
/// resolves the newest version lazily instead of trusting the event payload
/// (a page reload can sit behind the artifact's true state).
pub(super) async fn artifact_revs(
    s: &Arc<Shared>,
    name: &str,
    sess: Option<String>,
) -> HostResponse {
    if !safe_name(name) {
        return HostResponse::err(400, "bad artifact name".into());
    }
    HostResponse::json(serde_json::json!({
        "rev": sunmao_core::tool::artifact_rev(&artifact_dir(&sess_dir(s, sess.as_deref())), name),
    }))
}

pub(super) async fn artifact_notes(
    s: &Arc<Shared>,
    name: &str,
    sess: Option<String>,
) -> HostResponse {
    if !safe_name(name) {
        return HostResponse::err(400, "bad artifact name".into());
    }
    let p = artifact_dir(&sess_dir(s, sess.as_deref())).join(format!("{name}.state.json"));
    let v: serde_json::Value = tokio::fs::read_to_string(&p)
        .await
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({"annotations": []}));
    HostResponse::json(v)
}

pub(super) async fn artifact_annotate(
    s: &Arc<Shared>,
    name: &str,
    body: &[u8],
    sess: Option<String>,
) -> HostResponse {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return HostResponse::err(400, "bad annotate body".into());
    };
    let note = v["note"].as_str().unwrap_or("");
    // `sel` rides through verbatim — the GUI's element/region pick
    // ({tag,id,cls,text,css,rect}) lands in state.json for the agent
    HostResponse::json(serde_json::json!({
        "result": crate::commands::annotate(
            &sess_dir(s, sess.as_deref()),
            name,
            note,
            v.get("sel"),
        )
    }))
}

/// `GET /artifacts/{name}/ui` — the MCP Apps sidecar (`{name}.ui.json`):
/// which server/tool produced this island, the call's arguments + raw
/// result, and the resource's declared CSP. The island's sandbox proxy
/// needs it before it can handshake.
pub(super) async fn artifact_ui(s: &Arc<Shared>, name: &str, sess: Option<String>) -> HostResponse {
    if !safe_name(name) {
        return HostResponse::err(400, "bad artifact name".into());
    }
    let p = artifact_dir(&sess_dir(s, sess.as_deref())).join(format!("{name}.ui.json"));
    match tokio::fs::read_to_string(&p).await {
        Ok(text) => HostResponse::bytes(
            200,
            vec![(
                "content-type".into(),
                "application/json; charset=utf-8".into(),
            )],
            text.into_bytes(),
        ),
        Err(_) => HostResponse::err(404, "not an mcp-app artifact".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::artifact_csp;

    #[test]
    fn csp_default_locks_everything() {
        let csp = artifact_csp("<html><body>hi</body></html>");
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("script-src 'none'"));
        assert!(csp.contains("connect-src 'none'"));
        assert!(csp.contains("frame-src 'none'"));
    }

    #[test]
    fn csp_frontmatter_whitelists_domains() {
        let html = "---\ncsp.resourceDomains: [\"https://fonts.googleapis.com\", \"https://cdn.x.io\"]\ncsp.connectDomains: [\"https://api.example.com\"]\n---\n<html></html>";
        let csp = artifact_csp(html);
        assert!(csp.contains("https://fonts.googleapis.com"));
        assert!(csp.contains("https://cdn.x.io"));
        assert!(csp.contains("connect-src https://api.example.com"));
        assert!(csp.contains("script-src 'none'"));
    }

    #[test]
    fn csp_rejects_wildcards_and_bare_schemes() {
        let html =
            "---\ncsp.resourceDomains: [\"*\", \"http://evil.example.com\", \"data:\"]\n---\n<p/>";
        let csp = artifact_csp(html);
        assert!(!csp.contains("evil.example.com"));
        assert!(!csp.contains(" *"));
    }
}
