//! Goal-loop tests — `/goal` + `UpdateGoal` drive a self-continuing chain:
//! every Completed turn re-enters the driver with a continuation prompt
//! until a terminal status, the round budget, queued input, or a
//! non-clean outcome ends it.

use super::*;
use crate::context::MutexRecover;
use crate::tool::{GoalStatus, apply_blocker};

fn text_done() -> Vec<StreamDelta> {
    vec![
        StreamDelta::Content("done".into()),
        StreamDelta::Finish {
            reason: Some("stop".into()),
            usage: None,
        },
    ]
}

fn update_goal_call(call_id: &str, args: &str) -> Vec<StreamDelta> {
    vec![
        StreamDelta::ToolCalls(vec![ToolCallFragment {
            index: 0,
            id: Some(call_id.into()),
            name: Some("UpdateGoal".into()),
            arguments: Some(args.into()),
        }]),
        StreamDelta::Finish {
            reason: Some("tool_calls".into()),
            usage: None,
        },
    ]
}

fn mock_with(responses: Vec<Vec<StreamDelta>>) -> Arc<MockProvider> {
    Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(responses)),
        calls: std::sync::atomic::AtomicUsize::new(0),
    })
}

fn calls(p: &MockProvider) -> usize {
    p.calls.load(std::sync::atomic::Ordering::Relaxed)
}

/// The set → continue → report-complete arc: each Completed round hands the
/// loop a fresh continuation prompt until the model settles a verdict.
#[tokio::test]
async fn completed_turns_chain_until_complete() {
    let provider = mock_with(vec![
        // round 0: the model writes the goal, then settles the round
        update_goal_call("g1", "{\"objective\":\"ship the feature\"}"),
        text_done(),
        // round 1: a clean end hands the chain another continuation
        text_done(),
        // round 2: the verdict call, then the round settles
        update_goal_call("g2", "{\"status\":\"complete\"}"),
        text_done(),
    ]);
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    let rec = RecObserver(std::sync::Mutex::new(Vec::new()));
    let outcome = agent.run_turn("start", &rec).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    // rounds 0/1/2 — the chain consumed its completions
    assert_eq!(calls(&provider), 5, "calls: {}", calls(&provider));
    let goal = agent.goal().expect("the goal survives the chain");
    assert_eq!(goal.status, GoalStatus::Complete);
    assert_eq!(goal.rounds, 2, "rounds 0 + 1 advanced, round 2 ended it");
    // the durable spine: every apply_goal (write, round bumps, verdict)
    // lands a Goal event — 1 write + 2 bumps + 1 verdict
    let events = ctx.sessions.lock().await.events().await.unwrap();
    let goal_events = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::Goal { .. }))
        .count();
    assert_eq!(goal_events, 4, "one Goal event per state change");
    // the continuation prompt rides the log as a real user message —
    // a replay must show the same kick a live observer saw
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    assert!(
        msgs.iter()
            .any(|m| matches!(m.role, sunmao_llm::types::Role::User)
                && m.content_text()
                    .as_deref()
                    .is_some_and(|t| t.contains("ship the feature") && t.contains("round 1/32"))),
        "round-1 continuation lands as a user message"
    );
    // TurnEnd fires once — at the end of the chain, not per round
    let evs = rec.0.lock_or_recover();
    assert_eq!(
        evs.iter().filter(|t| t.starts_with("TurnEnd")).count(),
        1,
        "one TurnEnd for the whole chain — got {evs:?}"
    );
}

