//! Golden test: one `SessionEvent` stream folded by the TUI (`App::replay`)
//! and by the serve GUI (`renderReplay` in index.html, driven headlessly by
//! `serve/replay_parity.mjs`) must produce the same canonical transcript.
//! A diff here means the frontends diverged — fix the drift, not the format.
//!
//! Canonical line grammar (mirrored 1:1 by the mjs emitter):
//!   `user|<text>`            prompt the user typed
//!   `assistant|<text>`       markdown-rendered text, stripped to plain
//!   `tool|<d>|<name>|<N>x<state>[,<state>…]`   ok | err | interrupted;
//!                            `!` normalizes to `shell`, `↳ ` prefixes strip
//!   `artifact|<name>`        produced artifact
//!   `compacted`              compaction boundary — both sides drop history
//!   `task_done|<id>|<ok|fail>`
//!   `hook|<event>`           audit row ↔ EVLOG entry, interleaved by position
//!   `note|<text>`            anything else rendered as a note line
//!   `step_summary|<n>`       TUI fold-by-cap — emitted so a surprise fold
//!                            diffs loudly instead of being silently skipped
//!
//! Whitespace is collapsed (`\s+` → single space) on both sides: the
//! transcript compares content, not layout.

use std::path::PathBuf;
use std::process::Command;

use sunmao_core::SessionEvent as E;
use sunmao_llm::types::{FunctionCall, Message, ToolCall, Usage};

use super::app::App;
use super::blocks::{BlockKind, ToolBlock};

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: FunctionCall {
            name: name.into(),
            arguments: args.into(),
        },
    }
}
fn tc(c: ToolCall, depth: u8, lane: u8) -> E {
    E::ToolCall {
        call: c,
        depth,
        lane,
    }
}
fn tr(id: &str, name: &str, ok: bool, out: &str, depth: u8, lane: u8) -> E {
    E::ToolResult {
        call_id: id.into(),
        name: name.into(),
        ok,
        output: out.into(),
        depth,
        lane,
    }
}

