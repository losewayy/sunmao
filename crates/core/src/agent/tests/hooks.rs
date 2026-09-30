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