/// `blocked` is a verdict the model earns: one stalled round stays
/// in_progress (the report is recorded, the streak starts); only the SAME
/// blocker in a LATER round lets the write land.
#[tokio::test]
async fn blocked_verdict_needs_two_rounds_of_the_same_blocker() {
    let provider = mock_with(vec![
        update_goal_call("g1", "{\"objective\":\"reach the remote API\"}"),
        update_goal_call(
            "g2",
            "{\"blocker\":\"needs credentials\",\"status\":\"blocked\"}",
        ),
        // the refusal still counts as a Completed round — the chain bumps
        // to round 1 and hands back a continuation
        text_done(),
        // round 1: the same blocker reported again — the verdict lands
        update_goal_call(
            "g3",
            "{\"blocker\":\"needs credentials\",\"status\":\"blocked\"}",
        ),
        text_done(),
    ]);
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    let goal = agent.goal().expect("goal exists");
    assert_eq!(goal.status, GoalStatus::Blocked);
    assert_eq!(goal.blocker.as_deref(), Some("needs credentials"));
    // round 0's blocked write refused the verdict but kept the report —
    // the tool result reads the admission, not a silent downgrade
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    let results: Vec<String> = msgs
        .iter()
        .filter(|m| matches!(m.role, sunmao_llm::types::Role::Tool))
        .filter_map(|m| m.content_text())
        .collect();
    assert!(
        results.iter().any(|r| r.contains("stays in progress")),
        "the single-round report must explain why the goal didn't block — {results:?}"
    );
}

/// `apply_blocker` accounting in isolation — same-round re-reports can't
/// inflate the streak; a changed blocker restarts it.
#[test]
fn blocker_streak_counts_distinct_rounds() {
    let mut g = crate::tool::GoalState::new("x".into());
    apply_blocker(&mut g, Some("w".into()));
    assert_eq!(g.blocker_streak, 1);
    apply_blocker(&mut g, Some("w".into())); // same round — no growth
    assert_eq!(g.blocker_streak, 1);
    g.rounds = 1;
    apply_blocker(&mut g, Some("w".into())); // later round — grows
    assert_eq!(g.blocker_streak, 2);
    g.rounds = 2;
    apply_blocker(&mut g, Some("other".into())); // changed — restarts
    assert_eq!(g.blocker_streak, 1);
    apply_blocker(&mut g, None); // cleared report — streak dies
    assert_eq!(g.blocker_streak, 0);
    assert_eq!(g.blocker, None);
}

/// The round budget ends the chain without a verdict — the goal parks
/// in_progress and the log carries `goal.max_rounds` so a replay reads it
/// as a pause, not a crash.
#[tokio::test]
async fn max_rounds_parks_the_chain() {
    let provider = mock_with(vec![]); // default "done" reply every call
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    agent.set_goal("keep going", &NullObserver).await.unwrap();
    ctx.goal.lock_or_recover().as_mut().unwrap().max_rounds = 2;
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    // round 0 + one continuation; the second bump would overspend
    assert_eq!(calls(&provider), 2);
    let goal = agent.goal().unwrap();
    assert_eq!(goal.status, GoalStatus::InProgress);
    assert_eq!(goal.rounds, 1);
    let events = ctx.sessions.lock().await.events().await.unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Hook { event, .. } if event == "goal.max_rounds"
        )),
        "the budget pause is an audit fact, not silence"
    );
}

/// A queued user submission outranks the chain — `input_pending` yields
/// instead of holding the loop hostage, and the NEXT run_turn resumes it.
#[tokio::test]
async fn queued_input_yields_the_chain() {
    let provider = mock_with(vec![]);
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    agent.set_goal("keep going", &NullObserver).await.unwrap();
    ctx.goal.lock_or_recover().as_mut().unwrap().max_rounds = 3;
    ctx.input_pending
        .store(1, std::sync::atomic::Ordering::Relaxed);
    agent.run_turn("go", &NullObserver).await.unwrap();
    assert_eq!(
        calls(&provider),
        1,
        "pending input ends the chain after its current round"
    );
    assert_eq!(agent.goal().unwrap().rounds, 0, "no bump while yielding");
    // the queued turn ran — the chain picks back up and spends the budget:
    // typed turn → rounds 1 → continuation → rounds 2 → one more call
    // before rounds+1 == max_rounds parks it
    ctx.input_pending
        .store(0, std::sync::atomic::Ordering::Relaxed);
    agent.run_turn("typed", &NullObserver).await.unwrap();
    assert_eq!(calls(&provider), 4);
    assert_eq!(agent.goal().unwrap().rounds, 2);
}

/// A cancelled turn never earns another round — the chain only advances
/// on Completed, so the goal stays parked where the cancel caught it.
#[tokio::test]
async fn cancelled_turn_stops_the_chain() {
    let provider = mock_with(vec![]);
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    agent.set_goal("keep going", &NullObserver).await.unwrap();
    ctx.cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Cancelled));
    assert_eq!(calls(&provider), 0);
    assert_eq!(agent.goal().unwrap().rounds, 0);
}

