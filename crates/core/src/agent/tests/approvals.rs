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
