//! The event→block fold: `App`'s transcript grows only through these
//! sinks, whether the event arrived live off the wire or was replayed out
//! of a session log on `--resume`.

use std::time::Instant;

use super::app::{App, Focus};
use super::blocks::{Block, BlockKind};

impl App {
    // ── streaming events → blocks ────────────────────────────────────────

    /// Append streamed text to the last open block of `kind`, else open one.
    pub fn stream(&mut self, kind: BlockKind, text: &str) {
        self.busy = true;
        if self.busy_since.is_none() {
            self.busy_since = Some(Instant::now());
        }
        let last_ok = self.blocks.last_mut().filter(|b| b.kind == kind && b.open);
        match last_ok {
            Some(b) => {
                b.text.push_str(text);
                b.generation += 1;
            }
            None => {
                let mut b = Block::new(kind);
                b.text = text.to_string();
                self.blocks.push(b);
            }
        }
        self.follow_tail();
    }

    /// A tool call started. Verb-grouping: when the previous block is the
    /// same tool at the same depth+lane finished, re-arm it (one `Read ×3`
    /// row instead of three). `depth > 0` marks sub-agent calls; `lane`
    /// keeps parallel batch children distinct (same depth, same tool name
    /// would otherwise alias).
    pub fn tool_start(&mut self, name: &str, summary: &str, depth: u8, lane: u8) {
        if let Some(prev) = self.blocks.last_mut()
            && prev.kind == BlockKind::Tool
                && prev.tool.as_ref().is_some_and(|t| {
                    t.name == name && t.depth == depth && t.lane == lane && t.done.is_some()
                })
            {
                prev.rearm_tool(summary);
                return;
            }
        self.blocks
            .push(Block::new_tool(name, summary, depth, lane));
        self.follow_tail();
    }

