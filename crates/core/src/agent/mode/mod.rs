//! Approval modes (SPEC §4.6) — the session-level stance the dispatch gate
//! reads before deciding to ask.
//!
//! Four modes, matching the GUI's composer selector:
//!   `always_ask`  every mutating call prompts (deny rules and session
//!                 grants still short-circuit — a standing answer or a hard
//!                 veto beats the prompt)
//!   `auto`        default: permission rules + hook verdicts + the risk
//!                 classifier decide; safe reads and writes go through
//!   `read_only`   mutating calls are refused outright — reads, searches
//!                 and pure-read shell verbs pass
//!   `full_access` never asks; `deny` rules remain a hard refusal — the
//!                 one thing no mode can override
//!
//! The mode is session state (`Context.approval_mode`), durable as
//! `SessionEvent::ModeChanged` — "who switched to full access when" is an
//! auditable fact, not a memory toggle.

/// The session's approval stance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Ask before any mutating tool call.
    AlwaysAsk,
    /// Rule-driven (the default): deny/ask rules, hook verdicts and the
    /// risk-pattern table decide; the rest passes.
    #[default]
    Auto,
    /// Refuse mutations outright — reads and searches only.
    ReadOnly,
    /// Never prompt. `deny` permission rules still refuse.
    FullAccess,
}

impl ApprovalMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AlwaysAsk => "always_ask",
            Self::Auto => "auto",
            Self::ReadOnly => "read_only",
            Self::FullAccess => "full_access",
        }
    }

    /// Every spelling the frontends/ACP may send — snake ids, the GUI's
    /// Chinese labels, and the usual aliases.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "always_ask" | "always-ask" | "ask" | "请求批准" => Some(Self::AlwaysAsk),
            "auto" | "自动" => Some(Self::Auto),
            "read_only" | "read-only" | "readonly" | "ro" | "只读" => Some(Self::ReadOnly),
            "full_access" | "full-access" | "full" | "yolo" | "完全访问" => {
                Some(Self::FullAccess)
            }
            _ => None,
        }
    }

    /// The four canonical selectors, in menu order.
    pub const ALL: [Self; 4] = [
        Self::AlwaysAsk,
        Self::Auto,
        Self::ReadOnly,
        Self::FullAccess,
    ];
}

/// Read-only verbs — the Bash commands a `read_only` session still runs.
/// A project file `.sunmao/readonly-verbs.txt` extends this set (cold-plug:
/// the file IS the policy surface, same convention as risky-patterns.txt).
pub fn readonly_verbs(extra: &[String]) -> std::collections::HashSet<String> {
    let mut set: std::collections::HashSet<String> =
        include_str!("../../../assets/readonly-verbs.txt")
            .lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect();
    set.extend(extra.iter().cloned());
    set
}

/// `tool --version`/`--help`-only calls print and exit — a probe, not a
/// write. Shared by both shell dialects; the set is closed on purpose:
/// `-v` alone is verbose, not version.
pub(crate) const PROBE_FLAGS: &[&str] = &["--version", "-V", "version", "--help", "-h", "/?"];

/// Would running this tool call mutate anything outside the transcript?
/// Read-only tools (Read/Grep/Glob/WebFetch/JobOutput/JobList/SearchTools)
/// pass; everything write-shaped — files, artifacts, the task list,
/// sub-agents, a job's process tree (`JobStop`) — mutates.
/// `Bash` walks the parsed command list (or the pwsh segment scan under
/// `ShellBackend::Pwsh`). Unknown tools (MCP, ext) are mutations:
/// read-only mode must not guess at a surface it can't see.
pub fn call_mutates(
    tool: &str,
    args: &serde_json::Value,
    verbs: &std::collections::HashSet<String>,
    shell: crate::tool::ShellBackend,
) -> bool {
    match tool {
        "Read" | "Grep" | "Glob" | "WebFetch" | "JobOutput" | "JobList" | "SearchTools" => false,
        // fusion delegation is the Lead's whole point — exempting it lets
        // a read_only Lead still dispatch work to the Sidekick (the
        // mutation itself happens in the child's context, under ITS gate)
        "FusionExecute" => false,
        "Bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|cmd| {
                if shell == crate::tool::ShellBackend::Pwsh {
                    pwsh_mutates(cmd, verbs)
                } else {
                    bash_mutates(cmd, verbs)
                }
            })
            .unwrap_or(true),
        _ => true,
    }
}

