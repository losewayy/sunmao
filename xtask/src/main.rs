//! xtask arch — the architecture gate. `CODE-ARCHITECTURE.md` is prose; this
//! binary is what actually stops a violation from landing. Four checks:
//!
//!   god files   — a source file past the line budget gets split; the
//!                 serve/assets frontend files it embeds share the budget
//!   layers      — llm may not know core/cli; core may not know the cli exists
//!   prose       — long prose-ish string literals belong in assets/*.md|txt,
//!                 not `.rs` code (cold-plug rule 6)
//!   tokens      — serve/assets consumes design tokens; raw
//!                 colors/durations/radii live in tokens.css only
//!   js syntax   — every serve/assets/*.js parses under `node --check`
//!
//! `#[cfg(test)]` modules are exempt — tests carry fixtures and diagnostics
//! that would trip every rule by design.
//!
//! Exit 1 prints every violation; `cargo xtask arch` is the CI hook.

use std::path::{Path, PathBuf};

const GOD_FILE_BUDGET: usize = 600;
/// A string literal this long with this many spaces reads like prose, not a
/// wire string — wire strings (JSON, flags, marker tags) stay short or dense.
const PROSE_MIN_LEN: usize = 80;
const PROSE_MIN_SPACES: usize = 5;

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "arch".to_string());
    if mode != "arch" {
        eprintln!("usage: cargo xtask arch");
        std::process::exit(2);
    }
    let root = workspace_root();
    let mut violations = Vec::new();

    let mut rs_files = Vec::new();
    collect_rs(&root.join("crates"), &mut rs_files);
    rs_files.sort();
    let mut asset_files = Vec::new();
    collect_assets(&root.join("crates/cli/src/serve/assets"), &mut asset_files);
    asset_files.sort();

    for file in &rs_files {
        check_god_file(file, &root, &mut violations);
        check_layers(file, &root, &mut violations);
        check_prose(file, &root, &mut violations);
    }
    for file in &asset_files {
        check_god_file(file, &root, &mut violations);
    }
    check_js_syntax(&asset_files, &root, &mut violations);
    check_mirrors(&root, &mut violations);
    check_design_tokens(&root, &mut violations);

    if violations.is_empty() {
        println!(
            "arch gate: clean ({} rs + {} asset files checked)",
            rs_files.len(),
            asset_files.len()
        );
    } else {
        eprintln!("arch gate: {} violation(s)\n", violations.len());
        for v in &violations {
            eprintln!("  {v}");
        }
        std::process::exit(1);
    }
}

fn workspace_root() -> PathBuf {
    // the xtask binary runs from the workspace root by convention; fall back
    // to the crate manifest dir's parent when invoked from elsewhere.
    std::env::current_dir()
        .ok()
        .filter(|d| d.join("Cargo.toml").exists())
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."))
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() && e.file_name() != "target" {
            collect_rs(&p, out);
        } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
            out.push(p);
        }
    }
}

/// The serve frontend bundle (`crates/cli/src/serve/assets/`) shares the
/// god-file budget with `.rs` — same navigability argument; the gate only
/// checked Rust, which is how index.html grew past 2000 lines.
fn collect_assets(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_assets(&p, out);
        } else if matches!(
            p.extension().and_then(|x| x.to_str()),
            Some("html" | "js" | "css")
        ) {
            out.push(p);
        }
    }
}

/// JS syntax gate — `node --check` catches a malformed asset before it
/// ships inside the binary. Node absent locally → warn and skip; on CI
/// (windows-latest ships node) absence is itself a violation — a silently
/// skipped gate there is coverage debt.
fn check_js_syntax(files: &[PathBuf], root: &Path, violations: &mut Vec<String>) {
    let node_ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !node_ok {
        if std::env::var_os("CI").is_some() {
            violations
                .push("js syntax: node not on PATH on CI — serve assets unchecked".to_string());
        } else {
            eprintln!("node not found — js syntax gate skipped");
        }
        return;
    }
    for f in files
        .iter()
        .filter(|p| p.extension().map(|x| x == "js").unwrap_or(false))
    {
        let out = match std::process::Command::new("node")
            .arg("--check")
            .arg(f)
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                violations.push(format!(
                    "js syntax: node --check failed to run for {}: {e}",
                    rel(f, root)
                ));
                continue;
            }
        };
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = err.lines().rev().take(6).collect();
            violations.push(format!(
                "js syntax: {} fails `node --check`:\n    {}",
                rel(f, root),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n    ")
            ));
        }
    }
}

