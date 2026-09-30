//! Keymap — every keybinding lives here. `handle_key` routes through the
//! focus machine; each per-focus fn owns its own bindings.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::VecDeque;
use tokio::sync::mpsc;

use super::app::{self, App, Focus, Submit};
use super::menu;

/// Route a keypress through the focus machine. Returns true to quit.
pub(super) fn handle_key(
    app: &mut App,
    k: KeyEvent,
    tx_input: &mpsc::UnboundedSender<Submit>,
    tx_cancel: &mpsc::UnboundedSender<()>,
) -> bool {
    // Ctrl-C is global: deny a live card, cancel a busy turn, and only quit
    // on a second press when idle — a single stray Ctrl-C must not kill the
    // session (same two-press convention as the draft-clear Esc).
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
        if !app.input.is_empty() {
            // a live draft wins over quit intent — stash it like Esc×2
            app.draft_stash = Some(std::mem::take(&mut app.input));
            app.cursor = 0;
            app.toast("draft stashed — Ctrl+S restores · Ctrl-C again quits");
            app.quit_armed = Some(std::time::Instant::now());
            return false;
        }
        if app
            .quit_armed
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(800))
        {
            return true;
        }
        app.quit_armed = Some(std::time::Instant::now());
        app.toast("Ctrl-C again to quit");
        return false;
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

pub(super) fn resolve_card(app: &mut App, verdict: sunmao_core::approval::Approval) {
    if let Some(card) = app.approval.take() {
        let _ = card.reply.send(verdict);
        let mark = match verdict {
            sunmao_core::approval::Approval::Once => "[approved]",
            sunmao_core::approval::Approval::Session => "[approved for session]",
            sunmao_core::approval::Approval::Deny => "[denied]",
        };
        app.push_note(&format!("{mark} {}: {}", card.tool, card.detail));
        // session grants cover the identical call everywhere — a queued
        // request for the same (tool, specifier) inherits the verdict
        // instead of re-asking what the user just answered (the grant is
        // core-side too, so auto-approval and the gate can't disagree).
        if verdict == sunmao_core::approval::Approval::Session {
            let (inheriting, rest): (VecDeque<_>, VecDeque<_>) = app
                .approval_backlog
                .drain(..)
                .partition(|q| q.tool == card.tool && q.detail == card.detail);
            app.approval_backlog = rest;
            let inherited = inheriting.len();
            for q in inheriting {
                let _ = q.reply.send(sunmao_core::approval::Approval::Session);
            }
            if inherited > 0 {
                app.push_note(&format!(
                    "[session grant covers {inherited} queued identical request(s)]"
                ));
            }
        }
    }
    // the next queued request takes the focus back; else input owns it
    app.focus = Focus::Input;
    app.pop_approval();
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

pub(super) fn input_key(
    app: &mut App,
    k: KeyEvent,
    tx_input: &mpsc::UnboundedSender<Submit>,
) -> bool {
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
            // Enter *accepts* the highlighted candidate — the fragment in
            // the buffer must never be submitted. Then the kimi rule:
            // command-name completion submits immediately ONLY when the
            // accepted command is a no-arg builtin; a file command or an
            // arg-taking builtin just fills `/name ` and reopens args
            // completion (Tab's exact behavior) — a bare `/` or a menu
            // match the user hasn't reviewed must never fire blindly.
            // In arg mode a `provider/` prefix completes like Tab (the
            // arg isn't done), a leaf selector submits `/model sel`.
            // Path mode never submits — it rewrites the @-fragment.
            KeyCode::Enter => {
                let (name, frag, kind) = match &app.slash_menu {
                    Some(m) => (m.matches[m.selected].clone(), m.fragment.clone(), m.kind),
                    None => return false,
                };
                app.slash_menu = None;
                match kind {
                    menu::MenuKind::Path => {
                        app.accept_path_candidate(&name);
                        return false;
                    }
                    menu::MenuKind::Sessions => {
                        // session id completes to `/resume <id>` and
                        // submits — picking a session IS the command
                        app.input = format!("/resume {name}");
                        app.cursor = app.input.chars().count();
                        return submit_app(app, tx_input);
                    }
                    menu::MenuKind::Args => {
                        if name.ends_with('/') {
                            app.input = format!("/model {name}");
                            app.cursor = app.input.chars().count();
                            app.refresh_slash_menu();
                            return false;
                        }
                        app.input = format!("/model {name}");
                        app.cursor = app.input.chars().count();
                        return submit_app(app, tx_input);
                    }
                    menu::MenuKind::Command => {}
                }
                // commands that take an argument fill + reopen completion;
                // everything else is a no-arg action — run it. `/sessions`
                // is the odd one out: it's "browse and pick", so Enter
                // opens the same picker `/resume <frag>` serves instead
                // of printing the flat list.
                const TAKES_ARGS: &[&str] = &["model", "resume", "annotate"];
                if name == "sessions" {
                    app.input = "/sessions ".to_string();
                    app.cursor = app.input.chars().count();
                    app.refresh_slash_menu();
                    return false;
                }
                if TAKES_ARGS.contains(&name.as_str()) || frag.is_empty() {
                    // bare "/" has no fragment to stand on — complete like
                    // Tab instead of firing the first builtin alphabetically
                    app.input = format!("/{name} ");
                    app.cursor = app.input.chars().count();
                    app.refresh_slash_menu();
                    return false;
                }
                app.input = format!("/{name}");
                app.cursor = app.input.chars().count();
                return submit_app(app, tx_input);
            }
            // Tab stays the completion key: fill `/name ` so args can follow.
            KeyCode::Tab => {
                if let Some(m) = &app.slash_menu {
                    let name = m.matches[m.selected].clone();
                    let kind = m.kind;
                    app.slash_menu = None;
                    match kind {
                        menu::MenuKind::Path => {
                            app.accept_path_candidate(&name);
                        }
                        menu::MenuKind::Sessions => {
                            app.input = format!("/resume {name}");
                            app.cursor = app.input.chars().count();
                        }
                        menu::MenuKind::Args => {
                            app.input = format!("/model {name}");
                            app.cursor = app.input.chars().count();
                            app.refresh_slash_menu();
                        }
                        menu::MenuKind::Command => {
                            app.input = format!("/{name} ");
                            app.cursor = app.input.chars().count();
                            app.refresh_slash_menu();
                        }
                    }
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
        KeyCode::Enter => return submit_app(app, tx_input),
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
        // readline muscle memory: Ctrl+A/E = line ends, Ctrl+U = kill to
        // start, Ctrl+W = kill word back. Char-indexed throughout.
        KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::CONTROL) => app.cursor = 0,
        KeyCode::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.cursor = app.input.chars().count()
        }
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            // kill everything before the cursor
            let byte_idx = app::char_to_byte(&app.input, app.cursor);
            app.input.replace_range(..byte_idx, "");
            app.cursor = 0;
            app.refresh_slash_menu();
        }
        KeyCode::Char('w') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            // kill the word back: trailing spaces, then non-space run
            let chars: Vec<char> = app.input.chars().collect();
            let mut i = app.cursor.min(chars.len());
            while i > 0 && chars[i - 1].is_whitespace() {
                i -= 1;
            }
            while i > 0 && !chars[i - 1].is_whitespace() {
                i -= 1;
            }
            let from = app::char_to_byte(&app.input, i);
            let to = app::char_to_byte(&app.input, app.cursor);
            app.input.replace_range(from..to, "");
            app.cursor = i;
            app.refresh_slash_menu();
        }
        KeyCode::Up => {
            // recall a queued submission for editing before history browse —
            // Flush drops the driver's copies so nothing runs twice.
            if let Some(text) = app.recall_queued() {
                let _ = tx_input.send(Submit::Flush);
                let preview: String = text.chars().take(24).collect();
                app.toast(format!("recalled for editing: {preview}"));
                return false;
            }
            if app.hist_idx.is_none() && !app.history.is_empty() {
                // stash the in-progress draft so Down past the newest entry
                // brings back what the user was typing (readline convention)
                app.hist_draft = Some(app.input.clone());
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
                    app.input = app.hist_draft.take().unwrap_or_default();
                }
                app.cursor = app.input.chars().count();
            }
        }
        KeyCode::PageUp => app.scroll_back = app.scroll_back.saturating_add(10),
        KeyCode::PageDown => app.scroll_back = app.scroll_back.saturating_sub(10),
        KeyCode::Char('!') if app.input.is_empty() && !app.bash_mode => {
            app.bash_mode = true;
        }
        KeyCode::Char(c) => {
            app.quit_armed = None; // typing disarms the pending quit
            app.insert_char(c);
        }
        _ => {}
    }
    false
}

