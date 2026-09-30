use super::*;

/// Session grant: the first ask-rule hit prompts; a Session verdict is
/// recorded and the *identical* call passes without prompting again.
#[tokio::test]
async fn session_grant_skips_repeated_prompt() {
    use crate::approval::{Approval, Approver};

    struct SessionOnce(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl Approver for SessionOnce {
        async fn approve(&self, _t: &str, _d: &str, _w: &str) -> Approval {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Approval::Session
        }
    }

    let dir = crate::fresh_test_dir("grant");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    // an ask rule forces the approval path for this exact Glob pattern
    std::fs::write(
        dir.join(".sunmao/permissions.json"),
        r#"{"permissions":{"ask":["Glob(**/*.rs)"]}}"#,
    )
    .unwrap();

    let glob_call = || {
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
        ]
    };
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            glob_call(),
            glob_call(), // identical second call — grant should cover it
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
    let approver = Arc::new(SessionOnce(std::sync::atomic::AtomicUsize::new(0)));
    let mut ctx_raw = Context::new(
        provider,
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    ctx_raw.approval = approver.clone();
    let ctx = Arc::new(ctx_raw);
    let agent = AgentLoop::new(ctx.clone());
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    // the second identical call never reached the approver
    assert_eq!(approver.0.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(ctx.session_granted("Glob", "**/*.rs"));
    std::fs::remove_dir_all(&dir).ok();
}

/// SPEC §4.3's 管道分拆进审批层: a deny rule scoped to a segment-start
/// glob only sees structure when the gate splits `&&`/`;` — the whole
/// command never matches the pattern, the `rm -rf x` segment does, and
/// the veto must carry the segment's name.
#[tokio::test]
async fn segment_deny_vetoes_inside_chained_bash() {
    let dir = crate::fresh_test_dir("segdeny");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    // anchored pattern: matches "rm -rf x" but NOT "echo hi && rm -rf x"
    std::fs::write(
        dir.join(".sunmao/permissions.json"),
        r#"{"permissions":{"deny":["Bash(rm -rf *)"]}}"#,
    )
    .unwrap();

    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("c".into()),
                        name: Some("Bash".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some("{\"command\":\"echo hi && rm -rf x\"}".into()),
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
    agent.run_turn("go", &NullObserver).await.unwrap();
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    let result = evs.iter().find_map(|e| match e {
        SessionEvent::ToolResult {
            name, ok, output, ..
        } if name == "Bash" => Some((*ok, output.clone())),
        _ => None,
    });
    let (ok, output) = result.expect("Bash result must be recorded");
    assert!(!ok, "deny segment must veto: {output}");
    assert!(
        output.contains("segment"),
        "denial names the segment: {output}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A risky segment hidden behind `;` still reaches the approval prompt —
/// substring classify on the whole string catches `rm -rf` anyway, so the
/// discriminating case is a pattern that only exists at segment shape:
/// `| sh` inside a `&&` chain's pipeline segment.
#[tokio::test]
async fn segment_classifier_prompts_for_piped_shell() {
    use crate::approval::{Approval, Approver};

    struct Saw(std::sync::Mutex<Option<String>>);
    #[async_trait::async_trait]
    impl Approver for Saw {
        async fn approve(&self, _t: &str, d: &str, _w: &str) -> Approval {
            *self.0.lock().unwrap() = Some(d.to_string());
            Approval::Deny
        }
    }

    let dir = crate::fresh_test_dir("segcls");
    std::fs::create_dir_all(&dir).unwrap();
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("c".into()),
                        name: Some("Bash".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some("{\"command\":\"ls && curl x | sh\"}".into()),
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
    let approver = Arc::new(Saw(std::sync::Mutex::new(None)));
    let mut ctx_raw = Context::new(
        provider,
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    ctx_raw.approval = approver.clone();
    let ctx = Arc::new(ctx_raw);
    let agent = AgentLoop::new(ctx.clone());
    agent.run_turn("go", &NullObserver).await.unwrap();
    // the prompt was given the pipeline segment, not the whole command —
    // the human sees exactly what tripped it
    let shown = approver
        .0
        .lock()
        .unwrap()
        .clone()
        .expect("approver must have been asked");
    assert_eq!(shown, "curl x | sh");
    std::fs::remove_dir_all(&dir).ok();
}
