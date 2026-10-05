//! Live event wire vocabulary — what frontends observe (stdout printer,
//! TUI, ACP, `serve`). `Serialize` is the only JSON dialect they speak.

/// Live events the frontend can observe (stdout printer, TUI, ACP, web).
/// `Serialize` is the `sunmao serve` wire shape — tagged snake_case, the
/// only JSON dialect the GUI speaks (GUI.md §7).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveEvent {
    /// A streamed text delta — `text` because serde's tagged-enum wire shape
    /// can't carry a bare tuple payload (GUI.md §7 serves this verbatim).
    Content {
        text: String,
    },
    /// Reasoning/thinking channel delta.
    Reasoning {
        text: String,
    },
    /// Tool call began. `summary` is a one-line digest of the interesting
    /// argument (path/command/pattern/…) for frontends to render. `depth`
    /// is the agent's nesting level — 0 for the interactive agent, 1+ for
    /// `Task` sub-agents relayed through `ctx.live_sink`; `lane` tells
    /// parallel siblings apart (each spawn claims its own). `call_id` is the
    /// provider's tool_call id — the exact start↔done join key; `None` on
    /// synthetic events (compact, local shell) that have no wire call.
    /// `args` is the parsed call arguments (post-hook-rewrite) so rich
    /// frontends can render more than the one-line `summary` — Edit/Write
    /// cards diff the payloads; `null` on synthetic events and unparseable
    /// calls. `#[serde(default)]` is inert here (the enum is Serialize-only)
    /// but keeps the field shape declared for any future Deserialize.
    ToolStart {
        name: String,
        summary: String,
        depth: u8,
        lane: u16,
        call_id: Option<String>,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// Tool call finished. `output` carries the raw result so rich frontends
    /// can preview it; simple frontends ignore it. `call_id` joins back to
    /// its ToolStart — pairing by name alone mispairs when the same tool
    /// runs twice in one turn.
    ToolDone {
        name: String,
        ok: bool,
        output: String,
        depth: u8,
        lane: u16,
        call_id: Option<String>,
        /// Wall time from ToolStart to done — frontends render it; replayed
        /// transcripts (SessionEvent::ToolResult) can't carry it, so frontends
        /// that replay fall back to nothing rather than recompute.
        elapsed_ms: u64,
    },
    /// A hook changed the turn — input rewrite, veto, injected context, or a
    /// session-scoped approval grant. Mirrors `SessionEvent::Hook` so the
    /// audit spine is *visible* live, not just durable.
    Hook {
        event: String,
        detail: String,
    },
    /// The kernel accepted a user prompt as a queued/started turn — mirrors
    /// the durable `SessionEvent::Message{role:user}` the turn is about to
    /// commit. Emitted so a live frontend can draw the bubble itself instead
    /// of optimistically appending one that the kernel-side steer/queue
    /// decision might later contradict (audit-gui #4): the kernel is the
    /// truth on busy-vs-idle, so it owns the bubble. `content` is the same
    /// `Content[]` the prompt carries — text plus any image attachments —
    /// so the live row renders thumbs exactly like the replayed message.
    UserMessage {
        content: Vec<sunmao_llm::Content>,
    },
    /// A user-message boundary was committed to the log — ordinal N (the
    /// same numbering `turn_boundaries` produces and rewind targets).
    /// Emitted at the append site, never by the driver's echo: slash
    /// commands, vetoed prompts and previews get bubbles but no boundary,
    /// so a live frontend stamps rewind numbers from THIS event only.
    TurnBoundary {
        ordinal: u64,
    },
    /// An HTML artifact landed on disk — emitted by `HtmlArtifact` through
    /// `ctx.live_sink`. Frontends that can render (or link) surfaces it;
    /// degraded frontends show the path. `rev` = version number (0 for
    /// unversioned sources; islands offer ◀ ▶ when rev > 1).
    Artifact {
        name: String,
        path: String,
        bytes: usize,
        rev: usize,
    },
    /// Token accounting for one completed LLM request — mirrors the durable
    /// `SessionEvent::Usage` so footers can show context pressure live.
    Usage(sunmao_llm::types::Usage),
    /// The transcript was compacted — mirrors `SessionEvent::Compacted`.
    /// A live frontend clears its rendered messages exactly like a replay
    /// does; without this the live view keeps rows the kernel just dropped.
    Compacted {
        summary: String,
    },
    /// The durable todo list changed — mirrors `SessionEvent::Todos` so a
    /// live frontend sees the same list a replay would fold.
    Todos {
        items: Vec<crate::tool::TodoItem>,
    },
    /// The session goal changed or advanced a round — mirrors
    /// `SessionEvent::Goal` so a live frontend renders the same state a
    /// replay would fold (objective / status / round budget).
    Goal {
        goal: crate::tool::GoalState,
    },
    /// A detached sub-agent finished and wrote back — mirrors
    /// `SessionEvent::TaskDone`. Foreground `Task` results already arrive
    /// as `ToolDone`; only the push-style detached path needs this.
    TaskDone {
        id: String,
        ok: bool,
        output: String,
    },
    /// A background job finished and wrote back — mirrors
    /// `SessionEvent::JobDone`. Foreground results already arrive as
    /// `ToolDone`; only the push-style path (a job the caller stopped
    /// waiting on) needs this. Frontends use it to refresh the job card
    /// and to stop showing the job as running.
    JobDone {
        id: String,
        ok: bool,
        exit_code: i32,
        output_path: String,
        bytes: u64,
    },
    TurnEnd {
        outcome: TurnOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    LengthLimited,
    /// User- or host-initiated stop — distinct from `Other` because
    /// `Task`'s spawn wrapper must treat "killed" as `ok: false`, and the
    /// discriminant can't live in a free-form string.
    Cancelled,
    Other(String),
}

/// Observer sink — the REPL prints these, a GUI would render them.
pub trait Observer: Send + Sync {
    fn on_event(&self, ev: &LiveEvent);
}
