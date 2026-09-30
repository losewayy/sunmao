//! `sunmao serve` 的 artifact REST 面 — versioned read (`?rev=`)、revs、
//! notes、annotate、MCP Apps sidecar (`{name}.ui.json`)。白名单 `safe_name`
//! 与 /annotate 同源；路径全部锚定在 `.sunmao/artifacts` 下。

use super::*;

fn artifact_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    cwd.join(".sunmao").join("artifacts")
}

#[derive(serde::Deserialize)]
pub(super) struct RevQuery {
    rev: Option<usize>,
}

/// `GET /artifacts/{name}[?rev=k]` — the versioned read (GUI.md §3).
/// Latest is always `{name}.html`; history lives at `{name}.v{rev-1}.html`.
pub(super) async fn artifact_get(
    State(s): State<Arc<Shared>>,
    AxPath(name): AxPath<String>,
    Query(q): Query<RevQuery>,
) -> Response {
    if !safe_name(&name) {
        return (StatusCode::BAD_REQUEST, "bad artifact name").into_response();
    }
    let dir = artifact_dir(&s.cwd);
    let latest = sunmao_core::tool::artifact_rev(&dir, &name);
    if latest == 0 {
        return (StatusCode::NOT_FOUND, "no such artifact").into_response();
    }
    let p = match q.rev.unwrap_or(latest) {
        r if r == 0 || r == latest => dir.join(format!("{name}.html")),
        r if r < latest => dir.join(format!("{name}.v{r}.html")),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                format!("no rev — latest is {latest}"),
            )
                .into_response();
        }
    };
    match tokio::fs::read(&p).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            // declaration-based CSP (GUI.md §3): artifacts are untrusted —
            // default-deny everything; a frontmatter `csp.*:` list may
            // whitelist external resource/connect/frame domains.
            let headers = [
                (header::CONTENT_TYPE, "text/html; charset=utf-8".into()),
                (header::CONTENT_SECURITY_POLICY, artifact_csp(&text)),
            ];
            (headers, bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "no such revision").into_response(),
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
    State(s): State<Arc<Shared>>,
    AxPath(name): AxPath<String>,
) -> Response {
    if !safe_name(&name) {
        return (StatusCode::BAD_REQUEST, "bad artifact name").into_response();
    }
    Json(serde_json::json!({
        "rev": sunmao_core::tool::artifact_rev(&artifact_dir(&s.cwd), &name),
    }))
    .into_response()
}

pub(super) async fn artifact_notes(
    State(s): State<Arc<Shared>>,
    AxPath(name): AxPath<String>,
) -> Response {
    if !safe_name(&name) {
        return (StatusCode::BAD_REQUEST, "bad artifact name").into_response();
    }
    let p = artifact_dir(&s.cwd).join(format!("{name}.state.json"));
    let v: serde_json::Value = tokio::fs::read_to_string(&p)
        .await
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({"annotations": []}));
    Json(v).into_response()
}

#[derive(serde::Deserialize)]
pub(super) struct AnnotateBody {
    note: String,
}

pub(super) async fn artifact_annotate(
    State(s): State<Arc<Shared>>,
    AxPath(name): AxPath<String>,
    Json(body): Json<AnnotateBody>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "result": crate::tui::slash::annotate(&s.cwd, &name, &body.note)
    }))
}

/// `GET /artifacts/{name}/ui` — the MCP Apps sidecar (`{name}.ui.json`):
/// which server/tool produced this island, the call's arguments + raw
/// result, and the resource's declared CSP. The island's sandbox proxy
/// needs it before it can handshake.
pub(super) async fn artifact_ui(
    State(s): State<Arc<Shared>>,
    AxPath(name): AxPath<String>,
) -> Response {
    if !safe_name(&name) {
        return (StatusCode::BAD_REQUEST, "bad artifact name").into_response();
    }
    let p = artifact_dir(&s.cwd).join(format!("{name}.ui.json"));
    match tokio::fs::read_to_string(&p).await {
        Ok(text) => (
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            text,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not an mcp-app artifact").into_response(),
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
