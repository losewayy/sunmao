//! Minimal markdown → ratatui `Line` renderer for assistant blocks.
//! Covers what coding-agent output actually uses: headings, bold/italic/
//! strike inline emphasis, `code` spans, fenced code blocks, lists, quotes,
//! rules. Anything else degrades to plain text — never fails.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::THEME;

pub fn render(src: &str) -> Vec<Line<'static>> {
    if src.trim().is_empty() {
        return Vec::new();
    }
    let opts = Options::ENABLE_STRIKETHROUGH;
    let mut lines: Vec<Line<'static>> = Vec::new();
    // the line under construction — styled runs appended as spans
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut in_code_block = false;
    let mut code_buf = String::new();
    let mut list_prefix = String::new();
    let mut in_quote = false;
    let mut para_open = false;

    let flush = |lines: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>| {
        if !spans.is_empty() {
            lines.push(Line::from(std::mem::take(spans)));
        }
    };

    for ev in Parser::new_ext(src, opts) {
        match ev {
            Event::Start(Tag::Paragraph) => para_open = true,
            Event::End(TagEnd::Paragraph) => {
                flush(&mut lines, &mut spans);
                para_open = false;
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush(&mut lines, &mut spans);
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                let code_style = Style::default().fg(THEME.code).bg(THEME.panel_bg);
                for l in code_buf.trim_end_matches('\n').split('\n') {
                    lines.push(Line::from(Span::styled(format!(" {l} "), code_style)));
                }
                code_buf.clear();
            }
            Event::Start(Tag::Heading { .. }) => {
                style = style
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    .fg(THEME.assistant);
            }
            Event::End(TagEnd::Heading(_)) => {
                style = Style::default();
                flush(&mut lines, &mut spans);
            }
            Event::Start(Tag::Strong) => style = style.add_modifier(Modifier::BOLD),
            Event::End(TagEnd::Strong) => style = style.remove_modifier(Modifier::BOLD),
            Event::Start(Tag::Emphasis) => style = style.add_modifier(Modifier::ITALIC),
            Event::End(TagEnd::Emphasis) => style = style.remove_modifier(Modifier::ITALIC),
            Event::Start(Tag::Strikethrough) => style = style.add_modifier(Modifier::CROSSED_OUT),
            Event::End(TagEnd::Strikethrough) => {
                style = style.remove_modifier(Modifier::CROSSED_OUT)
            }
            Event::Start(Tag::Item) => {
                flush(&mut lines, &mut spans);
                spans.push(Span::styled(list_prefix.clone(), Style::default()));
            }
            Event::End(TagEnd::Item) => flush(&mut lines, &mut spans),
            Event::Start(Tag::List(start)) => {
                list_prefix = match start {
                    Some(n) => format!("  {n}. "),
                    None => "  • ".to_string(),
                };
            }
            Event::End(TagEnd::List(_)) => list_prefix.clear(),
            Event::Start(Tag::BlockQuote(_)) => {
                flush(&mut lines, &mut spans);
                in_quote = true;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(&mut lines, &mut spans);
                in_quote = false;
            }
            Event::Text(t) => {
                if in_code_block {
                    code_buf.push_str(&t);
                } else {
                    let st = if in_quote {
                        style.fg(THEME.faint)
                    } else {
                        style
                    };
                    let mut first = true;
                    for part in t.split('\n') {
                        if !first {
                            flush(&mut lines, &mut spans);
                            if in_quote {
                                spans.push(Span::styled(
                                    "▎ ".to_string(),
                                    Style::default().fg(THEME.faint),
                                ));
                            }
                        }
                        if !part.is_empty() {
                            spans.push(Span::styled(part.to_string(), st));
                        }
                        first = false;
                    }
                }
            }
            Event::Code(t) => {
                spans.push(Span::styled(
                    format!(" {t} "),
                    Style::default().fg(THEME.code).bg(THEME.panel_bg),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_code_block {
                    code_buf.push('\n');
                } else {
                    flush(&mut lines, &mut spans);
                }
            }
            Event::Rule => {
                flush(&mut lines, &mut spans);
                lines.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(THEME.faint),
                )));
            }
            Event::TaskListMarker(done) => {
                spans.push(Span::styled(
                    if done { "☑ " } else { "☐ " }.to_string(),
                    Style::default().fg(THEME.faint),
                ));
            }
            _ => {}
        }
    }
    let _ = para_open;
    flush(&mut lines, &mut spans);
    lines
}
