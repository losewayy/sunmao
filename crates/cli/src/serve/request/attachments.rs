//! `/attachments` surface — image uploads land in the viewed session's
//! `.sunmao/attachments/` (content-hashed names, extension kept) and read
//! back over `GET /attachments/{name}` for transcript `<img>` tags. Split
//! out of `request.rs` to keep the route table under the god-file budget.

use std::sync::Arc;

use super::super::host::{Shared, display_path};
use super::HostResponse;

/// The project dir an attachments request targets — the viewed session's
/// cwd when `sess` names a live host, else the launch dir.
fn root(s: &Arc<Shared>, sess: Option<&str>) -> std::path::PathBuf {
    sess.and_then(|id| s.host(id))
        .map(|h| h.agent.session_cwd())
        .unwrap_or_else(|| s.cwd.clone())
}

/// `POST /attachments[?sess=…&ext=…]` — store an image upload under the
/// session's `.sunmao/attachments/`. Body is the raw file bytes; `ext`
/// carries the original extension (validated against the image allowlist —
/// anything else degrades to `bin`, which `GET` then refuses to serve as
/// an image). A JSON `{"path": "…"}` body instead copies a local file —
/// the Tauri shell's drag-drop hands real paths, not blobs. Replies
/// `{name, path, mime}` — `path` is what a prompt frame's
/// `attachments[]` entry sends back.
pub(super) async fn put(
    s: &Arc<Shared>,
    sess: Option<String>,
    ext: Option<String>,
    body: &[u8],
) -> HostResponse {
    let root = root(s, sess.as_deref());
    let json_path = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["path"].as_str().map(std::path::PathBuf::from));
    let stored = match json_path {
        Some(p) => {
            if sunmao_llm::types::image_mime(&p).is_none() {
                return HostResponse::err(400, "not an image file".into());
            }
            crate::attachments::store_path(&root, &p)
        }
        None => {
            let ext = ext.unwrap_or_else(|| "png".into());
            crate::attachments::store_bytes(&root, &ext, body)
        }
    };
    match stored {
        Ok(p) => {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mime = sunmao_llm::types::image_mime(&p)
                .unwrap_or("application/octet-stream")
                .to_string();
            HostResponse::json(serde_json::json!({
                "name": name,
                "path": display_path(&p),
                "mime": mime,
            }))
        }
        Err(e) => HostResponse::err(500, format!("{e}")),
    }
}

/// `GET /attachments/{name}[?sess=…]` — serve one stored image back to the
/// transcript renderer. `name` is a bare filename (`[a-z0-9._-]`, no
/// separators) so the route can never read outside the attachments dir.
pub(super) fn get(s: &Arc<Shared>, name: &str, sess: Option<String>) -> HostResponse {
    let safe = !name.is_empty()
        && !name.contains(['/', '\\'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !safe {
        return HostResponse::err(400, "bad name".into());
    }
    let p = crate::attachments::dir(&root(s, sess.as_deref())).join(name);
    let Some(mime) = sunmao_llm::types::image_mime(&p) else {
        return HostResponse::err(404, "not found".into());
    };
    match std::fs::read(&p) {
        Ok(bytes) => HostResponse::bytes(
            200,
            vec![
                ("content-type".into(), mime.to_string()),
                ("cache-control".into(), "immutable".into()),
            ],
            bytes,
        ),
        Err(_) => HostResponse::err(404, "not found".into()),
    }
}
