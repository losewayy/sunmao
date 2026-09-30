use super::app::{App, ApprovalCard, Focus, Submit};
use super::input::{input_key, resolve_card};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Readline convention: Up browses history, Down past the newest entry
/// restores the half-typed draft — losing it is a data-loss bug, not a
/// cosmetic quirk.
#[test]
fn history_browse_restores_in_progress_draft() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.history = vec!["first".into(), "second".into()];

    // user was typing something, then pressed Up
    app.input = "half-typed draft".into();
    app.cursor = app.input.chars().count();
    input_key(&mut app, key(KeyCode::Up), &tx);
    assert_eq!(app.input, "second");
    input_key(&mut app, key(KeyCode::Up), &tx);
    assert_eq!(app.input, "first");
    // Down past the newest entry brings the draft back
    input_key(&mut app, key(KeyCode::Down), &tx);
    assert_eq!(app.input, "second");
    input_key(&mut app, key(KeyCode::Down), &tx);
    assert_eq!(app.input, "half-typed draft");
    assert!(app.hist_idx.is_none());
}

/// An empty draft still exits history cleanly on Down.
#[test]
fn history_down_past_end_with_empty_draft() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.history = vec!["only".into()];
    input_key(&mut app, key(KeyCode::Up), &tx);
    assert_eq!(app.input, "only");
    input_key(&mut app, key(KeyCode::Down), &tx);
    assert_eq!(app.input, "");
    assert!(app.hist_idx.is_none());
}

/// A submit while busy must surface as a queued turn — the driver
/// drains FIFO, so the app's only job is counting + telling the user.
#[test]
fn submit_while_busy_counts_queued() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.busy = true;
    app.input = "second prompt".into();
    input_key(&mut app, key(KeyCode::Enter), &tx);
    assert_eq!(app.queue.len(), 1);
    assert!(matches!(rx.try_recv(), Ok(Submit::Turn(_))));
    // a free submit leaves the queue alone
    app.busy = false;
    app.input = "third".into();
    input_key(&mut app, key(KeyCode::Enter), &tx);
    assert_eq!(app.queue.len(), 1);
}

/// ↑ on an empty composer while items wait pulls the tail back for
/// editing — and Flush is sent so the driver's copy can't run twice.
#[test]
fn up_recalls_queued_tail_for_editing() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.busy = true;
    app.input = "first queued".into();
    input_key(&mut app, key(KeyCode::Enter), &tx);
    app.input = "second queued".into();
    input_key(&mut app, key(KeyCode::Enter), &tx);
    assert_eq!(app.queue.len(), 2);
    assert!(rx.try_recv().is_ok());
    assert!(rx.try_recv().is_ok());

    input_key(&mut app, key(KeyCode::Up), &tx);
    assert_eq!(app.input, "second queued");
    assert_eq!(app.queue.len(), 1);
    assert!(matches!(rx.try_recv(), Ok(Submit::Flush)));
    // again — the last waiting item comes back too
    app.input.clear();
    app.cursor = 0;
    input_key(&mut app, key(KeyCode::Up), &tx);
    assert_eq!(app.input, "first queued");
    assert!(app.queue.is_empty());
    assert!(matches!(rx.try_recv(), Ok(Submit::Flush)));
}

/// With the slash menu open, Enter *runs* the highlighted command — the
/// fragment in the buffer must never reach the model, and a lone "/"
/// must never be submitted as a turn.
#[test]
fn slash_menu_enter_runs_selection() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    // user typed "/com", menu highlights "compact"
    for c in "/com".chars() {
        app.insert_char(c);
    }
    let m = app.slash_menu.as_ref().expect("menu must open on /com");
    assert_eq!(m.matches[m.selected], "compact");

    input_key(&mut app, key(KeyCode::Enter), &tx);
    // /compact resolves to the local Submit::Compact, never a Turn
    assert!(matches!(rx.try_recv(), Ok(Submit::Compact)));
    assert!(app.input.is_empty(), "submit drains the buffer");
    assert!(app.slash_menu.is_none());
}

/// Tab remains the completion key: it fills `/name ` for args and does
/// NOT submit.
#[test]
fn slash_menu_tab_completes_without_submit() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    for c in "/res".chars() {
        app.insert_char(c);
    }
    let m = app.slash_menu.as_ref().expect("menu must open on /res");
    assert_eq!(m.matches[m.selected], "resume");

    input_key(&mut app, key(KeyCode::Tab), &tx);
    assert_eq!(app.input, "/resume ");
    assert!(app.slash_menu.is_none());
    assert!(rx.try_recv().is_err(), "Tab must not submit anything");
}

/// A second approval request while a card is pending must queue, not
/// overwrite — dropping its oneshot resolves to Deny on the core side,
/// silently refusing a call the user never saw.
#[test]
fn pending_approval_card_is_never_overwritten() {
    let card = |tool: &str| {
        let (rtx, _rrx) = tokio::sync::oneshot::channel();
        ApprovalCard {
            tool: tool.into(),
            detail: "cmd".into(),
            why: "risky".into(),
            reply: rtx,
            selected: 0,
            parked: false,
        }
    };
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.queue_approval(card("Bash"));
    app.queue_approval(card("Write"));
    assert_eq!(app.approval.as_ref().unwrap().tool, "Bash");
    assert_eq!(app.approval_backlog.len(), 1);

    resolve_card(&mut app, sunmao_core::approval::Approval::Once);
    assert_eq!(app.approval.as_ref().unwrap().tool, "Write");
    assert_eq!(app.focus, Focus::Approval);

    resolve_card(&mut app, sunmao_core::approval::Approval::Deny);
    assert!(app.approval.is_none());
    assert_eq!(app.focus, Focus::Input);
}
