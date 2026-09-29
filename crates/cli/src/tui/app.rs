//! Application state: the block transcript, composer, focus machine,
//! approval card, and slash menu. Rendering decisions live in `mod.rs`;
//! this file owns *what is true*, not how it looks.

use std::time::{Duration, Instant};

use super::blocks::{Block, BlockKind};
use super::slash;

/// Where the keyboard currently lives. Modeled after grok-build's parkable
/// focus: an approval card or scrollback selection can hold the keys while
/// the composer stays untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// typing into the composer
    Input,
    /// scrollback selection active (j/k walk blocks, e folds, y copies)
    Scrollback,
    /// the approval card holds the keys
    Approval,
    /// full-screen viewer for one block (Enter on a scrollback selection)
    Viewer,
}

/// Approval card state. `parked` = user Esc'd to read the scrollback; the
/// card stays on screen (status bar offers Tab to return) but releases keys.
pub struct ApprovalCard {
    pub tool: String,
    pub detail: String,
    pub why: String,
    pub reply: tokio::sync::oneshot::Sender<sunmao_core::approval::Approval>,
    /// 0 = allow, 1 = deny (two-option card for now — scopes need a richer
    /// Approver API that doesn't exist yet)
    pub selected: usize,
    pub parked: bool,
}

/// Slash-command popup state. Open while the composer is exactly a `/…`
/// fragment with no whitespace; walks `slash::candidates`.
pub struct SlashMenu {
    /// name list filtered by the fragment after `/`
    pub matches: Vec<String>,
    pub selected: usize,
    /// the fragment after `/` that produced `matches` (drives the Search row)
    pub fragment: String,
}

pub struct App {
    /// project dir — slash commands and command .md resolution anchor here.
    pub cwd: std::path::PathBuf,
    /// model name shown in the status bar
    pub model: String,
    pub blocks: Vec<Block>,
    pub input: String,
    /// cursor as *char index* into `input` — never a byte offset.
    pub cursor: usize,
    /// visual lines scrolled back from the tail; 0 = pinned to bottom.
    pub scroll_back: u16,
    pub history: Vec<String>,
    pub hist_idx: Option<usize>,
    pub busy: bool,
    pub focus: Focus,
    /// index into `blocks` while Focus::Scrollback
    pub selected: usize,
    pub approval: Option<ApprovalCard>,
    pub slash_menu: Option<SlashMenu>,
    /// multiline composer: Enter inserts \n, Alt/Shift+Enter sends
    pub multiline: bool,
    /// transient status text shown in the status bar (e.g. copy confirm)
    pub toast: Option<(String, Instant)>,
    /// last Esc press for the double-Esc-clears-draft gesture
    pub last_esc: Option<Instant>,
    /// stash of a draft cleared by double-Esc (Ctrl+S restores)
    pub draft_stash: Option<String>,
    /// git branch of `cwd`, probed at startup (None = not a repo / no git)
    pub git_branch: Option<String>,
    /// last observed provider usage — the footer renders prompt tokens as
    /// "context filled" so the user sees context pressure before it bites.
    pub last_usage: Option<sunmao_llm::types::Usage>,
    /// `!` bash mode: the composer holds a shell command; submit wraps it
    /// for the Bash tool instead of sending it as a prompt.
    pub bash_mode: bool,
    /// Full-screen viewer: (title, body) of the block being read. Lives in
    /// app state so render stays pure.
    pub viewer: Option<Viewer>,
}

/// Content of the full-screen viewer — title line + the block's full text
/// (copy_text: header + complete output, not the 5-line preview).
pub struct Viewer {
    pub title: String,
    pub body: String,
    /// visual lines scrolled from the top
    pub scroll: u16,
}

/// How a submitted line should be dispatched — the driver task interprets.
#[derive(Debug)]
pub enum Submit {
    /// normal prompt or a resolved command body — goes to run_turn
    Turn(String),
    /// `!` local shell — run directly, never a model turn
    Bash(String),
    /// /compact
    Compact,
    /// command name didn't resolve — show a note, no turn
    Note(String),
    /// /quit or /exit
    Quit,
}

impl App {
    pub fn new(model: &str, cwd: std::path::PathBuf) -> Self {
        let mut app = Self {
            cwd,
            model: model.to_string(),
            blocks: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll_back: 0,
            history: Vec::new(),
            hist_idx: None,
            busy: false,
            focus: Focus::Input,
            selected: 0,
            approval: None,
            slash_menu: None,
            multiline: false,
            toast: None,
            last_esc: None,
            draft_stash: None,
            git_branch: None,
            last_usage: None,
            bash_mode: false,
            viewer: None,
        };
        let mut banner = Block::new(BlockKind::Note);
        banner.text = format!(
            "sunmao TUI — {model}\nEnter send · Esc/Ctrl-C quit · Tab browse blocks · / commands"
        );
        app.blocks.push(banner);
        app
    }

    // ── streaming events → blocks ────────────────────────────────────────

