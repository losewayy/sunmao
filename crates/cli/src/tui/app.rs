//! Application state: the block transcript, composer, focus machine,
//! approval card, and slash menu. The event→block fold (live streams and
//! session replay) lives in `replay.rs`; rendering decisions live in
//! `mod.rs` — this file owns *what is true*, not how it looks.

use std::time::{Duration, Instant};

use super::blocks::{Block, BlockKind};
use super::menu::SlashMenu;

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
    /// draft stashed while browsing history — Down past the newest entry
    /// restores it instead of landing on an empty composer.
    pub hist_draft: Option<String>,
    pub busy: bool,
    pub focus: Focus,
    /// index into `blocks` while Focus::Scrollback
    pub selected: usize,
    pub approval: Option<ApprovalCard>,
    /// approval requests that arrived while a card (or parked card) was
    /// pending — resolved in arrival order, never dropped. A dropped
    /// oneshot resolves to Deny on the core side, which would silently
    /// refuse a call the user never saw.
    pub approval_backlog: std::collections::VecDeque<ApprovalCard>,
    pub slash_menu: Option<SlashMenu>,
    /// multiline composer: Enter inserts \n, Alt/Shift+Enter sends
    pub multiline: bool,
    /// transient status text shown in the status bar (e.g. copy confirm)
    pub toast: Option<(String, Instant)>,
    /// last Esc press for the double-Esc-clears-draft gesture
    pub last_esc: Option<Instant>,
    /// stash of a draft cleared by double-Esc (Ctrl+S restores)
    pub draft_stash: Option<String>,
    /// first idle Ctrl-C arms this; a second within 800ms actually quits
    pub quit_armed: Option<Instant>,
    /// git branch of `cwd`, probed at startup (None = not a repo / no git)
    pub git_branch: Option<String>,
    /// last observed provider usage — the footer renders prompt tokens as
    /// "context filled" so the user sees context pressure before it bites.
    pub last_usage: Option<sunmao_llm::types::Usage>,
    /// `!` bash mode: the composer holds a shell command; submit wraps it
    /// for the Bash tool instead of sending it as a prompt.
    pub bash_mode: bool,
    /// Large-paste stash: pastes ≥ PASTE_STASH_LIMIT insert `[paste #N]`
    /// markers instead of raw text; `expand_pastes` inlines the content
    /// at submit time so the model gets the bytes, not the placeholder.
    /// Session-scoped on purpose — a paste is input, not an artifact.
    pub paste_stash: Vec<String>,
    /// turns/bash submitted while a turn was running — the queue holds the
    /// actual submissions (footer previews the head), not just a count, so
    /// `↑` on an empty composer can recall the tail item for editing.
    /// Recall sends Submit::Flush so the driver drops its matching copy —
    /// the turn never runs twice.
    pub queue: std::collections::VecDeque<Submit>,
    /// per-block wrapped-render cache, parallel to `blocks` — keyed on each
    /// block's `gen` + draw width so streaming only re-renders its own block.
    pub render_cache: Vec<Option<RenderEntry>>,
    /// Full-screen viewer: (title, body) of the block being read. Lives in
    /// app state so render stays pure.
    pub viewer: Option<Viewer>,
    /// when the current turn started working — footer shows elapsed time
    pub busy_since: Option<Instant>,
    /// completable `/model` selectors (`@routes` + `provider/` prefixes) —
    /// the driver fills this once at startup; slash-menu arg completion
    /// filters it.
    pub model_selectors: Vec<String>,
    /// enabled preset plugin roots — slash commands resolve against their
    /// `commands/` dirs too. The driver sets this once at startup.
    pub extra_roots: Vec<std::path::PathBuf>,
    /// repo-relative path pool for `@` mention completion — rebuilt when
    /// the path menu opens, kept while it stays open (a stale entry is a
    /// hint, the model's Read is ground truth).
    pub file_pool: Vec<String>,
    /// session ids for `/resume` completion — rescanned when the sessions
    /// menu opens (sub-agent sessions land mid-session).
    pub session_ids: Vec<String>,
}

/// Content of the full-screen viewer — title line + the block's full text
/// (copy_text: header + complete output, not the 5-line preview).
pub struct Viewer {
    pub title: String,
    pub body: String,
    /// visual lines scrolled from the top
    pub scroll: u16,
}

