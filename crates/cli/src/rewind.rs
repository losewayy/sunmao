//! `/rewind` — local-frontend implementation (REPL + TUI share this; the
//! serve host has its own mgmt-lane variant in `serve/host.rs`). Semantics:
//! `/rewind` lists the session's user-turn boundaries; `/rewind n [mode]`
//! rewinds to just before turn n — `session` forks the log at the boundary,
//! `code` restores files only, `both` (default) does the pair.

use std::path::Path;

use sunmao_core::agent::AgentLoop;

/// Which surfaces a rewind touches — the mode word after the turn number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// fork the session at the boundary AND restore code (default)
    Both,
    /// fork only — the working tree keeps its bytes
    Session,
    /// restore files only — the current session keeps running
    Code,
}

impl Mode {
    /// Parse the optional mode word — absent defaults to `both`.
    pub fn parse(s: Option<&str>) -> Option<Self> {
        match s.map(|m| m.to_ascii_lowercase()) {
            None => Some(Self::Both),
            Some(m) if m == "both" => Some(Self::Both),
            Some(m) if m == "session" => Some(Self::Session),
            Some(m) if m == "code" => Some(Self::Code),
            _ => None,
        }
    }
}

/// What the frontend prints/replays after `run`: the note text, and for
/// session-side rewinds the forked log's events (the transcript swaps to
/// the prefix — same flow /resume takes).
pub enum Outcome {
    /// session side happened — replay these events, print the note
    Forked {
        note: String,
        events: Vec<sunmao_core::SessionEvent>,
    },
    /// code-only — no session swap; print the note
    CodeOnly(String),
}

/// `/rewind` with no argument — the numbered boundary list.
pub async fn list(agent: &AgentLoop) -> String {
    let path = agent.session_path().await;
    let bounds = sunmao_core::checkpoints::turn_boundaries(&path);
    if bounds.is_empty() {
        "[no turns to rewind to]".to_string()
    } else {
        let rows = bounds
            .iter()
            .map(|b| format!("  {}  {}", b.n, b.preview))
            .collect::<Vec<_>>()
            .join("\n");
        format!("turn boundaries — /rewind <n> [session|code|both]:\n{rows}")
    }
}

/// `/rewind n [mode]` — `n`/`mode` arrive already parsed (`commands::parse`
/// owns the grammar). Resolve the boundary, restore code if the mode asks,
/// and fork the log at the boundary line if the mode has a session side.
/// The fork's checkpoint ledger inherits the prefix's snapshots (truncated
/// to < n) so rewinds inside the fork stay honest.
pub async fn run(agent: &AgentLoop, project: &Path, n: u64, mode: Mode) -> Result<Outcome, String> {
    let src = agent.session_path().await;
    let src_id = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let bounds = sunmao_core::checkpoints::turn_boundaries(&src);
    let Some(boundary) = bounds.iter().find(|b| b.n == n) else {
        return Err(format!(
            "[no turn {n} — {} boundaries (see /rewind)]",
            bounds.len()
        ));
    };

    let mut restored: Vec<String> = Vec::new();
    if mode != Mode::Session {
        restored = sunmao_core::checkpoints::restore_files(project, &src_id, n)
            .map_err(|e| format!("{e:#}"))?;
    }

    let mut note = format!(
        "[rewound to before turn {n} — {}",
        if restored.is_empty() {
            "no files to restore".to_string()
        } else {
            format!("{} file(s) restored", restored.len())
        }
    );
    if mode == Mode::Code {
        return Ok(Outcome::CodeOnly(format!("{note}]")));
    }

    // session side: byte-prefix copy into a fresh fork id, then swap —
    // same dst naming as /fork so log families sort together
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let new_id = format!("s-{ms}-fork");
    let dir = src.parent().unwrap_or(project);
    let dst = dir.join(format!("{new_id}.jsonl"));
    sunmao_core::checkpoints::copy_log_prefix(&src, &dst, boundary.line)
        .map_err(|e| format!("{e:#}"))?;
    sunmao_core::checkpoints::fork_checkpoints(project, &src_id, &new_id, n)
        .map_err(|e| format!("{e:#}"))?;
    let log = sunmao_core::SessionLog::open_path(&dst)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let events = agent.swap_session(log).await;
    note.push_str(&format!(", session forked → {new_id}]"));
    Ok(Outcome::Forked { note, events })
}
