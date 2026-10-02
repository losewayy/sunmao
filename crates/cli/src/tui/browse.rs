//! Scrollback browsing + surface gestures — the block selection, fold
//! toggle, full-screen viewer, layered Esc semantics, draft stash and
//! toast helpers. Split from `app.rs` under the file-size budget: App's
//! fields stay there, this module owns the verbs that walk them.

use std::time::{Duration, Instant};

use super::app::{App, Focus, Viewer};
use super::blocks::BlockKind;

impl App {
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
            && t.elapsed() > Duration::from_secs(3)
        {
            self.toast = None;
        }
        self.toast.as_ref().map(|(s, _)| s.as_str())
    }
}
