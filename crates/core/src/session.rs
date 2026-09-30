//! Event-sourced session log — the kernel's single source of truth.
//!
//! One `events.jsonl` per session; every fact is an appended line. The live
//! message list is a *fold* over the log, so replay/rebuild/audit are free.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use sunmao_llm::types::{Message, ToolCall};

/// One durable fact. Serialized one-per-line as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Session opened.
    Started { model: String, cwd: String },
    /// A full message committed to the transcript.
    Message { message: Message },
    /// A tool call dispatched by the assistant. `depth` tags sub-agent work
    /// (0 = main loop) so a replayed transcript can re-mark it with ↳;
    /// `lane` separates parallel siblings (each Task spawn claims one).
    ToolCall {
        call: ToolCall,
        #[serde(default)]
        depth: u8,
        #[serde(default)]
        lane: u8,
    },
    /// A tool call resolved (ok/fail recorded for replay fidelity).
    ToolResult {
        call_id: String,
        name: String,
        ok: bool,
        output: String,
        #[serde(default)]
        depth: u8,
        #[serde(default)]
        lane: u8,
    },
    /// Compaction boundary: earlier events are summarized away.
    Compacted { summary: String },
    /// An HTML artifact was produced — human-facing deliverable registered
    /// as a durable fact (SPEC §4.10).
    Artifact {
        name: String,
        path: String,
        bytes: usize,
    },
    /// Token usage for one LLM request — the accounting side of audit.
    Usage { usage: sunmao_llm::types::Usage },
    /// A hook-induced fact (input rewrite, veto, injected context) — durable
    /// audit evidence that stays OUT of the model-facing message fold:
    /// rewrites must be transparent to the model, visible to the auditor.
    Hook { event: String, detail: String },
    /// A `!` local-shell command the user ran in the TUI — durable fact AND
    /// folded into the message stream (as a tagged user message) so the next
    /// turn sees the evidence the user just produced. User-initiated, so it
    /// never passes the approval gate.
    LocalShell {
        command: String,
        exit_code: i32,
        output: String,
    },
    /// A detached `Task` (`run_in_background`) finished — push-style result
    /// delivery: no polling, the next model read sees it as a tagged user
    /// message. `id` doubles as the child's session-log name, so the full
    /// transcript survives this event's capped `output`.
    TaskDone {
        id: String,
        ok: bool,
        output: String,
    },
}

/// Where a session's event log lives — `<cwd>/.sunmao/sessions/<id>.jsonl`.
/// Shared by `SessionLog::open` callers and the hooks engine, which reports
/// it as `transcript_path` in the hook payload dialect.
pub fn session_log_path(cwd: &Path, session_id: &str) -> PathBuf {
    cwd.join(".sunmao")
        .join("sessions")
        .join(format!("{session_id}.jsonl"))
}

/// Append-only writer + replay reader for one session directory.
pub struct SessionLog {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    /// in-memory buffer for ephemeral logs — the file-backed path replays
    /// from disk, this one replays from memory; same fold either way.
    mem: Vec<SessionEvent>,
}

impl SessionLog {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open (creating) the log for `session_id` under `dir`.
    pub async fn open(dir: impl AsRef<Path>, session_id: &str) -> anyhow::Result<Self> {
        let dir = dir.as_ref();
        tokio::fs::create_dir_all(dir).await?;
        let path = dir.join(format!("{session_id}.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self {
            path,
            file: Some(file),
            mem: Vec::new(),
        })
    }

