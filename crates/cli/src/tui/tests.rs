use super::app::{App, ApprovalCard, Focus, Submit};
use super::input::{input_key, resolve_card};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Point the slash menu at a named match — the candidate pool folds in
/// user-level skills and file commands, so a prefix can win a different
/// first row on a real machine (e.g. a `research` skill shadows `/res` →
/// `resume`). Tests must select the target, not assume its sort slot.
fn select_match(app: &mut App, name: &str) {
    let m = app.slash_menu.as_mut().expect("slash menu must be open");
    m.selected = m
        .matches
        .iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("{name} must be among the matches: {:?}", m.matches));
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
    assert!(matches!(rx.try_recv(), Ok(Submit::Turn(..))));
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
    assert!(matches!(rx.try_recv(), Ok(Submit::Flush(1))));
    // again — the last waiting item comes back too
    app.input.clear();
    app.cursor = 0;
    input_key(&mut app, key(KeyCode::Up), &tx);
    assert_eq!(app.input, "first queued");
    assert!(app.queue.is_empty());
    assert!(matches!(rx.try_recv(), Ok(Submit::Flush(1))));
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
    select_match(&mut app, "resume");

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

    resolve_card(
        &mut app,
        sunmao_core::approval::Approval::Deny { reason: None },
    );
    assert!(app.approval.is_none());
    assert_eq!(app.focus, Focus::Input);
}

/// Bare "/" + Enter must not fire the alphabetically-first builtin — it
/// completes to `/name ` exactly like Tab. Blind-submitting the first
/// match was the bug the user hit.
#[test]
fn slash_bare_enter_completes_not_submits() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.insert_char('/');
    assert!(app.slash_menu.is_some());
    input_key(&mut app, key(KeyCode::Enter), &tx);
    assert!(rx.try_recv().is_err(), "bare / must never submit");
    assert!(app.input.starts_with('/'));
    assert!(app.input.ends_with(' '), "completion fills `/name `");
}

/// Arg-taking builtins complete instead of firing — `/res`+Enter fills
/// `/resume ` and reopens the session picker rather than resuming.
/// (`/mod` no longer works as the fixture: `mode` is a literal prefix of
/// `model` and sorts first — pick a non-colliding prefix.)
#[test]
fn slash_arg_command_enter_fills_and_reopens() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    for c in "/res".chars() {
        app.insert_char(c);
    }
    select_match(&mut app, "resume");
    input_key(&mut app, key(KeyCode::Enter), &tx);
    assert!(rx.try_recv().is_err(), "arg command must not submit");
    assert_eq!(app.input, "/resume ");
    // with sessions on disk the menu reopens in picker mode; a bare
    // fixture has none, so only the fill is contract here
}

/// "Approve for session" covers the identical call everywhere — queued
/// requests for the same (tool, specifier) inherit the verdict instead
/// of re-asking what the user just answered.
#[test]
fn session_grant_auto_resolves_identical_queued_cards() {
    let (tx, _rx) = mpsc::unbounded_channel::<Submit>();
    let mk = |tool: &str, detail: &str| {
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        (
            ApprovalCard {
                tool: tool.into(),
                detail: detail.into(),
                why: "risky".into(),
                reply: rtx,
                selected: 0,
                parked: false,
            },
            rrx,
        )
    };
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    let (c1, _r1) = mk("Bash", "rm -rf x");
    let (c2, mut r2) = mk("Bash", "rm -rf x");
    let (c3, _r3) = mk("Write", "other");
    app.queue_approval(c1);
    app.queue_approval(c2);
    app.queue_approval(c3);
    resolve_card(&mut app, sunmao_core::approval::Approval::Session);
    // the identical queued card was auto-approved; the different one was
    // promoted into the active card slot by pop_approval
    assert!(matches!(
        r2.try_recv(),
        Ok(sunmao_core::approval::Approval::Session)
    ));
    assert!(app.approval_backlog.is_empty());
    assert_eq!(app.approval.as_ref().unwrap().tool, "Write");
    let _ = tx;
}

