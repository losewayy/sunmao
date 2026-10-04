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
    // project-layer `loop:` picks are pinned — bare disarms the whole gate
    crate::hooks::trust::set_pin(&dir, &dir.join(".sunmao/plugin.json"), "loop:bare", true)
        .unwrap();
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

/// `SearchTools` is registered (scripts and explicit whitelists can reach
/// it) but stays OFF the wire under `full`/`bare` — with every real
/// declaration already advertised, a catalog-lookup schema is dead weight.
#[tokio::test]
async fn search_tools_hidden_outside_ptc() {
    let dir = crate::fresh_test_dir("st-hidden");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = Context::new(
        Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    assert_eq!(ctx.loop_driver, LoopDriver::Full);
    let names: Vec<String> = ctx
        .advertised_tools()
        .iter()
        .map(|t| t.function.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "RunCode"),
        "full still advertises RunCode — {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "SearchTools"),
        "SearchTools must not consume schema budget outside ptc — {names:?}"
    );
    // but it IS registered — a script's tools.SearchTools resolves
    assert!(
        ctx.tools
            .declarations()
            .iter()
            .any(|t| t.function.name == "SearchTools")
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
    assert_eq!(LoopDriver::parse("ptc").unwrap(), LoopDriver::Ptc);
    assert_eq!(LoopDriver::parse("codemode").unwrap(), LoopDriver::Ptc);
    assert!(LoopDriver::parse("wat").is_err());
}

/// A provider that records the advertised tool names of every request —
/// the PTC driver's observable contract is "RunCode is the whole surface".
/// First request emits one real RunCode call; the follow-up answers stop.
struct DeclProbe {
    seen: std::sync::Mutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl ProviderAdapter for DeclProbe {
    async fn stream(&self, req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        self.seen.lock_or_recover().push(
            req.tools
                .map(|ts| ts.iter().map(|t| t.function.name.clone()).collect())
                .unwrap_or_default(),
        );
        let deltas = if self.seen.lock_or_recover().len() == 1 {
            // a real RunCode call — proves the loop still dispatches the one
            // advertised tool and nested calls still land in the gate
            vec![
                Ok(StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("rc".into()),
                        name: Some("RunCode".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some(
                            "{\"code\":\"tools.Glob({pattern:'**/*.rs'}).then(r=>r.ok+':'+r.output)\"}"
                                .into(),
                        ),
                        ..Default::default()
                    },
                ])),
                Ok(StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                }),
            ]
        } else {
            vec![
                Ok(StreamDelta::Content("done".into())),
                Ok(StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                }),
            ]
        };
        Ok(Box::pin(stream::iter(deltas)))
    }
}

/// `--loop ptc` semantics: the contract loop keeps running (hooks, gate,
/// nested-call bridge all live) but the request advertises only RunCode,
/// and a nested `tools.*` call still lands in the gate's deny path.
#[tokio::test]
async fn ptc_driver_advertises_runcode_only() {
    let dir = crate::fresh_test_dir("ptc");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(dir.join(".sunmao/plugin.json"), r#"{"loop":"ptc"}"#).unwrap();
    crate::hooks::trust::set_pin(&dir, &dir.join(".sunmao/plugin.json"), "loop:ptc", true).unwrap();
    // a deny that must still catch the *nested* call
    std::fs::write(
        dir.join(".sunmao/permissions.json"),
        r#"{"permissions":{"deny":["Glob(**/*.rs)"]}}"#,
    )
    .unwrap();

    let probe = Arc::new(DeclProbe {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let ctx = Arc::new(Context::new(
        probe.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    assert_eq!(ctx.loop_driver, LoopDriver::Ptc, "manifest must resolve");

    let agent = AgentLoop::new(ctx.clone());
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));

    let reqs = probe.seen.lock_or_recover().clone();
    assert!(!reqs.is_empty(), "probe saw no requests");
    assert_eq!(
        reqs[0],
        vec!["RunCode", "SearchTools"],
        "ptc advertises the borrowed-tools pair only"
    );

    // the script ran: its Glob went through the gate and was denied —
    // {ok:false, output} folds into the script's return value
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    assert!(
        evs.iter().any(|e| matches!(
            e,
            SessionEvent::ToolResult { name, output, .. }
                if name == "RunCode" && output.contains("false")
        )),
        "nested deny must reach the script"
    );
    std::fs::remove_dir_all(&dir).ok();
}
