//! `Task{resume}` policy fixtures — resume goes through the same
//! `spawns:` whitelist as a fresh spawn (split from tests_steer_resume.rs
//! to keep both under the god-file budget).

use super::tests_steer_resume::MockProvider;
use super::*;
use crate::context::MutexRecover;
use crate::session::SessionLog;
use crate::tool::builtin_registry;
use std::sync::Arc;

/// `Task{resume}` goes through the same `spawns:` whitelist as a fresh
/// spawn — a restricted parent must not resume a def it couldn't spawn,
/// and a roster entry naming a def that no longer exists must not fall
/// back to a full-tool generic child.
#[tokio::test]
async fn resume_honors_the_spawns_whitelist() {
    let dir = crate::fresh_test_dir("resume-spawns");
    let agents = dir.join(".sunmao/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("parent.md"),
        "---
name: parent
spawns: helper
---
parent agent
",
    )
    .unwrap();
    std::fs::write(
        agents.join("helper.md"),
        "---
name: helper
---
helper agent
",
    )
    .unwrap();
    std::fs::write(
        agents.join("rogue.md"),
        "---
name: rogue
---
rogue agent
",
    )
    .unwrap();
    let mut ctx_raw = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    ctx_raw.agent_name = Some("parent".into());
    let ctx = Arc::new(ctx_raw);
    // roster entry for a def OUTSIDE the whitelist → refused at policy,
    // before the log lookup even runs
    ctx.live_tasks
        .lock_or_recover()
        .push(crate::context::TaskEntry {
            id: "sub-rogue".into(),
            lane: 3,
            agent: Some("rogue".into()),
            prompt: "x".into(),
            done: Some(true),
            steer: None,
            cancel: None,
        });
    let res = ctx
        .tools
        .call(
            "Task",
            &serde_json::json!({"resume": "sub-rogue", "prompt": "go on"}).to_string(),
            &ctx,
        )
        .await;
    assert!(
        !res.ok && res.output.contains("may not spawn"),
        "resume must respect spawns: {}",
        res.output
    );
    // a roster entry naming a def with no live definition is refused at
    // policy too — never a silent fallthrough to a generic child.
    ctx.live_tasks
        .lock_or_recover()
        .push(crate::context::TaskEntry {
            id: "sub-ghost".into(),
            lane: 4,
            agent: Some("deleted-def".into()),
            prompt: "x".into(),
            done: Some(true),
            steer: None,
            cancel: None,
        });
    let res = ctx
        .tools
        .call(
            "Task",
            &serde_json::json!({"resume": "sub-ghost", "prompt": "go on"}).to_string(),
            &ctx,
        )
        .await;
    assert!(
        !res.ok && res.output.contains("may not spawn"),
        "a def-less roster name refuses at policy, not widens: {}",
        res.output
    );
    std::fs::remove_dir_all(&dir).ok();
}
