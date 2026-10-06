//! Fusion Sidekick routing — split out of `fusion.rs`'s test file for the
//! god-file budget; `super::fusion` owns the shared fixtures.

use super::fusion::{queued, text, tool_call};
use super::fusion_fixture::{fusion_ctx, fusion_ctx_unconfigured};
use super::*;
use std::sync::atomic::Ordering;

/// A session-level Sidekick selection routes an unpinned delegation.
#[tokio::test]
async fn session_sidekick_routes_an_unpinned_delegation() {
    let dir = crate::fresh_test_dir("fusion-sidekick");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();
    let lead = Arc::new(MockProvider {
        responses: queued(vec![
            tool_call(
                "f1",
                "FusionExecute",
                r#"{"spec":{"goal":"create w.txt containing hello"},"files":["w.txt"],"verify_commands":["echo verified"]}"#,
            ),
            text("wrapped"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let sub = Arc::new(MockProvider {
        responses: queued(vec![
            tool_call("w1", "Write", r#"{"path":"w.txt","content":"hello"}"#),
            text("wrote it"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = fusion_ctx_unconfigured(&dir, SessionLog::ephemeral(), lead.clone(), sub.clone());
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_fusion_model(
            crate::context::FusionModelRole::Lead,
            Some("default/lead".into()),
        )
        .await
        .unwrap();
    agent
        .set_fusion_model(
            crate::context::FusionModelRole::Sidekick,
            Some("default/sidekick".into()),
        )
        .await
        .unwrap();
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();

    let outcome = agent.run_turn("build it", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    assert!(
        sub.calls.load(Ordering::Relaxed) >= 1,
        "the unpinned delegation ran on the session's Sidekick"
    );
    assert_eq!(
        crate::agent::fusion::sidekick::selector(&ctx, None).as_deref(),
        Some("default/sidekick")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("w.txt")).unwrap(),
        "hello",
        "the Sidekick write landed"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn fusion_mode_requires_both_session_roles() {
    let dir = crate::fresh_test_dir("fusion-required-roles");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();
    std::fs::write(
        dir.join(".sunmao/models.json"),
        r#"{"fusion_sidekick":"default/sidekick"}"#,
    )
    .unwrap();
    let lead = Arc::new(MockProvider {
        responses: queued(vec![]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let sidekick = Arc::new(MockProvider {
        responses: queued(vec![]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = fusion_ctx_unconfigured(&dir, SessionLog::ephemeral(), lead, sidekick);
    let agent = AgentLoop::new(ctx.clone());

    assert!(agent.fusion_model_problem().is_some());
    assert!(crate::agent::fusion::sidekick::adapter(&ctx, None).is_err());
    assert!(
        agent
            .set_turn_mode(TurnMode::Fusion, &NullObserver)
            .await
            .is_err()
    );
    agent
        .set_fusion_model(
            crate::context::FusionModelRole::Lead,
            Some("default/lead".into()),
        )
        .await
        .unwrap();
    assert!(agent.fusion_model_problem().is_some());
    assert!(
        agent
            .set_turn_mode(TurnMode::Fusion, &NullObserver)
            .await
            .is_err()
    );
    agent
        .set_fusion_model(
            crate::context::FusionModelRole::Sidekick,
            Some("default/sidekick".into()),
        )
        .await
        .unwrap();
    assert!(agent.fusion_model_problem().is_none());
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();
    assert_eq!(agent.turn_mode(), TurnMode::Fusion);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn stale_session_roles_downgrade_fusion_on_resume() {
    let dir = crate::fresh_test_dir("fusion-stale-resume");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(
        dir.join(".sunmao/models.json"),
        r#"{"providers":{"default":{"base_url":"http://unused/v1","catalog":[{"id":"lead"},{"id":"sidekick"}]}}}"#,
    )
    .unwrap();
    let probe = || {
        Arc::new(MockProvider {
            responses: queued(vec![]),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    };
    let lead = probe();
    let sidekick = probe();
    let log_dir = dir.join(".sunmao/sessions");
    let mut log = SessionLog::open(&log_dir, "s-stale-fusion").await.unwrap();
    log.append(&SessionEvent::FusionModelsChange {
        lead: Some("default/lead".into()),
        sidekick: Some("default/gone".into()),
    })
    .await
    .unwrap();
    log.append(&SessionEvent::TurnModeChange {
        mode: TurnMode::Fusion,
    })
    .await
    .unwrap();
    let mut raw = Context::new(lead.clone(), log, builtin_registry(), dir.clone());
    raw.models = Some(Arc::new(
        crate::models::ModelResolver::load(
            &dir,
            crate::models::ProviderDef {
                base_url: "http://unused/v1".into(),
                dialect: "openai".into(),
                ..Default::default()
            },
            "default",
        )
        .with_adapter("default/lead", lead.clone())
        .with_adapter("default/sidekick", sidekick.clone()),
    ));
    let ctx = Arc::new(raw);
    let agent = AgentLoop::new(ctx.clone());

    assert_eq!(agent.turn_mode(), TurnMode::Standard);
    assert!(agent.fusion_model_problem().is_some());
    assert!(crate::agent::fusion::sidekick::adapter(&ctx, None).is_err());
    assert!(
        crate::agent::fusion::sidekick::adapter(&ctx, Some("default/sidekick"))
            .unwrap()
            .is_some()
    );
    *ctx.turn_mode.write().unwrap() = TurnMode::Fusion;
    assert!(agent.run_turn("build it", &NullObserver).await.is_err());
    assert_eq!(lead.calls.load(Ordering::Relaxed), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn catalog_change_downgrades_active_fusion() {
    let dir = crate::fresh_test_dir("fusion-stale-catalog");
    let lead = Arc::new(MockProvider {
        responses: queued(vec![]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let sidekick = Arc::new(MockProvider {
        responses: queued(vec![]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = fusion_ctx(&dir, SessionLog::ephemeral(), lead, sidekick);
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();
    assert!(agent.fusion_ready());
    std::fs::write(
        dir.join(".sunmao/models.json"),
        r#"{"providers":{"default":{"base_url":"http://unused/v1","catalog":[{"id":"lead"}]}}}"#,
    )
    .unwrap();
    agent.reload_models().await;
    assert_eq!(agent.turn_mode(), TurnMode::Standard);
    assert!(!agent.fusion_ready());
    assert_eq!(agent.fusion_models().1.as_deref(), Some("default/sidekick"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn fusion_resume_without_session_roles_starts_standard() {
    let dir = crate::fresh_test_dir("fusion-no-role-resume");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    let mut log = SessionLog::open(&dir.join(".sunmao/sessions"), "s-no-fusion-roles")
        .await
        .unwrap();
    log.append(&SessionEvent::TurnModeChange {
        mode: TurnMode::Fusion,
    })
    .await
    .unwrap();
    let provider = || {
        Arc::new(MockProvider {
            responses: queued(vec![]),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    };
    let ctx = fusion_ctx_unconfigured(&dir, log, provider(), provider());
    let agent = AgentLoop::new(ctx);
    assert_eq!(agent.turn_mode(), TurnMode::Standard);
    assert!(agent.fusion_model_problem().is_some());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn fusion_model_overrides_are_session_scoped_and_resume() {
    let dir = crate::fresh_test_dir("fusion-session-models");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(
        dir.join(".sunmao/models.json"),
        r#"{"providers":{"default":{"base_url":"http://unused/v1","catalog":[{"id":"lead"},{"id":"sidekick"}]}}}"#,
    )
    .unwrap();
    let provider = || -> Arc<dyn ProviderAdapter> {
        Arc::new(MockProvider {
            responses: queued(vec![]),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    };
    let baseline = provider();
    let lead = provider();
    let sidekick = provider();
    let models = || {
        Arc::new(
            crate::models::ModelResolver::load(
                &dir,
                crate::models::ProviderDef {
                    base_url: "http://unused/v1".into(),
                    api_key_env: None,
                    api_key: None,
                    dialect: "openai".into(),
                    catalog: Vec::new(),
                    extra: Default::default(),
                },
                "default",
            )
            .with_adapter("default/lead", lead.clone())
            .with_adapter("default/sidekick", sidekick.clone()),
        )
    };
    let log = SessionLog::open(&dir.join(".sunmao/sessions"), "s-fusion-models")
        .await
        .unwrap();
    let log_path = log.path().to_path_buf();
    let mut raw = Context::new(baseline.clone(), log, builtin_registry(), dir.clone());
    raw.models = Some(models());
    let ctx = Arc::new(raw);
    let agent = AgentLoop::new(ctx.clone());
    assert!(agent.fusion_model_problem().is_some());
    assert!(
        agent
            .set_turn_mode(TurnMode::Fusion, &NullObserver)
            .await
            .is_err()
    );
    agent
        .set_fusion_model(
            crate::context::FusionModelRole::Lead,
            Some("default/lead".into()),
        )
        .await
        .unwrap();
    assert!(agent.fusion_model_problem().is_some());
    assert!(
        agent
            .set_turn_mode(TurnMode::Fusion, &NullObserver)
            .await
            .is_err()
    );
    agent
        .set_fusion_model(
            crate::context::FusionModelRole::Sidekick,
            Some("default/sidekick".into()),
        )
        .await
        .unwrap();
    assert!(agent.fusion_model_problem().is_none());
    assert_eq!(
        agent.fusion_models(),
        (Some("default/lead".into()), Some("default/sidekick".into()))
    );
    assert!(
        std::fs::read_to_string(&log_path)
            .unwrap()
            .contains("\"fusion_models_change\"")
    );
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&ctx.active_llm(), &lead));
    assert_eq!(
        crate::agent::fusion::sidekick::selector(&ctx, None).as_deref(),
        Some("default/sidekick")
    );
    drop(agent);
    drop(ctx);

    let log = SessionLog::open(&dir.join(".sunmao/sessions"), "s-fusion-models")
        .await
        .unwrap();
    let mut resumed = Context::new(baseline, log, builtin_registry(), dir.clone());
    resumed.models = Some(models());
    let resumed = Arc::new(resumed);
    let agent = AgentLoop::new(resumed.clone());
    assert_eq!(
        agent.fusion_models(),
        (Some("default/lead".into()), Some("default/sidekick".into()))
    );
    assert_eq!(agent.turn_mode(), TurnMode::Fusion);
    assert!(Arc::ptr_eq(&resumed.active_llm(), &lead));
    let empty = SessionLog::open(&dir.join(".sunmao/sessions"), "s-empty-fusion")
        .await
        .unwrap();
    agent.swap_session(empty).await;
    assert_eq!(agent.fusion_models(), (None, None));
    assert_eq!(agent.turn_mode(), TurnMode::Standard);
    assert!(agent.fusion_model_problem().is_some());
    let reopened = SessionLog::open(&dir.join(".sunmao/sessions"), "s-fusion-models")
        .await
        .unwrap();
    agent.swap_session(reopened).await;
    assert_eq!(agent.turn_mode(), TurnMode::Fusion);
    assert_eq!(
        agent.fusion_models(),
        (Some("default/lead".into()), Some("default/sidekick".into()))
    );
    assert!(
        !std::fs::read_to_string(dir.join(".sunmao/models.json"))
            .unwrap()
            .contains("fusion_lead")
    );
    std::fs::remove_dir_all(&dir).ok();
}
