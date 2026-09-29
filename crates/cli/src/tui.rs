//! ratatui TUI — CJK-native: input is char-indexed, cursor math uses
//! display width (`unicode-width`), and Paragraph::wrap handles wide chars.

use std::io;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use sunmao_core::agent::{AgentLoop, LiveEvent, Observer, TurnOutcome};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

enum Msg {
    Live(LiveEvent),
    Key(KeyEvent),
    Paste(String),
}

struct ChanObserver(mpsc::UnboundedSender<Msg>);

impl Observer for ChanObserver {
    fn on_event(&self, ev: &LiveEvent) {
        let _ = self.0.send(Msg::Live(ev.clone()));
    }
}

struct App {
    lines: Vec<Line<'static>>,
    input: String,
    /// cursor as *char index* into `input` — never a byte offset.
    cursor: usize,
    /// visual lines scrolled back from the tail; 0 = pinned to bottom.
    scroll_back: u16,
    history: Vec<String>,
    hist_idx: Option<usize>,
    busy: bool,
}

impl App {
    fn push_line(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    /// Append a streamed chunk to the last assistant line, or start one.
    fn append_stream(&mut self, text: &str, style: Style) {
        for (i, part) in text.split('\n').enumerate() {
            if i == 0 {
                match self.lines.last_mut() {
                    // only append to a plain content line (ours carry no marker)
                    Some(Line { spans, .. }) if spans.len() == 1 => {
                        let mut s = spans[0].clone();
                        s.content = format!("{}{}", s.content, part).into();
                        spans[0] = s;
                    }
                    _ => self
                        .lines
                        .push(Line::from(Span::styled(part.to_string(), style))),
                }
            } else {
                self.lines
                    .push(Line::from(Span::styled(part.to_string(), style)));
            }
        }
    }

    fn submit(&mut self) -> String {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.hist_idx = None;
        if !text.trim().is_empty() {
            self.history.push(text.clone());
            self.push_line(Line::from(Span::styled(
                format!("> {text}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        text
    }

    fn insert_char(&mut self, c: char) {
        let byte_idx = char_to_byte(&self.input, self.cursor);
        self.input.insert(byte_idx, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let byte_idx = char_to_byte(&self.input, self.cursor - 1);
            self.input.remove(byte_idx);
            self.cursor -= 1;
        }
    }

    /// Cursor column inside the input area = prompt width + display width of
    /// the text before the cursor (CJK counts 2 — this is the CJK-native part).
    fn cursor_col(&self) -> u16 {
        let byte_idx = char_to_byte(&self.input, self.cursor);
        (2 + UnicodeWidthStr::width(&self.input[..byte_idx])) as u16
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

pub async fn run(agent: AgentLoop, model: &str) -> Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend)?;
    let res = run_inner(&mut term, agent, model).await;
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    res
}

async fn run_inner(
    term: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
    agent: AgentLoop,
    model: &str,
) -> Result<()> {
    let agent = Arc::new(agent);
    let (tx_msg, mut rx_msg) = mpsc::unbounded_channel::<Msg>();
    let (tx_input, mut rx_input) = mpsc::unbounded_channel::<String>();
    let (tx_cancel, mut rx_cancel) = mpsc::unbounded_channel::<()>();

    // driver task: consume submitted inputs, stream LiveEvents back
    {
        let tx_msg = tx_msg.clone();
        tokio::spawn(async move {
            while let Some(input) = rx_input.recv().await {
                let obs = ChanObserver(tx_msg.clone());
                let mut turn = Box::pin(agent.run_turn(&input, &obs));
                loop {
                    tokio::select! {
                        res = &mut turn => {
                            let _ = res;
                            break;
                        }
                        _ = rx_cancel.recv() => {
                            agent.cancel(); // cooperative: loop sees it next iteration
                        }
                    }
                }
            }
        });
    }

    // forward crossterm events into the same channel
    {
        let tx_msg = tx_msg.clone();
        tokio::spawn(async move {
            let mut stream = EventStream::new();
            while let Some(Ok(ev)) = stream.next().await {
                let m = match ev {
                    Event::Key(k) => Msg::Key(k),
                    Event::Paste(p) => Msg::Paste(p),
                    _ => continue,
                };
                if tx_msg.send(m).is_err() {
                    break;
                }
            }
        });
    }

    let mut app = App {
        lines: vec![Line::from(Span::styled(
            format!("sunmao TUI — {model} — Enter 发送, Esc 退出, PgUp/PgDn 翻页"),
            Style::default().fg(Color::DarkGray),
        ))],
        input: String::new(),
        cursor: 0,
        scroll_back: 0,
        history: Vec::new(),
        hist_idx: None,
        busy: false,
    };

    loop {
        term.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(f.area());

            let transcript = Paragraph::new(app.lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((app.scroll_back, 0));
            f.render_widget(transcript, chunks[0]);

            let input = Paragraph::new(format!("❯ {}", app.input))
                .block(Block::default().borders(Borders::TOP))
                .wrap(Wrap { trim: false });
            f.render_widget(input, chunks[1]);
            f.set_cursor_position((chunks[1].x + app.cursor_col(), chunks[1].y + 1));

            let status = if app.busy { "working…" } else { "idle" };
            f.render_widget(
                Paragraph::new(format!(" {status} | scroll -{} ", app.scroll_back))
                    .style(Style::default().fg(Color::DarkGray)),
                chunks[2],
            );
        })?;

        match rx_msg.recv().await {
            None => break,
            Some(Msg::Live(ev)) => match ev {
                LiveEvent::Content(c) => {
                    app.busy = true;
                    app.append_stream(&c, Style::default());
                }
                LiveEvent::Reasoning(r) => {
                    app.busy = true;
                    app.append_stream(&r, Style::default().fg(Color::DarkGray));
                }
                LiveEvent::ToolStart { name } => app.push_line(Line::from(Span::styled(
                    format!("  → tool {name}"),
                    Style::default().fg(Color::Cyan),
                ))),
                LiveEvent::ToolDone { name, ok } => app.push_line(Line::from(Span::styled(
                    format!("  {} tool {name}", if ok { "✓" } else { "✗" }),
                    Style::default().fg(if ok { Color::Green } else { Color::Red }),
                ))),
                LiveEvent::TurnEnd { outcome } => {
                    app.busy = false;
                    if outcome != TurnOutcome::Completed {
                        app.push_line(Line::from(Span::styled(
                            format!("[turn: {outcome:?}]"),
                            Style::default().fg(Color::Magenta),
                        )));
                    }
                }
            },
            Some(Msg::Paste(p)) => {
                for c in p.chars() {
                    app.insert_char(c);
                }
            }
            Some(Msg::Key(k)) => match k.code {
                KeyCode::Esc => break,
                // Ctrl-C: cancel the in-flight turn if busy, else exit
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    if app.busy {
                        app.busy = false;
                        app.push_line(Line::from(Span::styled(
                            "[cancelled]",
                            Style::default().fg(Color::Red),
                        )));
                        let _ = tx_cancel.send(());
                    } else {
                        break;
                    }
                }
                KeyCode::Enter => {
                    let text = app.submit();
                    if !text.trim().is_empty() {
                        app.busy = true;
                        let _ = tx_input.send(text);
                    }
                }
                KeyCode::Backspace => app.backspace(),
                KeyCode::Delete if app.cursor < app.input.chars().count() => {
                    let byte_idx = char_to_byte(&app.input, app.cursor);
                    app.input.remove(byte_idx);
                }
                KeyCode::Left => app.cursor = app.cursor.saturating_sub(1),
                KeyCode::Right => app.cursor = (app.cursor + 1).min(app.input.chars().count()),
                KeyCode::Home => app.cursor = 0,
                KeyCode::End => app.cursor = app.input.chars().count(),
                KeyCode::Up => {
                    if app.hist_idx.is_none() && !app.history.is_empty() {
                        app.hist_idx = Some(app.history.len() - 1);
                        app.input = app.history[app.hist_idx.unwrap()].clone();
                        app.cursor = app.input.chars().count();
                    } else if let Some(i) = app.hist_idx {
                        if i > 0 {
                            app.hist_idx = Some(i - 1);
                            app.input = app.history[i - 1].clone();
                            app.cursor = app.input.chars().count();
                        }
                    }
                }
                KeyCode::Down => {
                    if let Some(i) = app.hist_idx {
                        if i + 1 < app.history.len() {
                            app.hist_idx = Some(i + 1);
                            app.input = app.history[i + 1].clone();
                        } else {
                            app.hist_idx = None;
                            app.input.clear();
                        }
                        app.cursor = app.input.chars().count();
                    }
                }
                KeyCode::PageUp => app.scroll_back = app.scroll_back.saturating_add(10),
                KeyCode::PageDown => app.scroll_back = app.scroll_back.saturating_sub(10),
                KeyCode::Char(c) => app.insert_char(c),
                _ => {}
            },
        }
    }
    Ok(())
}
