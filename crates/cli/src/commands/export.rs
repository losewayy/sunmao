//! `/export-md` — fold the session log into a shareable markdown
//! transcript; `/export-zip` — a debug bundle (transcript + the raw
//! session.jsonl + a manifest) for `--dataflow`-style forensics. Both
//! renderers/writers are shared across REPL/TUI/serve so all three
//! produce the same files.

#[cfg(test)]
use std::io::Read;
use std::io::Write;
use std::path::Path;

use sunmao_core::session::SessionEvent;
use sunmao_llm::types::{Content, Role};

/// Render a session's durable facts as markdown. Reads `events()` — the
/// raw log, not the message fold — so tool calls/results pair by position
/// and audit rows (Hook/Compacted) land where they happened.
pub fn markdown(session_id: &str, cwd: &Path, events: &[SessionEvent]) -> String {
    let mut out = String::new();
    // title: last SessionMeta wins, else first user prompt's head line
    let title = events
        .iter()
        .rev()
        .find_map(|e| match e {
            SessionEvent::SessionMeta { title } => Some(title.clone()),
            _ => None,
        })
        .or_else(|| {
            events.iter().find_map(|e| match e {
                SessionEvent::Message { message } if message.role == Role::User => {
                    message.content_text().and_then(|t| {
                        t.lines()
                            .next()
                            .map(|l| l.chars().take(60).collect::<String>())
                    })
                }
                _ => None,
            })
        })
        .unwrap_or_else(|| session_id.to_string());
    out.push_str(&format!(
        "# {title}\n\n- session: `{session_id}`\n- cwd: `{}`\n",
        cwd.display()
    ));
    for e in events {
        if let SessionEvent::Started { model, .. } = e {
            out.push_str(&format!("- model: {model}\n"));
            break;
        }
    }
    out.push_str("\n---\n");

    // per-tool-call arg prefix, paired with the result by position
    let mut pending_calls: Vec<&sunmao_llm::types::ToolCall> = Vec::new();
    // cumulative token accounting — one footer line
    let mut usage = (0u64, 0u64);

    for e in events {
        match e {
            SessionEvent::Message { message } => {
                let text = message.content_text().unwrap_or_default();
                let heading = match message.role {
                    Role::User => "## user",
                    Role::Assistant => "## agent",
                    Role::System => "## system",
                    Role::Tool => continue, // ToolResult events carry these
                };
                let mut body = String::new();
                for block in message.content.as_deref().unwrap_or(&[]) {
                    match block {
                        Content::Text { text } => {
                            body.push_str(text);
                            body.push('\n');
                        }
                        Content::Image { path, .. } => {
                            body.push_str(&format!("![]({path})\n"));
                        }
                    }
                }
                if let Some(calls) = &message.tool_calls {
                    for c in calls {
                        body.push_str(&format!(
                            "\n- **call** `{}` — args: `{}`",
                            c.function.name,
                            truncate(&c.function.arguments, 200)
                        ));
                    }
                }
                let _ = text; // body already covers it
                out.push_str(&format!("\n{heading}\n\n{body}\n"));
            }
            SessionEvent::ToolCall { call, depth, .. } => {
                // a ToolCall fact can arrive without an assistant Message
                // (replay fidelity) — surface it before the result lands
                if *depth > 0 {
                    out.push_str(&format!(
                        "\n*↳ {}…* `{}({})`\n",
                        depth,
                        call.function.name,
                        truncate(&call.function.arguments, 120)
                    ));
                }
                pending_calls.push(call);
            }
            SessionEvent::ToolResult {
                name,
                ok,
                output,
                depth,
                ..
            } => {
                let call = pending_calls
                    .iter()
                    .position(|c| c.function.name == *name)
                    .map(|i| pending_calls.remove(i));
                let args = call.map(|c| truncate(&c.function.arguments, 200));
                out.push_str(&format!(
                    "\n### {} {}\n\n{}{}```text\n{}\n```\n",
                    name,
                    if *ok { "✓" } else { "✗" },
                    match args {
                        Some(a) => format!("- args: `{a}`\n"),
                        None => String::new(),
                    },
                    if *depth > 0 {
                        format!("- sub-agent depth {depth}\n")
                    } else {
                        String::new()
                    },
                    output
                ));
            }
            SessionEvent::Compacted { summary } => {
                out.push_str(&format!(
                    "\n---\n\n*compacted — earlier turns folded*\n\n{summary}\n"
                ));
            }
            SessionEvent::LocalShell {
                command,
                exit_code,
                output,
            } => {
                out.push_str(&format!("\n### `!` local shell\n\n`{command}` → exit {exit_code}\n\n```text\n{output}\n```\n"));
            }
            SessionEvent::TaskDone { id, ok, output } => {
                out.push_str(&format!(
                    "\n### sub-agent {id} {}\n\n```text\n{}\n```\n",
                    if *ok { "done" } else { "failed" },
                    output
                ));
            }
            SessionEvent::Hook { event, detail } => {
                out.push_str(&format!("\n> *hook {event}:* {detail}\n"));
            }
            SessionEvent::PtcCall {
                name,
                args,
                ok,
                output,
                ..
            } => {
                // a RunCode script's nested call — an indented tool row,
                // same visibility as a top-level call but marked as
                // script-driven so readers don't look for a tool_use pair
                out.push_str(&format!(
                    "\n### ↳ {} {}\n\n- args: `{}`\n- nested in RunCode script\n\n```text\n{}\n```\n",
                    name,
                    if *ok { "✓" } else { "✗" },
                    truncate(args, 200),
                    output
                ));
            }
            SessionEvent::Artifact {
                name, path, bytes, ..
            } => {
                out.push_str(&format!("\n> *artifact* `{name}` — {bytes} B → `{path}`\n"));
            }
            SessionEvent::Usage { usage: u } => {
                usage.0 += u.prompt_tokens;
                usage.1 += u.completion_tokens;
            }
            SessionEvent::ModeChange { mode } => {
                out.push_str(&format!("\n> *approval mode → {}*\n", mode.as_str()));
            }
            SessionEvent::TurnModeChange { mode } => {
                out.push_str(&format!("\n> *turn mode → {}*\n", mode.as_str()));
            }
            SessionEvent::FusionSpec {
                seq,
                spec,
                sidekick,
                ..
            } => {
                out.push_str(&format!(
                    "\n> *fusion spec #{seq} → {sidekick}*\n\n```json\n{}\n```\n",
                    serde_json::to_string_pretty(spec).unwrap_or_default()
                ));
            }
            SessionEvent::FusionAccepted { spec_seq, sidekick } => {
                out.push_str(&format!("\n> *fusion accepted #{spec_seq} ({sidekick})*\n"));
            }
            SessionEvent::FusionEscalated { spec_seq, reason } => {
                out.push_str(&format!("\n> *fusion escalated #{spec_seq} — {reason}*\n"));
            }
            _ => {} // Started/Todos/Checkpoint/SessionMeta — meta, not transcript
        }
    }
    if usage.0 + usage.1 > 0 {
        out.push_str(&format!(
            "\n---\n\n*tokens: {} prompt / {} completion*\n",
            usage.0, usage.1
        ));
    }
    out
}

