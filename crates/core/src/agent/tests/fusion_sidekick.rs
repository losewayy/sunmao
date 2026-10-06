//! The Sidekick's model (`fusion_sidekick`) and its fallbacks — split out of
//! `fusion.rs`'s test file for the god-file budget; `super::fusion` owns the
//! shared fixtures.

use super::fusion::{fusion_ctx, queued, text, tool_call};
use super::*;
use std::sync::atomic::Ordering;

/// `fusion_sidekick` is the pair's cheap half: a delegation the Lead does not
/// pin itself runs on that selector, which is what makes the mode mean what a
/// user expects (strong model plans, cheaper one executes). The settings page
/// writes exactly this key.
#[tokio::test]
async fn fusion_sidekick_config_routes_an_unpinned_delegation() {
    let dir = crate::fresh_test_dir("fusion-sidekick");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();
    std::fs::write(
        dir.join(".sunmao/models.json"),
        r#"{"providers":{"default":{"base_url":"http://unused/v1"}},"fusion_sidekick":"@sidekick"}"#,
    )
    .unwrap();

    // NOTE: no `model` in the FusionExecute args — the config has to decide
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
    let ctx = fusion_ctx(&dir, SessionLog::ephemeral(), lead.clone(), sub.clone());
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();

    let outcome = agent.run_turn("build it", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    assert!(
        sub.calls.load(Ordering::Relaxed) >= 1,
        "the unpinned delegation ran on the configured Sidekick"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("w.txt")).unwrap(),
        "hello",
        "and its write landed"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// No `fusion_sidekick` = the old behaviour, pinned: the child inherits the
/// Lead's own adapter (the mode is usable without any config at all).
#[tokio::test]
async fn fusion_without_a_sidekick_config_inherits_the_lead() {
    let dir = crate::fresh_test_dir("fusion-inherit");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();

    let lead = Arc::new(MockProvider {
        responses: queued(vec![
            tool_call(
                "f1",
                "FusionExecute",
                r#"{"spec":{"goal":"create w.txt containing hello"},"files":["w.txt"]}"#,
            ),
            text("wrapped"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let sub = Arc::new(MockProvider {
        responses: queued(vec![text("wrote it")]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = fusion_ctx(&dir, SessionLog::ephemeral(), lead.clone(), sub.clone());
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();

    let outcome = agent.run_turn("build it", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(
        sub.calls.load(Ordering::Relaxed),
        0,
        "without a config the child must not land on the routed adapter"
    );
    assert!(
        lead.calls.load(Ordering::Relaxed) >= 2,
        "the child inherited the Lead's adapter"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A configured selector that no longer resolves must inherit rather than
/// break every delegation (a provider got renamed, a route got dropped); a
/// selector the CALL SITE named stays a hard error — the model asked for
/// something wrong and should hear about it.
#[tokio::test]
async fn a_stale_fusion_sidekick_config_falls_back_instead_of_failing() {
    let dir = crate::fresh_test_dir("fusion-stale");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(
        dir.join(".sunmao/models.json"),
        r#"{"fusion_sidekick":"@gone"}"#,
    )
    .unwrap();
    let probe = || {
        Arc::new(MockProvider {
            responses: queued(vec![]),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    };
    let ctx = fusion_ctx(&dir, SessionLog::ephemeral(), probe(), probe());

    assert_eq!(
        crate::agent::fusion::sidekick::selector(&ctx, None).as_deref(),
        Some("@gone"),
        "the config is what got read"
    );
    assert!(
        crate::agent::fusion::sidekick::adapter(&ctx, None)
            .unwrap()
            .is_none(),
        "a stale config inherits the Lead's adapter"
    );
    assert!(
        crate::agent::fusion::sidekick::adapter(&ctx, Some("@gone")).is_err(),
        "the call site keeps its hard error"
    );
    std::fs::remove_dir_all(&dir).ok();
}