/// Does this command mutate? The AST decides — output redirects (`>`, `>>`)
/// are mutations outright; every simple command's verb must be on the
/// read-only list, and a non-static verb ($VAR, `$(…)`) counts as mutating
/// because we can't see what it names. Parse failure is mutating too: we
/// never guess at structure we can't see.
pub fn bash_mutates(command: &str, verbs: &std::collections::HashSet<String>) -> bool {
    match deno_task_shell::parser::parse(command) {
        Ok(list) => list_mutates(&list, verbs),
        Err(_) => true,
    }
}

fn list_mutates(
    list: &deno_task_shell::parser::SequentialList,
    verbs: &std::collections::HashSet<String>,
) -> bool {
    list.items.iter().any(|i| seq_mutates(&i.sequence, verbs))
}

fn seq_mutates(
    seq: &deno_task_shell::parser::Sequence,
    verbs: &std::collections::HashSet<String>,
) -> bool {
    use deno_task_shell::parser::Sequence as S;
    match seq {
        S::ShellVar(_) => true, // persistent env assignment mutates shell state
        S::Pipeline(p) => pipeline_mutates(&p.inner, verbs),
        S::BooleanList(b) => seq_mutates(&b.current, verbs) || seq_mutates(&b.next, verbs),
    }
}

fn pipeline_mutates(
    inner: &deno_task_shell::parser::PipelineInner,
    verbs: &std::collections::HashSet<String>,
) -> bool {
    use deno_task_shell::parser::PipelineInner as P;
    match inner {
        P::Command(c) => command_mutates(c, verbs),
        P::PipeSequence(ps) => {
            command_mutates(&ps.current, verbs) || pipeline_mutates(&ps.next, verbs)
        }
    }
}

fn command_mutates(
    cmd: &deno_task_shell::parser::Command,
    verbs: &std::collections::HashSet<String>,
) -> bool {
    use deno_task_shell::parser::{CommandInner, RedirectOp};
    // `>`/`>>` to a file mutates; `<` reads. `>&2`-style fd merges are
    // IoFile::Fd — output to an fd isn't a filesystem write, allow it.
    if matches!(&cmd.redirect, Some(r) if matches!(r.op, RedirectOp::Output(_))
        && matches!(r.io_file, deno_task_shell::parser::IoFile::Word(_)))
    {
        return true;
    }
    match &cmd.inner {
        CommandInner::Subshell(list) => list_mutates(list, verbs),
        CommandInner::Simple(sc) => {
            let Some(verb) = sc.args.first().and_then(static_word_text) else {
                // no verb at all (bare env assignments handled by ShellVar
                // above) or a dynamic verb ($VAR/$(…)) we can't name —
                // can't prove read-only, treat as mutating.
                return true;
            };
            if !verbs.contains(verb.as_str()) && !(verb == "git" && git_reads(&sc.args)) {
                // `cargo --version` only prints — unknown verbs with
                // nothing but probe flags still count as reads
                let probe = sc.args.len() > 1
                    && sc.args.iter().skip(1).all(|w| {
                        static_word_text(w).is_some_and(|t| PROBE_FLAGS.contains(&t.as_str()))
                    });
                return !probe;
            }
            // read-shaped verbs whose FLAGS mutate: `find . -exec rm {} ;`,
            // `sort -o out`, `fd -x rm`. A non-static arg could expand into
            // a flag ($F="-exec") so it counts as mutating for these.
            if let Some((_, flags)) = MUTATING_ARGS.iter().find(|(v, _)| *v == verb) {
                let hides = |w: &deno_task_shell::parser::Word| {
                    static_word_text(w).is_none_or(|t| flags.iter().any(|f| t.starts_with(f)))
                };
                if sc.args.iter().skip(1).any(hides) {
                    return true;
                }
            }
            // `echo $(rm x)` — command substitution inside an arg runs the
            // inner list even under a read-only verb. Variables, tildes and
            // braces only expand to text.
            sc.args.iter().skip(1).any(word_mutates)
        }
    }
}

