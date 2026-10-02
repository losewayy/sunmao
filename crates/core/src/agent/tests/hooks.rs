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
        tool_msg.content_text().as_deref().map(str::trim_end),
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
    assert!(
        tool_msg
            .content_text()
            .as_deref()
            .unwrap()
            .contains("policy")
    );
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

    // every hook payload lands on stdin; `cat >> flag` appends each firing.
    // Notification + PostToolUseFailure are detached fires — poll for the
    // flag files rather than asserting synchronously after turn end.
    async fn flag_lines(dir: &std::path::Path, name: &str) -> usize {
        for _ in 0..60 {
            if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
                return text.lines().count();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        0
    }
    let posts = flag_lines(&dir, "post-flag.txt").await;
    assert_eq!(posts, 2, "PostToolUse fires for both outcomes");
    let fails = flag_lines(&dir, "fail-flag.txt").await;
    assert_eq!(fails, 1, "PostToolUseFailure fires only on the denied call");
    let bell = flag_lines(&dir, "bell-flag.txt").await;
    assert_eq!(bell, 1, "one ask prompt → one Notification");
    std::fs::remove_dir_all(&dir).ok();
}

/// Cancelling a RUNNING turn must deliver the Interrupt hook (audit sees
/// the interruption), while cancelling an idle session must not — the
/// turn_lock gate separates a real interrupt from noise. The provider
/// streams a never-ending response; only `cancel_notify` ends the turn.
#[tokio::test]
async fn interrupt_hook_fires_on_running_turn_only() {
    /// Streams nothing, forever — the turn dies only via cancel_notify.
    struct Stalled;
    #[async_trait::async_trait]
    impl ProviderAdapter for Stalled {
        async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
            Ok(Box::pin(stream::pending()))
        }
    }

    let dir = crate::fresh_test_dir("irq2");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"Interrupt":[{"matcher":"","hooks":[{"type":"command","command":"echo 1 >> irq-flag.txt"}]}]}}"#,
    )
    .unwrap();
    // idle cancel — a separate Context with no running turn; the flag file
    // must stay absent (the interrupt gate is turn_lock, not the caller)
    let idle = AgentLoop::new(Arc::new(Context::new(
        Arc::new(Stalled),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    )));
    idle.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !dir.join("irq-flag.txt").exists(),
        "idle cancel must not fire Interrupt"
    );

    // running turn — the stream parks forever; cancel both aborts it
    // (cancel_notify) and fires the hook (detached spawn). Fresh Context:
    // a cancel on THIS one would persist into the next turn's first
    // iteration (cancelled resets at turn END, not start).
    let ctx = Arc::new(Context::new(
        Arc::new(Stalled),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    let agent = Arc::new(AgentLoop::new(ctx.clone()));
    let t = tokio::spawn({
        let agent = agent.clone();
        async move { agent.run_turn("go", &NullObserver).await }
    });
    // let the turn actually reach the stream wait before cancelling —
    // busy-wait on the lock so a slow scheduler can't fake "not running"
    let mut held = false;
    for _ in 0..100 {
        if ctx.turn_lock.try_lock().is_err() {
            held = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(held, "turn must be holding turn_lock by now");
    agent.cancel();
    t.await.unwrap().ok();
    // the detached hook races the test — poll for the flag
    let mut fired = false;
    for _ in 0..40 {
        if dir.join("irq-flag.txt").exists() {
            fired = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(fired, "cancel on a running turn must fire Interrupt");
    std::fs::remove_dir_all(&dir).ok();
}