    /// Open an existing log file directly (for --resume).
    pub async fn open_path(path: &std::path::Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            mem: Vec::new(),
        })
    }

    /// In-memory log (tests / ephemeral sessions).
    pub fn ephemeral() -> Self {
        Self {
            path: PathBuf::new(),
            file: None,
            mem: Vec::new(),
        }
    }

    pub async fn append(&mut self, event: &SessionEvent) -> anyhow::Result<()> {
        if self.file.is_none() {
            self.mem.push(event.clone());
            return Ok(());
        }
        if let Some(file) = &mut self.file {
            let mut line = serde_json::to_vec(event)?;
            line.push(b'\n');
            file.write_all(&line).await?;
            file.flush().await?;
        }
        Ok(())
    }

    /// The full event vector — for transcript replay (TUI resume renders
    /// blocks from these) and any consumer that wants facts, not the fold.
    pub async fn events(&self) -> anyhow::Result<Vec<SessionEvent>> {
        if self.file.is_none() {
            return Ok(self.mem.clone());
        }
        let file = tokio::fs::File::open(&self.path).await?;
        let mut lines = tokio::io::BufReader::new(file).lines();
        let mut out = Vec::new();
        while let Some(line) = lines.next_line().await? {
            if let Ok(ev) = serde_json::from_str::<SessionEvent>(&line) {
                out.push(ev);
            }
        }
        Ok(out)
    }

    /// Fold the whole log into the message list the provider sees.
    /// Durable facts → protocol messages, in order.
    ///
    /// Crash tolerance, both directions: a corrupt event line is skipped
    /// with a warning (one bad write must not brick every future turn), and
    /// a crash-stranded assistant tool_call gets a synthetic
    /// "[interrupted]" tool_result — providers reject transcripts whose
    /// tool_use has no matching result.
    pub async fn messages(&self) -> anyhow::Result<Vec<Message>> {
        let mut out = Vec::new();
        if self.file.is_none() {
            // ephemeral: fold the in-memory buffer through the same reduce
            for ev in &self.mem {
                reduce_event(&mut out, ev);
            }
            return Ok(repair_dangling_calls(out));
        }
        let file = tokio::fs::File::open(&self.path).await?;
        let mut lines = tokio::io::BufReader::new(file).lines();
        while let Some(line) = lines.next_line().await? {
            match serde_json::from_str::<SessionEvent>(&line) {
                Ok(ev) => reduce_event(&mut out, &ev),
                Err(e) => {
                    tracing::warn!("{}: skipping corrupt event line — {e}", self.path.display())
                }
            }
        }
        Ok(repair_dangling_calls(out))
    }
}

/// One durable fact → zero or one protocol messages. Shared by both
/// `messages()` paths so ephemeral and file-backed folds can't drift apart.
fn reduce_event(out: &mut Vec<Message>, ev: &SessionEvent) {
    match ev {
        SessionEvent::Message { message } => out.push(message.clone()),
        SessionEvent::ToolResult {
            call_id, output, ..
        } => out.push(Message::tool_result(call_id.clone(), output.clone())),
        SessionEvent::Compacted { summary } => {
            out.clear();
            out.push(Message::system(format!("[context compacted]\n{summary}")));
        }
        SessionEvent::LocalShell {
            command,
            exit_code,
            output,
        } => out.push(local_shell_message(command, *exit_code, output)),
        SessionEvent::TaskDone { id, ok, output } => out.push(task_done_message(id, *ok, output)),
        _ => {}
    }
}

/// A crash mid-turn (process kill, power loss) strands an assistant
/// tool_call with no ToolResult event behind it — and providers (Anthropic
/// especially) hard-reject a transcript whose tool_use lacks its result.
/// Fold-time repair: every orphaned call id gets a legible interrupted
/// result, appended after the run so the pairing stays adjacent.
fn repair_dangling_calls(msgs: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(msgs.len() + 2);
    // call ids seen on the last assistant message that still owe a result
    let mut pending: Vec<String> = Vec::new();
    for m in msgs {
        match m.role {
            sunmao_llm::types::Role::Assistant => {
                if let Some(calls) = &m.tool_calls {
                    pending.extend(calls.iter().map(|c| c.id.clone()));
                }
                out.push(m);
            }
            sunmao_llm::types::Role::Tool => {
                if let Some(id) = &m.tool_call_id {
                    pending.retain(|p| p != id);
                }
                out.push(m);
            }
            _ => {
                for id in pending.drain(..) {
                    out.push(Message::tool_result(
                        id,
                        "[interrupted: session ended before this call returned]",
                    ));
                }
                out.push(m);
            }
        }
    }
    for id in pending.drain(..) {
        out.push(Message::tool_result(
            id,
            "[interrupted: session ended before this call returned]",
        ));
    }
    out
}

