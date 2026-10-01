//! Image attachments — the `@path` mention parser plus the
//! `.sunmao/attachments/` store every frontend shares. Attachments are
//! files (content-hashed names, original extension kept); the session log
//! records the block's path, never the bytes, so logs stay text-sized.
//! Resolution to base64 happens once per request inside
//! `Content::resolve` (crates/llm).

use std::io;
use std::path::{Path, PathBuf};

use sunmao_llm::Content;

/// The directory attachments live under — `<project>/.sunmao/attachments`.
pub(crate) fn dir(cwd: &Path) -> PathBuf {
    cwd.join(".sunmao").join("attachments")
}

/// Store raw bytes under a content-hashed name (`{hash}.{ext}`) — the same
/// upload twice dedupes to the same file. `ext` is sanitized to the image
/// allowlist implicitly: callers pass an extension they already validated
/// against `sunmao_llm::types::image_mime`, anything else is dropped to
/// `bin` so it never masquerades as a served image.
pub(crate) fn store_bytes(cwd: &Path, ext: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let dir = dir(cwd);
    std::fs::create_dir_all(&dir)?;
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    let ext = if sunmao_llm::types::IMAGE_EXTS
        .iter()
        .any(|e| ext.eq_ignore_ascii_case(e))
    {
        ext.to_ascii_lowercase()
    } else {
        "bin".to_string()
    };
    let path = dir.join(format!("{:016x}.{ext}", h.finish()));
    if !path.exists() {
        std::fs::write(&path, bytes)?;
    }
    Ok(path)
}

/// Copy a local file into the attachments store — the `@path` mention path.
pub(crate) fn store_path(cwd: &Path, src: &Path) -> io::Result<PathBuf> {
    let bytes = std::fs::read(src)?;
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin")
        .to_string();
    store_bytes(cwd, &ext, &bytes)
}

/// Characters that may appear inside an `@` mention token — paths only,
/// anything else (space, quotes, `@` itself) ends the token.
fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'\\')
}

/// Scan `text` for `@path` mentions, attaching the ones that resolve to an
/// existing image: relative tokens resolve against `cwd`, absolute ones
/// as-is. The prompt text is returned verbatim (the model sees the
/// `@path` marker alongside the image block; a recalled draft re-attaches
/// on resubmit) — a miss or a non-image simply produces no block.
pub(crate) fn attach_mentions(text: &str, cwd: &Path) -> (String, Vec<Content>) {
    let bytes = text.as_bytes();
    let mut blocks: Vec<Content> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let at = bytes[i] == b'@' && (i == 0 || bytes[i - 1].is_ascii_whitespace());
        if !at {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && token_char(bytes[end]) {
            end += 1;
        }
        let token = &text[start..end];
        if !token.is_empty() {
            let p = Path::new(token);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            };
            if abs.exists()
                && sunmao_llm::types::image_mime(&abs).is_some()
                && let Ok(stored) = store_path(cwd, &abs)
                && let Some(block) = Content::image(stored.display().to_string())
            {
                blocks.push(block);
            }
        }
        i = end.max(i + 1);
    }
    (text.to_string(), blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(path: &Path) {
        // smallest valid-looking blob suffices — resolution reads bytes,
        // not format headers
        std::fs::write(path, b"\x89PNG\r\n\x1a\nfake").unwrap();
    }

    #[test]
    fn mention_attaches_existing_image() {
        let dir = std::env::temp_dir().join(format!("sunmao-att-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        png(&dir.join("shot.png"));
        let (text, blocks) = attach_mentions("look at @shot.png please", &dir);
        assert_eq!(text, "look at @shot.png please");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Content::Image { path, mime } => {
                assert_eq!(mime, "image/png");
                // landed in the store, not the source path
                assert!(path.contains("attachments"));
            }
            other => panic!("expected image block, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn misses_and_non_images_stay_literal() {
        let dir = std::env::temp_dir().join(format!("sunmao-att2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), b"hi").unwrap();
        let (text, blocks) = attach_mentions("@missing.png @notes.txt @ end", &dir);
        assert_eq!(text, "@missing.png @notes.txt @ end");
        assert!(blocks.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mid_word_at_is_not_a_mention() {
        let dir = std::env::temp_dir();
        let (text, blocks) = attach_mentions("mail me@x.png now", &dir);
        assert_eq!(text, "mail me@x.png now");
        assert!(blocks.is_empty());
    }
}