/// `git` reads — the binary covers both sides of the line (`commit`/`push`
/// write; `status`/`log`/`diff` don't), so the verb alone can't classify.
/// PURE_READ subs pass with any static args; LIST_SUBS pass only in their
/// bare listing shape (`git branch -a`, `git remote -v`) — a positional is
/// a mutation target (`git branch foo` creates). Any `--output`-style arg
/// forces write treatment (`git diff --output=patch`).
fn git_reads(args: &[deno_task_shell::parser::Word]) -> bool {
    const PURE_READ: &[&str] = &[
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
    // listing subs → the flags that make them write
    const LIST_SUBS: &[(&str, &[&str])] = &[
        (
            "branch",
            &[
                "-c",
                "-C",
                "-d",
                "-D",
                "-f",
                "-m",
                "-M",
                "-u",
                "--copy",
                "--delete",
                "--edit-description",
                "--force",
                "--move",
                "--set-upstream",
                "--unset-upstream",
            ],
        ),
        (
            "tag",
            &[
                "-a",
                "-d",
                "-e",
                "-f",
                "-m",
                "-F",
                "-s",
                "-u",
                "--annotate",
                "--delete",
                "--edit",
                "--file",
                "--force",
                "--local-user",
                "--message",
                "--sign",
            ],
        ),
        ("remote", &[]),
    ];
    const REMOTE_READ_SUBS: &[&str] = &["show", "get-url"];
    // skip `git` itself and global flags; -C/-c take a following value that
    // must not be mistaken for the subcommand
    let mut it = args.iter().skip(1).peekable();
    let sub = loop {
        let Some(w) = it.next() else {
            return false; // bare `git` — no subcommand to classify
        };
        let Some(text) = static_word_text(w) else {
            return false;
        };
        if matches!(
            text.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            it.next(); // the flag's value
            continue;
        }
        if text.starts_with('-') {
            continue;
        }
        break text;
    };
    let rest: Vec<Option<String>> = it.map(static_word_text).collect();
    if PURE_READ.contains(&sub.as_str()) {
        // a non-static arg could expand into `--output=FILE` — can't prove
        // it doesn't, so it mutates
        return rest.iter().all(|t| {
            t.as_deref()
                .map(|s| !s.starts_with("--output"))
                .unwrap_or(false)
        });
    }
    let Some((_, mut_flags)) = LIST_SUBS.iter().find(|(s, _)| *s == sub) else {
        return false;
    };
    let mut remote_sub_seen = false;
    for t in &rest {
        let Some(s) = t.as_deref() else {
            return false;
        };
        if s.starts_with('-') {
            if mut_flags.iter().any(|f| s.starts_with(f)) || s.starts_with("--output") {
                return false;
            }
        } else if sub == "remote" && !remote_sub_seen {
            remote_sub_seen = true;
            if !REMOTE_READ_SUBS.contains(&s) {
                return false; // remote add/remove/update/set-url write config
            }
        } else if sub != "remote" {
            return false; // branch/tag positional is a mutation target
        }
    }
    true
}

/// Read-shaped verbs whose flags mutate — `find . -delete`, `fd -x rm`,
/// `sort -o out` write through args the verb whitelist can't see. Prefix
/// matching: every longer form of a listed flag is mutating too
/// (`-execdir`, `--exec-batch`, `-fprintf`, `-fls`).
const MUTATING_ARGS: &[(&str, &[&str])] = &[
    ("find", &["-exec", "-ok", "-delete", "-fprint", "-fls"]),
    ("fd", &["-x", "-X", "--exec"]),
    ("sort", &["-o", "--output"]),
];

/// A word containing command substitution can hide a mutation
/// (`echo $(rm x)`). Variables/tildes/braces only expand to text.
fn word_mutates(w: &deno_task_shell::parser::Word) -> bool {
    w.parts().iter().any(|p| match p {
        deno_task_shell::parser::WordPart::Command(_) => true,
        deno_task_shell::parser::WordPart::Quoted(parts) => parts
            .iter()
            .any(|q| matches!(q, deno_task_shell::parser::WordPart::Command(_))),
        _ => false,
    })
}

/// A word's literal text, when it's fully static.
fn static_word_text(w: &deno_task_shell::parser::Word) -> Option<String> {
    use deno_task_shell::parser::WordPart as P;
    let mut out = String::new();
    for part in w.parts() {
        match part {
            P::Text(t) => out.push_str(t),
            P::Quoted(parts) => {
                for q in parts {
                    match q {
                        P::Text(t) => out.push_str(t),
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

mod pwsh;
pub(crate) use pwsh::{pwsh_mutates, pwsh_segments};

#[cfg(test)]
mod tests {
    use super::*;

    fn verbs() -> std::collections::HashSet<String> {
        readonly_verbs(&[])
    }

    #[test]
    fn mode_roundtrip() {
        for m in ApprovalMode::ALL {
            assert_eq!(ApprovalMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(
            ApprovalMode::parse("readonly"),
            Some(ApprovalMode::ReadOnly)
        );
        assert_eq!(
            ApprovalMode::parse("完全访问"),
            Some(ApprovalMode::FullAccess)
        );
        assert_eq!(ApprovalMode::parse("bogus"), None);
    }

    #[test]
    fn bash_readonly_classification() {
        let v = verbs();
        // pure reads pass
        for cmd in [
            "ls -la",
            "cat foo.rs",
            "grep -r foo src",
            "ls && pwd",
            "git status",
            "git branch",
            "git branch -a",
            "git tag -l",
            "git remote -v",
            "git remote show origin",
            "git log --oneline | head -5",
            "find . -name '*.rs'",
            "cat < input.txt",
            "echo hi >&2",
            // `x --version`/`--help` probes print and exit — unknown
            // verbs with nothing but probe flags still read
            "cargo --version",
            "node --help",
            "git -C subdir status",
        ] {
            assert!(!bash_mutates(cmd, &v), "{cmd} should be read-only");
        }
        // mutations are caught
        for cmd in [
            "rm -rf x",
            "echo hi > file.txt",
            "ls && touch f",
            "npm install",
            "sed -i s/a/b/ f.rs",
            "find . -delete",
            "find . -exec rm {} ;",
            "fd -x rm",
            "sort -o out.txt in.txt",
            "git tag -d v1",
            "git branch foo",
            "git remote add o url",
            "git diff --output=patch.diff",
            "find $DIR",
            "cat $(rm x)",
            "FOO=1",
            "export FOO=1",
            "( rm x )",
            "mkdir -p out",
        ] {
            assert!(bash_mutates(cmd, &v), "{cmd} should be mutating");
        }
        // dynamic verb: can't see what it names → mutating
        assert!(bash_mutates("$TOOL --flag", &v));
        // garbage doesn't parse → mutating
        assert!(bash_mutates(">>>", &v));
    }

    #[test]
    fn tool_classification() {
        let v = verbs();
        let posix = crate::tool::ShellBackend::Posix;
        let no_args = serde_json::json!({});
        assert!(!call_mutates("Read", &no_args, &v, posix));
        assert!(!call_mutates("Grep", &no_args, &v, posix));
        assert!(call_mutates("Write", &no_args, &v, posix));
        assert!(call_mutates("Edit", &no_args, &v, posix));
        assert!(call_mutates("Task", &no_args, &v, posix));
        assert!(call_mutates("TodoWrite", &no_args, &v, posix));
        assert!(call_mutates("HtmlArtifact", &no_args, &v, posix));
        assert!(call_mutates("mcp__x__y", &no_args, &v, posix));
        assert!(!call_mutates("SearchTools", &no_args, &v, posix));
        // job tools: listing is a read, stopping a job kills a process tree
        // — a mutation the dispatch gate must be able to refuse or prompt on
        assert!(!call_mutates("JobList", &no_args, &v, posix));
        assert!(!call_mutates("JobOutput", &no_args, &v, posix));
        assert!(call_mutates("JobStop", &no_args, &v, posix));
        assert!(!call_mutates(
            "Bash",
            &serde_json::json!({"command":"ls"}),
            &v,
            posix
        ));
        assert!(call_mutates(
            "Bash",
            &serde_json::json!({"command":"rm x"}),
            &v,
            posix
        ));
        // no command arg → can't inspect → mutating
        assert!(call_mutates("Bash", &no_args, &v, posix));
    }

    #[test]
    fn pwsh_classification() {
        let v = verbs();
        let pwsh = crate::tool::ShellBackend::Pwsh;
        let bash = |c: &str| serde_json::json!({"command": c});
        // pure reads pass
        for cmd in [
            "Get-Content foo.txt",
            "gc foo.txt",
            "ls -la",
            "Get-ChildItem src",
            "pwd",
            "echo hi",
            "Write-Host 'ok'",
            "Get-Process | Select-Object -First 5",
            "git status",
            "Get-Content a.txt; pwd",
            // git's global flags take a value — `-C` must not read as
            // the subcommand (a Lead's `git -C . status` was refused)
            "git -C . status",
            "git -C . log --oneline -3",
            "git remote -v",
            // `x --version`/`--help` probes print and exit
            "cargo --version",
            "node -h",
            // the call operator's string target classifies by basename —
            // `& "X\cargo.exe" --version` reads like `cargo --version`
            "& \"C:\\tools\\cargo.exe\" --version",
            "& \"C:\\tools\\git.exe\" status",
            // a bare variable/member evaluation writes nothing
            "$env:TEMP",
            // a read-only scriptblock body stays readable
            "Get-ChildItem src | ForEach-Object { $_.Name }",
            "@{Name='x'; Length=1}",
            // a real Lead run hit both of these: `findstr` is the Windows
            // grep (pure read, no write flags) and `Get-*` is the
            // approved-verb read contract, not an enumerable name list
            "findstr /C:needle file.txt",
            "Get-FileHash a.txt -Algorithm SHA256 | Select-Object -ExpandProperty Hash",
            "Get-Content hello.txt | Select-String -Pattern 'x' -SimpleMatch",
        ] {
            assert!(
                !call_mutates("Bash", &bash(cmd), &v, pwsh),
                "{cmd} should be read-only"
            );
        }
        // mutations caught
        for cmd in [
            "Remove-Item x",
            "Set-Content f.txt 'x'",
            "echo hi > file.txt",
            "ls; touch f",
            "New-Item -ItemType Directory out",
            "$x = 5",
            "$env:FOO = '1'",
            "ls | Tee-Object log.txt",
            "Invoke-Expression 'rm x'",
            "mkdir out",
            "cat $(rm x).txt",
            "[Environment]::SetEnvironmentVariable('A','B')",
            // read_only escapes: bare call/dot-source operators leave an
            // empty verb after stripping; a scriptblock hides its verbs
            // behind the read-aliased outer command
            "& Remove-Item x",
            ". ./evil.ps1",
            "& { rm x }",
            "Get-Item *.log | % { Remove-Item $_ }",
            "gci | ? { $_.Length -gt 0 } | % { del $_ }",
            // the call operator's dynamic/unreadable targets stay refused
            "& $cmd --version",
            "& { rm x }",
            // a mutating scriptblock body still mutates through the
            // recursive classify
            "gci | % { Remove-Item $_.FullName }",
            // a mutating git sub behind a global flag is still write
            "git -C . push",
            "git stash drop",
            // a member CALL on a variable isn't a bare member read
            "$fs.Write('x')",
        ] {
            assert!(
                call_mutates("Bash", &bash(cmd), &v, pwsh),
                "{cmd} should be mutating"
            );
        }
        // quoted `>` is text, not a redirect
        assert!(!call_mutates("Bash", &bash("echo 'a > b'"), &v, pwsh));
        // a pipeline segment mutating makes the whole call mutating
        assert!(call_mutates("Bash", &bash("ls | rm"), &v, pwsh));
    }
}
