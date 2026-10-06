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

/// Split a pwsh line into command pieces on `;`, `|`, `&&`, `||`, and
/// top-level newlines — pwsh executes `\n`-separated statements, so a
/// "line" is really a script. Quote/backtick aware; `@"…"@`/`@'…'@`
/// verbatim strings keep their inner newlines. `$(`/`(`/`{` depth keeps
/// pipes and newlines inside subexpressions and scriptblocks from
/// splitting.
pub fn pwsh_segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;
    // `@"…"@` verbatim mode: literal contents, no escapes, closes on `"@`
    let mut verbatim = false;
    let mut depth = 0i32;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(_), '\\') | (Some(_), '`') if !verbatim => {
                cur.push(c);
                let _ = chars.next().map(|n| cur.push(n));
            }
            (Some(q), ch) if ch == q => {
                cur.push(c);
                if verbatim {
                    if chars.peek() == Some(&'@') {
                        cur.push(chars.next().unwrap());
                        quote = None;
                        verbatim = false;
                    }
                } else {
                    quote = None;
                }
            }
            (Some(_), ch) => cur.push(ch),
            (None, '@') if matches!(chars.peek(), Some('"') | Some('\'')) => {
                cur.push(c);
                let q = chars.next().unwrap();
                cur.push(q);
                quote = Some(q);
                verbatim = true;
            }
            (None, '"') | (None, '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, '\n') if depth == 0 => {
                let s = cur.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                cur.clear();
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
    // `{…}` — a bare block is a scriptblock: its inner statements classify
    // recursively instead of blanket-refusing, so `… | % { $_.Name }`
    // reads while `% { Remove-Item x }` still writes. `@{…}` hashtables
    // are data literals — a block inside one only executes if an operator
    // outside invokes it — so their content is blanked, not classified.
    // Unbalanced braces stay mutating: never guess at structure we can't
    // see.
    let Some(spans) = brace_spans(&unquoted) else {
        return true;
    };
    let mut clean = unquoted.clone();
    for (lo, hi, data) in spans {
        if !data
            && pwsh_segments(&unquoted[lo + 1..hi - 1])
                .iter()
                .any(|n| pwsh_segment_mutates(n, verbs))
        {
            return true;
        }
        clean.replace_range(lo..hi, &" ".repeat(hi - lo));
    }
    // `$x = …`, `Set-Item Env:foo`, `[Environment]::Set…` — persistent
    // state writes, same as ShellVar mutation in the POSIX walker.
    if pwsh_assignment(&clean) {
        return true;
    }
    // a lone `$x`/`$env:PATH` or a comma list of member reads
    // (`$_.Name, $_.Length`) evaluates values — nothing runs. A `(` marks
    // a method call (`$fs.Write(…)`) — those can mutate and stay out.
    if !clean.is_empty()
        && clean
            .split_whitespace()
            .all(|t| t == "," || (t.starts_with('$') && !t.contains('(')))
    {
        return false;
    }
    // `& cmd`/`. script` — call/sourcing operators. The invoked name is a
    // string we CAN classify (`& "X\git.exe" status` reads like `git
    // status`): take it from the original text — the quoted span was
    // blanked out of `unquoted` — and run the same verb rules on its
    // basename. A missing or dynamic target can't be named → mutating.
    let first = clean.split_whitespace().next().unwrap_or("");
    let (verb, rest): (String, Vec<String>) = if first == "&" || first == "." {
        match call_target(s) {
            Some(pair) => pair,
            None => return true,
        }
    } else {
        (
            first.to_string(),
            clean
                .split_whitespace()
                .skip(1)
                .map(str::to_string)
                .collect(),
        )
    };
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
    // `tool --version` / `node -h` prints and exits — a probe, not a
    // write. The flag set is closed on purpose: `-v` alone is verbose.
    if !rest.is_empty()
        && rest
            .iter()
            .all(|t| super::PROBE_FLAGS.contains(&t.as_str()))
    {
        return false;
    }
    // `git` subcommand semantics are dialect-independent — `git status`
    // reads under pwsh exactly as under bash. Mirror the POSIX walker's
    // read/lists split, including its global-flag skip so `git -C dir
    // status` classifies `status`, not `-C`.
    if v == "git" {
        return git_rest_mutates(&rest);
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
        "join-path",
        "split-path",
        "out-null",
        "convertfrom-json",
        "convertto-json",
        "get-itemproperty",
        "get-executionpolicy",
    ];
    if verbs.contains(&v) || PWSH_READS.contains(&v.as_str()) {
        // `gc`/`Get-Content` reading a file is fine — but `-Encoding` etc.
        // can't turn it into a write; arg-level mutation check not needed
        // for the read verb's own flags (unlike `find -exec` in POSIX).
        return false;
    }
    true
}

