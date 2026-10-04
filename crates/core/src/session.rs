//! Event-sourced session log — the kernel's single source of truth.
//!
//! One `<id>.jsonl` per session; every fact is an appended line. The live
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
    /// Session opened. `driver` records the loop driver the session's
    /// baked system prompt was assembled for — resume resolves against it
    /// so a manifest flip between runs can't mismatch prompt and surface.
    Started {
        model: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        driver: Option<String>,
    },
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
        lane: u16,
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
        lane: u16,
    },
    /// Compaction boundary: earlier events are summarized away.
    Compacted { summary: String },
    /// An HTML artifact was produced — human-facing deliverable registered
    /// as a durable fact (SPEC §4.10). `rev` is the version number (1-based;
    /// 0 = unversioned legacy event): rewrites archive the previous file as
    /// `{name}.v{rev-1}.html`, so the event sequence IS the version chain.
    Artifact {
        name: String,
        path: String,
        bytes: usize,
        #[serde(default)]
        rev: usize,
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
    /// The model's task list after a `TodoWrite` — durable so /resume and
    /// compaction never silently lose the plan. Kept OUT of the message
    /// fold: the turn loop injects the current list as a synthetic user
    /// message each iteration, so writing it into the transcript would
    /// duplicate it once per write.
    Todos { items: Vec<crate::tool::TodoItem> },
    /// The session's standing goal (`/goal` or `UpdateGoal`) — durable so
    /// resume/fork continue the same self-continuation loop. The fold
    /// ignores it: the turn loop re-injects the live snapshot per request
    /// and continuation prompts carry the objective themselves.
    Goal { goal: crate::tool::GoalState },
    /// The approval stance changed (`/mode`, ACP set_config_option, GUI
    /// selector). Audit, not conversation — the fold ignores it, but a
    /// resume reseeds `Context.approval_mode` from the latest one.
    ModeChange { mode: crate::agent::ApprovalMode },
    /// The turn mode changed (`/mode standard|fusion`) — a dedicated
    /// event, NOT a ModeChange payload: `ModeChange` carries an
    /// `ApprovalMode`, and `fusion` is not an approval stance. The fold
    /// ignores it; a resume reseeds `Context.turn_mode`/`read_only`.
    TurnModeChange { mode: crate::agent::TurnMode },
    /// The Lead issued a delegation spec (`FusionExecute`) — the full spec
    /// is durable because "what the Lead asked the Sidekick to change" is
    /// the fusion audit spine's whole point. `seq` orders it against the
    /// accepted/escalated facts; `spec_hash` ties them to one spec.
    FusionSpec {
        seq: u64,
        spec_hash: String,
        spec: serde_json::Value,
        /// the sidekick session driving it — its own log carries the run
        #[serde(default)]
        sidekick: String,
    },
    /// A delegation's verify ran clean — the audit verdict, naming the
    /// spec it closes so accepted/escalated associate back to it.
    FusionAccepted { spec_seq: u64, sidekick: String },
    /// A Sidekick burned through its verify-fail budget — the Lead unlocks
    /// for the rest of the turn. `reason` names the trigger.
    FusionEscalated { spec_seq: u64, reason: String },
    /// A `store()` write from a `RunCode` script — durable KV the sandbox
    /// shares across calls and resumes. Fold-ignored (like `Todos`): the
    /// store is state, not transcript; `load()` re-reads the snapshot.
    PtcStore { key: String, value: String },
    /// A `tools.*` call a `RunCode` script dispatched through the gate —
    /// durable *and* transcript-visible on replay (nested row under the
    /// script's RunCode call), but fold-ignored: a `ToolCall`/`ToolResult`
    /// pair would corrupt the fold — providers need tool_use/results issued
    /// by an assistant message, and these were issued by a script.
    PtcCall {
        call_id: String,
        name: String,
        /// the JSON args the script passed (post-hook-rewrite) — enough for
        /// a replay to re-render `call_summary` and for export/dataflow to
        /// attribute the call
        args: String,
        ok: bool,
        output: String,
        #[serde(default)]
        depth: u8,
        #[serde(default)]
        lane: u16,
    },
    /// The session's display title was renamed (serve `POST rename`). Audit,
    /// not conversation — the fold ignores it; readers (`session_meta`, the
    /// rail) take the LAST one as the title, overriding first-prompt
    /// derivation.
    SessionMeta { title: String },
    /// A file's pre-write bytes were snapshotted into the session's
    /// checkpoint ledger (`checkpoints.rs`). Durable audit fact, not
    /// model-facing — the fold ignores it; `/rewind` reads the manifest
    /// this event indexes, so `files` records the relative paths
    /// snapshotted at `turn`.
    Checkpoint {
        #[serde(default)]
        turn: u64,
        #[serde(default)]
        files: Vec<String>,
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
    /// Shared so `fork_writer` can hand a detached child a handle that
    /// keeps appending to THIS log after the parent swaps sessions.
    mem: std::sync::Arc<tokio::sync::Mutex<Vec<SessionEvent>>>,
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
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        // `open` can also RE-open an existing log (im-main, eval ids) —
        // same crash-tail heal as open_path, or the next append glues
        // onto the stranded fragment
        Self::heal_tail(&mut file, &path).await?;
        Ok(Self {
            path,
            file: Some(file),
            mem: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
        })
    }

    /// A crash mid-append can strand a partial last line — the next
    /// append would glue its JSON onto that fragment, corrupting both
    /// events. A non-empty file that doesn't end in '\n' gets one so
    /// the stranded fragment stays a single skippable bad line.
    async fn heal_tail(file: &mut tokio::fs::File, path: &std::path::Path) -> anyhow::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let len = file.metadata().await?.len();
        if len > 0 {
            let mut tail = tokio::fs::File::open(path).await?;
            tail.seek(std::io::SeekFrom::End(-1)).await?;
            let mut b = [0u8; 1];
            tail.read_exact(&mut b).await?;
            if b[0] != b'\n' {
                use tokio::io::AsyncWriteExt;
                file.write_all(b"\n").await?;
                file.flush().await?;
            }
        }
        Ok(())
    }

    /// Open an existing session log directly (for --resume). Strict
    /// contract: the file must already exist and carry the `.jsonl`
    /// extension — an arbitrary path (`--resume ~/notes.txt`) used to get
    /// a stray `\n` appended, and a bare missing id silently materialized
    /// an empty log that wiped the transcript on resume. Creating a fresh
    /// log is `open`'s job.
    pub async fn open_path(path: &std::path::Path) -> anyhow::Result<Self> {
        if path.extension().map(|e| e != "jsonl").unwrap_or(true) {
            anyhow::bail!(
                "not a session log (expected a .jsonl file): {}",
                path.display()
            );
        }
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .await
            .with_context(|| format!("no such session log: {}", path.display()))?;
        Self::heal_tail(&mut file, path).await?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            mem: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
        })
    }

    /// In-memory log (tests / ephemeral sessions).
    pub fn ephemeral() -> Self {
        Self {
            path: PathBuf::new(),
            file: None,
            mem: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// A second writer onto THIS log — a detached sub-agent's TaskDone
    /// lands on the session it spawned into, not whatever `swap_session`
    /// later installs. File-backed gets a fresh append handle on the same
    /// path; ephemeral shares the buffer.
    pub async fn fork_writer(&self) -> anyhow::Result<Self> {
        let file = match &self.path {
            p if p.as_os_str().is_empty() => None,
            p => Some(
                tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .await?,
            ),
        };
        Ok(Self {
            path: self.path.clone(),
            file,
            mem: self.mem.clone(),
        })
    }

    pub async fn append(&mut self, event: &SessionEvent) -> anyhow::Result<()> {
        if self.file.is_none() {
            self.mem.lock().await.push(event.clone());
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

    /// Append an audit fact that MUST surface on failure but must not abort
    /// the turn — Hook facts, Usage, Todos, ModeChange, TaskDone. The
    /// transcript's Message/ToolCall path propagates `?` (a lost turn is
    /// worse than a failed one); audit rows only lose observability, so we
    /// warn instead of erroring — a full disk now leaves a trace instead of
    /// silently swallowing the spine.
    pub async fn append_audit(&mut self, event: &SessionEvent) {
        if let Err(e) = self.append(event).await {
            tracing::warn!("session-log audit append failed: {e:#}");
        }
    }

    /// The full event vector — for transcript replay (TUI resume renders
    /// blocks from these) and any consumer that wants facts, not the fold.
    pub async fn events(&self) -> anyhow::Result<Vec<SessionEvent>> {
        if self.file.is_none() {
            return Ok(self.mem.lock().await.clone());
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
            for ev in self.mem.lock().await.iter() {
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
            // The system prompt is identity, not history — it survives the
            // fold. Everything else (incl. earlier summaries) is dropped.
            let system = out
                .iter()
                .find(|m| m.role == sunmao_llm::types::Role::System)
                .cloned();
            out.clear();
            if let Some(m) = system {
                out.push(m);
            }
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
                    // a result for a call the fold never saw (or one a
                    // non-tool message already repaired) is a provider-
                    // illegal orphan — drop it, same class of protection
                    // as the synthetic results above, other direction
                    if pending.iter().any(|p| p == id) {
                        pending.retain(|p| p != id);
                        out.push(m);
                    }
                } else {
                    out.push(m);
                }
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

/// The loop driver a log was created under — `Started.driver` when the
/// event carries one (logs written before the field existed parse as
/// `None`). Resume consults this so the advertised tool surface matches
/// the system prompt the log already baked; a manifest flip mid-life
/// can't drift them apart.
pub fn started_driver(log_path: &Path) -> Option<crate::agent::LoopDriver> {
    let file = std::fs::File::open(log_path).ok()?;
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        let Ok(line) = line else { break };
        let Ok(ev) = serde_json::from_str::<SessionEvent>(&line) else {
            continue;
        };
        match ev {
            SessionEvent::Started { driver, .. } => {
                return driver
                    .as_deref()
                    .and_then(|s| crate::agent::LoopDriver::parse(s).ok());
            }
            // Started is always written before the first Message (spawn
            // logs, audits, all writers) — hitting a Message first means
            // this log has no Started at all (sub-agent logs), so stop:
            // an unbounded scan would walk a whole session file for
            // nothing.
            SessionEvent::Message { .. } => return None,
            _ => continue,
        }
    }
    None
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
mod tests;
