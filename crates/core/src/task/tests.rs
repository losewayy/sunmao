use super::parts::spawn_parts;
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

/// Scripted provider — each queued Vec is one response replayed in order,
/// falling back to a plain "done" reply once the queue drains (shared with
/// sub-agents: the child dequeues the same queue).
struct QueuedProvider {
    responses: std::sync::Mutex<std::collections::VecDeque<Vec<StreamDelta>>>,
}
#[async_trait::async_trait]
impl ProviderAdapter for QueuedProvider {
    async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        let deltas = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                vec![
                    StreamDelta::Content("done".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ]
            });
        Ok(Box::pin(stream::iter(deltas.into_iter().map(Ok))))
    }
}

/// run_in_background returns a task id at once, and the finished child
/// pushes a TaskDone fact into the *parent's* session log — the fold
/// then surfaces it as a tagged user message (push delivery, no polling).
#[tokio::test]
async fn bg_task_pushes_result_into_parent_log() {
    let dir = crate::fresh_test_dir("bg");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = Arc::new(Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));

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
    let ctx = Arc::new(Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
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

    let mut ctx = Arc::new(Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    Arc::get_mut(&mut ctx).unwrap().agent_name = Some("orchestrator".into());

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

    let ctx = Arc::new(Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
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
    let mut ctx = Arc::new(Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
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
    Arc::get_mut(&mut ctx).unwrap().models = Some(Arc::new(resolver));

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

/// `permissions:` frontmatter reaches the child's dispatch gate
/// end-to-end: a `deny:Write` def's own Write call refuses inside the
/// spawn — a durable `permission.denied` row lands in the CHILD's log and
/// the file never exists. The overlay also outranks the shared session
/// grant (gate checks deny before grants — `build_sub_ctx`'s session_grants
/// comment is the contract): granting `Write` on the parent must not leak
/// into the restricted child.
#[tokio::test]
async fn subagent_deny_rule_hard_refuses_inside_the_spawn() {
    let dir = crate::fresh_test_dir("perms");
    let agents = dir.join(".sunmao/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("writer.md"),
        "---\nname: writer\ndescription: w\npermissions: deny:Write\n---\nyou write",
    )
    .unwrap();
    let ctx = Arc::new(Context::new(
        Arc::new(QueuedProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                // the child's first turn: try to Write
                vec![
                    StreamDelta::ToolCalls(vec![
                        sunmao_llm::ToolCallFragment {
                            index: 0,
                            id: Some("w".into()),
                            name: Some("Write".into()),
                            arguments: None,
                        },
                        sunmao_llm::ToolCallFragment {
                            index: 0,
                            arguments: Some(
                                "{\"path\":\"child-wrote.txt\",\"content\":\"x\"}".into(),
                            ),
                            ..Default::default()
                        },
                    ]),
                    StreamDelta::Finish {
                        reason: Some("tool_calls".into()),
                        usage: None,
                    },
                ],
                // the child's second turn: accept the refusal and finish
                vec![
                    StreamDelta::Content("can't write, done".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
            ])),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    // a standing grant on the PARENT — the child's deny must still win
    ctx.grant_session("Write", "child-wrote.txt");
    assert_eq!(
        ctx.permissions.check("Write", "child-wrote.txt"),
        crate::permissions::Verdict::Default,
        "the parent has no Write rule — denial can only come from the def"
    );

    let res = TaskTool
        .call(
            json!({"prompt": "write the file", "subagent_type": "writer"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(res.ok, "child should recover and finish: {}", res.output);
    assert!(res.output.contains("can't write"), "{}", res.output);
    // the deny never let the write execute
    assert!(
        !dir.join("child-wrote.txt").exists(),
        "denied Write must not create the file"
    );
    // and the refusal is durable in the child's own log — the overlay is
    // provably on the child's Context, not just the parent's view of it
    let sessions_dir = dir.join(".sunmao/sessions");
    let mut denied = None;
    for e in crate::sorted_entries(&sessions_dir) {
        let p = e.path();
        if !p
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("sub-"))
        {
            continue;
        }
        let log = SessionLog::open_path(&p).await.unwrap();
        if log.events().await.unwrap().iter().any(|e| {
            matches!(e, SessionEvent::Hook { event, detail }
                if event == "permission.denied" && detail.contains("Write"))
        }) {
            denied = Some(p);
        }
    }
    assert!(
        denied.is_some(),
        "child log must carry a permission.denied row for Write"
    );
    std::fs::remove_dir_all(&dir).ok();
}
