use super::app::{App, Submit};
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
