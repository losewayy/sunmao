use super::fusion_fixture::{events, fusion_ctx, queued, text, tool_call};
use super::*;
use std::sync::atomic::Ordering;

// ── fusion delegation contract — gate boundaries and verdict states ────

/// The whitelist bounds Write/Edit — NOT Bash: a Sidekick must run the
/// spec's build/test commands itself, and gating its shell on the file
/// set made every `cargo test` a refusal. A mutating Bash lands through
/// the normal gate, and the delegation result carries the child's own
/// tool-call digest so the Lead sees how the work actually went.
#[tokio::test]
async fn fusion_sidekick_bash_is_not_whitelist_gated() {
    let dir = crate::fresh_test_dir("fusion-bash");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();

    let lead = Arc::new(MockProvider {
        responses: queued(vec![
            tool_call(
                "f1",
                "FusionExecute",
                r#"{"spec":{"goal":"make made-by-sidekick"},"files":[],"verify_commands":["echo ok"],"model":"@sidekick"}"#,
            ),
            text("wrapped"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let sub = Arc::new(MockProvider {
        responses: queued(vec![
            // a mutating shell call with no whitelist file — under the
            // old gate this refused outright ("outside the delegated
            // file set"); now it flows through the normal gate
            tool_call("b1", "Bash", r#"{"command":"mkdir made-by-sidekick"}"#),
            text("made it"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = fusion_ctx(&dir, SessionLog::ephemeral(), lead, sub);
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();

    agent.run_turn("make a dir", &NullObserver).await.unwrap();
    assert!(
        dir.join("made-by-sidekick").is_dir(),
        "a Sidekick's Bash must run through the normal gate — the \
         whitelist bounds Write/Edit only"
    );

    // and the Lead's verdict carries the child's real tool trace, not
    // just its self-report
    let evs = events(&ctx).await;
    let result = evs.iter().find_map(|e| match e {
        SessionEvent::ToolResult { name, output, .. } if name == "FusionExecute" => {
            Some(output.clone())
        }
        _ => None,
    });
    assert!(
        result
            .as_deref()
            .is_some_and(|o| o.contains("sidekick ran:") && o.contains("Bash")),
        "the delegation result must carry the Sidekick's tool digest: {result:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// An environment failure is not a work failure: a verify command that
/// cannot even start (missing binary) reports `[inconclusive —
/// environment]` and must NOT burn the escalation streak — two
/// inconclusive verdicts in a row still leave the Lead read-only. And a
/// `steer`-only rework doesn't need `spec` — the Sidekick's own
/// transcript is the spec's continuation.
#[tokio::test]
async fn fusion_verify_inconclusive_does_not_escalate() {
    let dir = crate::fresh_test_dir("fusion-env");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();

    let lead = Arc::new(MockProvider {
        responses: queued(vec![
            // first delegation — env-broken verify (missing binary)
            tool_call(
                "f1",
                "FusionExecute",
                r#"{"spec":{"goal":"touch nothing"},"verify_commands":["definitely-not-a-real-binary-xyz"],"model":"@sidekick"}"#,
            ),
            // a steer-only rework must not need `spec` — and after an
            // inconclusive verdict the Lead must still be read-only
            tool_call(
                "f2",
                "FusionExecute",
                r#"{"steer":"try again — the toolchain was missing"}"#,
            ),
            text("wrapped"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let sub = Arc::new(MockProvider {
        responses: queued(vec![text("nothing to do"), text("still nothing")]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = fusion_ctx(&dir, SessionLog::ephemeral(), lead, sub.clone());
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();

    agent.run_turn("run it", &NullObserver).await.unwrap();

    let evs = events(&ctx).await;
    let outputs: Vec<String> = evs
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolResult { name, output, .. } if name == "FusionExecute" => {
                Some(output.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        outputs[0].contains("inconclusive"),
        "a verify that could not start reports inconclusive: {}",
        outputs[0]
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SessionEvent::FusionEscalated { .. })),
        "inconclusive verdicts must not count toward escalation"
    );
    assert!(
        sub.calls.load(Ordering::Relaxed) >= 2,
        "the steer-only FusionExecute resumed the same Sidekick"
    );
    assert!(
        ctx.read_only.load(Ordering::Relaxed) || !ctx.fusion.lock_or_recover().escalated,
        "the Lead never escalated"
    );
    std::fs::remove_dir_all(&dir).ok();
}
