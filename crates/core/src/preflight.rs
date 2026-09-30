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
//! with env-var assignments are skipped with them. deno_task_shell builtins
//! never touch the filesystem, so they are exempt by name.

use deno_task_shell::parser::{
    Command, CommandInner, PipelineInner, Sequence, SequentialList, WordPart,
};
use spawnfate::fs::RealFs;
use spawnfate::model::{Producer, Severity, Shell, SpawnInput, TargetParser, Verdict};
use std::path::Path;

/// deno_task_shell builtins — resolved in-process, never probed on disk.
/// (`builtin_commands()` is crate-private upstream; keep the names in sync
/// with shell/commands/mod.rs.)
const DENO_BUILTINS: &[&str] = &[
    "args", "cat", "cd", "cp", "echo", "exit", "export", "false", "head", "mkdir", "mv", "pwd",
    "rm", "set", "shopt", "sleep", "true", "unset", "xargs",
];

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
        if DENO_BUILTINS.contains(&file.as_str()) {
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
        // variables, ~ expansion, $(…) — runtime values, not predictable
        WordPart::Variable(_) | WordPart::Tilde | WordPart::Command(_) => false,
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
