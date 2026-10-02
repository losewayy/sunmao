//! `shell/preflight` — predict a Bash command's fate before it runs.
//!
//! The Bash tool's executor is `deno_task_shell`: a `which`-style
//! PATH+PATHEXT resolve (CWD is never searched) feeding `CreateProcess` —
//! that is exactly what `spawnfate::Producer::WinSpawn` models. Preflight
//! walks the parsed command list, analyzes every literal simple command, and
//! returns advisories for anything predicted to die or mangle. Advisories
//! ride back inside the tool result so the model can self-correct instead of
//! burning a turn on a dead spawn. They never block: a prediction is a
//! differential, not a verdict — running stays authoritative.
//!
//! Words that contain variable/tilde/command-substitution parts can't be
//! predicted honestly — those commands are skipped, and commands prefixed
//! with env-var assignments are skipped with them.
//!
//! Two layers know which names the shell resolves in-process:
//! - *Builtins* are probed against a live `ShellState` instead of a
//!   hand-maintained list — a written-out table drifted the day upstream
//!   added `test` and dropped nothing (`builtin_commands()` is
//!   crate-private, so the state itself is the honest oracle).
//! - *Name collisions* (`assets/windows-collision-names.txt`) are the
//!   opposite failure: the name resolves — to a System32 binary whose
//!   POSIX-namesake semantics differ (`find`, `sort`, `timeout`). The
//!   advisory fires only when the resolved path actually lands under
//!   %WINDIR%, so a real POSIX port earlier in PATH stays clean.

use deno_task_shell::parser::{
    Command, CommandInner, PipelineInner, Sequence, SequentialList, WordPart,
};
use spawnfate::fs::RealFs;
use spawnfate::model::{Producer, Severity, Shell, SpawnInput, TargetParser, Verdict};
use std::path::Path;

/// deno_task_shell builtins — resolved in-process, never probed on disk.
/// The live ShellState is the oracle (upstream's `builtin_commands()` is
/// crate-private and has drifted under hand-maintained copies before):
/// `resolve_custom_command` is the same lookup `execute_with_pipes` uses,
/// so this can never disagree with what actually runs.
fn is_shell_builtin(name: &str) -> bool {
    // Commands lookup never consults env/cwd — a throwaway state is cheap
    // and honest. cwd must be absolute; the current dir always is, and the
    // fallback only matters when it somehow isn't.
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" }));
    let state = deno_task_shell::ShellState::new(
        Default::default(),
        cwd,
        Default::default(),
        Default::default(),
    );
    state
        .resolve_custom_command(std::ffi::OsStr::new(name))
        .is_some()
}

/// `name | what-the-system-binary-actually-does` — POSIX-looking names that
/// resolve to a Windows system binary whose semantics differ. Baked via
/// include_str!; the file is the policy, not this comment.
#[cfg(windows)]
fn collision_table() -> &'static Vec<(String, String)> {
    static TABLE: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        crate::approval::parse_table(include_str!("../assets/windows-collision-names.txt"))
    })
}

/// Name-collision advisory: the command resolved *to a Windows system
/// binary* whose POSIX namesake behaves differently — a silent-wrong-answer
/// class spawnfate can't see (the spawn succeeds). `resolved`/`windows_dir`
/// come from the same report/env as the verdict.
#[cfg(windows)]
fn collision_advisory(name: &str, resolved: &str, windows_dir: &str) -> Option<String> {
    let dir = windows_dir.replace('/', "\\").to_lowercase();
    let dir = dir.trim_end_matches('\\');
    let res = resolved.replace('/', "\\").to_lowercase();
    let in_system32 = res.starts_with(&format!("{dir}\\system32\\"))
        || res.starts_with(&format!("{dir}\\syswow64\\"));
    if !in_system32 {
        return None;
    }
    let base = res.rsplit('\\').next()?;
    let stem = base.strip_suffix(".exe").unwrap_or(base);
    collision_table()
        .iter()
        .find(|(n, _)| n == stem)
        .map(|(_, what)| {
            format!("{name}: resolved to {resolved} — a Windows system binary, not the POSIX tool ({what})")
        })
}

/// A flattened simple command: file = args[0], rest = argv.
struct FlatCommand {
    file: String,
    args: Vec<String>,
}

