use super::*;
use futures_util::stream;
use sunmao_llm::types::Usage;
use sunmao_llm::{ChatRequest, DeltaStream, ProviderAdapter, StreamDelta};

struct MockProvider;
#[async_trait::async_trait]
impl ProviderAdapter for MockProvider {
    async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamDelta::Content("bg done".into())),
            Ok(StreamDelta::Finish {
                reason: Some("stop".into()),
                usage: Some(Usage::default()),
            }),
        ])))
    }
}

/// run_in_background returns a task id at once, and the finished child
/// pushes a TaskDone fact into the *parent's* session log — the fold
/// then surfaces it as a tagged user message (push delivery, no polling).
#[tokio::test]
async fn bg_task_pushes_result_into_parent_log() {
    let dir = std::env::temp_dir().join(format!("sunmao-bg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );

    let res = TaskTool
        .call(
            json!({"prompt": "scout it", "run_in_background": true}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(res.ok);
    assert!(res.output.contains("sub-"), "call returns the task id");

    // the detached child appends TaskDone once it finishes — give it a
    // moment, then check the parent's fold.
    let mut found = false;
    for _ in 0..200 {
        let evs = ctx.sessions.lock().await.events().await.unwrap_or_default();
        found = evs
            .iter()
            .any(|e| matches!(e, SessionEvent::TaskDone { ok: true, .. }));
        if found {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(found, "bg task must append TaskDone to the parent log");

    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    assert!(
        msgs.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.contains("<task-result") && c.contains("bg done"))),
        "TaskDone must fold into a tagged user message"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Two spawns in the same millisecond used to share `sub-<ms>.jsonl` —
/// lane suffix must keep session files distinct.
#[tokio::test]
async fn spawn_ids_are_unique_within_a_millisecond() {
    let dir = std::env::temp_dir().join(format!("sunmao-uniq-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    let (a, _) = spawn_parts(&ctx, None).await;
    let (b, _) = spawn_parts(&ctx, None).await;
    assert_ne!(a, b, "concurrent spawns must not share a session id");
    std::fs::remove_dir_all(&dir).ok();
}

/// `spawns:` whitelist gates a named agent's own Task calls: a type
/// outside the list fails, an omitted type defaults to the first listed
/// agent, and self-recursion is refused.
#[tokio::test]
async fn spawns_whitelist_gates_children() {
    let dir = std::env::temp_dir().join(format!("sunmao-spawns-{}", std::process::id()));
    let agents = dir.join(".sunmao/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("orchestrator.md"),
        "---\nname: orchestrator\ndescription: o\nspawns: scout\n---\norch",
    )
    .unwrap();
    std::fs::write(
        agents.join("scout.md"),
        "---\nname: scout\ndescription: s\n---\nscout body",
    )
    .unwrap();
    std::fs::write(
        agents.join("grader.md"),
        "---\nname: grader\ndescription: g\n---\ngrader body",
    )
    .unwrap();

    let mut ctx = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    ctx.agent_name = Some("orchestrator".into());

    // not whitelisted → refused
    let err = resolve_spawn_def(&ctx, Some("grader")).unwrap_err();
    assert!(err.to_string().contains("may not spawn `grader`"));
    // self-recursion → refused
    assert!(resolve_spawn_def(&ctx, Some("orchestrator")).is_err());
    // omitted → defaults to the first whitelist entry
    let def = resolve_spawn_def(&ctx, None).unwrap().unwrap();
    assert_eq!(def.name, "scout");
    // unknown name → refused with the known list (no silent generic spawn)
    assert!(resolve_spawn_def(&ctx, Some("ghost")).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

/// `tools:` trims the child's registry to the whitelist; a declared
/// `spawns:` list auto-adds Task, and the depth cap strips it at the leaf.
#[tokio::test]
async fn tools_whitelist_and_depth_cap_trim_registry() {
    let dir = std::env::temp_dir().join(format!("sunmao-tools-{}", std::process::id()));
    let agents = dir.join(".sunmao/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("reader.md"),
        "---\nname: reader\ndescription: r\ntools: Read, Grep\n---\nread only",
    )
    .unwrap();
    std::fs::write(
        agents.join("orch.md"),
        "---\nname: orch\ndescription: o\ntools: Read\nspawns: reader\n---\nspawner",
    )
    .unwrap();

    let ctx = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    let defs: Vec<_> = crate::agents::load_all(&dir, &[]);
    let reader = defs.iter().find(|d| d.name == "reader").unwrap();
    let (_, reader_ctx) = spawn_parts(&ctx, Some(reader)).await;
    let names: Vec<_> = reader_ctx
        .tools
        .declarations()
        .iter()
        .map(|t| t.function.name.clone())
        .collect();
    assert_eq!(names, vec!["Grep", "Read"], "tools: must trim the registry");

    // a declared spawns whitelist auto-adds Task even if tools omitted it
    let orch = defs.iter().find(|d| d.name == "orch").unwrap();
    let (_, orch_ctx) = spawn_parts(&ctx, Some(orch)).await;
    let names: Vec<_> = orch_ctx
        .tools
        .declarations()
        .iter()
        .map(|t| t.function.name.clone())
        .collect();
    assert!(names.contains(&"Task".to_string()), "spawns implies Task");
    std::fs::remove_dir_all(&dir).ok();
}
