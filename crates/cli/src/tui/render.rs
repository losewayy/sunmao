//! Frame rendering — transcript blocks, slash popup, approval card,
//! composer, status bar. Pure draw: all state lives in `app.rs`.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as WBlock, Borders, Paragraph, Wrap};

use super::app::{self, App, ApprovalCard, Focus, SlashMenu};
use super::theme::{self, THEME};
use super::wrap;

pub fn draw(f: &mut ratatui::Frame, app: &mut App) {
    // menu rows: border(1) + title+hint(1) + blank(1) + optional search row
    // + items(≤8) + scroll indicator(if any) + bottom border(1)
    let menu_rows = app
        .slash_menu
        .as_ref()
        .map(|m| {
            let items = m.matches.len().min(8) as u16;
            let search_row = u16::from(!m.fragment.is_empty());
            let more_row = u16::from(m.matches.len() > 8 || m.selected >= 8);
            4 + items + more_row + search_row
        })
        .unwrap_or(0);
    let card_rows = if app.focus == Focus::Approval { 5 } else { 0 };
    let input_rows = (app.input.lines().count().max(1) as u16 + 2).clamp(3, 8);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(menu_rows),
            Constraint::Length(card_rows),
            Constraint::Length(input_rows),
            Constraint::Length(2),
        ])
        .split(f.area());

    draw_transcript(f, app, chunks[0]);
    if let Some(m) = &app.slash_menu {
        draw_slash_menu(f, m, chunks[1]);
    }
    if app.focus == Focus::Approval {
        if let Some(c) = &app.approval {
            draw_card(f, c, chunks[2]);
        }
    }
    draw_input(f, app, chunks[3]);
    draw_status(f, app, chunks[4]);
}

fn draw_transcript(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let in_scroll = app.focus == Focus::Scrollback;
    let width = area.width as usize;
    // Pre-wrap into visual rows so scroll_back counts what the user actually
    // sees — Paragraph::wrap would fold at draw time and desync the math.
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut sel_start = 0usize;
    for (i, b) in app.blocks.iter().enumerate() {
        let selected = in_scroll && i == app.selected;
        if selected {
            sel_start = lines.len();
        }
        for l in b.render(selected, width) {
            lines.extend(wrap::wrap_line(&l, width));
        }
    }

    // keep the selected block inside the viewport when browsing
    let total = lines.len();
    if in_scroll && total > 0 {
        let view_h = area.height as usize;
        let top = total.saturating_sub(app.scroll_back as usize + view_h);
        if sel_start < top {
            app.scroll_back = (total.saturating_sub(sel_start + view_h)) as u16;
        } else if sel_start >= top + view_h {
            app.scroll_back = (total.saturating_sub(sel_start + 1)) as u16;
        }
    }
    let view_h = area.height as usize;
    let cap = total.saturating_sub(view_h) as u16;
    app.scroll_back = app.scroll_back.min(cap);

    let transcript = Paragraph::new(lines)
        .style(Style::default().fg(THEME.text))
        .scroll((app.scroll_back, 0));
    f.render_widget(transcript, area);
}

fn draw_slash_menu(f: &mut ratatui::Frame, m: &SlashMenu, area: Rect) {
    let width = area.width as usize;
    let rule = "─".repeat(width);
    let mut rows: Vec<Line> = vec![
        Line::from(Span::styled(rule.clone(), Style::default().fg(THEME.user))),
        Line::from(vec![
            Span::styled(
                " Commands".to_string(),
                Style::default().fg(THEME.user).add_modifier(Modifier::BOLD),
            ),
            if m.fragment.is_empty() {
                Span::styled("  (type to search)", Style::default().fg(THEME.faint))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(Span::styled(
            " ↑↓ navigate · Enter select · Tab complete · Esc cancel",
            Style::default().fg(THEME.faint),
        )),
        Line::from(""),
    ];
    if !m.fragment.is_empty() {
        rows.push(Line::from(vec![
            Span::styled(" Search: ", Style::default().fg(THEME.user)),
            Span::styled(m.fragment.clone(), Style::default().fg(THEME.text)),
        ]));
    }
    // windowed view: keep `selected` inside an 8-row window
    let start = m
        .selected
        .saturating_sub(7)
        .min(m.matches.len().saturating_sub(8));
    let end = (start + 8).min(m.matches.len());
    for (i, name) in m.matches.iter().enumerate().take(end).skip(start) {
        let sel = i == m.selected;
        rows.push(Line::from(Span::styled(
            format!("{} /{name}", if sel { " ❯" } else { "  " }),
            if sel {
                Style::default().fg(THEME.hi).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(THEME.muted)
            },
        )));
    }
    if m.matches.len() > 8 {
        rows.push(Line::from(Span::styled(
            format!(" ▼ {} of {}", m.selected + 1, m.matches.len()),
            Style::default().fg(THEME.faint),
        )));
    }
    rows.push(Line::from(Span::styled(
        rule,
        Style::default().fg(THEME.user),
    )));
    f.render_widget(Paragraph::new(rows), area);
}

fn draw_card(f: &mut ratatui::Frame, c: &ApprovalCard, area: Rect) {
    let opt = |label: &str, sel: bool, color| {
        Span::styled(
            format!(" {} {label} ", if sel { "▸" } else { " " }),
            if sel {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(THEME.muted)
            },
        )
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                " approve? ",
                Style::default().fg(THEME.warn).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}: {}", c.tool, c.detail),
                Style::default().fg(THEME.text),
            ),
        ]),
        Line::from(vec![
            opt("1) allow once", c.selected == 0, THEME.ok),
            Span::raw(" "),
            opt("2) allow session", c.selected == 1, THEME.ok),
            Span::raw(" "),
            opt("3) deny", c.selected == 2, THEME.err),
        ]),
        Line::from(Span::styled(
            format!(" why: {}", c.why),
            Style::default().fg(THEME.faint),
        )),
        Line::from(Span::styled(
            " ↑↓/Tab choose · 1-3/Enter pick · y/a/n quick · Esc park · Ctrl-C deny",
            Style::default().fg(THEME.faint),
        )),
    ];
    let p = Paragraph::new(lines).block(
        WBlock::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(THEME.warn)),
    );
    f.render_widget(p, area);
}

