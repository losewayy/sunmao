use super::*;

// ── approval modes (SPEC §4.6) ──────────────────────────────────────────

/// Counting approver — each `approve` call bumps it and returns a fixed
/// verdict, so tests can ask "was a prompt even raised".
struct Counting(
    #[allow(dead_code)] std::sync::atomic::AtomicUsize,
    crate::approval::Approval,
);
#[async_trait::async_trait]
impl crate::approval::Approver for Counting {
    async fn approve(&self, _t: &str, _d: &str, _w: &str) -> crate::approval::Approval {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.1.clone()
    }
}

/// Scripted one-call turn: model issues `tool(args)` then stops.
fn one_call_ctx(
    dir: &std::path::Path,
    tool: &str,
    args: &str,
    verdict: crate::approval::Approval,
    mode: crate::agent::ApprovalMode,
) -> (std::sync::Arc<Context>, std::sync::Arc<Counting>) {
    let provider = std::sync::Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("c1".into()),
                        name: Some(tool.into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some(args.to_string()),
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
    let approver = std::sync::Arc::new(Counting(std::sync::atomic::AtomicUsize::new(0), verdict));
    let mut ctx_raw = Context::new(
        provider,
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.to_path_buf(),
    );
    ctx_raw.approval = approver.clone();
    *ctx_raw.approval_mode.write().unwrap() = mode;
    (std::sync::Arc::new(ctx_raw), approver)
}

async fn bash_result(ctx: &std::sync::Arc<Context>) -> (bool, String) {
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    evs.iter()
        .find_map(|e| match e {
            SessionEvent::ToolResult {
                name, ok, output, ..
            } if name == "Bash" => Some((*ok, output.clone())),
            _ => None,
        })
        .expect("a Bash result must be recorded")
}

/// read_only refuses write-shaped tools and mutating commands outright;
/// pure reads pass without a prompt. deny still wins over every mode.
#[tokio::test]
async fn read_only_blocks_writes_and_mutating_bash() {
    use crate::agent::ApprovalMode;
    use crate::approval::Approval;
    let dir = crate::fresh_test_dir("ro-mode");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();

    // Read passes silently under read_only — zero prompts
    {
        let (ctx, appr) = one_call_ctx(
            &dir,
            "Read",
            "{\"path\":\"README.md\"}",
            Approval::Once,
            ApprovalMode::ReadOnly,
        );
        AgentLoop::new(ctx)
            .run_turn("go", &NullObserver)
            .await
            .unwrap();
        assert_eq!(appr.0.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
    // Write is refused outright
    {
        let (ctx, appr) = one_call_ctx(
            &dir,
            "Write",
            "{\"path\":\"x.txt\",\"content\":\"hi\"}",
            Approval::Once,
            ApprovalMode::ReadOnly,
        );
        AgentLoop::new(ctx.clone())
            .run_turn("go", &NullObserver)
            .await
            .unwrap();
        let evs = ctx.sessions.lock().await.events().await.unwrap();
        let r = evs
            .iter()
            .find_map(|e| match e {
                SessionEvent::ToolResult { ok, output, .. } => Some((*ok, output.clone())),
                _ => None,
            })
            .expect("result recorded");
        assert!(
            !r.0 && r.1.contains("read_only"),
            "Write must be refused: {}",
            r.1
        );
        assert_eq!(appr.0.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
    // a read-only Bash segment passes; a mutating one is refused
    {
        let (ctx, _a) = one_call_ctx(
            &dir,
            "Bash",
            "{\"command\":\"ls -la\"}",
            Approval::Once,
            ApprovalMode::ReadOnly,
        );
        AgentLoop::new(ctx.clone())
            .run_turn("go", &NullObserver)
            .await
            .unwrap();
        let (ok, out) = bash_result(&ctx).await;
        assert!(ok, "ls should run under read_only: {out}");
    }
    {
        let (ctx, _a) = one_call_ctx(
            &dir,
            "Bash",
            "{\"command\":\"ls && rm -rf x\"}",
            Approval::Once,
            ApprovalMode::ReadOnly,
        );
        AgentLoop::new(ctx.clone())
            .run_turn("go", &NullObserver)
            .await
            .unwrap();
        let (ok, out) = bash_result(&ctx).await;
        assert!(!ok, "mutating segment must be refused: {out}");
        assert!(out.contains("read_only") || out.contains("denied"), "{out}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// full_access never prompts — a risky command runs without the approver —
/// but a deny rule still refuses (the blacklist nothing overrides).
#[tokio::test]
async fn full_access_skips_prompts_but_not_deny() {
    use crate::agent::ApprovalMode;
    use crate::approval::Approval;
    let dir = crate::fresh_test_dir("full-mode");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/permissions.json"),
        r#"{"permissions":{"deny":["Bash(rm -rf *)"]}}"#,
    )
    .unwrap();
    // risky command: never prompted
    {
        let (ctx, appr) = one_call_ctx(
            &dir,
            "Bash",
            "{\"command\":\"git push origin\"}",
            Approval::Deny { reason: None }, // even a would-be deny is never reached
            ApprovalMode::FullAccess,
        );
        AgentLoop::new(ctx.clone())
            .run_turn("go", &NullObserver)
            .await
            .unwrap();
        assert_eq!(appr.0.load(std::sync::atomic::Ordering::Relaxed), 0);
        let (_, out) = bash_result(&ctx).await;
        // the command EXECUTED — the gate is what we're testing; whether
        // git itself succeeds in the test dir is irrelevant
        assert!(
            !out.contains("denied") && !out.contains("blocked"),
            "full_access must not stop risky commands: {out}"
        );
    }
    // deny rule: still refused
    {
        let (ctx, appr) = one_call_ctx(
            &dir,
            "Bash",
            "{\"command\":\"echo hi && rm -rf x\"}",
            Approval::Once,
            ApprovalMode::FullAccess,
        );
        AgentLoop::new(ctx.clone())
            .run_turn("go", &NullObserver)
            .await
            .unwrap();
        let (ok, out) = bash_result(&ctx).await;
        assert!(!ok, "deny rules win over full_access: {out}");
        assert_eq!(appr.0.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// always_ask prompts on mutating calls even when nothing else would flag
/// them — a plain Write or a harmless-looking `mkdir` still asks. Reads
/// stay silent.
#[tokio::test]
async fn always_ask_prompts_on_mutations() {
    use crate::agent::ApprovalMode;
    use crate::approval::Approval;
    let dir = crate::fresh_test_dir("ask-mode");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    {
        let (ctx, appr) = one_call_ctx(
            &dir,
            "Write",
            "{\"path\":\"x.txt\",\"content\":\"hi\"}",
            Approval::Once,
            ApprovalMode::AlwaysAsk,
        );
        AgentLoop::new(ctx)
            .run_turn("go", &NullObserver)
            .await
            .unwrap();
        assert_eq!(appr.0.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
    {
        let (ctx, appr) = one_call_ctx(
            &dir,
            "Read",
            "{\"path\":\"README.md\"}",
            Approval::Once,
            ApprovalMode::AlwaysAsk,
        );
        AgentLoop::new(ctx)
            .run_turn("go", &NullObserver)
            .await
            .unwrap();
        assert_eq!(
            appr.0.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "reads stay silent"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A mid-session switch is durable: `ModeChange` lands in the log, and a
/// reopened context reseeds the stance (resume keeps full_access).
#[tokio::test]
async fn mode_switch_is_durable_and_reseeds() {
    use crate::agent::ApprovalMode;
    let dir = crate::fresh_test_dir("mode-log");
    let dir_log = dir.join(".sunmao/sessions");
    let log = SessionLog::open(&dir_log, "s-mode").await.unwrap();
    let path = log.path().to_path_buf();
    let provider = std::sync::Arc::new(MockProvider {
        responses: std::sync::Mutex::new(Default::default()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = std::sync::Arc::new(Context::new(provider, log, builtin_registry(), dir.clone()));
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_approval_mode(ApprovalMode::FullAccess, &NullObserver)
        .await;
    // durable fact in the log
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("\"mode_change\""),
        "ModeChange must be logged: {text}"
    );
    assert!(text.contains("full_access"));
    // a reopened context sees the stance
    let log2 = SessionLog::open_path(&path).await.unwrap();
    let ctx2 = Context::new(
        std::sync::Arc::new(MockProvider {
            responses: std::sync::Mutex::new(Default::default()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        log2,
        builtin_registry(),
        dir.clone(),
    );
    assert_eq!(
        *ctx2.approval_mode.read().unwrap(),
        ApprovalMode::FullAccess
    );
    drop(ctx2);
    std::fs::remove_dir_all(&dir).ok();
}
