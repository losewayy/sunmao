use super::*;

#[tokio::test]
async fn subagent_tool_events_reach_live_sink_at_depth() {
    // The turn observer sees the parent's `Task` call itself; the
    // sub-agent's *inner* tool lifecycle must reach the frontend through
    // `live_sink` tagged depth=1 — and its TurnEnd must NOT leak
    // (forwarding it would unwind the outer turn's busy state).
    struct DepthRec(std::sync::Mutex<Vec<String>>);
    impl Observer for DepthRec {
        fn on_event(&self, ev: &LiveEvent) {
            match ev {
                LiveEvent::ToolStart { name, depth, .. } => self
                    .0
                    .lock()
                    .unwrap()
                    .push(format!("start:{name}:d{depth}")),
                LiveEvent::ToolDone {
                    name, ok, depth, ..
                } => self
                    .0
                    .lock()
                    .unwrap()
                    .push(format!("done:{name}:{ok}:d{depth}")),
                LiveEvent::TurnEnd { outcome } => {
                    self.0.lock().unwrap().push(format!("turnend:{outcome:?}"))
                }
                _ => {}
            }
        }
    }

    let glob_call = || {
        vec![
            StreamDelta::ToolCalls(vec![
                ToolCallFragment {
                    index: 0,
                    id: Some("g".into()),
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
        ]
    };
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            // parent turn: calls Task
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("t".into()),
                        name: Some("Task".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some("{\"prompt\":\"find files\"}".into()),
                        ..Default::default()
                    },
                ]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ],
            // sub-agent turn: calls Glob (shared provider — same queue)
            glob_call(),
            // sub-agent post-tool: text reply
            vec![
                StreamDelta::Content("sub found them".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
            // parent post-tool: text reply
            vec![
                StreamDelta::Content("all done".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let dir = std::env::temp_dir().join(format!("sunmao-depth-{}", std::process::id()));
    let ctx = Arc::new(Context::new(
        provider,
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    let sink = Arc::new(DepthRec(std::sync::Mutex::new(Vec::new())));
    let agent = AgentLoop::new(ctx.clone());
    agent.set_live_sink(sink.clone() as Arc<dyn Observer>);
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));

    let events = sink.0.lock().unwrap();
    assert!(
        events.iter().any(|e| e == "start:Glob:d1"),
        "inner tool start must surface at depth=1 — got {events:?}"
    );
    assert!(
        events.iter().any(|e| e == "done:Glob:true:d1"),
        "inner tool done must surface at depth=1 — got {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.starts_with("turnend:")),
        "sub-agent TurnEnd must never reach the sink — got {events:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn batch_tasks_fan_out_on_distinct_lanes() {
    // `tasks[]` runs children concurrently — each claims its own lane so
    // the frontend can tell parallel siblings apart. The turn must also
    // complete with the merged per-task output.
    struct LaneRec(std::sync::Mutex<Vec<u8>>);
    impl Observer for LaneRec {
        fn on_event(&self, ev: &LiveEvent) {
            if let LiveEvent::ToolStart { lane, .. } = ev {
                self.0.lock().unwrap().push(*lane);
            }
        }
    }

    let task_args = r#"{"context":"both answers","tasks":[{"prompt":"say A"},{"prompt":"say B"}]}"#;
    let glob_call = |id: &str| {
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
                    arguments: Some("{\"pattern\":\"*.rs\"}".into()),
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
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("t".into()),
                        name: Some("Task".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some(task_args.into()),
                        ..Default::default()
                    },
                ]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ],
            // whichever child dequeues first gets a Glob call; its
            // continuation (and everything else) falls back to "done"
            glob_call("g1"),
            glob_call("g2"),
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let dir = std::env::temp_dir().join(format!("sunmao-batch-{}", std::process::id()));
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    let sink = Arc::new(LaneRec(std::sync::Mutex::new(Vec::new())));
    let agent = AgentLoop::new(ctx.clone());
    agent.set_live_sink(sink.clone() as Arc<dyn Observer>);
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    // parent: Task call + continuation; both children ran on this shared
    // provider → ≥ 4 total streams.
    assert!(provider.calls.load(std::sync::atomic::Ordering::Relaxed) >= 4);
    // the merged result carries both item verdicts
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    let tool_msg = msgs
        .iter()
        .find(|m| matches!(m.role, sunmao_llm::types::Role::Tool))
        .expect("Task result must fold in");
    assert!(tool_msg
        .content
        .as_deref()
        .unwrap_or("")
        .contains("## task 1 ✓"));
    assert!(tool_msg
        .content
        .as_deref()
        .unwrap_or("")
        .contains("## task 2 ✓"));
    // two children → two distinct non-zero lanes
    let mut lanes = sink.0.lock().unwrap().clone();
    lanes.sort_unstable();
    lanes.dedup();
    assert_eq!(lanes.len(), 2, "parallel children need distinct lanes");
    assert!(lanes.iter().all(|l| *l > 0));
    std::fs::remove_dir_all(&dir).ok();
}
