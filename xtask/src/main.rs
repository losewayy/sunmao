//! xtask arch — the architecture gate. `CODE-ARCHITECTURE.md` is prose; this
//! binary is what actually stops a violation from landing. Three checks:
//!
//!   god files   — a source file past the line budget gets split
//!   layers      — llm may not know core/cli; core may not know the cli exists
//!   prose       — long prose-ish string literals belong in assets/*.md|txt,
//!                 not `.rs` code (cold-plug rule 6)
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

    for file in &rs_files {
        check_god_file(file, &root, &mut violations);
        check_layers(file, &root, &mut violations);
        check_prose(file, &root, &mut violations);
    }
    check_mirrors(&root, &mut violations);

    if violations.is_empty() {
        println!("arch gate: clean ({} files checked)", rs_files.len());
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

/// Rule 1 — past the budget, a file needs a responsibility split, not more
/// scrolling.
fn check_god_file(file: &Path, root: &Path, violations: &mut Vec<String>) {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(_) => return,
    };
    let lines = text.lines().count();
    if lines > GOD_FILE_BUDGET {
        violations.push(format!(
            "god file: {} has {lines} lines (budget {GOD_FILE_BUDGET}) — split by responsibility",
            rel(file, root)
        ));
    }
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

fn rel(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .display()
        .to_string()
        .replace('\\', "/")
}
