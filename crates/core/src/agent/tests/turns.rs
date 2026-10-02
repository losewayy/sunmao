use super::*;
use crate::context::MutexRecover;

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
    let agent = AgentLoop::new(ctx.clone());
    let rec = RecObserver(std::sync::Mutex::new(Vec::new()));
    let res = agent.run_turn("hi", &rec).await;
    assert!(res.is_err(), "stream failure must propagate");
    let events = rec.0.lock_or_recover();
    assert!(
        events.iter().any(|t| t.starts_with("TurnEnd:Other")),
        "frontends need TurnEnd even on error — got {events:?}"
    );
    // the flag can't strand the next turn either — reset happens at
    // run_turn's tail, not the driver's success tail
    assert!(
        !ctx.cancelled.load(std::sync::atomic::Ordering::Relaxed),
        "cancelled must reset on every exit"
    );
}

/// A UserPromptSubmit veto returns early from run_turn_inner — it must
/// still emit TurnEnd (frontends unwind busy state from it) and reset
/// cancelled, or the TUI hangs with the queue permanently off by one.
#[tokio::test]
async fn hook_veto_still_emits_turn_end() {
    let dir = crate::fresh_test_dir("veto");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join("reply.json"),
        r#"{"continue":false,"stopReason":"vetoed"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"UserPromptSubmit":[{"matcher":"","hooks":[{"type":"command","command":"cat reply.json"}]}]}}"#,
    )
    .unwrap();
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    ctx.hooks
        .trust_all
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let agent = AgentLoop::new(ctx.clone());
    let rec = RecObserver(std::sync::Mutex::new(Vec::new()));
    let outcome = agent.run_turn("hi", &rec).await.unwrap();
    assert!(
        matches!(outcome, TurnOutcome::Other(ref s) if s.contains("vetoed")),
        "expected veto, got {outcome:?}"
    );
    let events = rec.0.lock_or_recover();
    assert_eq!(
        events.iter().filter(|t| t.starts_with("TurnEnd")).count(),
        1,
        "exactly one TurnEnd — got {events:?}"
    );
    assert!(
        !ctx.cancelled.load(std::sync::atomic::Ordering::Relaxed),
        "cancelled must reset even on the veto early-return"
    );
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "vetoed turn never reaches the provider"
    );
    std::fs::remove_dir_all(&dir).ok();
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
    assert!(
        msgs.iter()
            .any(|m| matches!(m.role, sunmao_llm::types::Role::Tool))
    );
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
    let rec = RecObserver(std::sync::Mutex::new(Vec::new()));
    let outcome = agent.run_turn("list", &rec).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    // the malformed call still surfaces on the live stream — transcript
    // parity with replay (which renders it from the ToolCall/ToolResult pair)
    {
        let events = rec.0.lock_or_recover();
        assert_eq!(
            events.iter().filter(|t| *t == "ToolStart").count(),
            1,
            "malformed call must emit ToolStart — got {events:?}"
        );
        assert_eq!(
            events.iter().filter(|t| *t == "ToolDone").count(),
            1,
            "malformed call must emit ToolDone — got {events:?}"
        );
    }
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
    assert!(
        tool_msgs[0]
            .content_text()
            .as_deref()
            .unwrap()
            .contains("malformed")
    );
}

/// Auto-compaction used to check at the TOP of the loop — after the prompt
/// was already appended — so the Compacted boundary folded the user's fresh
/// question into the summary and the model never saw it as a live message.
/// The check must run before the append.
#[tokio::test]
async fn auto_compact_runs_before_prompt_append() {
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            // 1st call = the summarizer
            vec![
                StreamDelta::Content("summary of the old talk".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
            // 2nd call = the real turn answer
            vec![
                StreamDelta::Content("answer".into()),
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
    // seed enough history to trip a tiny threshold
    ctx.sessions
        .lock()
        .await
        .append(&crate::session::SessionEvent::Message {
            message: sunmao_llm::types::Message::user("x".repeat(400)),
        })
        .await
        .unwrap();
    let agent = AgentLoop::new(ctx.clone()).with_compact_threshold(60);
    agent
        .run_turn("fresh question", &NullObserver)
        .await
        .unwrap();
    // 2 provider calls: summarizer + the actual turn
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    let texts: Vec<String> = msgs.iter().filter_map(|m| m.content_text()).collect();
    let spos = texts
        .iter()
        .position(|t| t.contains("summary of the old talk"))
        .expect("compacted summary must be in the fold");
    let qpos = texts
        .iter()
        .position(|t| *t == "fresh question")
        .expect("the prompt must survive compaction as a live user message");
    assert!(qpos > spos, "prompt must land AFTER the compacted boundary");
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
    let agent = AgentLoop::new(ctx.clone());
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Cancelled));
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    // the terminal fact lands on the log — a cancelled turn reads as
    // "stopped," not "crashed mid-stream" on replay
    let events = ctx.sessions.lock().await.events().await.expect("read log");
    assert!(
        events.iter().any(|e| matches!(
            e,
            crate::session::SessionEvent::Hook { event, .. } if event == "cancelled"
        )),
        "cancelled turn must leave a `cancelled` audit row"
    );
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

/// Steering: a message queued while the turn runs folds into THIS turn at
/// the next request boundary — appended after the settled tool_result,
/// before the follow-up request. It never becomes a separate queued turn.
#[tokio::test]
async fn steer_folds_into_running_turn() {
    struct SteerOnce {
        ctx: std::sync::Mutex<Option<Arc<Context>>>,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ProviderAdapter for SteerOnce {
        async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n == 0 {
                // mid-first-request the user queues a steer; this call also
                // emits a tool call so the turn keeps looping — the boundary
                // drain folds the steer before request #2
                if let Some(ctx) = self.ctx.lock_or_recover().take() {
                    ctx.steer
                        .lock()
                        .unwrap()
                        .push_back((7, "steer follow-up".into()));
                }
                return Ok(Box::pin(stream::iter(vec![
                    Ok(StreamDelta::ToolCalls(vec![ToolCallFragment {
                        index: 0,
                        id: Some("call_1".into()),
                        name: Some("Glob".into()),
                        arguments: Some("{\"pattern\":\"*.rs\"}".into()),
                    }])),
                    Ok(StreamDelta::Finish {
                        reason: Some("tool_calls".into()),
                        usage: None,
                    }),
                ])));
            }
            Ok(Box::pin(stream::iter(vec![
                Ok(StreamDelta::Content("ack".into())),
                Ok(StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                }),
            ])))
        }
    }
    let provider = Arc::new(SteerOnce {
        ctx: std::sync::Mutex::new(None),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    *provider.ctx.lock_or_recover() = Some(ctx.clone());
    let agent = AgentLoop::new(ctx.clone());
    let outcome = agent.run_turn("hi", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    let roles: Vec<String> = msgs
        .iter()
        .map(|m| format!("{:?}:{}", m.role, m.content_text().unwrap_or_default()))
        .collect();
    // [user "hi", assistant(calls), tool(result), user "steer", assistant "ack"]
    let pos = roles
        .iter()
        .position(|r| r.contains("steer follow-up"))
        .expect("steered text must fold into this turn's messages — {roles:?}");
    assert!(
        roles[pos - 1].starts_with("Tool"),
        "steer lands after the settled tool_result, never inside the pair — {roles:?}"
    );
    assert!(
        roles.last().unwrap().starts_with("Assistant"),
        "the turn answers the steer — {roles:?}"
    );
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "steer rides the same turn — no third request"
    );
    assert!(
        ctx.steer.lock_or_recover().is_empty(),
        "the drain consumes the queue"
    );
}
