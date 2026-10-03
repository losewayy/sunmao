use super::app::{App, ApprovalCard, Submit};
use super::blocks::BlockKind;
use sunmao_core::SessionEvent as E;
use sunmao_llm::types::{Message, ToolCall};

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: sunmao_llm::types::FunctionCall {
            name: name.into(),
            arguments: args.into(),
        },
    }
}

/// --resume replay folds durable events into the same block shapes a
/// live turn produced: user band, assistant text, finished tool, audit
/// line — all closed.
#[test]
fn replay_rebuilds_block_transcript() {
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.replay(&[
        E::Message {
            message: Message::user("fix the bug"),
        },
        E::Message {
            message: Message::assistant(Some("looking".into()), vec![]),
        },
        E::ToolCall {
            call: call("c1", "Read", r#"{"path":"a.rs"}"#),
            depth: 0,
            lane: 0,
        },
        E::ToolResult {
            call_id: "c1".into(),
            name: "Read".into(),
            ok: true,
            output: "file body".into(),
            depth: 0,
            lane: 0,
        },
        E::Hook {
            event: "approval.session".into(),
            detail: "Bash: cargo test".into(),
        },
        E::LocalShell {
            command: "git status".into(),
            exit_code: 0,
            output: "clean".into(),
        },
    ]);
    let kinds: Vec<_> = app.blocks.iter().map(|b| b.kind).collect();
    // banner + user + assistant + tool + audit + local-shell tool
    assert_eq!(
        kinds,
        vec![
            BlockKind::Note,
            BlockKind::User,
            BlockKind::Assistant,
            BlockKind::Tool,
            BlockKind::Audit,
            BlockKind::Tool,
        ]
    );
    assert!(app.blocks.iter().all(|b| !b.open));
    // finished tools carry their verdict, not an interrupted mark
    assert_eq!(
        app.blocks[3].tool.as_ref().unwrap().done,
        Some(true),
        "replay must not mark finished tools interrupted"
    );
    assert!(!app.busy);
}

/// A note arriving while the turn is still alive (artifact, sub-agent
/// done, an approval verdict) must not clear `busy` — the footer and the
/// Ctrl-C path would both read a live turn as idle ("cancel" degrading to
/// "stash draft").
#[test]
fn mid_turn_note_keeps_busy() {
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.busy = true;
    app.busy_since = Some(std::time::Instant::now());
    app.push_note("[artifact 'x' → a.html (12 B)]");
    app.push_note("[context compacted] summary");
    assert!(app.busy, "notes must not masquerade as turn end");
    assert!(app.busy_since.is_some(), "the busy timer keeps ticking");
}

/// Only real end signals clear `busy`: close_turn for turns, end_op for
/// `/compact`-style foreground ops.
#[test]
fn close_turn_and_end_op_clear_busy() {
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.busy = true;
    app.close_turn();
    assert!(!app.busy);
    app.busy = true;
    app.end_op();
    assert!(!app.busy);
}

/// A cancelled turn answers every unanswered card `Cancelled` — the parked
/// card and the backlog both — so the suspended `approve()` unblocks as a
/// refusal instead of hanging the dispatcher past the turn end. Esc only
/// parks the card; the ask is still live.
#[test]
fn cancel_approvals_resolves_parked_and_queued_cards() {
    use sunmao_core::approval::Approval;
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    let card = |tool: &str| {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            ApprovalCard {
                tool: tool.into(),
                detail: "rm -rf x".into(),
                why: "danger".into(),
                reply: tx,
                selected: 0,
                parked: true,
            },
            rx,
        )
    };
    let (c1, r1) = card("Bash");
    let (c2, r2) = card("Bash");
    app.queue_approval(c1); // active card (focus taken, then Esc-parked here)
    app.approval.as_mut().unwrap().parked = true;
    app.focus = super::app::Focus::Scrollback;
    app.queue_approval(c2); // queued behind it
    app.busy = true; // the turn is still running underneath the cards

    app.cancel_approvals();

    assert_eq!(r1.blocking_recv().unwrap(), Approval::Cancelled);
    assert_eq!(r2.blocking_recv().unwrap(), Approval::Cancelled);
    assert!(app.approval.is_none() && app.approval_backlog.is_empty());
    assert!(app.busy, "a note must not pretend the turn ended");
}

/// `/model` is driver-routed: bare lists choices, an argument swaps.
#[test]
fn model_command_routes_to_driver() {
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.input = "/model".into();
    assert!(matches!(app.submit(), Submit::Model(None)));
    app.input = "/model default/qwen-flash".into();
    assert!(matches!(app.submit(), Submit::Model(Some(ref s)) if s == "default/qwen-flash"));
}

/// A tool call with no matching result in the log replays as
/// interrupted — same verdict close_turn gives a live-cancelled call.
#[test]
fn replay_marks_dangling_tool_interrupted() {
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.replay(&[E::ToolCall {
        call: call("c9", "Bash", r#"{"command":"rm -rf x"}"#),
        depth: 0,
        lane: 0,
    }]);
    let t = app.blocks[1].tool.as_ref().unwrap();
    assert_eq!(t.done, Some(false));
    assert!(t.output.contains("interrupted"));
}

/// Resumed sessions keep their last recorded context pressure — the
/// footer's ctx readout must come back with the replay, not reset to
/// nothing.
#[test]
fn replay_restores_last_usage() {
    let mut app = App::new("m", std::path::PathBuf::from("."), "s-test");
    app.replay(&[
        E::Usage {
            usage: sunmao_llm::types::Usage {
                prompt_tokens: 42_000,
                completion_tokens: 900,
                total_tokens: 42_900,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
        },
        E::Message {
            message: Message::user("hi"),
        },
    ]);
    assert_eq!(
        app.last_usage.as_ref().map(|u| u.prompt_tokens),
        Some(42_000)
    );
}