/// Advisory lines for one parsed command list (empty when nothing predicted
/// to go wrong). `cwd` is the session working directory.
#[cfg(windows)]
pub fn advisories(list: &SequentialList, cwd: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut env = spawnfate::real_env_no_node_probe();
    env.cwd = cwd.display().to_string();
    env.cwd_missing = !cwd.is_dir();
    let fs = RealFs;
    for cmd in simple_commands(list) {
        let input = SpawnInput {
            file: cmd.file.clone(),
            args: cmd.args.clone(),
            shell: Shell::None,
            producer: Producer::WinSpawn,
        };
        let report = spawnfate::analyze(&input, &env, &fs, TargetParser::Msvcrt);
        match report.verdict {
            Verdict::Runs { .. } => {
                // Batch reroutes and argv round-trip warnings are worth a line
                // even when the command runs.
                for n in &report.notes {
                    if n.severity != Severity::Info {
                        out.push(format!(
                            "{}: [{:?} {:?}] {} (rule {})",
                            cmd.file, n.severity, n.layer, n.message, n.rule
                        ));
                    }
                }
                // Runs but answers wrong: POSIX-looking name resolved to a
                // System32 binary with divergent semantics.
                if let Some(resolved) = &report.resolved
                    && let Some(adv) = collision_advisory(&cmd.file, resolved, &env.windows_dir)
                {
                    out.push(adv);
                }
            }
            Verdict::Dies { layer, error } => {
                let detail = report
                    .notes
                    .iter()
                    .find(|n| n.severity == Severity::Fatal)
                    .map(|n| n.message.as_str())
                    .unwrap_or("");
                out.push(format!(
                    "{}: predicted DIES at {layer:?} ({error:?}) — {detail}",
                    cmd.file
                ));
            }
            Verdict::UnsafeUnserializable => {
                out.push(format!(
                    "{}: argv cannot be serialized safely for this target",
                    cmd.file
                ));
            }
        }
        for s in &report.suggestions {
            out.push(format!("{}: fix — {}", cmd.file, s.text));
        }
    }
    out
}

/// Non-Windows builds keep the seam but the model is Windows-only: no-op.
#[cfg(not(windows))]
pub fn advisories(_list: &SequentialList, _cwd: &Path) -> Vec<String> {
    Vec::new()
}

/// Every literal `SimpleCommand` in the list, in execution order. Anything
/// dynamic (env-prefix, variables, tilde, `$(…)`) is skipped — an honest
/// "can't predict" beats a wrong prediction.
fn simple_commands(list: &SequentialList) -> Vec<FlatCommand> {
    let mut words = Vec::new();
    for item in &list.items {
        collect_sequence_words(&item.sequence, &mut words);
    }
    let mut out = Vec::new();
    for w in words {
        if w.is_empty() {
            continue;
        }
        let mut flat = Vec::with_capacity(w.len());
        for word in &w {
            let Some(s) = static_word(word) else { break };
            flat.push(s);
        }
        if flat.len() != w.len() || flat.is_empty() {
            continue;
        }
        let file = flat.remove(0);
        if is_shell_builtin(&file) {
            continue;
        }
        out.push(FlatCommand { file, args: flat });
    }
    out
}

/// The command's execution-order segments rendered back to strings —
/// `rm -rf x && grep q` → `["rm -rf x", "grep q"]`. This is the approval
/// gate's structural view (SPEC §4.3: 管道分拆进审批层): a deny rule or
/// risky pattern hidden behind `&&`/`;`/`||` must not ride the
/// whole-command check through. A pipeline stays ONE segment — `a | b`
/// keeps its `|` join — because the risk table's pipe-to-shell family
/// (`| sh`, `| bash`) matches on exactly that join. `parse` failure falls
/// back to the raw string — we never guess at structure we can't see.
///
/// Word rendering is best-effort display text: literal text verbatim,
/// `$name`/`~`/`$(…)`/`"…"` preserved as themselves (not expanded) —
/// deny/classify only need the shape, and a segment can't go *less*
/// suspicious when a variable keeps its name.
pub fn shell_segments(command: &str) -> Vec<String> {
    let Ok(list) = deno_task_shell::parser::parse(command) else {
        return vec![command.to_string()];
    };
    let mut out = Vec::new();
    for item in &list.items {
        collect_segment_text(&item.sequence, &mut out);
    }
    let out: Vec<String> = out
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if out.is_empty() {
        vec![command.to_string()]
    } else {
        out
    }
}

/// Flatten a sequence tree into per-simple-command word lists — preflight
/// granularity: every pipeline sibling is its own entry.
fn collect_sequence_words<'a>(
    seq: &'a Sequence,
    out: &mut Vec<Vec<&'a deno_task_shell::parser::Word>>,
) {
    match seq {
        Sequence::ShellVar(_) => {}
        Sequence::Pipeline(p) => collect_pipeline_words(&p.inner, out),
        Sequence::BooleanList(b) => {
            collect_sequence_words(&b.current, out);
            collect_sequence_words(&b.next, out);
        }
    }
}