/// Rule 1 — past the budget, a file needs a responsibility split, not more
/// scrolling.
fn check_god_file(file: &Path, root: &Path, violations: &mut Vec<String>) {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(_) => return,
    };
    let lines = text.lines().count();
    if lines <= GOD_FILE_BUDGET {
        return;
    }
    // The budget is a prompt to split, not a law of nature: a file that is one
    // cohesive thing can say so at the top and be allowed to stay large. The
    // reason is always printed, so the exceptions stay visible in every run.
    if let Some(reason) = allowed_god_file(&text) {
        println!(
            "god file allowed: {} has {lines} lines (budget {GOD_FILE_BUDGET}): {reason}",
            rel(file, root)
        );
        return;
    }
    violations.push(format!(
        "god file: {} has {lines} lines (budget {GOD_FILE_BUDGET}) — split by responsibility, or put `arch: allow-god-file <reason>` near the top",
        rel(file, root)
    ));
}

/// An over-budget file may say why it is one: a line near the top of the form
/// `arch: allow-god-file <reason>`, with a reason long enough to mean
/// something. Nothing else about the file is inspected.
fn allowed_god_file(text: &str) -> Option<String> {
    const MARK: &str = "arch: allow-god-file";
    text.lines()
        .take(12)
        .find_map(|l| l.split_once(MARK).map(|(_, r)| r.trim().trim_end_matches("*/").trim()))
        .filter(|r| r.chars().count() >= 12)
        .map(|r| r.to_string())
}

/// Rule 4 — layers run one way: cli → core → llm. A lower crate importing a
/// higher crate's name is the violation; the dep graph can lie (a stray
/// `use` inside a doc comment is harmless enough to catch with the same
/// cheap check and let humans argue it if it ever bites).
fn check_layers(file: &Path, root: &Path, violations: &mut Vec<String>) {
    let rel_path = rel(file, root);
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let (in_llm, in_core) = (
        rel_path.starts_with("crates/llm/"),
        rel_path.starts_with("crates/core/"),
    );
    if !in_llm && !in_core {
        return;
    }
    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if in_llm && (line.contains("sunmao_core") || line.contains("sunmao::")) {
            violations.push(format!(
                "layer violation: {rel_path}:{} — llm must not reference core/cli",
                line_no + 1
            ));
        }
        if in_core && (line.contains("sunmao::tui") || line.contains("::tui::")) {
            violations.push(format!(
                "layer violation: {rel_path}:{} — core must not know frontends exist",
                line_no + 1
            ));
        }
    }
}

/// Rule 6 — product semantics live in asset files. Heuristic: a string
/// literal long enough and airy enough (many spaces) is prose. Wire shapes
/// (JSON, regexes, marker tags) stay under the space budget naturally.
/// `#[cfg(test)]` modules are skipped — fixtures and diagnostics live there
/// by design.
fn check_prose(file: &Path, root: &Path, violations: &mut Vec<String>) {
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let rel_path = rel(file, root);
    for (line_no, lit) in literals_outside_tests(&text) {
        if lit.len() >= PROSE_MIN_LEN
            && lit.matches(' ').count() >= PROSE_MIN_SPACES
            && !looks_like_wire(&lit)
        {
            violations.push(format!(
                "prose in code: {rel_path}:{line_no} — {} chars, {} spaces; \
                 move product-facing text to assets/ (cold-plug rule 6)",
                lit.len(),
                lit.matches(' ').count()
            ));
        }
    }
}

/// A string literal inside a `Tool::function(...)` call is wire dialect —
/// decl descriptions are the contract itself (CODE-ARCHITECTURE's explicit
/// counter-example). Track paren depth after `Tool::function(` and skip
/// everything until it closes.
struct WireSpan {
    depth: usize,
}

