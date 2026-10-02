//! ratatui TUI — block-based transcript (fold/copy/select), card-style
//! approval with parkable focus, slash-command popup, markdown rendering.
//! CJK-native: input is char-indexed, cursor math uses display width.

mod app;
#[cfg(test)]
mod app_tests;
mod blocks;
mod driver;
mod input;
mod md;
mod menu;
mod render;
mod replay;
#[cfg(test)]
mod replay_parity;
#[cfg(test)]
mod tests;
mod theme;
mod wrap;

use std::io;
use std::sync::Arc;

use anyhow::Result;
use crossterm::ExecutableCommand;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEvent, KeyEventKind, MouseEventKind,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
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
    /// mouse wheel — scrolls the transcript directly
    Wheel(i16),
    /// 1 Hz heartbeat — redraws so the busy timer ticks while the model
    /// streams nothing (long thinking gaps would otherwise freeze it).
    Tick,
    /// /resume swapped the session — replay these events into the transcript
    Replay(Vec<sunmao_core::SessionEvent>),
    /// /model swap landed — carries the resolved model label for the footer
    Model(String),
    /// /mode switch landed — footer shows the new approval stance
    Mode(sunmao_core::agent::ApprovalMode),
    /// a queued `!` submission ran to completion — pop the queue head;
    /// `!` emits ToolDone but never a TurnEnd, so the queue would
    /// otherwise hold a phantom entry forever.
    QueuePop,
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
            return sunmao_core::approval::Approval::Deny { reason: None };
        }
        rx.await
            .unwrap_or(sunmao_core::approval::Approval::Deny { reason: None })
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
    extra_roots: Vec<std::path::PathBuf>,
) -> Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    // bracketed paste ON — without it terminals send paste as key events
    // and Msg::Paste never fires (the bulk-insert path is dead code).
    let _ = io::stdout().execute(EnableBracketedPaste);
    // mouse capture: wheel scrolls the transcript; clicks we ignore (text
    // selection on the alternate screen is the terminal's own business).
    let _ = io::stdout().execute(EnableMouseCapture);
    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend)?;
    let res = run_inner(
        &mut term,
        agent,
        model,
        cwd,
        rx_approval,
        replay,
        extra_roots,
    )
    .await;
    let _ = io::stdout().execute(DisableBracketedPaste);
    let _ = io::stdout().execute(DisableMouseCapture);
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
    extra_roots: Vec<std::path::PathBuf>,
) -> Result<()> {
    let agent = Arc::new(agent);
    let (tx_msg, mut rx_msg) = mpsc::unbounded_channel::<Msg>();
    // sub-agent tool lifecycle relays through the sink — install once
    agent.set_live_sink(Arc::new(ChanObserver(tx_msg.clone())));
    // session id before the driver takes ownership of `agent`
    let session_id = agent
        .session_path()
        .await
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "?".into());
    // grab before the driver task takes `agent` — slash-menu arg completion
    // filters this list for `/model <sel>`.
    let model_selectors = agent.model_selectors();
    // MCP `/srv:prompt` names complete like builtin commands
    let mcp_prompts = agent.mcp_prompt_names();
    let (tx_input, rx_input) = mpsc::unbounded_channel::<Submit>();
    let (tx_cancel, rx_cancel) = mpsc::unbounded_channel::<()>();

    // driver task: consume submissions, stream LiveEvents back.
    // `/name` file commands resolve in the driver (needs cwd); builtins
    // are already resolved into Submit variants by the app.
    driver::spawn(
        agent.clone(),
        tx_msg.clone(),
        rx_input,
        rx_cancel,
        cwd.clone(),
        extra_roots.clone(),
    );

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
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => Msg::Wheel(3),
                        MouseEventKind::ScrollDown => Msg::Wheel(-3),
                        _ => continue,
                    },
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

    // 1 Hz tick — repaints the busy timer and ages out toasts even when no
    // other event arrives.
    {
        let tx_msg = tx_msg.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                iv.tick().await;
                if tx_msg.send(Msg::Tick).is_err() {
                    break;
                }
            }
        });
    }

    let mut app = App::new(model, cwd.clone(), &session_id);
    app.model_selectors = model_selectors;
    app.mcp_prompts = mcp_prompts;
    app.extra_roots = extra_roots;
    // the log's recorded stance wins — a resumed full_access session must
    // not look like it was auto all along
    app.approval_mode = agent.approval_mode();
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
                // requests queue behind a pending card — dropping the reply
                // sender silently resolves to Deny, which would refuse calls
                // the user never saw.
                app.queue_approval(ApprovalCard {
                    tool,
                    detail,
                    why,
                    reply,
                    selected: 0,
                    parked: false,
                });
            }
            Some(Msg::Live(ev)) => match ev {
                LiveEvent::Content { text } => app.stream(BlockKind::Assistant, &text),
                LiveEvent::Reasoning { text } => app.stream(BlockKind::Thinking, &text),
                LiveEvent::ToolStart {
                    name,
                    summary,
                    depth,
                    lane,
                    call_id,
                    ..
                } => app.tool_start(&name, &summary, depth, lane, call_id),
                LiveEvent::ToolDone {
                    name,
                    ok,
                    output,
                    depth,
                    lane,
                    call_id,
                    ..
                } => app.tool_done(&name, ok, &output, depth, lane, call_id.as_deref()),
                LiveEvent::Hook { event, detail } => {
                    app.push_audit(&format!("{event} — {detail}"));
                }
                LiveEvent::Artifact {
                    name,
                    path,
                    bytes,
                    rev,
                } => {
                    let v = if rev > 1 {
                        format!(" · rev {rev}")
                    } else {
                        String::new()
                    };
                    app.push_note(&format!("[artifact '{name}' → {path} ({bytes} B{v})]"));
                }
                LiveEvent::Usage(u) => app.last_usage = Some(u),
                // the durable facts' live mirrors — the TUI renders them as
                // the same notes a replay of the log would produce
                LiveEvent::Compacted { summary } => {
                    app.blocks.clear();
                    app.render_cache.clear();
                    app.push_note(&format!("[context compacted] {summary}"));
                }
                LiveEvent::Todos { .. } => {} // tool echo already carries it
                // serve-only live mirror of the durable user message — the
                // TUI prints its own prompt on submit, so it never lands
                LiveEvent::UserMessage { .. } => {}
                LiveEvent::TaskDone { id, ok, .. } => {
                    app.push_note(&format!(
                        "[sub-agent {id} {}]",
                        if ok { "done" } else { "failed" }
                    ));
                }
                LiveEvent::TurnEnd { outcome } => {
                    app.close_turn();
                    // one queued submission moved from waiting → running
                    app.queue.pop_front();
                    if outcome != TurnOutcome::Completed {
                        app.push_note(&format!("[turn: {outcome:?}]"));
                    }
                }
            },
            Some(Msg::Note(note)) => app.push_note(&note),
            Some(Msg::Branch(b)) => app.git_branch = b,
            Some(Msg::Model(label)) => app.model = label,
            Some(Msg::Mode(m)) => app.approval_mode = m,
            Some(Msg::QueuePop) => {
                app.queue.pop_front();
            }
            Some(Msg::Wheel(d)) => {
                // wheel: scrolls transcript; in the viewer it scrolls that.
                if app.focus == Focus::Viewer {
                    if let Some(v) = &mut app.viewer {
                        v.scroll = v.scroll.saturating_add_signed(-d);
                    }
                } else if d > 0 {
                    app.scroll_back = app.scroll_back.saturating_add(d as u16);
                } else {
                    app.scroll_back = app.scroll_back.saturating_sub(-d as u16);
                }
            }
            Some(Msg::Tick) => {} // just redraw
            Some(Msg::Replay(events)) => {
                app.blocks.clear();
                app.selected = 0;
                app.scroll_back = 0;
                app.render_cache.clear();
                app.replay(&events);
                app.push_note("[session resumed]");
            }
            Some(Msg::Paste(p)) => app.insert_str(&p),
            Some(Msg::Key(k)) => {
                if input::handle_key(&mut app, k, &tx_input, &tx_cancel) {
                    break;
                }
            }
        }
    }
    Ok(())
}