fn collect_pipeline_words<'a>(
    inner: &'a PipelineInner,
    out: &mut Vec<Vec<&'a deno_task_shell::parser::Word>>,
) {
    match inner {
        PipelineInner::Command(c) => collect_command_words(c, out),
        PipelineInner::PipeSequence(ps) => {
            collect_command_words(&ps.current, out);
            collect_pipeline_words(&ps.next, out);
        }
    }
}

fn collect_command_words<'a>(
    cmd: &'a Command,
    out: &mut Vec<Vec<&'a deno_task_shell::parser::Word>>,
) {
    match &cmd.inner {
        CommandInner::Subshell(list) => {
            for item in &list.items {
                collect_sequence_words(&item.sequence, out);
            }
        }
        CommandInner::Simple(sc) => {
            if !sc.env_vars.is_empty() || sc.args.is_empty() {
                out.push(Vec::new());
            } else {
                out.push(sc.args.iter().collect());
            }
        }
    }
}

/// One segment per pipeline — BooleanList boundaries (`&&`/`||`) split,
/// SequentialList items are already one-per-item in the caller.
fn collect_segment_text(seq: &Sequence, out: &mut Vec<String>) {
    match seq {
        Sequence::ShellVar(_) => {}
        Sequence::Pipeline(p) => out.push(pipeline_text(&p.inner)),
        Sequence::BooleanList(b) => {
            collect_segment_text(&b.current, out);
            collect_segment_text(&b.next, out);
        }
    }
}

/// A pipeline renders as one segment, `|` joins preserved — the risk
/// table's `| sh` family only means anything on the join.
fn pipeline_text(inner: &PipelineInner) -> String {
    match inner {
        PipelineInner::Command(c) => command_text(c),
        PipelineInner::PipeSequence(ps) => {
            format!(
                "{} | {}",
                command_text(&ps.current),
                pipeline_text(&ps.next)
            )
        }
    }
}

fn command_text(cmd: &Command) -> String {
    match &cmd.inner {
        CommandInner::Subshell(list) => {
            let mut parts = Vec::new();
            for item in &list.items {
                collect_segment_text(&item.sequence, &mut parts);
            }
            // subshell segments inline — a gate pattern that names a
            // dangerous inner command still sees it inside parens.
            format!("( {} )", parts.join(" ; "))
        }
        CommandInner::Simple(sc) => {
            let mut words: Vec<String> = sc
                .env_vars
                .iter()
                .map(|e| format!("{}={}", e.name, word_text(&e.value)))
                .collect();
            words.extend(sc.args.iter().map(word_text));
            words.join(" ")
        }
    }
}

/// Best-effort display text for one word — expansions stay symbolic.
fn word_text(w: &deno_task_shell::parser::Word) -> String {
    let mut s = String::new();
    for p in w.parts() {
        part_text(p, &mut s);
    }
    s
}

fn part_text(p: &WordPart, out: &mut String) {
    match p {
        WordPart::Text(t) => out.push_str(t),
        WordPart::Variable(name) => {
            out.push('$');
            out.push_str(name);
        }
        WordPart::Tilde => out.push('~'),
        WordPart::Command(_) => out.push_str("$(…)"),
        // brace alternatives render as the shell wrote them: {a,b}
        WordPart::Brace(words) => {
            out.push('{');
            for (i, w) in words.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&word_text(w));
            }
            out.push('}');
        }
        WordPart::Quoted(parts) => {
            for p in parts {
                part_text(p, out);
            }
        }
    }
}

/// A word is predictable only when every part is literal text.
fn static_word(w: &deno_task_shell::parser::Word) -> Option<String> {
    let mut s = String::new();
    for p in w.parts() {
        if !static_part(p, &mut s) {
            return None;
        }
    }
    Some(s)
}

