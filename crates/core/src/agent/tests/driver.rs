use super::*;

/// `bare` is a real driver, not a flag soup: a project manifest naming it
/// resolves at Context::new, and the loop then runs tools WITHOUT the
/// dispatch gate or hooks — the fixture plants both a deny rule and a
/// vetoing hook to prove neither fires.
#[tokio::test]
async fn bare_driver_runs_tools_without_gate_or_hooks() {
    let dir = crate::fresh_test_dir("bare");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(dir.join(".sunmao/plugin.json"), r#"{"loop":"bare"}"#).unwrap();
    // a deny that would hard-refuse this exact call under `full`
    std::fs::write(
        dir.join(".sunmao/permissions.json"),
        r#"{"permissions":{"deny":["Glob(**/*.rs)"]}}"#,
    )
    .unwrap();
    // a hook that would veto every PreToolUse under `full`
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"","hooks":[{"type":"command","command":"exit 2"}]}]}}"#,
    )
    .unwrap();

    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("c".into()),
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
    assert_eq!(ctx.loop_driver, LoopDriver::Bare, "manifest must resolve");

    let agent = AgentLoop::new(ctx.clone());
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));

    // Glob actually ran — ok:true ToolResult — despite deny + vetoing hook
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    assert!(
        evs.iter().any(|e| matches!(
            e,
            SessionEvent::ToolResult {
                name,
                ok: true,
                ..
            } if name == "Glob"
        )),
        "bare must dispatch ungated — deny rules and hooks never see it"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Same fixture minus the `loop:` key resolves `full` — and the planted
/// deny then DOES refuse the call (the bare test isn't a false positive
/// on a broken deny).
#[tokio::test]
async fn full_driver_still_enforces_gate() {
    let dir = crate::fresh_test_dir("full");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/permissions.json"),
        r#"{"permissions":{"deny":["Glob(**/*.rs)"]}}"#,
    )
    .unwrap();

    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("c".into()),
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
    assert_eq!(ctx.loop_driver, LoopDriver::Full);
    let agent = AgentLoop::new(ctx.clone());
    agent.run_turn("go", &NullObserver).await.unwrap();
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    assert!(
        evs.iter().any(|e| matches!(
            e,
            SessionEvent::ToolResult {
                name,
                ok: false,
                ..
            } if name == "Glob"
        )),
        "full loop must honor the deny rule"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Layering: a preset manifest's `loop:` key wins over the project's —
/// later layers own the driver slot.
#[tokio::test]
async fn preset_loop_key_overrides_project() {
    let dir = crate::fresh_test_dir("lp");
    let preset = dir.join("preset-min");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::create_dir_all(&preset).unwrap();
    // project says bare; preset says full — preset wins
    std::fs::write(dir.join(".sunmao/plugin.json"), r#"{"loop":"bare"}"#).unwrap();
    std::fs::write(preset.join("plugin.json"), r#"{"loop":"full"}"#).unwrap();

    let ctx = Context::new(
        Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    )
    .with_extra_plugin_roots(vec![preset]);
    assert_eq!(ctx.loop_driver, LoopDriver::Full);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn driver_parse_accepts_known_names() {
    assert_eq!(LoopDriver::parse("full").unwrap(), LoopDriver::Full);
    assert_eq!(LoopDriver::parse("BARE").unwrap(), LoopDriver::Bare);
    assert_eq!(LoopDriver::parse("minimal").unwrap(), LoopDriver::Bare);
    assert!(LoopDriver::parse("wat").is_err());
}
