//! Transcript blocks — the unit the scrollback is made of. Streaming text
//! accumulates into a block; the block renders to styled `Line`s (markdown
//! for assistant text) and carries fold/copy semantics.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::md;
use super::theme::{self, THEME};

/// The kind decides chrome (prefix glyph + base style) and fold default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// `❯ prompt` the user sent — full-width band
    User,
    /// Assistant text — markdown-rendered
    Assistant,
    /// Reasoning channel — dim, starts folded
    Thinking,
    /// One tool invocation: header + arg digest + output preview panel
    Tool,
    /// Frontend status line ([compacted], [unknown command], …)
    Note,
}

/// A tool block keeps structured fields instead of one status string so the
/// render can lay out header / digest / output panel separately. `done`
/// doubles as the lifecycle: `None` = still running.
#[derive(Debug)]
pub struct ToolBlock {
    pub name: String,
    /// one-line argument digest (the bash command, a path, …)
    pub summary: String,
    /// truncated tool output — rendered as a preview panel
    pub output: String,
    /// None = running; Some(ok) = finished with that verdict
    pub done: Option<bool>,
    /// consecutive calls of the same name folded into this block
    pub group_count: usize,
}

#[derive(Debug)]
pub struct Block {
    pub kind: BlockKind,
    /// Accumulated raw text (assistant/thinking/user/note).
    pub text: String,
    /// Tool blocks carry the structured invocation here; `text` stays empty.
    pub tool: Option<ToolBlock>,
    /// Collapsed shows the first line + a fold hint.
    pub collapsed: bool,
    /// A streaming block accepts appended chunks until the turn ends.
    pub open: bool,
}

/// Output preview cap, in lines — the full text stays in the session log.
const PREVIEW_LINES: usize = 5;
/// Echoed user prompts collapse after this many lines (grok-style).
const USER_CAP: usize = 4;

