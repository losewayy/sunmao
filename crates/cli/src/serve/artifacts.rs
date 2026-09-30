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
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "no such revision").into_response(),
    }
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