/// Resume: a reopened log's last Goal event becomes the live state
/// (`Context::new` seeds it), and `swap_session` re-points the snapshot —
/// a goal-less log clears whatever was there.
#[tokio::test]
async fn resume_seeds_and_swap_reseeds_the_goal() {
    let dir = crate::fresh_test_dir("goal-resume");
    let mut log = SessionLog::open(&dir, "s1").await.unwrap();
    let mut g = crate::tool::GoalState::new("survive restart".into());
    g.rounds = 2;
    log.append(&SessionEvent::Goal { goal: g }).await.unwrap();
    let log = SessionLog::open(&dir, "s1").await.unwrap();
    let ctx = Arc::new(Context::new(
        Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        log,
        builtin_registry(),
        dir.clone(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    let goal = agent.goal().expect("a reopened log seeds its goal");
    assert_eq!(goal.objective, "survive restart");
    assert_eq!(goal.rounds, 2);
    // swap to a goal-less session — the snapshot must follow the log
    let empty = SessionLog::open(&dir, "s2").await.unwrap();
    agent.swap_session(empty).await;
    assert!(
        agent.goal().is_none(),
        "a goal-less log clears the snapshot"
    );
    // swap back — the in-progress goal reseeds, ready to keep chaining
    let back = SessionLog::open(&dir, "s1").await.unwrap();
    agent.swap_session(back).await;
    let goal = agent.goal().expect("swap reseeds the goal");
    assert_eq!(goal.objective, "survive restart");
    std::fs::remove_dir_all(&dir).ok();
}

/// `/goal clear` writes `abandoned` — an explicit human stop distinct
/// from the model's verdicts, and it ends any live chain.
#[tokio::test]
async fn clear_goal_records_abandoned() {
    let provider = mock_with(vec![]);
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    agent.set_goal("keep going", &NullObserver).await.unwrap();
    agent.clear_goal(&NullObserver).await.unwrap();
    let goal = agent.goal().expect("clearing keeps the goal visible");
    assert_eq!(goal.status, GoalStatus::Abandoned);
    let outcome = agent.run_turn("go", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    assert_eq!(calls(&provider), 1, "an abandoned goal doesn't chain");
}

/// While a goal is in progress every request carries a synthetic
/// `[goal round n/m …]` tail — the objective survives compaction and
/// steer noise without duplicating the durable event.
#[tokio::test]
async fn in_progress_goal_injects_into_every_request() {
    struct Spy {
        calls: std::sync::atomic::AtomicUsize,
        seen: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ProviderAdapter for Spy {
        async fn stream(&self, req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if req
                .messages
                .last()
                .and_then(|m| m.content_text())
                .is_some_and(|t| t.starts_with("[goal round "))
            {
                self.seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(Box::pin(stream::iter(vec![
                Ok(StreamDelta::Content("ok".into())),
                Ok(StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                }),
            ])))
        }
    }
    let provider = Arc::new(Spy {
        calls: std::sync::atomic::AtomicUsize::new(0),
        seen: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    agent.set_goal("keep going", &NullObserver).await.unwrap();
    ctx.goal.lock_or_recover().as_mut().unwrap().max_rounds = 2;
    agent.run_turn("go", &NullObserver).await.unwrap();
    assert_eq!(
        provider.seen.load(std::sync::atomic::Ordering::Relaxed),
        provider.calls.load(std::sync::atomic::Ordering::Relaxed),
        "every request in the chain carries the goal tail"
    );
}

/// The seed scanner keys on the serialized line prefix — lock the wire
/// spelling (`{"type":"goal"`) against a serde-shape drift.
#[test]
fn goal_line_prefix_matches_serializer() {
    let line = serde_json::to_string(&SessionEvent::Goal {
        goal: crate::tool::GoalState::new("x".into()),
    })
    .unwrap();
    assert!(
        line.starts_with(crate::tool::GOAL_LINE_PREFIX),
        "seed scan reads by prefix — {line}"
    );
}
