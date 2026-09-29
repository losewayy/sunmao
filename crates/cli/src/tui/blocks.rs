//! Transcript blocks — the unit the scrollback is made of. Streaming text
//! accumulates into a block; the block renders to styled `Line`s (markdown
//! for assistant text) and carries fold/copy semantics.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::md;

/// The kind decides chrome (prefix glyph + base style) and fold default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// `> prompt` the user sent
    User,
    /// Assistant text — markdown-rendered
    Assistant,
    /// Reasoning channel — dim, starts folded
    Thinking,
    /// One tool invocation (`→ name` → `✓ name` / `✗ name`)
    Tool,
    /// Frontend status line ([compacted], [unknown command], …)
    Note,
}

#[derive(Debug)]
pub struct Block {
    pub kind: BlockKind,
    /// Accumulated raw text. Tool blocks carry the status line itself.
    pub text: String,
    /// For thinking blocks: append target for streaming. Tools don't stream.
    pub collapsed: bool,
    /// A streaming block accepts appended chunks until the turn ends.
    pub open: bool,
}

impl Block {
    pub fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            text: String::new(),
            collapsed: matches!(kind, BlockKind::Thinking),
            open: matches!(kind, BlockKind::Assistant | BlockKind::Thinking),
        }
    }

    /// Chrome for this block's first line.
    fn glyph(&self) -> &'static str {
        match self.kind {
            BlockKind::User => "› ",
            BlockKind::Assistant => "",
            BlockKind::Thinking => "◌ ",
            BlockKind::Tool => "  ",
            BlockKind::Note => "· ",
        }
    }

    fn base_style(&self) -> Style {
        match self.kind {
            BlockKind::User => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            BlockKind::Assistant => Style::default(),
            BlockKind::Thinking => Style::default().fg(Color::DarkGray),
            BlockKind::Tool => Style::default().fg(Color::Cyan),
            BlockKind::Note => Style::default().fg(Color::DarkGray),
        }
    }

    /// Lines this block contributes, honoring the fold. `selected` paints a
    /// left rail on every visible line.
    pub fn render(&self, selected: bool) -> Vec<Line<'static>> {
        let base = self.base_style();
        let glyph = self.glyph();
        let mut out: Vec<Line<'static>> = Vec::new();

        if self.collapsed {
            let hidden = self.text.lines().count().saturating_sub(1);
            let mut spans = vec![Span::styled(
                format!("{glyph}{}", self.text.lines().next().unwrap_or("")),
                base,
            )];
            if hidden > 0 || self.open {
                spans.push(Span::styled(
                    format!("  ▶ {hidden} hidden — e to expand"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            out.push(Line::from(spans));
        } else if self.kind == BlockKind::Assistant {
            // markdown-aware render; fallback to raw when empty
            for l in md::render(&self.text) {
                out.push(l);
            }
            if self.open {
                out.push(Line::from(Span::styled("▌", base.fg(Color::Gray))));
            }
        } else {
            for (i, line) in self.text.lines().enumerate() {
                let prefix = if i == 0 { glyph } else { "  " };
                out.push(Line::from(Span::styled(format!("{prefix}{line}"), base)));
            }
            if self.open {
                out.push(Line::from(Span::styled("  ▌", base.fg(Color::Gray))));
            }
        }

        // separator between blocks keeps the transcript breathable
        out.push(Line::from(""));

        if selected {
            for line in &mut out {
                line.spans
                    .insert(0, Span::styled("▎", Style::default().fg(Color::Magenta)));
                line.style = line
                    .style
                    .patch(Style::default().bg(Color::Rgb(36, 33, 46)));
            }
        }
        out
    }

    /// Plain text for OSC52 copy — raw markdown for assistants, status for
    /// tools, glyph-free for everything.
    pub fn copy_text(&self) -> String {
        self.text.clone()
    }
}