/// One stream exercising every transcript-visible event kind, in both
/// pre- and post-compaction phases. Keep each turn's step count ≤ the
/// TURN_CAP in replay.rs so fold-by-cap stays out of the comparison.
fn fixture() -> Vec<E> {
    use sunmao_core::tool::TodoItem;
    vec![
        E::Started {
            model: "test-model".into(),
            cwd: "proj".into(),
        },
        E::Message {
            message: Message::system("identity block"),
        },
        // turn 1: prose + grouped tool runs + error + hooks + a dangling call
        E::Message {
            message: Message::user("read the readme and summarize it"),
        },
        E::Message {
            message: Message::assistant(Some("I will read the file first.".to_string()), vec![]),
        },
        tc(call("c1", "Read", r#"{"path":"README.md"}"#), 0, 0),
        tr("c1", "Read", true, "file contents here", 0, 0),
        tc(call("c2", "Read", r#"{"path":"src/lib.rs"}"#), 0, 0),
        tr("c2", "Read", true, "lib contents", 0, 0),
        tc(call("c3", "Bash", r#"{"command":"false"}"#), 0, 0),
        tr("c3", "Bash", false, "exit status 1", 0, 0),
        E::Hook {
            event: "PreToolUse.block".into(),
            detail: "Write: out.txt: denied".into(),
        },
        tc(call("c4", "Write", r#"{"path":"out.txt"}"#), 0, 0),
        tr("c4", "Write", false, "blocked by hook", 0, 0),
        E::Hook {
            event: "approval.session".into(),
            detail: "Bash: cargo *".into(),
        },
        tc(call("c5", "Grep", r#"{"pattern":"todo"}"#), 0, 0), // orphaned → cleared away
        E::Message {
            message: Message::assistant(Some("done with that.".to_string()), vec![]),
        },
        E::Todos {
            items: vec![
                TodoItem {
                    content: "audit hooks".into(),
                    status: sunmao_core::tool::TodoStatus::Pending,
                },
                TodoItem {
                    content: "write report".into(),
                    status: sunmao_core::tool::TodoStatus::Done,
                },
            ],
        },
        E::Usage {
            usage: Usage {
                prompt_tokens: 1200,
                completion_tokens: 300,
                total_tokens: 1500,
                cache_read_input_tokens: 50,
                cache_creation_input_tokens: 0,
            },
        },
        // compaction boundary — everything above must vanish on both sides
        E::Compacted {
            summary: "earlier turns condensed".into(),
        },
        // turn 2: sub-agent lane, folded message variants, shells, tasks,
        // artifact, stray result, fresh dangling call
        E::Message {
            message: Message::user("now check the src dir"),
        },
        E::Message {
            message: Message::assistant(Some("Listing it.".to_string()), vec![]),
        },
        tc(call("c6", "Glob", r#"{"pattern":"*.rs"}"#), 1, 2),
        tr("c6", "Glob", true, "src/lib.rs", 1, 2),
        E::Message {
            message: Message::user("[hook context] hook says hi"),
        },
        E::Message {
            message: Message::user("<local-shell>\n$ ls\na.txt\n[exit 0]\n</local-shell>"),
        },
        E::LocalShell {
            command: "ls".into(),
            exit_code: 0,
            output: "a.txt".into(),
        },
        E::Hook {
            event: "approval.deny".into(),
            detail: "Write: tmp.txt".into(),
        },
        E::ModeChange {
            mode: sunmao_core::agent::ApprovalMode::ReadOnly,
        },
        E::TaskDone {
            id: "bg-1".into(),
            ok: true,
            output: "sub-agent finished".into(),
        },
        E::TaskDone {
            id: "bg-2".into(),
            ok: false,
            output: "sub-agent crashed".into(),
        },
        E::Artifact {
            name: "report".into(),
            path: ".sunmao/artifacts/report.html".into(),
            bytes: 2048,
            rev: 2,
        },
        // a second same-name pair — post-compaction so the verb-grouping
        // case survives into the canonical transcript
        tc(call("c8", "Read", r#"{"path":"a.txt"}"#), 0, 0),
        tr("c8", "Read", true, "a", 0, 0),
        tc(call("c9", "Read", r#"{"path":"b.txt"}"#), 0, 0),
        tr("c9", "Read", true, "b", 0, 0),
        tr("zz", "Write", true, "written", 0, 0), // stray result, no call
        tc(call("c7", "Grep", r#"{"pattern":"end"}"#), 0, 0), // dangling → interrupted
        E::Message {
            message: Message::assistant(Some("all done.".to_string()), vec![]),
        },
    ]
}

fn flat(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tool_state(t: &ToolBlock) -> &'static str {
    match t.done {
        Some(true) => "ok",
        // orphaned calls are closed with the "interrupted" marker output
        Some(false) if t.output.ends_with("interrupted") => "interrupted",
        Some(false) => "err",
        None => "interrupted",
    }
}

type ToolRun = (u8, String, usize, Vec<String>);

fn flush_tool(out: &mut Vec<String>, run: &mut Option<ToolRun>) {
    let Some((depth, name, count, states)) = run.take() else {
        return;
    };
    let uniform = states.iter().all(|s| *s == states[0]);
    let states = if uniform {
        states[0].clone()
    } else {
        states.join(",")
    };
    out.push(format!("tool|{depth}|{name}|{count}x{states}"));
}

fn note_line(text: &str) -> Option<String> {
    let flat = flat(text);
    if flat.starts_with("sunmao TUI") {
        return None; // session banner — chrome, not transcript
    }
    if flat.starts_with("[context compacted]") {
        return Some("compacted".into());
    }
    if flat.starts_with("task list:") {
        return None; // durable state, shown once on both sides
    }
    if let Some(rest) = flat.strip_prefix("[artifact '")
        && let Some(end) = rest.find('\'')
    {
        return Some(format!("artifact|{}", &rest[..end]));
    }
    Some(format!("note|{flat}"))
}

fn audit_line(text: &str) -> String {
    let flat = flat(text);
    if let Some(rest) = flat.strip_prefix("task ")
        && let Some((id, tail)) = rest.split_once(" — ")
    {
        let st = match tail {
            "done" => "ok",
            "failed" => "fail",
            other => other,
        };
        return format!("task_done|{id}|{st}");
    }
    let event = flat.split(" — ").next().unwrap_or(&flat);
    format!("hook|{event}")
}

/// Fold the block transcript into canonical lines — the exact shape the
/// mjs driver emits after `renderReplay`.
fn canonical(app: &App) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut run: Option<ToolRun> = None;
    for b in &app.blocks {
        if b.kind == BlockKind::Tool {
            let t = b.tool.as_ref().expect("tool block carries tool state");
            let name = if t.name == "!" {
                "shell".to_string()
            } else {
                t.name.clone()
            };
            let state = tool_state(t).to_string();
            match &mut run {
                Some((d, n, count, states)) if *d == t.depth && *n == name => {
                    *count += t.group_count;
                    states.extend(std::iter::repeat_n(state, t.group_count));
                }
                _ => {
                    flush_tool(&mut out, &mut run);
                    run = Some((t.depth, name, t.group_count, vec![state; t.group_count]));
                }
            }
            continue;
        }
        flush_tool(&mut out, &mut run);
        match b.kind {
            BlockKind::User => out.push(format!("user|{}", flat(&b.text))),
            BlockKind::Assistant => out.push(format!("assistant|{}", flat(&b.text))),
            BlockKind::Thinking => out.push(format!("thinking|{}", flat(&b.text))),
            BlockKind::StepSummary => out.push(format!("step_summary|{}", b.folded.len())),
            BlockKind::Audit => out.push(audit_line(&b.text)),
            BlockKind::Note => {
                if let Some(line) = note_line(&b.text) {
                    out.push(line);
                }
            }
            BlockKind::Tool => unreachable!(),
        }
    }
    flush_tool(&mut out, &mut run);
    out
}

#[test]
fn replay_parity_tui_vs_gui() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let html_path = manifest.join("src/serve/assets/index.html");
    let driver = manifest.join("src/serve/replay_parity.mjs");

    // node availability gates the GUI half — absent node skips, never fails
    let node = match Command::new("node").arg("--version").output() {
        Ok(o) if o.status.success() => true,
        _ => {
            eprintln!("skipping replay parity test: node not on PATH");
            return;
        }
    };
    assert!(node);

    let events = fixture();
    let jsonl: String = events
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let events_path =
        std::env::temp_dir().join(format!("sunmao-parity-{}.jsonl", std::process::id()));
    std::fs::write(&events_path, &jsonl).unwrap();

    let mut app = App::new("test-model", PathBuf::from("proj"), "parity");
    app.blocks.clear(); // drop the session banner — replay starts from empty
    app.replay(&events);
    let tui_lines = canonical(&app);

    let out = Command::new("node")
        .arg(&driver)
        .arg(&html_path)
        .arg(&events_path)
        .output()
        .expect("spawn node");
    let _ = std::fs::remove_file(&events_path);
    assert!(
        out.status.success(),
        "gui driver failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let gui_text = String::from_utf8_lossy(&out.stdout);
    let gui_lines: Vec<String> = gui_text
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // sanity: the fixture must actually exercise the interesting rows —
    // an all-empty or degenerate transcript would pass vacuously.
    for needle in [
        "compacted",
        "tool|0|Read|2xok",
        "tool|1|Glob|1xok",
        "tool|0|shell|1xok",
        "tool|0|Grep|1xinterrupted",
        "task_done|bg-1|ok",
        "task_done|bg-2|fail",
        "artifact|report",
        "hook|approval.deny",
        "hook|approval.mode",
        "hook|hook injected context",
        "note|✓ Write",
    ] {
        assert!(
            tui_lines.iter().any(|l| l == needle),
            "fixture lost {needle} from the tui fold: {tui_lines:?}"
        );
    }

    assert_eq!(
        tui_lines,
        gui_lines,
        "replay divergence — tui vs gui canonical transcripts differ:\n\
         tui:\n{}\n\ngui:\n{}",
        tui_lines.join("\n"),
        gui_lines.join("\n"),
    );
}