/// A turn over the cap folds earlier steps into one StepSummary row —
/// audit/note lines interleaved in the folded range stay in place, and
/// `e` (toggle_fold) splices the steps back where they were.
#[test]
fn fold_by_cap_compresses_old_steps_and_e_restores() {
    use super::blocks::BlockKind;
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    let banner_len = app.blocks.len(); // startup note
    app.echo_user("do the thing");
    // alternate names so verb-grouping doesn't merge them into one block
    for i in 0..15 {
        let name = if i % 2 == 0 { "Read" } else { "Bash" };
        app.tool_start(name, &format!("f{i}.rs"), 0, 0, None);
        app.tool_done(name, true, "ok", 0, 0, None);
        if i == 7 {
            app.push_audit("grant — Bash session");
        }
    }
    app.stream(BlockKind::Assistant, "done");
    app.stream(BlockKind::Thinking, "hmm");
    let unfolded = app.blocks.len();
    app.close_turn();

    // 17 steps > CAP 12 → 5 earliest tools folded into a StepSummary;
    // the audit row sat between folded steps and survives in place.
    let summaries: Vec<usize> = app
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| (b.kind == BlockKind::StepSummary).then_some(i))
        .collect();
    assert_eq!(summaries.len(), 1);
    let pos = summaries[0];
    assert_eq!(pos, banner_len + 1, "summary sits where the fold began");
    assert_eq!(app.blocks[pos].folded.len(), 5);
    assert_eq!(app.blocks[pos].text, "5 tool calls folded");
    assert_eq!(
        app.blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Tool)
            .count(),
        10
    );
    assert!(app.blocks.iter().any(|b| b.kind == BlockKind::Audit));
    assert_eq!(app.blocks.len(), unfolded - 5 + 1);

    // e expands: summary is replaced by its folded steps in order.
    app.selected = pos;
    app.toggle_fold();
    assert!(app.blocks.iter().all(|b| b.kind != BlockKind::StepSummary));
    assert_eq!(app.blocks.len(), unfolded);
    assert_eq!(app.blocks[pos].kind, BlockKind::Tool);
    let t = app.blocks[pos].tool.as_ref().unwrap();
    assert_eq!(t.summary, "f0.rs");
}

/// Scrolling while a turn folds keeps the selection on the same block —
/// a folded-away selection lands on the summary, a kept one shifts by
/// (removed − 1 for the summary row itself).
#[test]
fn fold_by_cap_remaps_selection() {
    use super::blocks::BlockKind;
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.echo_user("q");
    for i in 0..20 {
        let name = if i % 2 == 0 { "Read" } else { "Bash" };
        app.tool_start(name, &format!("f{i}.rs"), 0, 0, None);
        app.tool_done(name, true, "ok", 0, 0, None);
    }
    // last tool block stays selected through the fold
    app.selected = app.blocks.len() - 1;
    app.close_turn();
    // 20 steps, 8 folded + summary at index 2; old tail index was 21,
    // → 21 - 8 + 1 = 14 and it must still be f19
    assert_eq!(app.blocks[app.selected].kind, BlockKind::Tool);
    assert_eq!(
        app.blocks[app.selected].tool.as_ref().unwrap().summary,
        "f19.rs"
    );
}

/// A short turn folds nothing — cap is strictly "more than TURN_CAP".
#[test]
fn fold_by_cap_leaves_short_turns_alone() {
    use super::blocks::BlockKind;
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.echo_user("q");
    for i in 0..5 {
        let name = if i % 2 == 0 { "Read" } else { "Bash" };
        app.tool_start(name, &format!("f{i}.rs"), 0, 0, None);
        app.tool_done(name, true, "ok", 0, 0, None);
    }
    let len = app.blocks.len();
    app.close_turn();
    assert_eq!(app.blocks.len(), len);
    assert!(app.blocks.iter().all(|b| b.kind != BlockKind::StepSummary));
}