/// `/help` body — the keymap cheat-sheet. Short on purpose: the footer
/// already narrates the active focus's keys.
const HELP_TEXT: &str = "keys — Tab browse blocks · Enter expand · e fold · y copy · \
g/G ends · ! bash · / commands · Esc×2 stash draft · Ctrl+S restore · \
Ctrl+A/E/U/W line edit · Ctrl-C cancel, ×2 quits
commands — /compact · /model · /multiline · /clear · /resume [id] · /tasks · /todos · /artifacts · /annotate · /help · /quit · \
+ every *.md in .sunmao/commands, .claude/commands, plugins/*/commands";

/// Pastes at or above this many bytes stash into `paste_stash` and insert
/// a `[paste #N]` marker instead of raw text — the composer stays small
/// and the model gets the full content at submit.
const PASTE_STASH_LIMIT: usize = 2048;

/// One cached transcript entry: the block's `gen` and the width/selection
/// it was wrapped at, plus the wrapped lines themselves.
type RenderEntry = (u64, usize, bool, Vec<ratatui::text::Line<'static>>);

/// How a submitted line should be dispatched — the driver task interprets.
#[derive(Debug, Clone)]
pub enum Submit {
    /// normal prompt or a resolved command body — goes to run_turn
    Turn(String),
    /// `!` local shell — run directly, never a model turn
    Bash(String),
    /// /resume [id|path] — swap the session log; bare = list recent
    Resume(Option<String>),
    /// /fork <id|path> — copy the log to a fresh id, resume the copy
    Fork(Option<String>),
    /// /compact
    Compact,
    /// command name didn't resolve — show a note, no turn
    Note(String),
    /// /quit or /exit
    Quit,
    /// drop everything still waiting in the submission channel — the app
    /// sends this after recalling queued items for editing.
    Flush,
    /// /model [selector] — None lists choices, Some swaps the active adapter
    Model(Option<String>),
    /// /tasks — the live sub-agent roster
    Tasks,
    /// /todos — the model's session task list
    Todos,
    /// /artifacts — the .sunmao/artifacts listing
    Artifacts,
    /// /annotate <name> <note> — human notes into artifact state.json
    Annotate(String, String),
}

impl App {
    pub fn new(model: &str, cwd: std::path::PathBuf, session_id: &str) -> Self {
        let mut app = Self {
            cwd,
            model: model.to_string(),
            blocks: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll_back: 0,
            history: Vec::new(),
            hist_idx: None,
            hist_draft: None,
            busy: false,
            focus: Focus::Input,
            selected: 0,
            approval: None,
            approval_backlog: std::collections::VecDeque::new(),
            slash_menu: None,
            multiline: false,
            toast: None,
            last_esc: None,
            draft_stash: None,
            quit_armed: None,
            git_branch: None,
            last_usage: None,
            bash_mode: false,
            paste_stash: Vec::new(),
            queue: std::collections::VecDeque::new(),
            render_cache: Vec::new(),
            viewer: None,
            busy_since: None,
            model_selectors: Vec::new(),
            extra_roots: Vec::new(),
            file_pool: Vec::new(),
            session_ids: Vec::new(),
        };
        let mut banner = Block::new(BlockKind::Note);
        banner.text = format!(
            "sunmao TUI — {model} · session {session_id}\nEnter send · Ctrl-C×2 quits · Tab browse blocks · / commands"
        );
        app.blocks.push(banner);
        app
    }

    /// A new approval request while a card is pending — active or parked —
    /// queues behind it in arrival order. The card itself never gets
    /// swapped out from under the user.
    pub fn queue_approval(&mut self, card: ApprovalCard) {
        if self.approval.is_none() {
            self.approval = Some(card);
            self.focus = Focus::Approval;
        } else {
            self.approval_backlog.push_back(card);
            self.toast(format!(
                "another approval queued ({} waiting)",
                self.approval_backlog.len()
            ));
        }
    }

    /// After a verdict, surface the next queued request if any.
    pub fn pop_approval(&mut self) {
        if let Some(next) = self.approval_backlog.pop_front() {
            self.approval = Some(next);
            self.focus = Focus::Approval;
        }
    }

    /// `↑` on an empty composer while items wait: pull the tail back into
    /// the editor for revision — the driver drops its copy via Flush, so
    /// the recalled item doesn't run twice. Returns the restored text.
    pub fn recall_queued(&mut self) -> Option<String> {
        if !self.input.is_empty() || self.hist_idx.is_some() {
            return None;
        }
        let sub = self.queue.pop_back()?;
        let text = match sub {
            Submit::Turn(t) => t,
            Submit::Bash(c) => {
                self.bash_mode = true;
                c
            }
            sub => {
                // not editable (Model/Resume/Compact) — put it back, nothing
                // the user could type would improve it
                self.queue.push_back(sub);
                return None;
            }
        };
        self.input = text.clone();
        self.cursor = text.chars().count();
        Some(text)
    }

    // ── composer ─────────────────────────────────────────────────────────

    pub fn insert_char(&mut self, c: char) {
        let byte_idx = char_to_byte(&self.input, self.cursor);
        self.input.insert(byte_idx, c);
        self.cursor += 1;
        self.refresh_slash_menu();
    }

    /// Bulk insert (bracketed paste): one splice + one menu refresh instead
    /// of per-char O(n²). Large pastes stash and insert a `[paste #N]`
    /// marker — the composer stays editable and the content rides at
    /// submit (expand_pastes), kimi-style.
    pub fn insert_str(&mut self, s: &str) {
        let s = if s.len() >= PASTE_STASH_LIMIT {
            self.paste_stash.push(s.to_string());
            let marker = format!("[paste #{}]", self.paste_stash.len());
            self.toast(format!(
                "pasted {} chars → {} (expands on send)",
                s.len(),
                marker
            ));
            marker
        } else {
            s.to_string()
        };
        let byte_idx = char_to_byte(&self.input, self.cursor);
        self.input.insert_str(byte_idx, &s);
        self.cursor += s.chars().count();
        self.refresh_slash_menu();
    }

    /// Inline `[paste #N]` markers with their stashed content, wrapped in
    /// a tagged block the model can parse. Unknown/removed markers are
    /// left as literal text — never invent content.
    fn expand_pastes(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (i, content) in self.paste_stash.iter().enumerate() {
            let marker = format!("[paste #{}]", i + 1);
            if out.contains(&marker) {
                out = out.replace(
                    &marker,
                    &format!("\n<pasted-text>\n{content}\n</pasted-text>\n"),
                );
            }
        }
        out
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

    /// Take the draft and decide where it goes. Echo + history happen here.
    pub fn submit(&mut self) -> Submit {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.hist_idx = None;
        self.hist_draft = None;
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
        // recalled `!`-history entry or a pasted line. Markers stay literal
        // here: the local shell isn't the model's pasted-text convention.
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
                    "help" | "h" | "?" => Submit::Note(HELP_TEXT.to_string()),
                    "clear" => {
                        self.blocks.clear();
                        self.paste_stash.clear();
                        self.selected = 0;
                        self.scroll_back = 0;
                        Submit::Note("[transcript cleared — session log untouched]".into())
                    }
                    // /resume needs the session dir + agent — driver-side
                    "resume" | "sessions" => {
                        let arg = cmd_line.split_whitespace().nth(1).map(|s| s.to_string());
                        Submit::Resume(arg)
                    }
                    // /fork copies the log to a fresh id, resumes the copy
                    "fork" => {
                        let arg = cmd_line.split_whitespace().nth(1).map(|s| s.to_string());
                        Submit::Fork(arg)
                    }
                    // /model resolves through the session's ModelResolver —
                    // only the driver holds the agent.
                    "model" => {
                        let arg = cmd_line.split_whitespace().nth(1).map(|s| s.to_string());
                        Submit::Model(arg)
                    }
                    // /tasks — the live sub-agent roster, driver-side too
                    "tasks" => Submit::Tasks,
                    // /todos — the model's task list, driver-side too
                    "todos" => Submit::Todos,
                    // /artifacts — .sunmao/artifacts listing, driver-side
                    "artifacts" => Submit::Artifacts,
                    // /annotate <name> <note> — margin notes for the agent
                    "annotate" => {
                        let mut it = cmd_line.splitn(3, char::is_whitespace);
                        let _ = it.next(); // command name
                        match (it.next(), it.next()) {
                            (Some(name), Some(note)) => {
                                Submit::Annotate(name.to_string(), note.trim().to_string())
                            }
                            _ => Submit::Note("[usage: /annotate <name> <note>]".into()),
                        }
                    }
                    // file commands resolve in the driver (needs cwd) —
                    // paste markers expand too, $ARGUMENTS flows through
                    _ => Submit::Turn(self.expand_pastes(&format!("/{cmd_line}"))),
                }
            }
            None => Submit::Turn(self.expand_pastes(&text)),
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
        if self
            .blocks
            .get(self.selected)
            .is_some_and(|b| b.kind == BlockKind::StepSummary)
        {
            // expanding a step summary splices its folded blocks back where
            // it sat — one-way by design, close_turn refolds if still over
            // the cap.
            let mut summary = self.blocks.remove(self.selected);
            let mut tail = self.blocks.split_off(self.selected);
            self.blocks.append(&mut summary.folded);
            self.blocks.append(&mut tail);
            self.render_cache.clear();
            self.ensure_selection();
            return;
        }
        if let Some(b) = self.blocks.get_mut(self.selected) {
            b.collapsed = !b.collapsed;
            b.generation += 1;
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

    /// Set a transient status-bar toast (copy confirm, queue notice).
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    /// Age-out the toast after 3 s.
    pub fn toast_text(&mut self) -> Option<&str> {
        if let Some((_, t)) = self.toast
            && t.elapsed() > Duration::from_secs(3) {
                self.toast = None;
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
