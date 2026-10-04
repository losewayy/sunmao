//! Skill discovery — the directories `<name>/SKILL.md|SKILL.html` may
//! live in, the `(name, description, path)` index built from their
//! frontmatter, and the YAML/HTML meta readers behind it. The prompt
//! index and the `/name` slash resolver both consume this — one scan,
//! no disagreements.

use std::path::{Path, PathBuf};

/// Every directory that may hold `<name>/SKILL.md|SKILL.html` — project,
/// claude-compat, plugin bundles, the user-level ecosystem scan (sunmao's
/// own layer first; `.claude` and the cross-harness `.agents` after), then
/// preset roots last. The `/name` resolver reads this same list so the
/// slash menu and the prompt index can never disagree on what exists.
pub fn skills_dirs(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = vec![
        cwd.join(".sunmao").join("skills"),
        cwd.join(".claude").join("skills"),
        cwd.join(".sunmao").join("plugin").join("skills"),
    ];
    // plugin bundles: .sunmao/plugins/<name>/skills/, .claude/plugins/<name>/skills/
    for base in [
        cwd.join(".sunmao").join("plugins"),
        cwd.join(".claude").join("plugins"),
    ] {
        for p in crate::sorted_entries(&base) {
            dirs.push(p.path().join("skills"));
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let h = std::path::Path::new(&home);
        dirs.push(h.join(".sunmao").join("skills"));
        dirs.push(h.join(".claude").join("skills"));
        dirs.push(h.join(".agents").join("skills"));
    }
    for root in extra_roots {
        dirs.push(root.join("skills"));
    }
    dirs
}

/// `(name, description, skill-file path)` for every skill under
/// `skills_dirs` — the prompt index and the `/name` slash resolver both
/// consume this; earlier dirs win a name collision (same layering as the
/// index itself).
pub fn skills_index(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<(String, String, PathBuf)> {
    let mut out: Vec<(String, String, PathBuf)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for skills_dir in skills_dirs(cwd, extra_roots) {
        for e in crate::sorted_entries(&skills_dir) {
            // a skill dir may speak either doc language — SKILL.md (the
            // ecosystem contract) or SKILL.html (the first-class payload:
            // a page for the human, structured text for the agent)
            let mut skill = e.path().join("SKILL.md");
            let is_html_skill = if skill.exists() {
                false
            } else {
                let html = e.path().join("SKILL.html");
                if html.exists() {
                    skill = html;
                    true
                } else {
                    continue;
                }
            };
            if let Ok(text) = std::fs::read_to_string(&skill) {
                let (name, desc) = if is_html_skill {
                    html_skill_meta(&text)
                } else {
                    md_skill_meta(&text, &e.file_name().to_string_lossy())
                };
                if seen.insert(name.clone()) {
                    out.push((name, desc, skill));
                }
            }
        }
    }
    out
}

/// Strip YAML scalar quoting — `name: "x"` and `name: 'x'` both mean `x`.
pub(crate) fn yaml_scalar(v: &str) -> String {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v)
        .to_string()
}

/// `SKILL.md` frontmatter parse: first 20 lines for `name:`/`description:`,
/// dir name as the fallback. `description: |`/`>` block scalars (the
/// ecosystem's multi-line convention) read their indented body, folded to
/// one line.
pub(crate) fn md_skill_meta(text: &str, fallback: &str) -> (String, String) {
    let mut name = fallback.to_string();
    let mut desc = String::new();
    let lines: Vec<&str> = text.lines().take(20).collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(v) = line.strip_prefix("name:") {
            name = yaml_scalar(v);
        } else if let Some(v) = line.strip_prefix("description:") {
            let v = v.trim();
            if v.starts_with('|') || v.starts_with('>') {
                let mut parts = Vec::new();
                for l in &lines[i + 1..] {
                    let t = l.trim();
                    if l.starts_with([' ', '\t']) && !t.is_empty() {
                        parts.push(t);
                    } else if !t.is_empty() {
                        break;
                    }
                }
                desc = parts.join(" ");
            } else {
                desc = yaml_scalar(v);
            }
        }
        i += 1;
    }
    (name, desc)
}

/// `SKILL.html` meta — the page already carries its identity: `<title>`
/// is the name, `<meta name="description">` the description. Falls back
/// to the first <h1> for the name; description may be empty.
pub(crate) fn html_skill_meta(text: &str) -> (String, String) {
    let tag_text = |open: &str, close: &str| -> Option<String> {
        let start = text.find(open)? + open.len();
        let end = text[start..].find(close)? + start;
        Some(html_unescape(text[start..end].trim()))
    };
    let name = tag_text("<title>", "</title>")
        .or_else(|| tag_text("<h1>", "</h1>"))
        .unwrap_or_default();
    let desc = html_meta_description(text).unwrap_or_default();
    (name, desc)
}

/// Scan `<meta>` tags for `name="description"` and read its `content`
/// attribute — attribute order varies in the wild.
pub(crate) fn html_meta_description(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(i) = rest.find("<meta") {
        let tail = &rest[i..];
        let end = tail.find('>')? + 1;
        let tag = &tail[..end];
        if tag.contains("name=\"description\"") || tag.contains("name='description'") {
            return attr_value(tag, "content").map(|v| html_unescape(v.trim()));
        }
        rest = &tail[end..];
    }
    None
}

/// `attr="..."` or `attr='...'` inside a tag — returns the quoted body.
pub(crate) fn attr_value<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    for pat in [format!("{attr}=\""), format!("{attr}='")] {
        if let Some(i) = tag.find(&pat) {
            let s = i + pat.len();
            let q = tag.as_bytes()[s - 1] as char;
            return tag[s..].find(q).map(|e| &tag[s..s + e]);
        }
    }
    None
}

/// The tiny escape set HTML skill titles realistically use — entities we
/// can fix without a parser; unknown entities pass through untouched.
pub(crate) fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}
