use super::*;

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

/// The loop-head auto-compact check re-reads `est_tokens` after a
/// compaction — but that estimate used to trust the last `Usage` fact,
/// which still describes the PRE-compact transcript. The check re-tripped
/// and the next iteration compacted the fresh summary. The estimate must
/// stop at the Compacted boundary.
#[tokio::test]
async fn auto_compact_does_not_run_twice_on_stale_usage() {
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            // 1st call = the summarizer
            vec![
                StreamDelta::Content("summary".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
            // 2nd call = the real turn answer (carries tool calls so the
            // loop iterates — iteration 1's loop-head check must NOT
            // re-trip on the stale pre-compact usage). The leading Content
            // matters: if a phantom second compaction consumes THIS
            // response, it still yields a non-empty summary and writes a
            // detectable second Compacted boundary.
            vec![
                StreamDelta::Content("calling tools".into()),
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
            ],
            // 3rd call = the wrap-up
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
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    // a Usage fact describing the PRE-compact transcript — the seed message
    // alone (~5k est-tokens) trips the head check, and the folded
    // post-compact transcript stays ~1.3k. Threshold sits between them so
    // only a stale usage reading can re-trip the loop-head check.
    ctx.sessions
        .lock()
        .await
        .append(&crate::session::SessionEvent::Message {
            message: sunmao_llm::types::Message::user("x".repeat(20_000)),
        })
        .await
        .unwrap();
    ctx.sessions
        .lock()
        .await
        .append(&crate::session::SessionEvent::Usage {
            usage: Usage {
                prompt_tokens: 100_000,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let agent = AgentLoop::new(ctx.clone()).with_compact_threshold(2_000);
    agent
        .run_turn("fresh question", &NullObserver)
        .await
        .unwrap();
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "summarizer + turn + wrap-up — a second compaction would need a 4th call"
    );
    let events = ctx.sessions.lock().await.events().await.unwrap();
    let compactions = events
        .iter()
        .filter(|e| matches!(e, crate::session::SessionEvent::Compacted { .. }))
        .count();
    assert_eq!(compactions, 1, "the summary must not be re-summarized");
}