/// `!` local-shell facts fold in as a tagged user message — the model sees
/// exactly what ran and what came back, framed so it can't be mistaken for
/// its own tool calls.
fn local_shell_message(command: &str, exit_code: i32, output: &str) -> Message {
    Message::user(format!(
        "<local-shell>\n$ {command}\n{output}\n[exit {exit_code}]\n</local-shell>"
    ))
}

/// Background sub-agent results fold in as a tagged user message — the
/// model gets the verdict and capped output, and can open
/// `.sunmao/sessions/<id>.jsonl` for the full transcript when it needs it.
fn task_done_message(id: &str, ok: bool, output: &str) -> Message {
    let status = if ok { "done" } else { "failed" };
    Message::user(format!(
        "<task-result id=\"{id}\" status=\"{status}\">\n{output}\n</task-result>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunmao_llm::types::Message;

    #[tokio::test]
    async fn fold_replays_messages_and_tool_results() {
        let mut log = SessionLog::ephemeral();
        log.append(&SessionEvent::Message {
            message: Message::user("hi"),
        })
        .await
        .unwrap();
        log.append(&SessionEvent::ToolResult {
            call_id: "c1".into(),
            name: "Read".into(),
            ok: true,
            output: "x".into(),
            depth: 0,
            lane: 0,
        })
        .await
        .unwrap();
        let msgs = log.messages().await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("c1"));
    }

    #[tokio::test]
    async fn compacted_boundary_clears_prior_transcript() {
        let mut log = SessionLog::ephemeral();
        log.append(&SessionEvent::Message {
            message: Message::user("old1"),
        })
        .await
        .unwrap();
        log.append(&SessionEvent::Message {
            message: Message::user("old2"),
        })
        .await
        .unwrap();
        log.append(&SessionEvent::Compacted {
            summary: "summary text".into(),
        })
        .await
        .unwrap();
        log.append(&SessionEvent::Message {
            message: Message::user("new"),
        })
        .await
        .unwrap();
        let msgs = log.messages().await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].content.as_deref().unwrap().contains("summary text"));
        assert_eq!(msgs[1].content.as_deref(), Some("new"));
    }

    /// LocalShell folds into the message stream on BOTH paths — ephemeral
    /// (in-mem) and file-backed (replayed from disk). The invariant list in
    /// AGENTS.md holds them to identical fold semantics, so one test asserts
    /// both.
    #[tokio::test]
    async fn local_shell_folds_into_messages_both_paths() {
        let ev = SessionEvent::LocalShell {
            command: "echo hi".into(),
            exit_code: 0,
            output: "hi".into(),
        };
        // ephemeral
        let mut log = SessionLog::ephemeral();
        log.append(&ev).await.unwrap();
        let msgs = log.messages().await.unwrap();
        assert_eq!(msgs.len(), 1);
        let c = msgs[0].content.as_deref().unwrap();
        assert!(c.contains("$ echo hi") && c.contains("[exit 0]"));

        // file-backed — same fold through the disk replay path. Unique dir
        // name: tests share a pid and run in parallel, a generic name here
        // once deleted a sibling test's fixture mid-assert.
        let dir = crate::fresh_test_dir("ls");
        let mut log = SessionLog::open(&dir, "ls-fold").await.unwrap();
        log.append(&ev).await.unwrap();
        drop(log);
        let log = SessionLog::open(&dir, "ls-fold").await.unwrap();
        let msgs = log.messages().await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, sunmao_llm::types::Role::User);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sub-agent depth survives the disk round-trip AND logs written before
    /// the field existed still parse (serde default → depth 0). Frontends
    /// replay depth>0 as ↳ blocks — losing it silently flattens transcripts.
    #[tokio::test]
    async fn tool_event_depth_roundtrips_and_defaults() {
        let dir = crate::fresh_test_dir("sess-depth");
        let call = ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: sunmao_llm::types::FunctionCall {
                name: "Glob".into(),
                arguments: "{}".into(),
            },
        };
        let mut log = SessionLog::open(&dir, "d").await.unwrap();
        log.append(&SessionEvent::ToolCall {
            call,
            depth: 1,
            lane: 2,
        })
        .await
        .unwrap();
        // a pre-depth log line: same shape, no `depth`/`lane` keys
        let legacy =
            r#"{"type":"tool_result","call_id":"c1","name":"Glob","ok":true,"output":"x"}"#;
        {
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::OpenOptions::new()
                .append(true)
                .open(log.path())
                .await
                .unwrap();
            f.write_all(legacy.as_bytes()).await.unwrap();
            f.write_all(b"\n").await.unwrap();
        }
        drop(log);

        let log = SessionLog::open(&dir, "d").await.unwrap();
        let events = log.events().await.unwrap();
        assert!(matches!(
            &events[0],
            SessionEvent::ToolCall {
                depth: 1,
                lane: 2,
                ..
            }
        ));
        assert!(matches!(
            &events[1],
            SessionEvent::ToolResult {
                depth: 0,
                lane: 0,
                ..
            }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt JSONL line must not brick every future turn — the fold
    /// skips it and keeps the good events on both sides.
    #[tokio::test]
    async fn corrupt_line_is_skipped_not_fatal() {
        let dir = crate::fresh_test_dir("corrupt");
        let mut log = SessionLog::open(&dir, "c").await.unwrap();
        log.append(&SessionEvent::Message {
            message: Message::user("before"),
        })
        .await
        .unwrap();
        {
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::OpenOptions::new()
                .append(true)
                .open(log.path())
                .await
                .unwrap();
            f.write_all(b"{not json\n").await.unwrap();
        }
        log.append(&SessionEvent::Message {
            message: Message::user("after"),
        })
        .await
        .unwrap();
        drop(log);

        let log = SessionLog::open(&dir, "c").await.unwrap();
        let msgs = log.messages().await.unwrap();
        let texts: Vec<_> = msgs.iter().filter_map(|m| m.content.as_deref()).collect();
        assert_eq!(texts, vec!["before", "after"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A crash-stranded assistant tool_call (no ToolResult event) would make
    /// providers reject the whole transcript — the fold synthesizes an
    /// interrupted result so resume-after-crash still works. Both paths.
    #[tokio::test]
    async fn dangling_tool_call_gets_interrupted_result() {
        let call = ToolCall {
            id: "c-dead".into(),
            kind: "function".into(),
            function: sunmao_llm::types::FunctionCall {
                name: "Bash".into(),
                arguments: "{}".into(),
            },
        };
        let evs = [
            SessionEvent::Message {
                message: Message::user("run it"),
            },
            SessionEvent::Message {
                message: Message::assistant(None, vec![call.clone()]),
            },
            // crash happens here — no ToolResult event
            SessionEvent::Message {
                message: Message::user("next prompt after resume"),
            },
        ];

        // ephemeral path
        let mut log = SessionLog::ephemeral();
        for e in &evs {
            log.append(e).await.unwrap();
        }
        let msgs = log.messages().await.unwrap();
        let result = msgs
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("c-dead"))
            .expect("orphaned tool_call must gain a synthetic result");
        assert!(result.content.as_deref().unwrap().contains("interrupted"));
        // the repair lands before the following user message (adjacency)
        let idx = msgs
            .iter()
            .position(|m| m.tool_call_id.as_deref() == Some("c-dead"))
            .unwrap();
        assert_eq!(msgs[idx + 1].role, sunmao_llm::types::Role::User);

        // file-backed path
        let dir = crate::fresh_test_dir("dangle");
        let mut log = SessionLog::open(&dir, "d").await.unwrap();
        for e in &evs {
            log.append(e).await.unwrap();
        }
        drop(log);
        let log = SessionLog::open(&dir, "d").await.unwrap();
        let msgs = log.messages().await.unwrap();
        assert!(
            msgs.iter()
                .any(|m| m.tool_call_id.as_deref() == Some("c-dead")
                    && m.content.as_deref().unwrap().contains("interrupted")),
            "file-backed fold must repair dangling calls too"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
