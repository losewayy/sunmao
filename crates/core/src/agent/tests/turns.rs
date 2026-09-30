use super::*;

#[tokio::test]
async fn error_path_still_emits_turn_end() {
    // provider that always fails to establish the stream
    struct FailProvider;
    #[async_trait::async_trait]
    impl ProviderAdapter for FailProvider {
        async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
            anyhow::bail!("provider down")
        }
    }
    let ctx = Arc::new(Context::new(
        Arc::new(FailProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx);
    let rec = RecObserver(std::sync::Mutex::new(Vec::new()));
    let res = agent.run_turn("hi", &rec).await;
    assert!(res.is_err(), "stream failure must propagate");
    let events = rec.0.lock().unwrap();
    assert!(
        events.iter().any(|t| t.starts_with("TurnEnd:Other")),
        "frontends need TurnEnd even on error — got {events:?}"
    );
}

#[tokio::test]
async fn turn_completes_on_plain_text() {
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    let outcome = agent.run_turn("hi", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    // log holds user + assistant messages
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1].role, sunmao_llm::types::Role::Assistant);
}

#[tokio::test]
async fn tool_call_roundtrip_feeds_back() {
    // first stream: a Glob tool call (real filesystem tool), then finish
    // second stream: plain text → turn completes
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("call_1".into()),
                        name: Some("Glob".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some("{\"pattern\":\"**/*.rs\"}".into()),
                        ..Default::default()
                    },
                ]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ],
            vec![
                StreamDelta::Content("I found the files".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    let outcome = agent.run_turn("list files", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    // two provider calls: original turn + post-tool continuation
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    // events: user msg, assistant msg (w/ tool_calls), tool_call fact,
    // tool_result, assistant text
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    assert!(msgs
        .iter()
        .any(|m| matches!(m.role, sunmao_llm::types::Role::Tool)));
}

#[tokio::test]
async fn malformed_tool_args_become_failed_result() {
    // provider emits a Glob call with broken JSON args, then a text reply —
    // turn must complete and the bad call must surface as a ToolResult fail,
    // not an abort.
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("bad1".into()),
                        name: Some("Glob".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some("{not json".into()),
                        ..Default::default()
                    },
                ]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ],
            vec![
                StreamDelta::Content("sorry, retrying".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    let outcome = agent.run_turn("list", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    // two provider calls: the model got the failure fed back
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    // tool message present containing the malformed-call error — and
    // exactly ONE of them: the fold derives the protocol message from the
    // ToolResult event, so a stray Message::tool_result append would
    // double-report the call and providers hard-reject the transcript.
    let tool_msgs: Vec<_> = msgs
        .iter()
        .filter(|m| matches!(m.role, sunmao_llm::types::Role::Tool))
        .collect();
    assert_eq!(tool_msgs.len(), 1, "one result per call — no duplicates");
    assert!(tool_msgs[0]
        .content
        .as_deref()
        .unwrap()
        .contains("malformed"));
}

#[tokio::test]
async fn cancel_flag_breaks_loop() {
    // provider would return tool_calls forever; cancel must interrupt
    let mut responses = std::collections::VecDeque::new();
    for _ in 0..10 {
        responses.push_back(vec![
            StreamDelta::ToolCalls(vec![ToolCallFragment {
                index: 0,
                id: Some("c".into()),
                name: Some("Glob".into()),
                arguments: Some("{\"pattern\":\"*\"}".into()),
            }]),
            StreamDelta::Finish {
                reason: Some("tool_calls".into()),
                usage: None,
            },
        ]);
    }
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(responses),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    ctx.cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let agent = AgentLoop::new(ctx);
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Other(ref s) if s == "cancelled"));
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
}

/// The turn fence: a context's turns serialize — a second run_turn queues
/// on `ctx.turn_lock` instead of interleaving facts into the same log.
/// Proven by holding the lock externally: the turn never reaches the
/// provider until it's released.
#[tokio::test]
async fn concurrent_turns_queue_on_the_fence() {
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![vec![
            StreamDelta::Content("hi".into()),
            StreamDelta::Finish {
                reason: Some("stop".into()),
                usage: None,
            },
        ]])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    // hold the fence — the spawned turn must stall before its first call
    let guard = ctx.turn_lock.lock().await;
    let agent = AgentLoop::new(ctx.clone());
    let turn = tokio::spawn(async move { agent.run_turn("go", &NullObserver).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a queued turn must not reach the provider behind the fence"
    );
    drop(guard);
    let outcome = turn.await.unwrap().unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
}
