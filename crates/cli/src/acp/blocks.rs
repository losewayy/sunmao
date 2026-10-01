//! Prompt payload → kernel input — an ACP `PromptRequest` carries content
//! *blocks*, not a string. Text blocks join on newlines; image blocks
//! decode base64 into the session's `.sunmao/attachments/` store so the
//! session log keeps a path, never the payload (same rule as the GUI's
//! `POST /attachments`).

use std::path::Path;

use agent_client_protocol::schema::v2;

/// `(text, attachments)` — text is every Text block joined on newlines
/// (an undecodable image degrades to a `[image attachment dropped]` text
/// line so the model sees that something was lost, not a silent gap).
pub(super) fn prompt_blocks(
    blocks: &[v2::ContentBlock],
    session_cwd: &Path,
) -> (String, Vec<sunmao_llm::Content>) {
    let mut attachments: Vec<sunmao_llm::Content> = Vec::new();
    let text = blocks
        .iter()
        .filter_map(|b| match b {
            v2::ContentBlock::Text(t) => Some(t.text.clone()),
            v2::ContentBlock::Image(img) => {
                match decode_image(img.data.as_str(), img.mime_type.as_ref(), session_cwd) {
                    Some(b) => attachments.push(b),
                    None => return Some("[image attachment dropped]".to_string()),
                }
                None
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (text, attachments)
}

/// base64 → stored file → image block. `mime`'s subtype becomes the stored
/// extension (`jpeg` normalizes to `jpg`); unknown subtypes fall back to
/// `bin` via the store's allowlist.
fn decode_image(data: &str, mime: &str, cwd: &Path) -> Option<sunmao_llm::Content> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    let ext = match mime.rsplit('/').next() {
        Some("jpeg") => "jpg",
        Some(e) => e,
        None => "bin",
    };
    let p = crate::attachments::store_bytes(cwd, ext, &bytes).ok()?;
    Some(sunmao_llm::Content::Image {
        path: p.display().to_string(),
        mime: mime.to_string(),
    })
}
