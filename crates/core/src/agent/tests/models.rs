use super::*;

#[tokio::test]
async fn agent_def_model_selector_routes_the_spawn() {
    // A `model:` frontmatter selector must swap the sub-agent's adapter —
    // the parent's provider serves the Task call + continuation, while a
    // separate (observable) provider serves everything inside the child.
    let dir = crate::fresh_test_dir("route");
    std::fs::create_dir_all(dir.join(".sunmao/agents")).unwrap();
    std::fs::write(
        dir.join(".sunmao/agents/scout.md"),
        "---\nname: scout\ndescription: cheap scout\nmodel: other/x\n---\nYou scout.",
    )
    .unwrap();

    let parent = Arc::new(MockProvider {
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
                        arguments: Some(
                            "{\"prompt\":\"scout it\",\"subagent_type\":\"scout\"}".into(),
                        ),
                        ..Default::default()
                    },
                ]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ],
            vec![
                StreamDelta::Content("parent done".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let routed = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut ctx_raw = Context::new(
        parent.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    ctx_raw.models = Some(Arc::new(
        crate::models::ModelResolver::load(
            &dir,
            crate::models::ProviderDef {
                base_url: "http://unused".into(),
                api_key_env: None,
                api_key: None,
                dialect: "openai".into(),
            },
            "default",
        )
        .with_adapter("other/x", routed.clone()),
    ));
    let ctx = Arc::new(ctx_raw);
    let outcome = AgentLoop::new(ctx)
        .run_turn("go", &NullObserver)
        .await
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    // parent: Task call + post-tool continuation; child: its whole turn
    // went to the routed adapter.
    assert_eq!(parent.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert!(routed.calls.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn swap_model_installs_override_adapter() {
    // `/model sel` must move the next request onto the resolved adapter
    // and record the switch as a durable session fact. Unknown selectors
    // resolve to None — the baseline model stays.
    let dir = crate::fresh_test_dir("swap");
    std::fs::create_dir_all(&dir).unwrap();
    let base = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let alt = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut ctx_raw = Context::new(
        base.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    ctx_raw.models = Some(Arc::new(
        crate::models::ModelResolver::load(
            &dir,
            crate::models::ProviderDef {
                base_url: "http://unused".into(),
                api_key_env: None,
                api_key: None,
                dialect: "openai".into(),
            },
            "default",
        )
        .with_adapter("alt/x", alt.clone()),
    ));
    let ctx = Arc::new(ctx_raw);
    let agent = AgentLoop::new(ctx.clone());

    assert!(
        agent.swap_model("ghost/x").is_none(),
        "unknown must not swap"
    );
    let label = agent.swap_model("alt/x").unwrap();
    // the override bypasses file resolution, so the label falls back to
    // the selector itself — the swap is still real.
    agent.record_model_change("alt/x", &label).await;
    agent.run_turn("hi", &NullObserver).await.unwrap();

    assert_eq!(alt.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(base.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    let events = ctx.sessions.lock().await.events().await.unwrap_or_default();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Hook { event, detail }
                if event == "model.change" && detail.contains("alt/x")
        )),
        "model swap must be a durable session fact"
    );
    std::fs::remove_dir_all(&dir).ok();
}
