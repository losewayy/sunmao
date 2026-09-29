//! ratatui TUI — block-based transcript (fold/copy/select), card-style
//! approval with parkable focus, slash-command popup, markdown rendering.
//! CJK-native: input is char-indexed, cursor math uses display width.

mod app;
mod blocks;
mod md;
mod render;
pub mod slash;
mod theme;
mod wrap;

use std::io;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
    EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use futures_util::StreamExt;
use ratatui::Terminal;
use sunmao_core::agent::{AgentLoop, LiveEvent, Observer, TurnOutcome};
use tokio::sync::mpsc;

use app::{App, ApprovalCard, Focus, Submit};
use blocks::BlockKind;
use render::draw;

enum Msg {
    Live(LiveEvent),
    Key(KeyEvent),
    Paste(String),
    /// Status/feedback line from the driver — clears `busy`.
    Note(String),
    /// Driver says quit (e.g. /quit reached the task).
    Quit,
    /// approval request from a tool (risky command) — carries the reply channel
    ApprovalReq(ApprovalReq),
    /// git branch probe finished — footer shows it next to the cwd
    Branch(Option<String>),
}

/// A risky tool call suspended on user verdict.
pub struct ApprovalReq {
    pub tool: String,
    pub detail: String,
    pub why: String,
    pub reply: tokio::sync::oneshot::Sender<sunmao_core::approval::Approval>,
}

/// Approval seam for the TUI — risky calls suspend on a oneshot until the
/// card resolves.
pub struct TuiApprover {
    pub tx: mpsc::UnboundedSender<ApprovalReq>,
}

#[async_trait::async_trait]
impl sunmao_core::approval::Approver for TuiApprover {
    async fn approve(
        &self,
        tool: &str,
        detail: &str,
        why: &str,
    ) -> sunmao_core::approval::Approval {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(ApprovalReq {
                tool: tool.to_string(),
                detail: detail.to_string(),
                why: why.to_string(),
                reply: tx,
            })
            .is_err()
        {
            return sunmao_core::approval::Approval::Deny;
        }
        rx.await.unwrap_or(sunmao_core::approval::Approval::Deny)
    }
}

struct ChanObserver(mpsc::UnboundedSender<Msg>);

impl Observer for ChanObserver {
    fn on_event(&self, ev: &LiveEvent) {
        let _ = self.0.send(Msg::Live(ev.clone()));
    }
}

pub async fn run(
    agent: AgentLoop,
    model: &str,
    cwd: std::path::PathBuf,
    rx_approval: mpsc::UnboundedReceiver<ApprovalReq>,
    replay: Vec<sunmao_core::SessionEvent>,
) -> Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    // bracketed paste ON — without it terminals send paste as key events
    // and Msg::Paste never fires (the bulk-insert path is dead code).
    let _ = io::stdout().execute(EnableBracketedPaste);
    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend)?;
    let res = run_inner(&mut term, agent, model, cwd, rx_approval, replay).await;
    let _ = io::stdout().execute(DisableBracketedPaste);
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    res
}

