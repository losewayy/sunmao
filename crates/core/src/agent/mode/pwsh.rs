//! `ShellBackend::Pwsh` mutation classifier — no AST exists for
//! PowerShell here, so a quote-aware segment splitter + verb list does
//! the job the deno parser does for POSIX. Conservatism is identical:
//! anything we can't prove read-only is mutating.

// ---------- PowerShell mutation classifier (ShellBackend::Pwsh) ----------

/// Does a pwsh command mutate? No AST exists here — a segment scanner
/// splits on `;`/`|`/`&&`/`||` outside quotes and classifies each piece:
/// redirects (`>`,`>>`,`| Tee-Object`), env/variable writes (`$x = `,
/// `Set-Item Env:`), and the verb's membership in the read-only list.
/// Anything we can't prove read-only is mutating — same conservatism as
/// the bash AST walk.
pub fn pwsh_mutates(command: &str, verbs: &std::collections::HashSet<String>) -> bool {
    for seg in pwsh_segments(command) {
        if pwsh_segment_mutates(&seg, verbs) {
            return true;
        }
    }
    false
}

/// Split a pwsh line into command pieces on `;`, `|`, `&&`, `||` — quote
/// and backtick aware. `$(`/`(`/`{` depth keeps pipes inside subexpressions
/// and scriptblocks from splitting.
pub fn pwsh_segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;
    let mut depth = 0i32;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), '\\') | (Some(q), '`') => {
                cur.push(c);
                let _ = chars.next().map(|n| cur.push(n));
                let _ = q;
            }
            (Some(q), ch) if ch == q => {
                quote = None;
                cur.push(c);
            }
            (Some(_), ch) => cur.push(ch),
            (None, '"') | (None, '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, '(') | (None, '{') | (None, '[') => {
                depth += 1;
                cur.push(c);
            }
            (None, ')') | (None, '}') | (None, ']') => {
                depth = (depth - 1).max(0);
                cur.push(c);
            }
            (None, ';') if depth == 0 => {
                let s = cur.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                cur.clear();
            }
            (None, '|') if depth == 0 => {
                // `||` is a boolean op, `|` is a pipeline — both split.
                let s = cur.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                cur.clear();
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
            }
            (None, '&') if depth == 0 && chars.peek() == Some(&'&') => {
                chars.next();
                let s = cur.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let s = cur.trim().to_string();
    if !s.is_empty() {
        out.push(s);
    }
    out
}

/// One pwsh segment's verdict. Three escalation rules before the verb
/// check: redirects write, `$(`/`new-item`-class verbs write, and any
/// assignment to `$x`/`env:`/`script:`/`global:` is state, not output.
fn pwsh_segment_mutates(seg: &str, verbs: &std::collections::HashSet<String>) -> bool {
    let s = seg.trim();
    // a segment of pure comment is read-only
    if s.starts_with('#') {
        return false;
    }
    // a raw `>`/`>>` outside quotes is always a redirect in pwsh — the
    // comparison ops are `-gt`/`-ge` (no `>` char), so there's no false
    // friend. Strings are stripped first so `"a > b"` doesn't trip it.
    let unquoted = strip_pwsh_strings(s);
    if unquoted.contains('>') {
        return true;
    }
    // `$(…)`/`` ` ``/backtick — a subshell inside an arg runs code we
    // can't see; same "dynamic verb = mutating" rule as the bash walker.
    if unquoted.contains("$(") || unquoted.contains('`') {
        return true;
    }
    // `$x = …`, `Set-Item Env:foo`, `[Environment]::Set…` — persistent
    // state writes, same as ShellVar mutation in the POSIX walker.
    if pwsh_assignment(&unquoted) {
        return true;
    }
    // first non-comment token = the verb. `Get-*`/`Write-Host`/`echo` are
    // reads; `Set-*`/`New-*`/`Remove-*`/`Move-*`/`Copy-*`/`Rename-*`/
    // `Invoke-*`/`Start-*`/`Stop-*` writes by PowerShell convention.
    let verb = unquoted
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches('&')
        .trim_start_matches('.');
    if verb.is_empty() {
        return false;
    }
    let v = verb.to_ascii_lowercase();
    // prefix classes that always write — the verb list can't enumerate
    // every `Set-*`/`New-*` the OS ships.
    const WRITE_PREFIXES: &[&str] = &[
        "set-",
        "new-",
        "remove-",
        "move-",
        "copy-",
        "rename-",
        "invoke-",
        "start-",
        "stop-",
        "restart-",
        "clear-",
        "add-",
        "enable-",
        "disable-",
        "mount-",
        "unmount-",
        "push-",
        "pop-",
        "out-file",
        "tee-object",
        "export-",
        "import-",
        "install-",
        "uninstall-",
        "register-",
        "unregister-",
        "send-",
        "publish-",
        "save-",
        "update-",
        "write-eventlog",
    ];
    if WRITE_PREFIXES.iter().any(|p| v.starts_with(p)) {
        return true;
    }
    // `git` subcommand semantics are dialect-independent — `git status`
    // reads under pwsh exactly as under bash. Mirror the POSIX walker's
    // read/lists split; a bare listing sub is read-only.
    if v == "git" {
        let toks: Vec<&str> = unquoted.split_whitespace().collect();
        let sub = toks.get(1).copied().unwrap_or("").to_ascii_lowercase();
        const GIT_READS: &[&str] = &[
            "status",
            "log",
            "diff",
            "show",
            "blame",
            "rev-parse",
            "rev-list",
            "ls-files",
            "ls-remote",
            "ls-tree",
            "describe",
            "shortlog",
            "whatchanged",
            "verify-commit",
            "verify-tag",
            "count-objects",
            "cat-file",
            "name-rev",
        ];
        const GIT_LISTS: &[&str] = &["branch", "tag", "remote", "stash"];
        let after_sub = &toks[2.min(toks.len())..];
        let listing_bare =
            GIT_LISTS.contains(&sub.as_str()) && after_sub.iter().all(|t| t.starts_with('-'));
        if GIT_READS.contains(&sub.as_str()) || listing_bare || (toks.len() == 1) {
            return false;
        }
        return true;
    }
    // alias shortforms: `%`/`foreach`/`where`/`sort`/`select`/`gc`/`type`
    // — the builtin verbs table holds the POSIX names; pwsh aliases need
    // explicit entries (lowercase compare).
    const PWSH_READS: &[&str] = &[
        "get-content",
        "gc",
        "type",
        "cat",
        "get-childitem",
        "gci",
        "ls",
        "dir",
        "get-location",
        "gl",
        "pwd",
        "echo",
        "write-output",
        "write-host",
        "select-string",
        "sls",
        "select-object",
        "select",
        "sort-object",
        "sort",
        "where-object",
        "where",
        "?",
        "foreach-object",
        "foreach",
        "%",
        "measure-object",
        "measure",
        "format-table",
        "format-list",
        "ft",
        "fl",
        "out-string",
        "test-path",
        "resolve-path",
        "compare-object",
        "diff",
        "get-process",
        "get-service",
        "get-item",
        "gi",
        "get-variable",
        "get-help",
        "help",
        "man",
        "get-command",
        "gcm",
        "get-history",
        "history",
        "get-module",
        "gmo",
        "get-member",
        "gm",
        "get-date",
        "read-host",
    ];
    if verbs.contains(&v) || PWSH_READS.contains(&v.as_str()) {
        // `gc`/`Get-Content` reading a file is fine — but `-Encoding` etc.
        // can't turn it into a write; arg-level mutation check not needed
        // for the read verb's own flags (unlike `find -exec` in POSIX).
        return false;
    }
    true
}

/// Strip single/double-quoted spans so `"a > b"` doesn't count as a
/// redirect. Backtick-escapes are kept simple — a `'` inside `"` doesn't
/// end it.
fn strip_pwsh_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), '\\') | (Some(q), '`') => {
                out.push(c);
                if let Some(n) = chars.next() {
                    out.push(n);
                }
                let _ = q;
            }
            (Some(q), ch) if ch == q => {
                quote = None;
                out.push(' ');
            }
            (Some(_), _) => out.push(' '),
            (None, '"') | (None, '\'') => {
                quote = Some(c);
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// `$x = ` / `Set-Item Env:` / `[Environment]::SetEnvironmentVariable` —
/// state writes the verb list can't see.
fn pwsh_assignment(unquoted: &str) -> bool {
    // `$name = value` — `=` inside a `$(` was already caught above; a bare
    // `=` at statement level is assignment.
    let t = unquoted.trim();
    if t.starts_with('$') && t.contains('=') {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    lower.contains("setenvironmentvariable")
        || lower.starts_with("set-item env:")
        || lower.starts_with("set-variable")
        || lower.starts_with("$env:")
        || lower.contains("= new-object")
        || lower.contains("::write")
}