    /// Append streamed text to the last open block of `kind`, else open one.
    pub fn stream(&mut self, kind: BlockKind, text: &str) {
        self.busy = true;
        let last_ok = self.blocks.last_mut().filter(|b| b.kind == kind && b.open);
        match last_ok {
            Some(b) => b.text.push_str(text),
            None => {
                let mut b = Block::new(kind);
                b.text = text.to_string();
                self.blocks.push(b);
            }
        }
        self.follow_tail();
    }

    /// A tool call started. Verb-grouping: when the previous block is the
    /// same tool finished, re-arm it (one `Read ×3` row instead of three).
    pub fn tool_start(&mut self, name: &str, summary: &str) {
        if let Some(prev) = self.blocks.last_mut() {
            if prev.kind == BlockKind::Tool
                && prev
                    .tool
                    .as_ref()
                    .is_some_and(|t| t.name == name && t.done.is_some())
            {
                prev.rearm_tool(summary);
                return;
            }
        }
        self.blocks.push(Block::new_tool(name, summary));
        self.follow_tail();
    }

    /// A tool call finished — backfills the matching running block.
    pub fn tool_done(&mut self, name: &str, ok: bool, output: &str) {
        if let Some(b) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| b.is_running_tool(name))
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
    /// never got its ToolDone — call it interrupted.
    pub fn close_turn(&mut self) {
        for b in &mut self.blocks {
            b.open = false;
            if b.tool.as_ref().is_some_and(|t| t.done.is_none()) {
                b.finish_tool(false, "interrupted");
            }
        }
        self.busy = false;
    }

    pub fn push_note(&mut self, note: &str) {
        self.busy = false;
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
    fn echo_user(&mut self, text: &str) {
        let mut b = Block::new(BlockKind::User);
        b.text = text.to_string();
        self.blocks.push(b);
    }

    // ── composer ─────────────────────────────────────────────────────────

    pub fn insert_char(&mut self, c: char) {
        let byte_idx = char_to_byte(&self.input, self.cursor);
        self.input.insert(byte_idx, c);
        self.cursor += 1;
        self.refresh_slash_menu();
    }

    /// Bulk insert (bracketed paste): one splice + one menu refresh instead
    /// of per-char O(n²).
    pub fn insert_str(&mut self, s: &str) {
        let byte_idx = char_to_byte(&self.input, self.cursor);
        self.input.insert_str(byte_idx, s);
        self.cursor += s.chars().count();
        self.refresh_slash_menu();
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
        self.slash_menu = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let byte_idx = char_to_byte(&self.input, self.cursor - 1);
            self.input.remove(byte_idx);
            self.cursor -= 1;
            self.refresh_slash_menu();
        }
    }

    /// The composer is a slash fragment: starts with `/`, no whitespace yet.
    /// Bash mode owns the buffer, so no slash menu there.
    fn slash_fragment(&self) -> Option<&str> {
        if self.bash_mode {
            return None;
        }
        self.input
            .strip_prefix('/')
            .filter(|s| !s.chars().any(char::is_whitespace))
    }

    pub fn refresh_slash_menu(&mut self) {
        match self.slash_fragment() {
            Some(frag) => {
                let matches = slash::candidates(&self.cwd)
                    .into_iter()
                    .filter(|c| c.starts_with(frag) || c.contains(frag))
                    .collect::<Vec<_>>();
                if matches.is_empty() {
                    self.slash_menu = None;
                } else {
                    let sel = self
                        .slash_menu
                        .as_ref()
                        .map(|m| m.selected.min(matches.len() - 1))
                        .unwrap_or(0);
                    self.slash_menu = Some(SlashMenu {
                        matches,
                        selected: sel,
                        fragment: frag.to_string(),
                    });
                }
            }
            None => self.slash_menu = None,
        }
    }

    /// Take the draft and decide where it goes. Echo + history happen here.
    pub fn submit(&mut self) -> Submit {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.hist_idx = None;
        self.slash_menu = None;
        if self.bash_mode {
            self.bash_mode = false;
            let cmd = text.trim();
            if cmd.is_empty() {
                return Submit::Note(String::new());
            }
            self.history.push(format!("!{cmd}"));
            // no user-band echo — the local-shell block header already
            // shows `$ cmd`; two copies would be noise.
            return Submit::Bash(cmd.to_string());
        }
        if text.trim().is_empty() {
            return Submit::Note(String::new());
        }
        self.history.push(text.clone());
        self.echo_user(&text);

        // literal `!cmd` works without entering bash mode — same route as a
        // recalled `!`-history entry or a pasted line.
        if let Some(cmd) = text.trim().strip_prefix('!') {
            return if cmd.trim().is_empty() {
                Submit::Note(String::new())
            } else {
                Submit::Bash(cmd.trim().to_string())
            };
        }

        let trimmed = text.trim();
        match trimmed.strip_prefix('/') {
            Some(cmd_line) => {
                let name = cmd_line.split_whitespace().next().unwrap_or("");
                match name {
                    "quit" | "exit" => Submit::Quit,
                    "compact" => Submit::Compact,
                    "multiline" | "ml" => {
                        self.multiline = !self.multiline;
                        Submit::Note(format!(
                            "[multiline {}]",
                            if self.multiline { "on" } else { "off" }
                        ))
                    }
                    // file commands resolve in the driver (needs cwd)
                    _ => Submit::Turn(format!("/{cmd_line}")),
                }
            }
            None => Submit::Turn(text),
        }
    }