async fn run_inner(
    term: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
    agent: AgentLoop,
    model: &str,
    cwd: std::path::PathBuf,
    mut rx_approval: mpsc::UnboundedReceiver<ApprovalReq>,
    replay: Vec<sunmao_core::SessionEvent>,
) -> Result<()> {
    let agent = Arc::new(agent);
    let (tx_msg, mut rx_msg) = mpsc::unbounded_channel::<Msg>();
    let (tx_input, mut rx_input) = mpsc::unbounded_channel::<Submit>();
    let (tx_cancel, mut rx_cancel) = mpsc::unbounded_channel::<()>();

    // driver task: consume submissions, stream LiveEvents back.
    // `/name` file commands resolve here (needs cwd); builtins are already
    // resolved into Submit variants by the app.
    {
        let tx_msg = tx_msg.clone();
        let driver_cwd = cwd.clone();
        tokio::spawn(async move {
            while let Some(sub) = rx_input.recv().await {
                match sub {
                    Submit::Quit => {
                        let _ = tx_msg.send(Msg::Quit);
                        return;
                    }
                    Submit::Note(n) => {
                        if !n.is_empty() {
                            let _ = tx_msg.send(Msg::Note(n));
                        }
                        continue;
                    }
                    Submit::Compact => {
                        let obs = ChanObserver(tx_msg.clone());
                        let note = match agent.compact(&obs, "manual").await {
                            Ok(()) => "[compacted]".to_string(),
                            Err(e) => format!("[compact failed] {e:#}"),
                        };
                        let _ = tx_msg.send(Msg::Note(note));
                        continue;
                    }
                    Submit::Bash(cmd) => {
                        // `!` local shell — the user runs it, so no approval
                        // gate and no LLM involvement. Same deno_task_shell
                        // engine the Bash tool uses; the durable fact folds
                        // into the next turn's context via LocalShell.
                        let _ = tx_msg.send(Msg::Live(LiveEvent::ToolStart {
                            name: "!".into(),
                            summary: format!("$ {cmd}"),
                        }));
                        let cwd = driver_cwd.clone();
                        let (ok, output, code) =
                            match sunmao_core::tool::run_foreground(&cmd, cwd, 120).await {
                                Ok(run) => {
                                    let ok = run.exit_code == 0;
                                    (ok, sunmao_core::tool::render_run(&run), run.exit_code)
                                }
                                Err(msg) => (false, msg, -1),
                            };
                        agent.record_local_shell(&cmd, code, &output).await;
                        let _ = tx_msg.send(Msg::Live(LiveEvent::ToolDone {
                            name: "!".into(),
                            ok,
                            output,
                        }));
                        continue;
                    }
                    Submit::Turn(input) => {
                        let prompt = if let Some(cmd_line) = input.trim().strip_prefix('/') {
                            let name = cmd_line.split_whitespace().next().unwrap_or("");
                            let rest = cmd_line[name.len()..].trim();
                            match slash::command_body(&driver_cwd, name) {
                                Some(body) => {
                                    if rest.is_empty() {
                                        body
                                    } else {
                                        format!("{body}\n\n{rest}")
                                    }
                                }
                                None => {
                                    let _ = tx_msg
                                        .send(Msg::Note(format!("[unknown command: /{name}]")));
                                    continue;
                                }
                            }
                        } else {
                            input
                        };
                        let obs = ChanObserver(tx_msg.clone());
                        let mut turn = Box::pin(agent.run_turn(&prompt, &obs));
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
                }
            }
        });
    }

    // forward approval requests into the same channel
    {
        let tx_msg = tx_msg.clone();
        tokio::spawn(async move {
            while let Some(req) = rx_approval.recv().await {
                let _ = tx_msg.send(Msg::ApprovalReq(req));
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
                    // Windows consoles emit Press, Repeat AND Release — only
                    // Press/Repeat produce input, else every char doubles.
                    Event::Key(k) if k.kind != KeyEventKind::Release => Msg::Key(k),
                    Event::Paste(p) => Msg::Paste(p),
                    _ => continue,
                };
                if tx_msg.send(m).is_err() {
                    break;
                }
            }
        });
    }

    // probe the git branch off the UI thread — `git` may not exist or the
    // cwd may not be a repo; None is a fine answer either way.
    {
        let tx_msg = tx_msg.clone();
        let probe_cwd = cwd.clone();
        tokio::spawn(async move {
            let branch = tokio::task::spawn_blocking(move || {
                let out = std::process::Command::new("git")
                    .args(["rev-parse", "--abbrev-ref", "HEAD"])
                    .current_dir(probe_cwd)
                    .stdin(std::process::Stdio::null())
                    .output()
                    .ok()?;
                if !out.status.success() {
                    return None;
                }
                let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
                (!b.is_empty() && b != "HEAD").then_some(b)
            })
            .await
            .ok()
            .flatten();
            let _ = tx_msg.send(Msg::Branch(branch));
        });
    }

    let mut app = App::new(model, cwd.clone());
    if !replay.is_empty() {
        app.replay(&replay);
    }

    loop {
        // synchronized-output bracket: terminals that grok CSI ?2026 render
        // the whole frame atomically (no torn paint mid-scroll); the rest
        // treat the markers as harmless no-ops.
        let _ = io::stdout().execute(BeginSynchronizedUpdate);
        let drawn = term.draw(|f| draw(f, &mut app));
        let _ = io::stdout().execute(EndSynchronizedUpdate);
        drawn?;

        match rx_msg.recv().await {
            None => break,
            Some(Msg::Quit) => break,
            Some(Msg::ApprovalReq(ApprovalReq {
                tool,
                detail,
                why,
                reply,
            })) => {
                app.approval = Some(ApprovalCard {
                    tool,
                    detail,
                    why,
                    reply,
                    selected: 0,
                    parked: false,
                });
                app.focus = Focus::Approval;
            }
            Some(Msg::Live(ev)) => match ev {
                LiveEvent::Content(c) => app.stream(BlockKind::Assistant, &c),
                LiveEvent::Reasoning(r) => app.stream(BlockKind::Thinking, &r),
                LiveEvent::ToolStart { name, summary } => app.tool_start(&name, &summary),
                LiveEvent::ToolDone { name, ok, output } => app.tool_done(&name, ok, &output),
                LiveEvent::Hook { event, detail } => {
                    app.push_audit(&format!("{event} — {detail}"));
                }
                LiveEvent::Usage(u) => app.last_usage = Some(u),
                LiveEvent::TurnEnd { outcome } => {
                    app.close_turn();
                    if outcome != TurnOutcome::Completed {
                        app.push_note(&format!("[turn: {outcome:?}]"));
                    }
                }
            },
            Some(Msg::Note(note)) => app.push_note(&note),
            Some(Msg::Branch(b)) => app.git_branch = b,
            Some(Msg::Paste(p)) => app.insert_str(&p),
            Some(Msg::Key(k)) => {
                if handle_key(&mut app, k, &tx_input, &tx_cancel) {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Route a keypress through the focus machine. Returns true to quit.
fn handle_key(
    app: &mut App,
    k: KeyEvent,
    tx_input: &mpsc::UnboundedSender<Submit>,
    tx_cancel: &mpsc::UnboundedSender<()>,
) -> bool {
    // Ctrl-C is global: cancel a busy turn, deny a live card, else quit.
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        if app.focus == Focus::Approval {
            resolve_card(app, sunmao_core::approval::Approval::Deny);
            return false;
        }
        if app.busy {
            app.push_note("[cancelled]");
            let _ = tx_cancel.send(());
            return false;
        }
        return true;
    }
    // Ctrl-S restores a stashed draft (double-Esc clear undo).
    if k.code == KeyCode::Char('s') && k.modifiers.contains(KeyModifiers::CONTROL) {
        app.restore_draft();
        return false;
    }

    match app.focus {
        Focus::Approval => card_key(app, k),
        Focus::Scrollback => scroll_key(app, k),
        Focus::Viewer => viewer_key(app, k),
        Focus::Input => input_key(app, k, tx_input),
    }
}

fn resolve_card(app: &mut App, verdict: sunmao_core::approval::Approval) {
    if let Some(card) = app.approval.take() {
        let _ = card.reply.send(verdict);
        let mark = match verdict {
            sunmao_core::approval::Approval::Once => "[approved]",
            sunmao_core::approval::Approval::Session => "[approved for session]",
            sunmao_core::approval::Approval::Deny => "[denied]",
        };
        app.push_note(&format!("{mark} {}: {}", card.tool, card.detail));
    }
    app.focus = Focus::Input;
}

fn card_key(app: &mut App, k: KeyEvent) -> bool {
    use sunmao_core::approval::Approval;
    match k.code {
        KeyCode::Esc => app.on_esc(), // parks the card
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
            if let Some(c) = &mut app.approval {
                c.selected = c.selected.saturating_sub(1)
            }
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') | KeyCode::Tab => {
            if let Some(c) = &mut app.approval {
                c.selected = (c.selected + 1) % 3
            }
        }
        KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => {
            resolve_card(app, Approval::Once)
        }
        KeyCode::Char('2') | KeyCode::Char('a') | KeyCode::Char('A') => {
            resolve_card(app, Approval::Session)
        }
        KeyCode::Char('3') | KeyCode::Char('n') | KeyCode::Char('N') => {
            resolve_card(app, Approval::Deny)
        }
        KeyCode::Enter => {
            let v = match app.approval.as_ref().map(|c| c.selected).unwrap_or(0) {
                1 => Approval::Session,
                2 => Approval::Deny,
                _ => Approval::Once,
            };
            resolve_card(app, v);
        }
        _ => {}
    }
    false
}

fn scroll_key(app: &mut App, k: KeyEvent) -> bool {
    match k.code {
        KeyCode::Esc | KeyCode::Tab => app.focus = Focus::Input,
        KeyCode::Enter => app.open_viewer(),
        KeyCode::Up | KeyCode::Char('k') => app.select_delta(-1),
        KeyCode::Down | KeyCode::Char('j') => app.select_delta(1),
        KeyCode::Char('e') => app.toggle_fold(),
        KeyCode::Char('y') => {
            if let Some(text) = app.selected_copy() {
                let ok = app::osc52_copy(&text);
                app.toast = Some((
                    if ok { "copied" } else { "copy failed" }.to_string(),
                    std::time::Instant::now(),
                ));
            }
        }
        KeyCode::Char('g') => app.selected = 0,
        KeyCode::Char('G') => app.selected = app.blocks.len().saturating_sub(1),
        KeyCode::PageUp => app.scroll_back = app.scroll_back.saturating_add(10),
        KeyCode::PageDown => app.scroll_back = app.scroll_back.saturating_sub(10),
        _ => {}
    }
    false
}

/// Full-screen viewer: j/k + PageUp/Down scroll the body, Esc/q returns to
/// the scrollback selection it came from.
fn viewer_key(app: &mut App, k: KeyEvent) -> bool {
    let Some(v) = &mut app.viewer else {
        app.focus = Focus::Scrollback;
        return false;
    };
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Tab => {
            app.viewer = None;
            app.focus = Focus::Scrollback;
        }
        KeyCode::Up | KeyCode::Char('k') => v.scroll = v.scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => v.scroll = v.scroll.saturating_add(1),
        KeyCode::PageUp => v.scroll = v.scroll.saturating_sub(10),
        KeyCode::PageDown => v.scroll = v.scroll.saturating_add(10),
        KeyCode::Home | KeyCode::Char('g') => v.scroll = 0,
        KeyCode::Char('y') => {
            let text = v.body.clone();
            let ok = app::osc52_copy(&text);
            app.toast = Some((
                if ok { "copied" } else { "copy failed" }.to_string(),
                std::time::Instant::now(),
            ));
        }
        _ => {}
    }
    false
}

fn input_key(app: &mut App, k: KeyEvent, tx_input: &mpsc::UnboundedSender<Submit>) -> bool {
    // slash popup holds the nav + completion keys first
    if app.slash_menu.is_some() {
        match k.code {
            KeyCode::Esc => {
                app.slash_menu = None;
                return false;
            }
            KeyCode::Up => {
                if let Some(m) = &mut app.slash_menu {
                    m.selected = m.selected.saturating_sub(1)
                }
                return false;
            }
            KeyCode::Down => {
                if let Some(m) = &mut app.slash_menu {
                    m.selected = (m.selected + 1).min(m.matches.len().saturating_sub(1))
                }
                return false;
            }
            KeyCode::Tab | KeyCode::Enter => {
                if let Some(m) = &app.slash_menu {
                    let name = m.matches[m.selected].clone();
                    app.input = format!("/{name} ");
                    app.cursor = app.input.chars().count();
                    app.slash_menu = None;
                }
                return false;
            }
            _ => {}
        }
    }

    match k.code {
        KeyCode::Esc => {
            if app.bash_mode && app.input.is_empty() {
                // bash-mode Esc leaves the mode first; with text present it
                // falls through to the draft-clear gesture instead.
                app.bash_mode = false;
            } else {
                app.on_esc()
            }
        }
        KeyCode::Tab => {
            app.focus = if app.approval.as_ref().map(|c| c.parked).unwrap_or(false) {
                if let Some(c) = &mut app.approval {
                    c.parked = false;
                }
                Focus::Approval
            } else {
                app.ensure_selection();
                Focus::Scrollback
            };
        }
        KeyCode::Enter
            if !app.bash_mode
                && app.multiline
                && !k
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
        {
            app.insert_newline();
        }
        KeyCode::Enter => match app.submit() {
            Submit::Quit => return true,
            Submit::Note(n) => {
                if !n.is_empty() {
                    app.push_note(&n);
                }
            }
            Submit::Compact => {
                app.busy = true;
                let _ = tx_input.send(Submit::Compact);
            }
            Submit::Bash(cmd) => {
                let _ = tx_input.send(Submit::Bash(cmd));
            }
            Submit::Turn(t) => {
                app.busy = true;
                let _ = tx_input.send(Submit::Turn(t));
            }
        },
        KeyCode::Backspace => {
            if app.bash_mode && app.input.is_empty() {
                // kimi-style: empty bash buffer eats the mode itself
                app.bash_mode = false;
            } else {
                app.backspace()
            }
        }
        KeyCode::Delete if app.cursor < app.input.chars().count() => {
            let byte_idx = app::char_to_byte(&app.input, app.cursor);
            app.input.remove(byte_idx);
            app.refresh_slash_menu();
        }
        KeyCode::Left => app.cursor = app.cursor.saturating_sub(1),
        KeyCode::Right => app.cursor = (app.cursor + 1).min(app.input.chars().count()),
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.chars().count(),
        KeyCode::Up => {
            if app.hist_idx.is_none() && !app.history.is_empty() {
                app.hist_idx = Some(app.history.len() - 1);
                app.input = app.history[app.hist_idx.unwrap()].clone();
            } else if let Some(i) = app.hist_idx {
                if i > 0 {
                    app.hist_idx = Some(i - 1);
                    app.input = app.history[i - 1].clone();
                }
            }
            app.cursor = app.input.chars().count();
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
        KeyCode::Char('!') if app.input.is_empty() && !app.bash_mode => {
            app.bash_mode = true;
        }
        KeyCode::Char(c) => app.insert_char(c),
        _ => {}
    }
    false
}