impl Block {
    pub fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            text: String::new(),
            tool: None,
            collapsed: matches!(kind, BlockKind::Thinking),
            open: matches!(kind, BlockKind::Assistant | BlockKind::Thinking),
        }
    }

    pub fn new_tool(name: &str, summary: &str) -> Self {
        let mut b = Self::new(BlockKind::Tool);
        b.tool = Some(ToolBlock {
            name: name.to_string(),
            summary: summary.to_string(),
            output: String::new(),
            done: None,
            group_count: 1,
        });
        b
    }

    /// Is this a tool block for `name` still awaiting its ToolDone?
    pub fn is_running_tool(&self, name: &str) -> bool {
        self.kind == BlockKind::Tool
            && self
                .tool
                .as_ref()
                .is_some_and(|t| t.name == name && t.done.is_none())
    }

    /// Verb-grouping: a finished tool block for `name` gets re-armed by the
    /// next same-name call — visually folding a run of identical tools.
    /// Prior outputs stay in the panel; the newest digest leads the header.
    pub fn rearm_tool(&mut self, summary: &str) {
        if let Some(t) = &mut self.tool {
            t.done = None;
            t.summary = summary.to_string();
            t.group_count += 1;
        }
    }

    /// Record the verdict + (truncated) output of a finished call. Grouped
    /// calls append with a thin separator so the panel shows every run.
    pub fn finish_tool(&mut self, ok: bool, output: &str) {
        if let Some(t) = &mut self.tool {
            t.done = Some(ok);
            if output.is_empty() {
                return;
            }
            if t.output.is_empty() {
                t.output = output.to_string();
            } else {
                t.output.push_str("\n  ──\n");
                t.output.push_str(output);
            }
        }
    }

    /// Lines this block contributes, honoring the fold. `width` lets banded
    /// blocks (user prompt, tool output) pad to the full row.
    pub fn render(&self, selected: bool, width: usize) -> Vec<Line<'static>> {
        let mut out = if self.collapsed
            && matches!(self.kind, BlockKind::Assistant | BlockKind::User)
        {
            self.render_folded()
        } else {
            match self.kind {
                BlockKind::User => self.render_user(width),
                BlockKind::Assistant => self.render_assistant(),
                BlockKind::Thinking => self.render_plain("◌ ", Style::default().fg(THEME.thinking)),
                BlockKind::Tool => self.render_tool(width),
                BlockKind::Note => self.render_plain("· ", Style::default().fg(THEME.faint)),
            }
        };

        // breathing room between blocks
        out.push(Line::from(""));

        if selected {
            for line in &mut out {
                line.spans
                    .insert(0, Span::styled("▎", Style::default().fg(THEME.sel_rail)));
                line.style = line.style.patch(Style::default().bg(THEME.sel_bg));
            }
        }
        out
    }

    /// Generic fold: first line + hidden-count hint (assistant/user blocks).
    fn render_folded(&self) -> Vec<Line<'static>> {
        let first = self.text.lines().next().unwrap_or("");
        let hidden = self.text.lines().count().saturating_sub(1);
        let base = match self.kind {
            BlockKind::User => Style::default()
                .fg(THEME.user)
                .bg(THEME.band_bg)
                .add_modifier(Modifier::BOLD),
            _ => Style::default(),
        };
        vec![Line::from(vec![
            Span::styled(first.to_string(), base),
            Span::styled(
                format!("  ▶ {hidden} hidden — e to expand"),
                Style::default()
                    .fg(THEME.faint)
                    .add_modifier(Modifier::ITALIC),
            ),
        ])]
    }

    /// User prompt: `❯ text` on a full-width band — the visual anchor that
    /// makes your own messages pop out of the transcript.
    fn render_user(&self, width: usize) -> Vec<Line<'static>> {
        let band = Style::default().bg(THEME.band_bg);
        let mut out = Vec::new();
        let lines: Vec<&str> = self.text.lines().collect();
        let hidden = lines.len().saturating_sub(USER_CAP);
        for (i, l) in lines.iter().take(USER_CAP).enumerate() {
            let prefix = if i == 0 { theme::prompt_glyph() } else { "  " };
            let used = UnicodeWidthStr::width(prefix) + UnicodeWidthStr::width(*l);
            let pad = width.saturating_sub(used);
            let mut spans = vec![
                Span::styled(
                    prefix.to_string(),
                    band.fg(THEME.user).add_modifier(Modifier::BOLD),
                ),
                Span::styled((*l).to_string(), band.fg(THEME.text)),
            ];
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), band));
            }
            out.push(Line::from(spans).style(band));
        }
        if hidden > 0 {
            out.push(band_line(
                &format!("  … {hidden} more lines"),
                band.fg(THEME.faint).add_modifier(Modifier::ITALIC),
                width,
            ));
        }
        if out.is_empty() {
            out.push(band_line(
                theme::prompt_glyph(),
                band.fg(THEME.user).add_modifier(Modifier::BOLD),
                width,
            ));
        }
        out
    }

    fn render_assistant(&self) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = md::render(&self.text);
        if self.open {
            out.push(Line::from(Span::styled(
                "▌",
                Style::default().fg(THEME.muted),
            )));
        }
        out
    }

    /// glyph-prefixed dim block (thinking / note)
    fn render_plain(&self, glyph: &'static str, base: Style) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if self.collapsed {
            let first = self.text.lines().next().unwrap_or("");
            let hidden = self.text.lines().count().saturating_sub(1);
            let mut spans = vec![Span::styled(format!("{glyph}{first}"), base)];
            if hidden > 0 {
                spans.push(Span::styled(
                    format!("  ▶ {hidden} hidden — e to expand"),
                    Style::default()
                        .fg(THEME.faint)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            out.push(Line::from(spans));
            return out;
        }
        for (i, line) in self.text.lines().enumerate() {
            let prefix = if i == 0 { glyph } else { "  " };
            out.push(Line::from(Span::styled(format!("{prefix}{line}"), base)));
        }
        if self.open {
            out.push(Line::from(Span::styled(
                "  ▌",
                Style::default().fg(THEME.muted),
            )));
        }
        out
    }

    /// Tool block: `✓ Name digest` header (verb-grouped runs show `×N`),
    /// then a dim output panel capped at PREVIEW_LINES.
    fn render_tool(&self, width: usize) -> Vec<Line<'static>> {
        let Some(t) = &self.tool else {
            return Vec::new();
        };
        let (glyph, gstyle) = match t.done {
            None => ("◌", Style::default().fg(THEME.running)),
            Some(true) => ("✓", Style::default().fg(THEME.ok)),
            Some(false) => ("✗", Style::default().fg(THEME.err)),
        };
        let mut spans = vec![
            Span::styled(format!(" {glyph} "), gstyle),
            Span::styled(
                t.name.clone(),
                Style::default().fg(THEME.tool).add_modifier(Modifier::BOLD),
            ),
        ];
        if t.group_count > 1 {
            spans.push(Span::styled(
                format!(" ×{}", t.group_count),
                Style::default().fg(THEME.muted),
            ));
        }
        if !t.summary.is_empty() {
            spans.push(Span::styled(
                format!("  {}", t.summary),
                Style::default().fg(THEME.muted),
            ));
        }
        if t.done.is_none() {
            spans.push(Span::styled("  …", Style::default().fg(THEME.running)));
        }
        let mut out = vec![Line::from(spans)];

        if t.output.is_empty() {
            return out;
        }
        if self.collapsed {
            out.push(Line::from(Span::styled(
                "    ▶ output folded — e to expand",
                Style::default()
                    .fg(THEME.faint)
                    .add_modifier(Modifier::ITALIC),
            )));
            return out;
        }
        let failed = t.done == Some(false);
        let panel = Style::default()
            .fg(if failed { THEME.err } else { THEME.muted })
            .bg(THEME.panel_bg);
        let lines: Vec<&str> = t.output.lines().collect();
        for l in lines.iter().take(PREVIEW_LINES) {
            out.push(band_line(&format!("  {l}"), panel, width));
        }
        if lines.len() > PREVIEW_LINES {
            out.push(band_line(
                &format!("  ⋯ {} more lines", lines.len() - PREVIEW_LINES),
                panel.fg(THEME.faint).add_modifier(Modifier::ITALIC),
                width,
            ));
        }
        out
    }

    /// Plain text for OSC52 copy — tool blocks copy header + full output.
    pub fn copy_text(&self) -> String {
        match &self.tool {
            Some(t) => format!("{} {}\n{}", t.name, t.summary, t.output),
            None => self.text.clone(),
        }
    }
}

/// One full-width padded line on `bg` — the "band" primitive.
fn band_line(text: &str, style: Style, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(UnicodeWidthStr::width(text));
    Line::from(vec![
        Span::styled(text.to_string(), style),
        Span::styled(" ".repeat(pad), style),
    ])
    .style(style)
}
