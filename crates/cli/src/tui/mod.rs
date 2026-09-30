//! ratatui TUI — block-based transcript (fold/copy/select), card-style
//! approval with parkable focus, slash-command popup, markdown rendering.
//! CJK-native: input is char-indexed, cursor math uses display width.

mod app;
#[cfg(test)]
mod app_tests;
mod blocks;
mod input;
mod md;
mod render;
mod replay;
pub mod slash;
#[cfg(test)]
mod tests;
mod theme;
mod wrap;

use std::io;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEvent, KeyEventKind, MouseEventKind,
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
    /// mouse wheel — scrolls the transcript directly
    Wheel(i16),
    /// 1 Hz heartbeat — redraws so the busy timer ticks while the model
    /// streams nothing (long thinking gaps would otherwise freeze it).
    Tick,
    /// /resume swapped the session — replay these events into the transcript
    Replay(Vec<sunmao_core::SessionEvent>),
    /// /model swap landed — carries the resolved model label for the footer
    Model(String),
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
    let (tx_input, mut rx_input) = mpsc::unbounded_channel::<Submit>();
    let (tx_cancel, mut rx_cancel) = mpsc::unbounded_channel::<()>();

    // driver task: consume submissions, stream LiveEvents back.
    // `/name` file commands resolve here (needs cwd); builtins are already
    // resolved into Submit variants by the app.
    {
        let tx_msg = tx_msg.clone();
        let driver_cwd = cwd.clone();
        let driver_roots = extra_roots.clone();
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
                            Ok(s) if s.is_empty() => "[compacted: nothing to fold]".to_string(),
                            Ok(s) => format!("[compacted]\n{s}"),
                            Err(e) => format!("[compact failed] {e:#}"),
                        };
                        let _ = tx_msg.send(Msg::Note(note));
                        continue;
                    }
                    Submit::Flush => {
                        // the app recalled queued items for editing — drop
                        // everything still pending so nothing runs twice.
                        while rx_input.try_recv().is_ok() {}
                        continue;
                    }
                    Submit::Model(sel) => {
                        match sel {
                            None => {
                                let choices = agent.model_choices();
                                let _ = tx_msg.send(Msg::Note(if choices.is_empty() {
                                    "[no models.json — session model only]".into()
                                } else {
                                    format!("available models:\n{}", choices.join("\n"))
                                }));
                            }
                            Some(sel) => match agent.swap_model(&sel) {
                                Some(label) => {
                                    agent.record_model_change(&sel, &label).await;
                                    let _ = tx_msg.send(Msg::Model(label.clone()));
                                    let _ = tx_msg.send(Msg::Note(format!("[model → {label}]")));
                                }
                                None => {
                                    let _ = tx_msg.send(Msg::Note(format!(
                                        "[unknown selector: {sel} — try /model for the list]"
                                    )));
                                }
                            },
                        }
                        continue;
                    }
                    Submit::Tasks => {
                        // the live roster — detached spawns until done
                        let tasks = agent.task_roster();
                        let text = if tasks.is_empty() {
                            "[no sub-agents this session]".to_string()
                        } else {
                            let rows = tasks
                                .iter()
                                .map(|t| {
                                    let status = match t.done {
                                        None => "running",
                                        Some(true) => "done",
                                        Some(false) => "failed",
                                    };
                                    let agent = t
                                        .agent
                                        .as_deref()
                                        .map(|a| format!(" @{a}"))
                                        .unwrap_or_default();
                                    format!("  {status:<7} {}{} — {}", t.id, agent, t.prompt)
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            format!("sub-agents:\n{rows}")
                        };
                        let _ = tx_msg.send(Msg::Note(text));
                        continue;
                    }
                    Submit::Artifacts => {
                        let _ = tx_msg.send(Msg::Note(slash::artifacts_text(&driver_cwd)));
                        continue;
                    }
                    Submit::Resume(arg) => {
                        match arg {
                            None => {
                                // list recent sessions, newest first
                                let dir = driver_cwd.join(".sunmao/sessions");
                                let mut entries: Vec<_> = std::fs::read_dir(&dir)
                                    .map(|rd| {
                                        rd.flatten()
                                            .filter_map(|e| {
                                                let p = e.path();
                                                let stem =
                                                    p.file_stem()?.to_string_lossy().to_string();
                                                let m = e.metadata().ok()?.modified().ok()?;
                                                Some((m, stem))
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                entries.sort_by_key(|b| std::cmp::Reverse(b.0));
                                let list = entries
                                    .iter()
                                    .take(8)
                                    .map(|(_, s)| format!("  /resume {s}"))
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                let _ = tx_msg.send(Msg::Note(if list.is_empty() {
                                    "[no sessions]".into()
                                } else {
                                    format!("recent sessions:\n{list}")
                                }));
                            }
                            Some(id) => {
                                let p = std::path::PathBuf::from(&id);
                                let path = if p.exists() {
                                    p
                                } else {
                                    driver_cwd
                                        .join(".sunmao/sessions")
                                        .join(format!("{id}.jsonl"))
                                };
                                match sunmao_core::SessionLog::open_path(&path).await {
                                    Ok(log) => {
                                        let events = agent.swap_session(log).await;
                                        let _ = tx_msg.send(Msg::Replay(events));
                                    }
                                    Err(e) => {
                                        let _ = tx_msg
                                            .send(Msg::Note(format!("[resume failed] {e:#}")));
                                    }
                                }
                            }
                        }
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
                            depth: 0,
                            lane: 0,
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
                            depth: 0,
                            lane: 0,
                        }));
                        continue;
                    }
                    Submit::Turn(input) => {
                        let prompt = if let Some(cmd_line) = input.trim().strip_prefix('/') {
                            let name = cmd_line.split_whitespace().next().unwrap_or("");
                            let rest = cmd_line[name.len()..].trim();
                            match slash::command_body(&driver_cwd, &driver_roots, name) {
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
    app.extra_roots = extra_roots;
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
                LiveEvent::Content(c) => app.stream(BlockKind::Assistant, &c),
                LiveEvent::Reasoning(r) => app.stream(BlockKind::Thinking, &r),
                LiveEvent::ToolStart {
                    name,
                    summary,
                    depth,
                    lane,
                } => app.tool_start(&name, &summary, depth, lane),
                LiveEvent::ToolDone {
                    name,
                    ok,
                    output,
                    depth,
                    lane,
                } => app.tool_done(&name, ok, &output, depth, lane),
                LiveEvent::Hook { event, detail } => {
                    app.push_audit(&format!("{event} — {detail}"));
                }
                LiveEvent::Artifact { name, path, bytes } => {
                    app.push_note(&format!("[artifact '{name}' → {path} ({bytes} B)]"));
                }
                LiveEvent::Usage(u) => app.last_usage = Some(u),
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
