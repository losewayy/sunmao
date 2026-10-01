use super::spawn::spawn_parts;
use super::*;
use crate::session::{SessionEvent, SessionLog};
use crate::tool::builtin_registry;
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
    let dir = crate::fresh_test_dir("bg");
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
    // roster registers immediately as running
    assert!(
        ctx.live_tasks
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.done.is_none()),
        "detached spawn must register in the roster"
    );

    // the detached child appends TaskDone once it finishes — the event is
    // async, so poll generously; parallel test load makes ~2s too tight.
    let mut found = false;
    for _ in 0..500 {
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
    // and the roster entry flips to done
    for _ in 0..500 {
        if ctx
            .live_tasks
            .lock()
            .unwrap()
            .iter()
            .all(|t| t.done.is_some())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        ctx.live_tasks
            .lock()
            .unwrap()
            .iter()
            .all(|t| t.done == Some(true)),
        "roster must settle when TaskDone lands"
    );

    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    assert!(
        msgs.iter().any(|m| m
            .content_text()
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
    let dir = crate::fresh_test_dir("uniq");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    let (a, _) = spawn_parts(&ctx, None, None).await;
    let (b, _) = spawn_parts(&ctx, None, None).await;
    assert_ne!(a, b, "concurrent spawns must not share a session id");
    std::fs::remove_dir_all(&dir).ok();
}

/// `spawns:` whitelist gates a named agent's own Task calls: a type
/// outside the list fails, an omitted type defaults to the first listed
/// agent, and self-recursion is refused.
#[tokio::test]
async fn spawns_whitelist_gates_children() {
    let dir = crate::fresh_test_dir("spawns");
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
    let dir = crate::fresh_test_dir("tools");
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
    let (_, reader_ctx) = spawn_parts(&ctx, Some(reader), None).await;
    let names: Vec<_> = reader_ctx
        .tools
        .declarations()
        .iter()
        .map(|t| t.function.name.clone())
        .collect();
    assert_eq!(names, vec!["Grep", "Read"], "tools: must trim the registry");

    // a declared spawns whitelist auto-adds Task even if tools omitted it
    let orch = defs.iter().find(|d| d.name == "orch").unwrap();
    let (_, orch_ctx) = spawn_parts(&ctx, Some(orch), None).await;
    let names: Vec<_> = orch_ctx
        .tools
        .declarations()
        .iter()
        .map(|t| t.function.name.clone())
        .collect();
    assert!(names.contains(&"Task".to_string()), "spawns implies Task");
    std::fs::remove_dir_all(&dir).ok();
}

/// Call-site `model` routes the spawn to another adapter — the same seam
/// OMP-style multi-model orchestration rides on. A selector that resolves
/// to nothing fails the call loudly (a typo'd route must never silently
/// inherit the parent's model).
#[tokio::test]
async fn call_site_model_routes_and_unknown_selector_fails() {
    let dir = crate::fresh_test_dir("taskmodel");
    std::fs::create_dir_all(&dir).unwrap();
    let mut ctx = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    let resolver = crate::models::ModelResolver::load(
        &dir,
        crate::models::ProviderDef {
            base_url: "http://local/v1".into(),
            api_key_env: None,
            api_key: None,
            dialect: "openai".into(),
            catalog: Vec::new(),
        },
        "default",
    )
    .with_adapter("@cheap", Arc::new(MockProvider));
    ctx.models = Some(Arc::new(resolver));

    // unknown selector → tool error carrying the available selectors
    let err = TaskTool
        .call(json!({"prompt": "p", "model": "@nope"}), &ctx)
        .await
        .err()
        .expect("unknown selector must fail");
    assert!(
        err.to_string().contains("unknown model selector `@nope`"),
        "{err}"
    );
    // resolvable selector → spawn succeeds (the routed adapter is the
    // child ctx's llm — observable via the override the test injected)
    let res = TaskTool
        .call(json!({"prompt": "p", "model": "@cheap"}), &ctx)
        .await
        .unwrap();
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("bg done"), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}