fn draw_input(f: &mut ratatui::Frame, app: &App, area: Rect) {
    // bash mode gets its own prompt + accent so the mode is never invisible
    let (prompt, pstyle) = if app.bash_mode {
        (
            "! ",
            Style::default().fg(THEME.ok).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            theme::prompt_glyph(),
            Style::default().fg(THEME.user).add_modifier(Modifier::BOLD),
        )
    };
    let mut lines: Vec<Line> = Vec::new();
    for (i, l) in app.input.split('\n').enumerate() {
        let prefix = if i == 0 {
            Span::styled(prompt.to_string(), pstyle)
        } else {
            Span::styled("  ".to_string(), Style::default().fg(THEME.faint))
        };
        lines.push(Line::from(vec![
            prefix,
            Span::styled(l.to_string(), Style::default().fg(THEME.text)),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(prompt.to_string(), pstyle)));
    }
    let input = Paragraph::new(lines)
        .block(
            WBlock::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(THEME.faint)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(input, area);
    // cursor: row = newlines before cursor; col = width of last-line prefix
    let byte_idx = app::char_to_byte(&app.input, app.cursor);
    let before = &app.input[..byte_idx];
    let row = before.matches('\n').count() as u16;
    let col = before
        .rsplit('\n')
        .next()
        .map(wrap::display_width)
        .unwrap_or(0) as u16;
    let prompt_w = wrap::display_width(prompt) as u16;
    f.set_cursor_position((area.x + prompt_w + col, area.y + 1 + row));
}

fn draw_status(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let short_cwd = app
        .cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| app.cwd.display().to_string());
    let mode = if app.multiline { "ml" } else { "1line" };
    let branch = match &app.git_branch {
        Some(b) => format!("({b})"),
        None => String::new(),
    };
    let ctx = match app.last_usage.as_ref().filter(|u| u.prompt_tokens > 0) {
        Some(u) => format!(
            "{} · {short_cwd} {branch} · {mode} · ctx {}",
            app.model,
            human_tokens(u.prompt_tokens)
        ),
        None => format!("{} · {short_cwd} {branch} · {mode}", app.model),
    };

    let (state, scol) = if app.busy {
        ("● working", THEME.running)
    } else {
        ("○ idle", THEME.ok)
    };
    let line1 = Line::from(vec![
        Span::styled(format!(" {state} "), Style::default().fg(scol)),
        Span::styled(ctx, Style::default().fg(THEME.muted)),
    ]);
    let line2 = if let Some(t) = app.toast_text() {
        Line::from(Span::styled(
            format!(" {t} "),
            Style::default().fg(THEME.hi).add_modifier(Modifier::BOLD),
        ))
    } else {
        let hints = match app.focus {
            Focus::Approval => "↑↓/Tab · 1-2 · Esc park".to_string(),
            Focus::Scrollback => format!(
                "block {}/{} · j/k · e fold · y copy · g/G · Tab input",
                app.selected + 1,
                app.blocks.len()
            ),
            Focus::Input => {
                let parked = app.approval.as_ref().map(|c| c.parked).unwrap_or(false);
                if parked {
                    "card parked — Tab returns".to_string()
                } else if app.bash_mode {
                    "local shell — output joins context · Esc/⌫ exits mode".to_string()
                } else {
                    "Tab blocks · / commands · ! bash · Esc×2 clear · Ctrl-C quit".to_string()
                }
            }
        };
        Line::from(Span::styled(
            format!(" {hints} "),
            Style::default().fg(THEME.faint),
        ))
    };
    f.render_widget(
        Paragraph::new(vec![line1, line2]).style(Style::default().bg(theme::base::BG)),
        area,
    );
}

/// 12_345 → "12.3k"; below 1k print raw so tiny prompts stay exact.
fn human_tokens(n: u64) -> String {
    if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