fn literals_outside_tests(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    // test-module tracking: a `#[cfg(test)]` line arms `pending_cfg`; the
    // next `mod … {` opens the exempt region at `test_base` brace depth;
    // the region ends when the depth returns to `test_base`.
    let mut in_test = false;
    let mut test_base = 0usize;
    let mut pending_cfg = false;
    let mut brace_depth: usize = 0;
    let mut in_block_comment = false;
    let mut wire: Option<WireSpan> = None;

    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        // strip comments + string contents → count braces on the clean line
        let mut chars = raw.chars().peekable();
        let mut line_clean = String::with_capacity(raw.len());
        let mut in_str = false;
        while let Some(c) = chars.next() {
            match (in_str, in_block_comment, c, chars.peek().copied()) {
                (false, false, '/', Some('/')) => break, // // comment
                (false, false, '/', Some('*')) => {
                    in_block_comment = true;
                    chars.next();
                }
                (false, true, '*', Some('/')) => {
                    in_block_comment = false;
                    chars.next();
                }
                (false, true, _, _) => {}
                (false, false, '"', _) => in_str = true,
                (true, false, '\\', _) => {
                    chars.next(); // consume escaped char
                }
                (true, false, '"', _) => in_str = false,
                (true, false, _, _) => {} // inside a string — swallowed
                (false, false, _, _) => line_clean.push(c),
                (true, true, _, _) => {} // unreachable pair, harmless
            }
        }

        if !in_test {
            if line_clean.contains("#[cfg(test)]") {
                pending_cfg = true;
            }
            if pending_cfg && line_clean.contains("mod ") && line_clean.contains('{') {
                in_test = true;
                test_base = brace_depth; // depth before this line's braces
                pending_cfg = false;
            }
        }

        // wire-span tracking on the clean line (strings stripped): a
        // `Tool::function(` opens a paren span we stay inside until closed.
        let mut paren_depth = wire.as_ref().map(|w| w.depth).unwrap_or(0);
        let mut line_inside_wire = wire.is_some();
        let mut seen = String::new();
        for c in line_clean.chars() {
            seen.push(c);
            match c {
                '(' => paren_depth += 1,
                ')' => paren_depth = paren_depth.saturating_sub(1),
                _ => {}
            }
            if !line_inside_wire && seen.ends_with("Tool::function(") {
                line_inside_wire = true;
            }
        }
        wire = if line_inside_wire && paren_depth > 0 {
            Some(WireSpan { depth: paren_depth })
        } else {
            None
        };

        for c in line_clean.chars() {
            match c {
                '{' => brace_depth += 1,
                '}' => brace_depth = brace_depth.saturating_sub(1),
                _ => {}
            }
        }

        if in_test {
            if brace_depth <= test_base {
                in_test = false;
            }
            continue; // test code exempt by design — fixtures live there
        }
        if line_inside_wire {
            continue; // decl descriptions are wire dialect, not prose
        }

        for lit in extract_literals(raw) {
            out.push((line_no, lit));
        }
    }
    out
}

/// Pull `"…"` literal bodies off one line (no raw strings — a `r#"…"#`
/// literal is deliberate data, e.g. test fixtures and SQL).
fn extract_literals(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == 'r' && i + 1 < chars.len() && chars[i + 1] == '#' {
            // r#"…"# — skip the whole literal
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '"' && chars[i + 1] == '#') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if chars[i] == 'r' && i + 1 < chars.len() && chars[i + 1] == '"' {
            // r"…" — skip
            i += 2;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            i += 1;
            continue;
        }
        if chars[i] == '"' {
            i += 1;
            let mut body = String::new();
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                body.push(chars[i]);
                i += 1;
            }
            out.push(body);
            i += 1;
            continue;
        }
        i += 1;
    }
    out
}

/// A long literal that is still wire-shaped: dense braces/quotes, an actual
/// URL, a printf-style format skeleton, or a file path. These aren't prose
/// — they're contract strings, which rule 6 explicitly keeps in code.
fn looks_like_wire(lit: &str) -> bool {
    lit.contains("http://")
        || lit.contains("https://")
        || lit.contains('{')
        || lit.contains('}')
        || lit.contains(":\\")   // windows path
        || lit.contains('/') // path/regex-ish
}

/// Mirrored files — a bundled copy that must stay byte-identical to its
/// canonical source (self-contained plugin bundles can't reach outside
/// their dir, so the copy exists on purpose; the guard exists so it can't
/// silently drift). No mirrored pairs today — re-add blocks as bundles
/// need them.
fn check_mirrors(_root: &Path, _violations: &mut Vec<String>) {}

