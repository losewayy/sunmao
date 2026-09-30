use super::*;

/// The rtk contract end-to-end through the real loop: PreToolUse hook
/// returns `hookSpecificOutput.updatedInput` → the tool executes the
/// REWRITTEN command, the log records a Hook fact + the effective
/// ToolCall, and the model sees the result of the rewritten command.
#[tokio::test]
async fn pretooluse_updated_input_rewrites_dispatch() {
    let dir = crate::fresh_test_dir("rtk");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    // the rtk hook shape: JSON on stdin, updatedInput on stdout
    // write the hook's stdout JSON to a file the hook cats — avoids
    // quoting an entire JSON doc inside a shell command string
    std::fs::write(
        dir.join("hook-response.json"),
        r#"{"hookSpecificOutput":{"updatedInput":{"command":"echo rewritten"}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"cat hook-response.json"}]}]}}"#,
    )
    .unwrap();
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("call_1".into()),
                        name: Some("Bash".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some("{\"command\":\"git status\"}".into()),
                        ..Default::default()
                    },
                ]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ],
            vec![
                StreamDelta::Content("done".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider,
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    let outcome = agent.run_turn("run it", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    // the tool result must be the REWRITTEN command's output
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    let tool_msg = msgs
        .iter()
        .find(|m| matches!(m.role, sunmao_llm::types::Role::Tool))
        .expect("tool result message");
    assert_eq!(
        tool_msg.content.as_deref().map(str::trim_end),
        Some("rewritten")
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A PreToolUse `permissionDecision:"deny"` (the newer dialect spelling)
/// must block the call before dispatch — same as exit-2 veto.
#[tokio::test]
async fn pretooluse_permission_deny_blocks() {
    let dir = crate::fresh_test_dir("deny");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join("hook-response.json"),
        r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"policy"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash|Read|Write","hooks":[{"type":"command","command":"cat hook-response.json"}]}]}}"#,
    )
    .unwrap();
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![
                StreamDelta::ToolCalls(vec![ToolCallFragment {
                    index: 0,
                    id: Some("c".into()),
                    name: Some("Bash".into()),
                    arguments: Some("{\"command\":\"echo hi\"}".into()),
                }]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ],
            vec![
                StreamDelta::Content("ok".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider,
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    agent.run_turn("go", &NullObserver).await.unwrap();
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    let tool_msg = msgs
        .iter()
        .find(|m| matches!(m.role, sunmao_llm::types::Role::Tool))
        .expect("tool result message");
    assert!(tool_msg.content.as_deref().unwrap().contains("policy"));
    std::fs::remove_dir_all(&dir).ok();
}

/// The union event surface: `PostToolUseFailure` fires only on a settled
/// bad result, `Notification` fires when the gate opens a prompt. One
/// turn: an allowed Glob (ok) then a denied one — the failure hook must
/// append exactly once, and the ask-rule prompt must have rung the bell.
#[tokio::test]
async fn failure_and_notification_events_fire_on_the_right_edges() {
    use crate::approval::{Approval, Approver};

    struct AlwaysOnce;
    #[async_trait::async_trait]
    impl Approver for AlwaysOnce {
        async fn approve(&self, _t: &str, _d: &str, _w: &str) -> Approval {
            Approval::Once
        }
    }

    let dir = crate::fresh_test_dir("union");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    // allowed call asks first (rings Notification), denied one is refused
    // outright — the failure hook still sees the settled bad result
    std::fs::write(
        dir.join(".sunmao/permissions.json"),
        r#"{"permissions":{"ask":["Glob(**/*.txt)"],"deny":["Glob(**/*.rs)"]}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{
          "PostToolUse":[{"matcher":"","hooks":[{"type":"command","command":"cat >> post-flag.txt; echo >> post-flag.txt"}]}],
          "PostToolUseFailure":[{"matcher":"","hooks":[{"type":"command","command":"cat >> fail-flag.txt; echo >> fail-flag.txt"}]}],
          "Notification":[{"matcher":"","hooks":[{"type":"command","command":"cat >> bell-flag.txt; echo >> bell-flag.txt"}]}]
        }}"#,
    )
    .unwrap();
    std::fs::write(dir.join("a.txt"), "x").unwrap();

    let glob = |pattern: &str, id: &str| {
        vec![
            StreamDelta::ToolCalls(vec![
                ToolCallFragment {
                    index: 0,
                    id: Some(id.into()),
                    name: Some("Glob".into()),
                    arguments: None,
                },
                ToolCallFragment {
                    index: 0,
                    arguments: Some(format!("{{\"pattern\":\"{pattern}\"}}")),
                    ..Default::default()
                },
            ]),
            StreamDelta::Finish {
                reason: Some("tool_calls".into()),
                usage: None,
            },
        ]
    };
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            glob("**/*.txt", "ok1"), // asks → approved → runs → PostToolUse only
            glob("**/*.rs", "bad1"), // deny rule → fails → PostToolUse + Failure
            vec![
                StreamDelta::Content("done".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut ctx_raw = Context::new(
        provider,
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    ctx_raw.approval = Arc::new(AlwaysOnce);
    let ctx = Arc::new(ctx_raw);
    let agent = AgentLoop::new(ctx.clone());
    agent.run_turn("go", &NullObserver).await.unwrap();

    // every hook payload lands on stdin; `cat >> flag` appends each firing
    let posts = std::fs::read_to_string(dir.join("post-flag.txt")).unwrap();
    assert_eq!(
        posts.lines().count(),
        2,
        "PostToolUse fires for both outcomes"
    );
    let fails = std::fs::read_to_string(dir.join("fail-flag.txt")).unwrap();
    assert_eq!(
        fails.lines().count(),
        1,
        "PostToolUseFailure fires only on the denied call"
    );
    let bell = std::fs::read_to_string(dir.join("bell-flag.txt")).unwrap();
    assert_eq!(bell.lines().count(), 1, "one ask prompt → one Notification");
    std::fs::remove_dir_all(&dir).ok();
}
