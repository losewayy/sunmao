//! Event-sourced session log — the kernel's single source of truth.
//!
//! One `events.jsonl` per session; every fact is an appended line. The live
//! message list is a *fold* over the log, so replay/rebuild/audit are free.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
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
    /// A tool call dispatched by the assistant.
    ToolCall { call: ToolCall },
    /// A tool call resolved (ok/fail recorded for replay fidelity).
    ToolResult {
        call_id: String,
        name: String,
        ok: bool,
        output: String,
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

    /// Fold the whole log into the message list the provider sees.
    /// Durable facts → protocol messages, in order.
    pub async fn messages(&self) -> anyhow::Result<Vec<Message>> {
        let mut out = Vec::new();
        if self.file.is_none() {
            // ephemeral: fold the in-memory buffer through the same reduce
            for ev in &self.mem {
                match ev {
                    SessionEvent::Message { message } => out.push(message.clone()),
                    SessionEvent::ToolResult {
                        call_id, output, ..
                    } => out.push(Message::tool_result(call_id.clone(), output.clone())),
                    SessionEvent::Compacted { summary } => {
                        out.clear();
                        out.push(Message::system(format!(
                            "[context compacted]
{summary}"
                        )));
                    }
                    _ => {}
                }
            }
            return Ok(out);
        }
        let file = tokio::fs::File::open(&self.path).await?;
        let mut lines = tokio::io::BufReader::new(file).lines();
        while let Some(line) = lines.next_line().await? {
            let ev: SessionEvent = serde_json::from_str(&line)
                .with_context(|| format!("corrupt event line: {line}"))?;
            match ev {
                SessionEvent::Message { message } => out.push(message),
                SessionEvent::ToolResult {
                    call_id, output, ..
                } => out.push(Message::tool_result(call_id, output)),
                SessionEvent::Compacted { summary } => {
                    out.clear();
                    out.push(Message::system(format!("[context compacted]\n{summary}")));
                }
                _ => {}
            }
        }
        Ok(out)
    }
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
}