/// Shared Enter-path: what the composer's Enter and the slash menu's
/// select-run both do. `true` means quit.
fn submit_app(app: &mut App, tx_input: &mpsc::UnboundedSender<Submit>) -> bool {
    match app.submit() {
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
            if app.busy {
                app.queue.push_back(Submit::Bash(cmd.clone()));
                app.toast(format!("queued — `{cmd}` runs after this turn"));
            }
            let _ = tx_input.send(Submit::Bash(cmd));
        }
        Submit::Resume(arg) => {
            let _ = tx_input.send(Submit::Resume(arg));
        }
        Submit::Turn(t) => {
            if app.busy {
                // the driver drains submissions FIFO — the queue holds the
                // real text so ↑ can recall it and the footer can show it.
                app.queue.push_back(Submit::Turn(t.clone()));
                app.toast(format!(
                    "queued #{} — runs after this turn · ↑ recalls",
                    app.queue.len()
                ));
            }
            app.busy = true;
            let _ = tx_input.send(Submit::Turn(t));
        }
        // Flush is app→driver only — submit() never produces it
        Submit::Flush => {}
        Submit::Model(sel) => {
            let _ = tx_input.send(Submit::Model(sel));
        }
        Submit::Tasks => {
            let _ = tx_input.send(Submit::Tasks);
        }
        Submit::Todos => {
            let _ = tx_input.send(Submit::Todos);
        }
        Submit::Artifacts => {
            let _ = tx_input.send(Submit::Artifacts);
        }
        Submit::Annotate(name, note) => {
            let _ = tx_input.send(Submit::Annotate(name, note));
        }
    }
    false
}