/// Balanced `{…}` spans on already-stripped text — `(lo, hi, is_data)`
/// byte offsets of each TOP-LEVEL brace pair. `@{…}` hashtables are data
/// literals (`is_data = true`): a scriptblock inside one only runs if an
/// operator outside the literal invokes it, so its content is blanked
/// rather than classified. A bare `{…}` is a scriptblock — the caller
/// recurses into its inner statements. `None` on unbalanced braces: never
/// guess at structure we can't see.
fn brace_spans(s: &str) -> Option<Vec<(usize, usize, bool)>> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, bool)> = Vec::new();
    for (i, b) in s.char_indices() {
        match b {
            '{' => {
                // `@{` opens a hashtable literal — the `@` belongs to the
                // data span too, else it survives blanking as a stray verb
                let at = s[..i].trim_end();
                let (lo, data) = if at.ends_with('@') {
                    (at.len() - 1, true)
                } else {
                    (i, false)
                };
                stack.push((lo, data));
            }
            '}' => match stack.pop() {
                Some((lo, data)) if stack.is_empty() => out.push((lo, i + 1, data)),
                Some(_) => {}
                None => return None,
            },
            _ => {}
        }
    }
    stack.is_empty().then_some(out)
}

/// `& "C:\tools\cargo.exe" --version` — the call/sourcing operator's
/// target is a quoted or bare word on the ORIGINAL segment text (quoted
/// names were blanked out of `unquoted`). Returns the target's basename
/// verb and the remaining arg tokens; `None` when the target is missing
/// or dynamic (`& $x`, `& {…}`) — the invoked name can't be proven.
fn call_target(s: &str) -> Option<(String, Vec<String>)> {
    let rest = s.trim_start()[1..].trim_start();
    let (name, tail) = match rest.chars().next()? {
        q @ ('"' | '\'') => {
            let close = rest[1..].find(q)? + 1;
            (&rest[1..close], &rest[close + 1..])
        }
        _ => match rest.find(char::is_whitespace) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        },
    };
    if name.is_empty() || name.starts_with(['$', '(', '{']) {
        return None;
    }
    Some((
        basename_verb(name),
        tail.split_whitespace().map(str::to_string).collect(),
    ))
}

/// `C:\tools\git.exe` → `git`; `pwsh` stays `pwsh` — basename minus the
/// executable suffixes, lowercased.
fn basename_verb(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_end_matches(['"', '\'']);
    let lower = base.to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".ps1"] {
        if let Some(stem) = lower.strip_suffix(ext) {
            return stem.to_string();
        }
    }
    lower
}

/// `git`'s subcommand decides read vs write — same split as the POSIX
/// walker's `git_reads`: pure-read subs pass (a `--output` arg writes a
/// patch), listing subs pass only in bare flag-listing shape, and global
/// flags (`-C dir`, `-c k=v`) take a value that must not be mistaken for
/// the subcommand.
fn git_rest_mutates(rest: &[String]) -> bool {
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
    let mut it = rest.iter().peekable();
    let sub = loop {
        let Some(t) = it.next() else {
            return false; // bare `git` — nothing to classify
        };
        let tl = t.to_ascii_lowercase();
        if matches!(
            tl.as_str(),
            "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            it.next(); // the flag's value
            continue;
        }
        if tl.starts_with('-') {
            continue;
        }
        break tl;
    };
    let after: Vec<&String> = it.collect();
    if GIT_READS.contains(&sub.as_str()) {
        return after.iter().any(|t| t.starts_with("--output"));
    }
    if GIT_LISTS.contains(&sub.as_str()) {
        // bare listing shape only — a positional is a mutation target
        return after.iter().any(|t| !t.starts_with('-'));
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
    // `$env:X = v` is caught by the `$`-and-`=` rule above — a bare
    // `$env:TEMP` read is not an assignment and must not match here
    lower.contains("setenvironmentvariable")
        || lower.starts_with("set-item env:")
        || lower.starts_with("set-variable")
        || lower.contains("= new-object")
        || lower.contains("::write")
}
