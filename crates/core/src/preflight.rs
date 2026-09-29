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
    Command, CommandInner, PipeSequence, PipelineInner, Sequence, SequentialList, WordPart,
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
    let mut out = Vec::new();
    for item in &list.items {
        collect_sequence(&item.sequence, &mut out);
    }
    out
}

fn collect_sequence(seq: &Sequence, out: &mut Vec<FlatCommand>) {
    match seq {
        Sequence::ShellVar(_) => {}
        Sequence::Pipeline(p) => collect_pipeline_inner(&p.inner, out),
        Sequence::BooleanList(b) => {
            collect_sequence(&b.current, out);
            collect_sequence(&b.next, out);
        }
    }
}

fn collect_pipeline_inner(inner: &PipelineInner, out: &mut Vec<FlatCommand>) {
    match inner {
        PipelineInner::Command(c) => collect_command(c, out),
        PipelineInner::PipeSequence(ps) => {
            collect_pipe_sequence(ps, out);
        }
    }
}

fn collect_pipe_sequence(ps: &PipeSequence, out: &mut Vec<FlatCommand>) {
    collect_command(&ps.current, out);
    collect_pipeline_inner(&ps.next, out);
}

fn collect_command(cmd: &Command, out: &mut Vec<FlatCommand>) {
    match &cmd.inner {
        CommandInner::Subshell(list) => {
            for item in &list.items {
                collect_sequence(&item.sequence, out);
            }
        }
        CommandInner::Simple(sc) => {
            // `FOO=1 cmd` — env prefix makes the call's env differ; skip the
            // whole command rather than predict under wrong assumptions.
            if !sc.env_vars.is_empty() || sc.args.is_empty() {
                return;
            }
            let mut flat = Vec::with_capacity(sc.args.len());
            for w in &sc.args {
                let Some(s) = static_word(w) else { return };
                flat.push(s);
            }
            let file = flat.remove(0);
            if DENO_BUILTINS.contains(&file.as_str()) {
                return;
            }
            out.push(FlatCommand { file, args: flat });
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
}