/// Rule: design tokens (DESIGN-SYSTEM.md §10). `tokens.css` is the only file
/// allowed to carry raw values; every other stylesheet plus `index.html`
/// consume `var(--*)` tokens, and every referenced token must resolve.
/// Lines carrying a `/* token-exempt: … */` trailer opt out individually.
fn check_design_tokens(root: &Path, violations: &mut Vec<String>) {
    use regex::Regex;
    let dir = root.join("crates/cli/src/serve/assets");
    let Ok(index_html) = std::fs::read_to_string(dir.join("index.html")) else {
        violations.push("design token: index.html missing".to_string());
        return;
    };
    let tokens_css = std::fs::read_to_string(dir.join("tokens.css")).unwrap_or_default();
    if tokens_css.is_empty() {
        violations.push("design token: tokens.css missing".to_string());
        return;
    }
    // every stylesheet in the bundle — app.css was split per responsibility,
    // the gate scans all of them (raw values are tokens.css-only everywhere)
    let mut css_files: Vec<PathBuf> = Vec::new();
    collect_assets(&dir, &mut css_files);
    css_files.retain(|p| p.extension().map(|x| x == "css").unwrap_or(false));
    css_files.sort();
    let css_srcs: Vec<(String, String)> = css_files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok().map(|t| (rel(f, root), t)))
        .collect();
    // the page's logic moved out of index.html into per-responsibility
    // .js files — token checks that scanned the inline script scan these now
    let mut js_files: Vec<PathBuf> = Vec::new();
    collect_assets(&dir, &mut js_files);
    js_files.retain(|p| p.extension().map(|x| x == "js").unwrap_or(false));
    js_files.sort();
    let js_srcs: Vec<(String, String)> = js_files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok().map(|t| (rel(f, root), t)))
        .collect();

    // ---- component css: banned raw values ---------------------------------
    let comment = Regex::new(r"(?s)/\*.*?\*/").unwrap();
    // strip comments file-wide but keep newlines so line numbers survive
    let strip = |text: &str| -> String {
        comment
            .replace_all(text, |m: &regex::Captures| {
                m[0].chars()
                    .map(|c| if c == '\n' { '\n' } else { ' ' })
                    .collect::<String>()
            })
            .into_owned()
    };
    let checks: &[(&str, &str)] = &[
        (r"#[0-9a-fA-F]{3,8}\b", "raw color; use a --c-* token"),
        (
            r"\brgba?\(\s*\d|\bhsla?\(",
            "raw color fn; use a --c-* token",
        ),
        (r"cubic-bezier\(", "raw curve; use an --ease-* token"),
        (
            r"font(-size)?:[^;]*\b\d+(\.\d+)?px",
            "raw font size/height px; use --fs-* and --lh-*",
        ),
        (
            r"border-radius:[^;]*\b[1-9]\d*(\.\d+)?px|border-radius:\s*50%",
            "raw radius; use an --r-* token",
        ),
        (r"z-index:\s*-?\d", "raw z-index; use a --z-* token"),
        (
            r"\[data-theme=",
            "theme fork; values belong in tokens.css [data-theme]",
        ),
        (r"@keyframes", "keyframes live in tokens.css only"),
    ];
    let duration = Regex::new(r"\b\d*\.?\d+m?s\b").unwrap();
    let ease_kw = Regex::new(r"\b(ease|ease-in|ease-out|ease-in-out)\b").unwrap();
    let var_ref = Regex::new(r"var\(\s*--[a-zA-Z0-9_-]+").unwrap();
    for (name, text) in &css_srcs {
        if name == "crates/cli/src/serve/assets/tokens.css" {
            continue; // tokens.css is where raw values live
        }
        let clean_text = strip(text);
        for (line_no, line) in text.lines().enumerate() {
            if line.contains("token-exempt") {
                continue;
            }
            let clean = clean_text.lines().nth(line_no).unwrap_or("");
            for (pat, msg) in checks {
                if Regex::new(pat).unwrap().is_match(clean) {
                    violations.push(format!("design token: {name}:{} — {msg}", line_no + 1));
                }
            }
            // durations + ease keywords can hide inside var() args; blank them first
            let novar = var_ref.replace_all(clean, "");
            for m in duration.find_iter(&novar) {
                if m.as_str() != "0s" {
                    violations.push(format!(
                        "design token: {name}:{} — raw duration {}; use a --dur-*/--loop-*/--hold-* token",
                        line_no + 1,
                        m.as_str()
                    ));
                }
            }
            for m in ease_kw.find_iter(&novar) {
                violations.push(format!(
                    "design token: {name}:{} — raw easing keyword {}; use an --ease-* token",
                    line_no + 1,
                    m.as_str()
                ));
            }
        }
    }

    // ---- index.html: no <style>, no style="…color/ms", no JS literals ----
    if index_html.contains("<style") {
        violations.push(
            "design token: index.html — <style> block; styles live in the css bundle".to_string(),
        );
    }
    let style_attr = Regex::new(r#"style="([^"]*)""#).unwrap();
    let attr_bad = Regex::new(r"#[0-9a-fA-F]{3,8}\b|\brgba?\(|\bhsla?\(|\d*\.?\d+m?s\b").unwrap();
    for (line_no, line) in index_html.lines().enumerate() {
        for m in style_attr.captures_iter(line) {
            let val = &m[1];
            if val.trim_start().starts_with("--") {
                continue; // local custom property (--_c etc.) is not a raw value
            }
            if attr_bad.is_match(val) {
                violations.push(format!(
                    "design token: index.html:{} — style= attribute carries color/duration; use tokens",
                    line_no + 1
                ));
            }
        }
    }
    let script_lit =
        Regex::new(r"cubic-bezier\(|duration\s*:\s*\d|setTimeout\([^)]*,\s*\d{3,}\)").unwrap();
    for (name, text) in js_srcs.iter().map(|(n, t)| (n.as_str(), t.as_str())) {
        for (line_no, line) in text.lines().enumerate() {
            for m in script_lit.find_iter(line) {
                violations.push(format!(
                    "design token: {name}:{} — raw timing {} in script; go through the motion bridge",
                    line_no + 1,
                    m.as_str()
                ));
            }
        }
    }

    // ---- var() resolution: every reference must be defined ---------------
    let def_re = Regex::new(r"(--[a-zA-Z0-9_-]+)\s*:").unwrap();
    let ref_re = Regex::new(r"var\(\s*(--[a-zA-Z0-9_-]+)").unwrap();
    let setprop_re = Regex::new(r#"setProperty\(\s*['"](--[a-zA-Z0-9_-]+)"#).unwrap();
    let defined: std::collections::HashSet<String> = css_srcs
        .iter()
        .flat_map(|(_, t)| {
            def_re
                .captures_iter(t.as_str())
                .map(|c| c[1].to_string())
                .collect::<Vec<_>>()
        })
        .chain(
            setprop_re
                .captures_iter(&index_html)
                .map(|c| c[1].to_string()),
        )
        .chain(js_srcs.iter().flat_map(|(_, t)| {
            setprop_re
                .captures_iter(t.as_str())
                .map(|c| c[1].to_string())
        }))
        .collect();
    for (name, text) in &css_srcs {
        let stripped = strip(text);
        for (line_no, line) in stripped.lines().enumerate() {
            for m in ref_re.captures_iter(line) {
                let tok = &m[1];
                if tok.starts_with("--_") {
                    continue; // component-local custom property
                }
                if !defined.contains(tok) {
                    violations.push(format!(
                        "design token: {name}:{} — var({tok}) has no definition in the css bundle or apply()",
                        line_no + 1
                    ));
                }
            }
        }
    }
}

fn rel(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .display()
        .to_string()
        .replace('\\', "/")
}

#[cfg(test)]
mod god_file_tests {
    use super::allowed_god_file;

    #[test]
    fn an_over_budget_file_may_say_why() {
        let text = "//! header
// arch: allow-god-file one cohesive scanner, split would hurt
fn a() {}
";
        assert_eq!(
            allowed_god_file(text).as_deref(),
            Some("one cohesive scanner, split would hurt")
        );
    }

    #[test]
    fn a_marker_needs_a_real_reason_and_a_place_near_the_top() {
        assert_eq!(allowed_god_file("// arch: allow-god-file
"), None, "no reason");
        assert_eq!(allowed_god_file("// arch: allow-god-file too short
"), None, "reason too short");
        let far = "// filler
".repeat(20) + "// arch: allow-god-file a perfectly good reason
";
        assert_eq!(allowed_god_file(&far), None, "the marker has to be near the top");
    }
}