/// Write the transcript under `<cwd>/.sunmao/exports/<session_id>.md` and
/// return the path for the frontend's note.
pub fn export_md(cwd: &Path, session_id: &str, events: &[SessionEvent]) -> anyhow::Result<String> {
    let dir = cwd.join(".sunmao").join("exports");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{session_id}.md"));
    std::fs::write(&path, markdown(session_id, cwd, events))?;
    Ok(path.display().to_string())
}

/// Write the debug bundle — `<cwd>/.sunmao/exports/<session_id>.zip`
/// containing `transcript.md`, the raw `session.jsonl` (skipped for
/// ephemeral logs — nothing durable to ship), and a `manifest.txt`
/// naming the session/cwd/model so the bundle is self-describing even
/// detached from the frontend that produced it. Store compression keeps
/// the dependency surface at `zip` alone (no deflate crate).
pub fn export_zip(
    cwd: &Path,
    session_id: &str,
    events: &[SessionEvent],
    session_log: &Path,
) -> anyhow::Result<String> {
    let dir = cwd.join(".sunmao").join("exports");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{session_id}.zip"));
    let file = std::fs::File::create(&path)?;
    let mut zw = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    zw.start_file("manifest.txt", opts)?;
    let model = events.iter().find_map(|e| match e {
        SessionEvent::Started { model, .. } => Some(model.clone()),
        _ => None,
    });
    zw.write_all(
        format!(
            "session: {session_id}\ncwd: {}\nmodel: {}\nevents: {}\n",
            cwd.display(),
            model.as_deref().unwrap_or("(unknown)"),
            events.len()
        )
        .as_bytes(),
    )?;
    zw.start_file("transcript.md", opts)?;
    zw.write_all(markdown(session_id, cwd, events).as_bytes())?;
    if session_log.exists() {
        zw.start_file(format!("session-{session_id}.jsonl"), opts)?;
        zw.write_all(&std::fs::read(session_log)?)?;
    }
    zw.finish()?;
    Ok(path.display().to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunmao_llm::types::Message;

    #[test]
    fn markdown_pairs_calls_and_results() {
        let events = vec![
            SessionEvent::Started {
                model: "m".into(),
                cwd: "/x".into(),
                driver: None,
            },
            SessionEvent::Message {
                message: Message::user("hello"),
            },
            SessionEvent::ToolCall {
                call: sunmao_llm::types::ToolCall {
                    id: "c1".into(),
                    kind: "function".into(),
                    function: sunmao_llm::types::FunctionCall {
                        name: "read".into(),
                        arguments: "{\"path\":\"a.rs\"}".into(),
                    },
                },
                depth: 0,
                lane: 0,
            },
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                name: "read".into(),
                ok: true,
                output: "fn main() {}".into(),
                depth: 0,
                lane: 0,
            },
            SessionEvent::Message {
                message: Message::assistant(Some("done".into()), vec![]),
            },
        ];
        let md = markdown("s-1", Path::new("/x"), &events);
        assert!(md.contains("## user"), "user section: {md}");
        assert!(md.contains("hello"), "user text: {md}");
        assert!(md.contains("### read ✓"), "tool result heading: {md}");
        assert!(md.contains("args"), "call args surfaced: {md}");
        assert!(md.contains("## agent"), "assistant section: {md}");
        assert!(md.contains("model: m"), "model line: {md}");
    }

    #[test]
    fn compacted_and_hook_are_visible() {
        let events = vec![
            SessionEvent::Compacted {
                summary: "short".into(),
            },
            SessionEvent::Hook {
                event: "Stop".into(),
                detail: "vetoed".into(),
            },
        ];
        let md = markdown("s-2", Path::new("/x"), &events);
        assert!(md.contains("compacted"), "compaction marker: {md}");
        assert!(md.contains("hook Stop"), "hook row: {md}");
    }

    #[test]
    fn export_zip_bundles_manifest_transcript_and_log() {
        let dir = std::env::temp_dir().join(format!("sunmao-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("session.jsonl");
        std::fs::write(&log, "{\"kind\":\"x\"}\n").unwrap();
        let events = vec![
            SessionEvent::Started {
                model: "m".into(),
                cwd: dir.display().to_string(),
                driver: None,
            },
            SessionEvent::Message {
                message: sunmao_llm::types::Message::user("hello"),
            },
        ];
        let out = export_zip(&dir, "s-z", &events, &log).unwrap();
        let file = std::fs::File::open(&out).unwrap();
        let mut z = zip::ZipArchive::new(file).unwrap();
        let mut names: Vec<String> = z.file_names().map(|n| n.to_string()).collect();
        names.sort();
        assert!(
            names.iter().any(|n| n == "manifest.txt"),
            "names: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "transcript.md"),
            "names: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("session-") && n.ends_with(".jsonl")),
            "names: {names:?}"
        );
        let mut manifest = String::new();
        z.by_name("manifest.txt")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        assert!(manifest.contains("s-z"), "manifest: {manifest}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_zip_skips_missing_log_for_ephemeral() {
        let dir = std::env::temp_dir().join(format!("sunmao-zipe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let events = vec![SessionEvent::Message {
            message: sunmao_llm::types::Message::user("e"),
        }];
        let missing = dir.join("nope.jsonl");
        let out = export_zip(&dir, "s-e", &events, &missing).unwrap();
        let file = std::fs::File::open(&out).unwrap();
        let z = zip::ZipArchive::new(file).unwrap();
        let names: Vec<&str> = z.file_names().collect();
        assert!(!names.iter().any(|n| n.ends_with(".jsonl")), "{names:?}");
        assert_eq!(names.len(), 2, "manifest + transcript only: {names:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
