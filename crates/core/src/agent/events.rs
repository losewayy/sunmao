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
        lane: u8,
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
        lane: u8,
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
    TurnEnd {
        outcome: TurnOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    LengthLimited,
    Other(String),
}

/// Observer sink — the REPL prints these, a GUI would render them.
pub trait Observer: Send + Sync {
    fn on_event(&self, ev: &LiveEvent);
}