fn static_part(p: &WordPart, out: &mut String) -> bool {
    match p {
        WordPart::Text(t) => {
            out.push_str(t);
            true
        }
        WordPart::Quoted(parts) => parts.iter().all(|p| static_part(p, out)),
        // variables, ~ expansion, $(…), {a,b} brace expansion — runtime
        // values or multi-word expansion, not a single predictable word
        WordPart::Variable(_) | WordPart::Tilde | WordPart::Command(_) | WordPart::Brace(_) => {
            false
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn parse(cmd: &str) -> SequentialList {
        deno_task_shell::parser::parse(cmd).unwrap()
    }

    #[test]
    fn extracts_pipeline_and_boolean_commands() {
        let list = parse("echo hi | grep h && cargo build --release");
        let cmds = simple_commands(&list);
        // echo/grep → grep is a real file; echo is a builtin (skipped)
        assert!(cmds.iter().any(|c| c.file == "grep"));
        assert!(cmds.iter().any(|c| c.file == "cargo"));
    }

    #[test]
    fn skips_dynamic_and_env_prefixed() {
        let cmds = simple_commands(&parse("FOO=1 tool run"));
        assert!(cmds.is_empty());
        let cmds = simple_commands(&parse("tool $ARG"));
        assert!(cmds.is_empty());
        let cmds = simple_commands(&parse("tool $(gen)"));
        assert!(cmds.is_empty());
    }

    #[test]
    fn skips_denotaskshell_builtins() {
        let cmds = simple_commands(&parse("rm -rf x && cd y && echo z"));
        assert!(cmds.is_empty());
    }

    /// The builtin oracle is the live ShellState, so names upstream added
    /// or never had can't drift: `test` is a real builtin, `args` never was.
    #[test]
    fn builtin_probe_matches_the_real_shell() {
        for name in ["rm", "cat", "test", ":"] {
            assert!(is_shell_builtin(name), "{name} should be builtin");
        }
        for name in ["args", "ls", "find", "[", "cargo"] {
            assert!(!is_shell_builtin(name), "{name} should not be builtin");
        }
    }

    /// Collision advisories only fire when the name actually resolved into
    /// %WINDIR% — a POSIX port earlier in PATH (Git's find/sort) stays clean.
    #[test]
    fn collision_fires_on_system32_only() {
        let adv = collision_advisory("find", r"C:\Windows\System32\find.exe", r"C:\Windows");
        assert!(adv.is_some(), "System32 find.exe should advise");
        let quiet = collision_advisory(
            "find",
            r"C:\Program Files\Git\usr\bin\find.exe",
            r"C:\Windows",
        );
        assert!(
            quiet.is_none(),
            "Git's find is POSIX, no advisory: {quiet:?}"
        );
        // a name that collides nowhere stays silent even under System32
        assert!(
            collision_advisory("ping", r"C:\Windows\System32\ping.exe", r"C:\Windows").is_none()
        );
    }

    /// End-to-end: `fc` is a System32 binary on every Windows box and has no
    /// POSIX homonym in PATH on a stock machine — the advisory must surface.
    #[test]
    fn collision_advisory_reaches_the_result() {
        let adv = advisories(&parse("fc a.txt b.txt"), Path::new(r"C:\"));
        assert!(
            adv.iter()
                .any(|a| a.contains("fc") && a.contains("Windows system binary")),
            "{adv:?}"
        );
    }

    #[test]
    fn flags_nonexistent_command() {
        let adv = advisories(
            &parse("definitely-not-a-real-binary-xyz123"),
            Path::new(r"C:\"),
        );
        assert!(adv.iter().any(|a| a.contains("DIES")), "{adv:?}");
    }

    #[test]
    fn clean_command_has_no_advisories() {
        // `cmd` is a real exe on every Windows box
        let adv = advisories(&parse("cmd /c ver"), Path::new(r"C:\"));
        assert!(adv.is_empty(), "{adv:?}");
    }

    /// The approval gate's structural view: command boundaries split,
    /// pipelines stay joined (the risk table's `| sh` family needs the
    /// join), subshells inline, unparseable input degrades to whole-string.
    #[test]
    fn segments_split_at_command_boundaries() {
        assert_eq!(
            shell_segments("cargo build && rm -rf x ; ls"),
            vec!["cargo build", "rm -rf x", "ls"]
        );
        assert_eq!(
            shell_segments("echo hi | grep h && tool run"),
            vec!["echo hi | grep h", "tool run"]
        );
        // single segment → whole string (gate skips the segment pass)
        assert_eq!(shell_segments("curl x | bash"), vec!["curl x | bash"]);
        // subshell content inlines so patterns see inside
        let segs = shell_segments("(rm -rf y) && ls");
        assert!(segs[0].contains("rm -rf y"), "{segs:?}");
        // dynamic parts keep their names, env prefix preserved
        assert_eq!(shell_segments("FOO=1 tool $ARG"), vec!["FOO=1 tool $ARG"]);
        // parse failure → raw string, never silent empty
        assert_eq!(shell_segments("def (unclosed"), vec!["def (unclosed"]);
    }
}