    // ── scrollback selection / folds ─────────────────────────────────────

    pub fn ensure_selection(&mut self) {
        if self.blocks.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.blocks.len() - 1);
        }
    }

    pub fn select_delta(&mut self, d: isize) {
        self.ensure_selection();
        let max = self.blocks.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + d).clamp(0, max) as usize;
    }

    pub fn toggle_fold(&mut self) {
        self.ensure_selection();
        if let Some(b) = self.blocks.get_mut(self.selected) {
            b.collapsed = !b.collapsed;
        }
    }

    pub fn selected_copy(&self) -> Option<String> {
        self.blocks.get(self.selected).map(|b| b.copy_text())
    }

    /// Enter the full-screen viewer for the selected block — the "expand"
    /// half of scrollback browsing, the transcript itself stays compact.
    pub fn open_viewer(&mut self) {
        self.ensure_selection();
        if let Some(b) = self.blocks.get(self.selected) {
            let title = match &b.tool {
                Some(t) => format!("{} {}", t.name, t.summary),
                None => format!("{:?}", b.kind).to_lowercase(),
            };
            self.viewer = Some(Viewer {
                title,
                body: b.copy_text(),
                scroll: 0,
            });
            self.focus = Focus::Viewer;
        }
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
                E::Started { .. } | E::Usage { .. } | E::Artifact { .. } => {}
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
                E::ToolCall { call } => {
                    let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                        .unwrap_or(serde_json::Value::Null);
                    self.tool_start(
                        &call.function.name,
                        &sunmao_core::agent::call_summary(&call.function.name, &args),
                    );
                }
                E::ToolResult {
                    name, ok, output, ..
                } => self.tool_done(name, *ok, output),
                E::Hook { event, detail } => {
                    self.push_audit(&format!("{event} — {detail}"));
                }
                E::LocalShell {
                    command,
                    exit_code,
                    output,
                } => {
                    self.tool_start("!", &format!("$ {command}"));
                    self.tool_done("!", *exit_code == 0, output);
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

    // ── scroll helpers ───────────────────────────────────────────────────

    /// Keep pinned to the bottom when the user hasn't scrolled away.
    fn follow_tail(&mut self) {
        if self.scroll_back == 0 && self.focus != Focus::Scrollback {
            // nothing — Paragraph::scroll(0) already shows the tail
        }
    }

    // ── misc ─────────────────────────────────────────────────────────────

    /// Esc semantics, layered like grok-build but slimmer:
    /// card Esc → park; scrollback Esc → back to input; busy Esc → hint;
    /// input Esc → double-tap clears the draft (stashed, Ctrl+S restores).
    /// Returns true when the app should quit (no — Esc never quits).
    pub fn on_esc(&mut self) {
        match self.focus {
            Focus::Approval => {
                if let Some(card) = &mut self.approval {
                    card.parked = true;
                }
                self.focus = Focus::Scrollback;
                self.toast = Some(("card parked — Tab returns".into(), Instant::now()));
            }
            Focus::Scrollback => {
                self.focus = Focus::Input;
            }
            Focus::Viewer => {
                self.viewer = None;
                self.focus = Focus::Scrollback;
            }
            Focus::Input => {
                if self.busy {
                    self.toast = Some(("Ctrl-C cancels the turn".into(), Instant::now()));
                } else if !self.input.is_empty() {
                    let now = Instant::now();
                    if self
                        .last_esc
                        .is_some_and(|t| now.duration_since(t) < Duration::from_millis(800))
                    {
                        self.draft_stash = Some(std::mem::take(&mut self.input));
                        self.cursor = 0;
                        self.toast =
                            Some(("draft stashed — Ctrl+S restores".into(), Instant::now()));
                        self.last_esc = None;
                    } else {
                        self.last_esc = Some(now);
                        self.toast = Some(("Esc again clears the draft".into(), Instant::now()));
                    }
                }
            }
        }
    }

    pub fn restore_draft(&mut self) {
        if let Some(d) = self.draft_stash.take() {
            self.input = d;
            self.cursor = self.input.chars().count();
        }
    }

    /// Age-out the toast after 3 s.
    pub fn toast_text(&mut self) -> Option<&str> {
        if let Some((_, t)) = self.toast {
            if t.elapsed() > Duration::from_secs(3) {
                self.toast = None;
            }
        }
        self.toast.as_ref().map(|(s, _)| s.as_str())
    }
}

pub fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

/// OSC 52 clipboard write — works over SSH/most modern terminals.
pub fn osc52_copy(text: &str) -> bool {
    use base64::Engine;
    use std::io::Write;
    let mut out = std::io::stdout();
    let payload = base64::engine::general_purpose::STANDARD.encode(text);
    write!(out, "\x1b]52;c;{payload}\x07").is_ok() && out.flush().is_ok()
}