    /// A tool call finished — backfills the matching running block.
    pub fn tool_done(&mut self, name: &str, ok: bool, output: &str, depth: u8, lane: u8) {
        if let Some(b) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| b.is_running_tool(name, depth, lane))
        {
            b.finish_tool(ok, output);
        } else {
            // ToolDone without a start (shouldn't happen) — note it.
            let mark = if ok { "✓" } else { "✗" };
            self.push_note(&format!("{mark} {name}"));
            return;
        }
        self.follow_tail();
    }

    /// Turn ended: close open streaming blocks; a tool still marked running
    /// never got its ToolDone — call it interrupted. Then fold-by-cap: a
    /// long turn's earlier steps compress into one StepSummary row so the
    /// tail stays readable (`e` on it splices the steps back).
    pub fn close_turn(&mut self) {
        for b in &mut self.blocks {
            if b.open {
                b.open = false;
                b.generation += 1;
            }
            if b.tool.as_ref().is_some_and(|t| t.done.is_none()) {
                b.finish_tool(false, "interrupted");
            }
        }
        self.busy = false;
        self.busy_since = None;
        self.fold_turn();
    }

    /// Keep the last TURN_CAP steps of the current turn visible; older ones
    /// move into a StepSummary block where the first folded step sat.
    /// Audit/note rows interleaved in the range stay — spine, not steps.
    fn fold_turn(&mut self) {
        /// Steps are the turn's working blocks — a user band ends the turn,
        /// notes/audits/summaries aren't counted or collected.
        fn is_step(b: &Block) -> bool {
            matches!(
                b.kind,
                BlockKind::Assistant | BlockKind::Thinking | BlockKind::Tool
            )
        }
        const TURN_CAP: usize = 12;

        let turn_start = self
            .blocks
            .iter()
            .rposition(|b| b.kind == BlockKind::User)
            .map(|i| i + 1)
            .unwrap_or(0);
        let foldable: Vec<usize> = (turn_start..self.blocks.len())
            .filter(|&i| is_step(&self.blocks[i]))
            .collect();
        if foldable.len() <= TURN_CAP {
            return;
        }
        let fold_set: std::collections::HashSet<usize> = foldable[..foldable.len() - TURN_CAP]
            .iter()
            .copied()
            .collect();

        // keep the scrollback selection pointing at the same content:
        // a folded block selects the summary, a kept one shifts down.
        let sel = self.selected;
        let sel_folded = fold_set.contains(&sel);
        let removed_before = fold_set.iter().filter(|i| **i < sel).count();

        let old = std::mem::take(&mut self.blocks);
        let mut new_blocks = Vec::with_capacity(old.len() + 1);
        let mut folded = Vec::with_capacity(fold_set.len());
        let mut pos = None;
        for (i, b) in old.into_iter().enumerate() {
            if fold_set.contains(&i) {
                if pos.is_none() {
                    pos = Some(new_blocks.len());
                }
                folded.push(b);
            } else {
                new_blocks.push(b);
            }
        }
        let pos = pos.expect("fold_set is non-empty");
        let mut summary = Block::new(BlockKind::StepSummary);
        summary.text = Self::fold_summary(&folded);
        summary.folded = folded;
        new_blocks.insert(pos, summary);
        self.blocks = new_blocks;
        self.render_cache.clear();
        self.selected = if sel_folded {
            pos
        } else if sel > pos {
            // the inserted summary restores one slot before sel
            sel - removed_before + 1
        } else {
            sel
        };
        self.ensure_selection();
    }

    pub fn push_note(&mut self, note: &str) {
        self.busy = false;
        self.busy_since = None;
        let mut b = Block::new(BlockKind::Note);
        b.text = note.to_string();
        self.blocks.push(b);
        self.follow_tail();
    }

    /// An audit fact (hook rewrite/veto/injection, session grant) — always
    /// visible, never folded into a tool block. Keeps `busy` untouched.
    pub fn push_audit(&mut self, detail: &str) {
        let mut b = Block::new(BlockKind::Audit);
        b.text = detail.to_string();
        self.blocks.push(b);
        self.follow_tail();
    }

    /// Echo the user's submitted prompt as its own block.
    pub(super) fn echo_user(&mut self, text: &str) {
        let mut b = Block::new(BlockKind::User);
        b.text = text.to_string();
        self.blocks.push(b);
    }

    /// Rebuild the transcript from a session log on `--resume` — the same
    /// blocks a live turn would have produced, all closed.
    pub fn replay(&mut self, events: &[sunmao_core::SessionEvent]) {
        use sunmao_core::SessionEvent as E;
        use sunmao_llm::types::Role;
        // assistant text that precedes a tool-call batch streams before the
        // ToolCall events, same as live — order preserved by construction.
        for ev in events {
            match ev {
                E::Usage { usage } => {
                    // the footer resumes the last recorded context pressure —
                    // a resumed session shouldn't look emptier than it was.
                    self.last_usage = Some(usage.clone());
                }
                E::Started { .. } | E::Artifact { .. } => {}
                E::Todos { items } => {
                    // the task list is durable state, not transcript — a
                    // resumed session shows it once, as a note.
                    if !items.is_empty() {
                        self.push_note(&format!(
                            "task list:\n{}",
                            sunmao_core::tool::render_todos(items)
                        ));
                    }
                }
                E::Message { message } => match message.role {
                    Role::User => {
                        if let Some(c) = &message.content {
                            if c.starts_with("[hook context]") {
                                self.push_audit("hook injected context");
                            } else if c.starts_with("<local-shell>") {
                                // folded evidence — the real block comes
                                // from the LocalShell event itself
                            } else {
                                self.echo_user(c);
                            }
                        }
                    }
                    Role::Assistant => {
                        if let Some(c) = message.content.as_ref().filter(|c| !c.is_empty()) {
                            self.stream(BlockKind::Assistant, c);
                        }
                    }
                    _ => {}
                },
                E::ToolCall { call, depth, lane } => {
                    let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                        .unwrap_or(serde_json::Value::Null);
                    self.tool_start(
                        &call.function.name,
                        &sunmao_core::agent::call_summary(&call.function.name, &args),
                        *depth,
                        *lane,
                    );
                }
                E::ToolResult {
                    name,
                    ok,
                    output,
                    depth,
                    lane,
                    ..
                } => self.tool_done(name, *ok, output, *depth, *lane),
                E::Hook { event, detail } => {
                    self.push_audit(&format!("{event} — {detail}"));
                }
                E::TaskDone { id, ok, .. } => {
                    // replays mirror the live `task.bg.done` audit line; the
                    // model-facing <task-result> fold already carries output.
                    self.push_audit(&format!(
                        "task {id} — {}",
                        if *ok { "done" } else { "failed" }
                    ));
                }
                E::LocalShell {
                    command,
                    exit_code,
                    output,
                } => {
                    self.tool_start("!", &format!("$ {command}"), 0, 0);
                    self.tool_done("!", *exit_code == 0, output, 0, 0);
                }
                E::Compacted { summary } => {
                    self.blocks.clear();
                    self.push_note(&format!("[context compacted] {summary}"));
                }
            }
            // every replayed block is history — close text streams after
            // each event; tool done-state waits for the final close_turn so
            // a call→result pair isn't pre-marked interrupted.
            for b in &mut self.blocks {
                b.open = false;
            }
        }
        self.close_turn(); // dangling tools → interrupted; busy=false
    }

    /// Summary line for a folded run: "N tool calls · M thinking folded".
    fn fold_summary(folded: &[Block]) -> String {
        let mut tools = 0usize;
        let mut thinking = 0usize;
        let mut msgs = 0usize;
        for b in folded {
            match b.kind {
                BlockKind::Tool => tools += 1,
                BlockKind::Thinking => thinking += 1,
                _ => msgs += 1,
            }
        }
        let mut parts = Vec::new();
        if tools > 0 {
            parts.push(format!("{tools} tool calls"));
        }
        if thinking > 0 {
            parts.push(format!("{thinking} thinking"));
        }
        if msgs > 0 {
            parts.push(format!("{msgs} messages"));
        }
        format!("{} folded", parts.join(" · "))
    }

    // ── scroll helpers ───────────────────────────────────────────────────

    /// Keep pinned to the bottom when the user hasn't scrolled away.
    fn follow_tail(&mut self) {
        if self.scroll_back == 0 && self.focus != Focus::Scrollback {
            // nothing — Paragraph::scroll(0) already shows the tail
        }
    }
}