/// `@` opens the path menu (Files kind): directories descend instead of
/// terminating, files fill and close — and Enter never submits the
/// half-typed fragment, same contract as the command menu.
#[test]
fn at_mention_completes_paths_with_descent() {
    let dir = std::env::temp_dir().join(format!("sunmao-at-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.join("README.md"), "x").unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", dir.clone(), "s-test");
    for c in "@sr".chars() {
        app.insert_char(c);
    }
    let dir_pos = {
        let m = app.slash_menu.as_ref().expect("menu opens on @sr");
        assert_eq!(m.kind, super::menu::MenuKind::Path);
        m.matches
            .iter()
            .position(|c| c == "src/")
            .expect("src/ completes")
    };

    // selecting the directory descends — the menu stays open
    app.slash_menu.as_mut().unwrap().selected = dir_pos;
    input_key(&mut app, key(KeyCode::Enter), &tx);
    assert_eq!(app.input, "@src/");
    assert!(app.slash_menu.is_some(), "dir descend keeps the menu");
    for c in "main".chars() {
        app.insert_char(c);
    }
    input_key(&mut app, key(KeyCode::Enter), &tx);
    assert_eq!(app.input, "@src/main.rs ");
    assert!(app.slash_menu.is_none(), "a file terminates completion");
    assert!(rx.try_recv().is_err(), "path accept never submits");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `@` mid-word is not a mention — `x@sr` (email-shaped text) opens no
/// menu; only a whitespace-boundary or leading `@` counts.
#[test]
fn at_mention_needs_word_boundary() {
    let (_tx, _rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    for c in "x@sr".chars() {
        app.insert_char(c);
    }
    assert!(app.slash_menu.is_none());
}

/// A `@` fragment inside an otherwise normal sentence completes against
/// the same pool — the mention rewrites in place, nothing else moves.
#[test]
fn at_mention_rewrites_only_the_fragment() {
    let dir = std::env::temp_dir().join(format!("sunmao-at2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "x").unwrap();

    let mut app = App::new("m", dir.clone(), "s-test");
    app.input = "check @no please".into();
    app.cursor = "check @no".chars().count();
    app.refresh_slash_menu();
    assert!(app.slash_menu.is_some());
    assert!(app.accept_path_candidate("note.txt"));
    assert_eq!(app.input, "check @note.txt please");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `/resume <frag>` opens the session picker (Sessions kind): Enter on a
/// candidate submits `/resume <id>` — picking a session IS the command.
/// Completing the command name (`/res`+Enter) fills `/resume ` and the
/// picker opens on the freshly scanned session dir.
#[test]
fn resume_picker_lists_sessions_and_enter_resumes() {
    let dir = std::env::temp_dir().join(format!("sunmao-sess-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sdir = dir.join(".sunmao").join("sessions");
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(sdir.join("s-alpha.jsonl"), "").unwrap();
    std::fs::write(sdir.join("s-beta.jsonl"), "").unwrap();
    std::fs::write(sdir.join("other.txt"), "").unwrap(); // non-jsonl ignored

    let (tx, mut rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", dir.clone(), "s-test");
    for c in "/res".chars() {
        app.insert_char(c);
    }
    // resume is arg-taking: Enter fills `/resume `, then the picker opens
    select_match(&mut app, "resume");
    input_key(&mut app, key(KeyCode::Enter), &tx);
    assert_eq!(app.input, "/resume ");
    let m = app
        .slash_menu
        .as_ref()
        .expect("sessions picker must open after fill");
    assert_eq!(m.kind, super::menu::MenuKind::Sessions);
    assert!(m.matches.contains(&"s-alpha".to_string()));
    assert!(m.matches.contains(&"s-beta".to_string()));
    assert!(!m.matches.iter().any(|c| c == "other"));

    // filter by fragment, then Enter resumes the selected session
    for c in "beta".chars() {
        app.insert_char(c);
    }
    let m = app.slash_menu.as_ref().expect("picker stays on filter");
    assert_eq!(m.matches, vec!["s-beta".to_string()]);
    input_key(&mut app, key(KeyCode::Enter), &tx);
    match rx.try_recv() {
        Ok(Submit::Resume(Some(id))) => assert_eq!(id, "s-beta"),
        other => panic!("expected Resume(Some(s-beta)), got {other:?}"),
    }
    assert!(app.input.is_empty(), "submit drains the buffer");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Tab on a session candidate fills `/resume <id>` without submitting —
/// the user may want `/resume --fork`-style edits first.
#[test]
fn resume_picker_tab_fills_without_submit() {
    let dir = std::env::temp_dir().join(format!("sunmao-sess2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sdir = dir.join(".sunmao").join("sessions");
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(sdir.join("s-only.jsonl"), "").unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", dir.clone(), "s-test");
    for c in "/resume s".chars() {
        app.insert_char(c);
    }
    assert!(app.slash_menu.is_some());
    input_key(&mut app, key(KeyCode::Tab), &tx);
    assert_eq!(app.input, "/resume s-only");
    assert!(rx.try_recv().is_err(), "Tab must not submit");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Transcript virtualization: only the viewport window materializes —
/// the tail shows the newest block at scroll 0, a deep scroll shows the
/// oldest. Caught via the real draw path on a test backend.
#[test]
fn transcript_virtualizes_offscreen_blocks() {
    use super::render::draw;
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    for i in 0..60 {
        app.push_note(&format!("note-{i}"));
    }
    let backend = ratatui::backend::TestBackend::new(60, 24);
    let mut term = ratatui::Terminal::new(backend).unwrap();

    // pinned to tail: the newest note is visible, the oldest is not
    term.draw(|f| draw(f, &mut app)).unwrap();
    let text: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("note-59"), "tail block must render");
    assert!(
        !text.contains("note-00"),
        "offscreen head must not materialize"
    );

    // scrolled to the top: the oldest note becomes visible, newest leaves
    app.scroll_back = 500;
    term.draw(|f| draw(f, &mut app)).unwrap();
    let text: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("note-0"), "scrolled-back head must render");
    assert!(
        !text.contains("note-59"),
        "offscreen tail must not materialize"
    );
    // scroll got clamped to a real value, not stuck at 500
    assert!(app.scroll_back <= 200);
}

/// A large paste stashes to `[paste #N]` — the composer holds the marker,
/// submit expands it to a tagged block the model can parse. Small pastes
/// insert verbatim; !-mode keeps markers literal (local shell, not the
/// model's convention).
#[test]
fn large_paste_stashes_and_expands_on_submit() {
    let (_tx, _rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    let big = "x".repeat(3000);
    app.insert_str(&big);
    assert_eq!(app.input, "[paste #1]");
    assert_eq!(app.paste_stash.len(), 1);
    // small paste goes straight in
    app.insert_str(" small");
    assert_eq!(app.input, "[paste #1] small");

    let sub = app.submit();
    let Submit::Turn(text, _) = sub else {
        panic!("expected a turn");
    };
    assert!(text.contains("<pasted-text>"), "{text}");
    assert!(text.contains(&"x".repeat(3000)), "content must expand");
    assert!(!text.contains("[paste #1]"), "marker must not leak");

    // !-mode: marker stays literal — the local shell isn't the model
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.insert_str(&"y".repeat(3000));
    app.input = format!("!cat {}", app.input);
    app.cursor = app.input.chars().count();
    let Submit::Bash(cmd) = app.submit() else {
        panic!("expected bash");
    };
    assert!(cmd.contains("[paste #1]"), "bash keeps the literal marker");
}

/// History stores the compact marker, not the expanded paste — Up-browse
/// restores what the composer showed.
#[test]
fn paste_history_stores_the_marker() {
    let (_tx, _rx) = mpsc::unbounded_channel::<Submit>();
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.insert_str(&"z".repeat(3000));
    let _ = app.submit();
    assert_eq!(app.history.last().unwrap(), "[paste #1]");
}
