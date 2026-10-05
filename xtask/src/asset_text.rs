//! Rule: asset text stays plain. The serve bundle builds markup by
//! interpolating `t()` into template literals, so one string is injected both
//! as text and into an attribute — and the two contexts need opposite
//! spellings. Four checks keep one line of copy from breaking the page:
//!
//!   plain values   — a shipped translation carries no `"`, `<`, `>` and no
//!                    HTML entity: a `"` would end a double-quoted attribute
//!                    early, a `<` would be parsed as markup, and an entity
//!                    (`&lt;`) would smuggle either past the template
//!   plain keys     — a source key renders verbatim in Chinese, so `<>` in
//!                    one is parsed instead of shown
//!   escaped sites  — every `t(...)` interpolated into an attribute value is
//!                    wrapped in esc(); the delimiter is the break-out point
//!   no tail slash  — an attribute value ending in a bare `\` is the one
//!                    spelling that reads as an escape and parses by luck;
//!                    write a literal backslash as `&#92;`
//!
//! Keys and values are read from the two GENERATED dictionaries (the bytes
//! that ship), sites from the hand-written scripts beside them. The callers in
//! `main.rs` walk the bundle and print what these return.
//! `_scratch/i18n/merge.mjs` enforces the same rules on the json sources at
//! generation time; this is the CI side of the same fence.

use regex::Regex;

/// Dictionary entries that break the plain-text rule, as `name:line` messages.
pub fn dictionary_violations(name: &str, text: &str) -> Vec<String> {
    let pair = Regex::new(r#"^\s*"((?:[^"\\]|\\.)*)"\s*:\s*"((?:[^"\\]|\\.)*)",?\s*$"#).unwrap();
    let entity = Regex::new(r"&(?:#[0-9]+|[A-Za-z][A-Za-z0-9]*);").unwrap();
    let mut out = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        let Some(c) = pair.captures(line) else {
            continue;
        };
        let key = json_unescape(&c[1]);
        let val = json_unescape(&c[2]);
        if val.contains('"') || val.contains('<') || val.contains('>') || entity.is_match(&val) {
            out.push(format!(
                "asset text: {name}:{} — translation is not plain text (quotes are “ ”, markup belongs to the template): {val:?}",
                line_no + 1
            ));
        } else if key.contains('<') || key.contains('>') {
            out.push(format!(
                "asset text: {name}:{} — source key carries markup, so it renders as a tag: {key:?}",
                line_no + 1
            ));
        }
    }
    out
}

/// Attribute-position `t()` calls that nothing escapes, as `name:line` messages.
pub fn site_violations(name: &str, src: &str) -> Vec<String> {
    attr_sites(src)
        .into_iter()
        .map(|(line, snippet)| {
            format!("asset text: {name}:{line} — attribute-position t() is not escaped; wrap it in esc(): {snippet}")
        })
        .collect()
}

/// HTML attribute values ending in a bare backslash, as `name:line` messages.
pub fn backslash_tails(name: &str, text: &str) -> Vec<String> {
    let attrs = Regex::new(r#"[-\w]+="([^"\n]*)""#).unwrap();
    let mut out = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        for c in attrs.captures_iter(line) {
            if c[1].ends_with('\\') {
                out.push(format!(
                    "asset text: {name}:{} — attribute value ends in a bare backslash; write it as &#92;: {}",
                    line_no + 1,
                    &c[0]
                ));
            }
        }
    }
    out
}

/// Every `t(...)` inside an HTML attribute value that no `esc()`/`ta()` wraps,
/// as `(line, snippet)`. Attribute values here are built inside one-line
/// template literals, so a value is read to the closing quote on the same line.
fn attr_sites(src: &str) -> Vec<(usize, String)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 2;
    while i + 2 < b.len() {
        let quote = b[i];
        let anchor = (quote == b'"' || quote == b'\'')
            && b[i - 1] == b'='
            && (b[i - 2] as char).is_ascii_alphanumeric()
            && b[i + 1] == b'$'
            && b[i + 2] == b'{';
        if !anchor {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut end = None;
        while j < b.len() {
            if b[j] == b'\n' {
                break;
            }
            if b[j] == b'$' && j + 1 < b.len() && b[j + 1] == b'{' {
                j = close_interp(b, j);
            } else if b[j] == quote {
                end = Some(j);
                break;
            }
            j += 1;
        }
        let Some(end) = end else {
            i += 1;
            continue;
        };
        let value = &src[i + 1..end];
        for (off, _) in value.match_indices("t(") {
            let prev = value.as_bytes().get(off.wrapping_sub(1)).copied();
            if off > 0 && (prev.unwrap_or(b' ') as char).is_ascii_alphanumeric() {
                continue;
            }
            if off > 0 && matches!(prev, Some(b'.') | Some(b'$')) {
                continue;
            }
            let before = value[..off].trim_end();
            if before.ends_with("esc(") || before.ends_with("ta(") {
                continue;
            }
            let line = src[..i + 1 + off].matches('\n').count() + 1;
            out.push((line, value[off..].chars().take(42).collect()));
        }
        i = end + 1;
    }
    out
}

/// Index of the quote closing a JS string that starts at `i` (escapes skipped).
fn skip_quoted(b: &[u8], i: usize) -> usize {
    let q = b[i];
    let mut i = i + 1;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            return i;
        }
        i += 1;
    }
    b.len().saturating_sub(1)
}

/// Index of the `}` closing the `${` that starts at `at`.
fn close_interp(b: &[u8], at: usize) -> usize {
    let mut depth = 0i32;
    let mut i = at + 1;
    while i < b.len() {
        match b[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            b'\'' | b'"' | b'`' => i = skip_quoted(b, i),
            _ => {}
        }
        i += 1;
    }
    b.len().saturating_sub(1)
}

/// Decode a JSON string body (the text between its quotes, escapes intact).
fn json_unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other), // \" \\ \/ and any other escape
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_sites_pass_and_bare_ones_are_reported() {
        let ok = "`<b data-tip=\"${esc(t('后退'))}\" aria-label=\"${esc(t('后退'))}\">`";
        assert!(site_violations("x.js", ok).is_empty());
        let bad = "`<b data-tip=\"${t('后退')}\" aria-label=\"${esc(t('后退'))}\">`";
        let hits = site_violations("x.js", bad);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].contains("x.js:1"), "{hits:?}");
    }

    #[test]
    fn a_value_carrying_its_delimiter_is_reported() {
        let bad = "  \"保存失败：{msg}\": \"Save failed: \\\"{msg}\\\",\",\n";
        let hits = dictionary_violations("i18n.en.panels.js", bad);
        assert_eq!(hits.len(), 1, "{hits:?}");
        let ok = "  \"保存失败：{msg}\": \"Save failed: {msg}\",\n";
        assert!(dictionary_violations("i18n.en.panels.js", ok).is_empty());
        let backslash = "  \"数据面板|Ctrl \\\\\": \"Panels|Ctrl \\\\\",\n";
        assert!(dictionary_violations("i18n.en.js", backslash).is_empty());
    }

    #[test]
    fn a_key_carrying_markup_is_reported() {
        let bad = "  \"可用 /effort <level> 直接指定\": \"set one with /effort\",\n";
        assert_eq!(dictionary_violations("i18n.en.js", bad).len(), 1);
    }

    #[test]
    fn an_attribute_value_may_not_end_in_a_bare_backslash() {
        assert_eq!(
            backslash_tails("index.html", "<b data-tip=\"Ctrl \\\">\n").len(),
            1
        );
        assert!(backslash_tails("index.html", "<b data-tip=\"Ctrl &#92;\">\n").is_empty());
    }
}
