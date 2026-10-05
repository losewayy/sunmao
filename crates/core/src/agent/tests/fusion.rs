use super::*;
use crate::context::RwLockRecover;
use std::sync::atomic::Ordering;

// ── fusion mode (Lead/Sidekick) ─────────────────────────────────────────

/// A MockProvider that also records each request — the fusion tests must
/// prove the tail inject and the trimmed tool surface reached the wire,
/// not just that a turn ran. `requests` holds (tool names, last-message
/// text) per call.
struct RecProvider {
    responses: std::sync::Mutex<std::collections::VecDeque<Vec<StreamDelta>>>,
    requests: std::sync::Mutex<Vec<(Vec<String>, String)>>,
}

#[async_trait::async_trait]
impl ProviderAdapter for RecProvider {
    async fn stream(&self, req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        let tools: Vec<String> = req
            .tools
            .map(|t| t.iter().map(|d| d.function.name.clone()).collect())
            .unwrap_or_default();
        let last = req
            .messages
            .last()
            .and_then(|m| m.content.as_ref())
            .map(|cs| {
                cs.iter()
                    .filter_map(|c| match c {
                        sunmao_llm::types::Content::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        self.requests.lock_or_recover().push((tools, last));
        let deltas = self
            .responses
            .lock_or_recover()
            .pop_front()
            .unwrap_or_else(|| {
                vec![
                    StreamDelta::Content("done".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: Some(Usage::default()),
                    },
                ]
            });
        Ok(Box::pin(stream::iter(deltas.into_iter().map(Ok))))
    }
}

fn tool_call(id: &str, name: &str, args: &str) -> Vec<StreamDelta> {
    vec![
        StreamDelta::ToolCalls(vec![
            ToolCallFragment {
                index: 0,
                id: Some(id.into()),
                name: Some(name.into()),
                arguments: None,
            },
            ToolCallFragment {
                index: 0,
                arguments: Some(args.into()),
                ..Default::default()
            },
        ]),
        StreamDelta::Finish {
            reason: Some("tool_calls".into()),
            usage: None,
        },
    ]
}

fn text(s: &str) -> Vec<StreamDelta> {
    vec![
        StreamDelta::Content(s.into()),
        StreamDelta::Finish {
            reason: Some("stop".into()),
            usage: Some(Usage::default()),
        },
    ]
}

fn queued(
    v: Vec<Vec<StreamDelta>>,
) -> std::sync::Mutex<std::collections::VecDeque<Vec<StreamDelta>>> {
    std::sync::Mutex::new(std::collections::VecDeque::from(v))
}

/// A context whose `models` routes `@sidekick` to a second mock — the
/// FusionExecute args pin `model:"@sidekick"` so the child never shares
/// the Lead's response queue.
fn fusion_ctx(
    dir: &std::path::Path,
    log: SessionLog,
    lead: Arc<MockProvider>,
    sidekick: Arc<MockProvider>,
) -> Arc<Context> {
    let mut raw = Context::new(lead, log, builtin_registry(), dir.to_path_buf());
    raw.models = Some(Arc::new(
        crate::models::ModelResolver::load(
            dir,
            crate::models::ProviderDef {
                base_url: "http://unused".into(),
                api_key_env: None,
                api_key: None,
                dialect: "openai".into(),
                catalog: Vec::new(),
                extra: Default::default(),
            },
            "default",
        )
        .with_adapter("@sidekick", sidekick),
    ));
    Arc::new(raw)
}

async fn events(ctx: &Arc<Context>) -> Vec<SessionEvent> {
    ctx.sessions.lock().await.events().await.unwrap_or_default()
}

/// `/mode fusion` flips the third axis: the `turn_mode_change` fact is
/// durable, the per-context read_only flag arms on the spot, and a
/// reopened context reseeds both. Switching back disarms.
#[tokio::test]
async fn fusion_switch_is_durable_and_reseeds() {
    let dir = crate::fresh_test_dir("fusion-mode");
    let log = SessionLog::open(&dir.join(".sunmao/sessions"), "s-fuse")
        .await
        .unwrap();
    let path = log.path().to_path_buf();
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(Default::default()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(provider, log, builtin_registry(), dir.clone()));
    let agent = AgentLoop::new(ctx.clone());

    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();
    assert_eq!(agent.turn_mode(), TurnMode::Fusion);
    assert!(
        ctx.read_only.load(Ordering::Relaxed),
        "fusion arms the Lead's read_only flag"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("\"turn_mode_change\"") && text.contains("fusion"),
        "the mode flip must be a durable fact: {text}"
    );

    // a reopened context reseeds mode + flag from the log
    let log2 = SessionLog::open_path(&path).await.unwrap();
    let ctx2 = Context::new(
        Arc::new(MockProvider {
            responses: std::sync::Mutex::new(Default::default()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        log2,
        builtin_registry(),
        dir.clone(),
    );
    assert_eq!(*ctx2.turn_mode.read_or_recover(), TurnMode::Fusion);
    assert!(ctx2.read_only.load(Ordering::Relaxed));
    drop(ctx2);

    agent
        .set_turn_mode(TurnMode::Standard, &NullObserver)
        .await
        .unwrap();
    assert!(!ctx.read_only.load(Ordering::Relaxed));
    std::fs::remove_dir_all(&dir).ok();
}

/// fusion ⊥ ptc: under the RunCode-only surface the Lead could never emit
/// the delegation call, so the switch refuses instead of silently arming
/// a crippled mode.
#[tokio::test]
async fn fusion_switch_refused_under_ptc() {
    let dir = crate::fresh_test_dir("fusion-ptc");
    let mut raw = Context::new(
        Arc::new(MockProvider {
            responses: std::sync::Mutex::new(Default::default()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    raw.loop_driver = crate::agent::LoopDriver::Ptc;
    let agent = AgentLoop::new(Arc::new(raw));
    let err = agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .expect_err("ptc must refuse fusion");
    assert!(err.contains("ptc"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// The declared surface tracks the axis: Standard hides FusionExecute
/// (dead schema, like SearchTools); an armed Lead gets the read tools +
/// Bash + UpdateGoal + FusionExecute and loses Task/Write/Edit/TodoWrite;
/// an escalated Lead returns to the standard surface minus the
/// delegation tool.
#[tokio::test]
async fn advertised_tools_track_turn_mode() {
    let dir = crate::fresh_test_dir("fusion-tools");
    let ctx = Arc::new(Context::new(
        Arc::new(MockProvider {
            responses: std::sync::Mutex::new(Default::default()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    let names = |c: &Context| -> std::collections::BTreeSet<String> {
        c.advertised_tools()
            .into_iter()
            .map(|t| t.function.name)
            .collect()
    };

    let std_names = names(&ctx);
    assert!(std_names.contains("Task") && !std_names.contains("FusionExecute"));

    *ctx.turn_mode.write_or_recover() = TurnMode::Fusion;
    let lead: std::collections::BTreeSet<String> = names(&ctx);
    let expect: std::collections::BTreeSet<String> = [
        "Read",
        "Grep",
        "Glob",
        "WebFetch",
        "JobOutput",
        "Bash",
        "UpdateGoal",
        "FusionExecute",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        lead, expect,
        "the Lead's surface is the read set + delegate"
    );

    ctx.fusion.lock_or_recover().escalated = true;
    let esc = names(&ctx);
    assert!(
        esc.contains("Write") && esc.contains("Task") && !esc.contains("FusionExecute"),
        "an escalated Lead finishes itself — standard surface, no delegate"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A fusion turn's request carries the Lead contract as a tail-of-request
/// user message AND the trimmed surface — both must reach the wire.
#[tokio::test]
async fn fusion_turn_carries_lead_contract_and_surface() {
    let dir = crate::fresh_test_dir("fusion-wire");
    let provider = Arc::new(RecProvider {
        responses: queued(vec![text("ok")]),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let raw = Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    *raw.turn_mode.write_or_recover() = TurnMode::Fusion;
    raw.read_only.store(true, Ordering::Relaxed);
    let ctx = Arc::new(raw);
    AgentLoop::new(ctx)
        .run_turn("go", &NullObserver)
        .await
        .unwrap();

    let reqs = provider.requests.lock_or_recover();
    let (tools, last) = reqs.first().expect("one request");
    assert!(tools.iter().any(|t| t == "FusionExecute") && !tools.iter().any(|t| t == "Write"));
    assert!(
        last.contains("fusion mode") && last.contains("FusionExecute"),
        "the Lead contract rides the request tail: {last}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// End to end: the Lead's FusionExecute spawns a Sidekick on the routed
/// adapter, its Write lands, the harness runs the verify command for real
/// (exit 0), and the durable spine records spec + accepted.
#[tokio::test]
async fn fusion_execute_end_to_end() {
    let dir = crate::fresh_test_dir("fusion-e2e");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();

    let lead = Arc::new(MockProvider {
        responses: queued(vec![
            tool_call(
                "f1",
                "FusionExecute",
                r#"{"spec":{"goal":"create w.txt containing hello"},"files":["w.txt"],"verify_commands":["echo verified"],"model":"@sidekick"}"#,
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
    let ctx = fusion_ctx(&dir, SessionLog::ephemeral(), lead, sub.clone());
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();

    let outcome = agent.run_turn("build it", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));
    assert!(
        sub.calls.load(Ordering::Relaxed) >= 1,
        "the delegation reached the routed adapter"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("w.txt")).unwrap(),
        "hello",
        "the Sidekick's write landed inside the whitelist"
    );

    let evs = events(&ctx).await;
    assert!(
        evs.iter()
            .any(|e| matches!(e, SessionEvent::FusionSpec { .. })),
        "the spec must be durable"
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, SessionEvent::FusionAccepted { .. })),
        "a clean delegation records FusionAccepted"
    );
    // the accepted ToolResult carries the harness verdict
    let result = evs.iter().find_map(|e| match e {
        SessionEvent::ToolResult { name, output, .. } if name == "FusionExecute" => {
            Some(output.clone())
        }
        _ => None,
    });
    assert!(
        result
            .as_deref()
            .is_some_and(|o| o.contains("accepted") && o.contains("verified")),
        "the result reports the real verify run: {result:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The spec's file grant is a hard line at the child's gate: a Write
/// outside `files` is refused (not prompted), the audit fact names it,
/// and nothing lands on disk.
#[tokio::test]
async fn fusion_whitelist_refuses_outside_writes() {
    let dir = crate::fresh_test_dir("fusion-wl");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();

    let lead = Arc::new(MockProvider {
        responses: queued(vec![
            tool_call(
                "f1",
                "FusionExecute",
                r#"{"spec":"create w.txt","files":["w.txt"],"verify_commands":["echo ok"],"model":"@sidekick"}"#,
            ),
            text("wrapped"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let sub = Arc::new(MockProvider {
        responses: queued(vec![
            // the child tries to stray — the gate must refuse it
            tool_call("w1", "Write", r#"{"path":"nope.txt","content":"x"}"#),
            text("gave up"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = fusion_ctx(&dir, SessionLog::ephemeral(), lead, sub);
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();
    agent.run_turn("build it", &NullObserver).await.unwrap();

    assert!(
        !dir.join("nope.txt").exists(),
        "a Write outside the whitelist must not land"
    );
    // the refusal is durable on the child's own log
    let sub_id = ctx.fusion.lock_or_recover().sidekick_id.clone().unwrap();
    let child_log =
        SessionLog::open_path(&dir.join(".sunmao/sessions").join(format!("{sub_id}.jsonl")))
            .await
            .unwrap();
    let child_evs = child_log.events().await.unwrap();
    let refused = child_evs.iter().any(|e| {
        matches!(
            e,
            SessionEvent::ToolResult { name, ok: false, output, .. }
                if name == "Write" && output.contains("fusion whitelist")
        )
    });
    assert!(refused, "the child's log must carry the whitelist refusal");
    assert!(
        child_evs.iter().any(|e| matches!(
            e,
            SessionEvent::Hook { event, .. } if event == "fusion.whitelist.denied"
        )),
        "the denial is an audit fact, not just a result"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Rework + escalation: a failing verify feeds the verdict back, `steer`
/// resumes the SAME Sidekick (one roster row, one sidekick_id), a widened
/// `files` grant is audited — and the second consecutive failure
/// escalates: read_only disarms mid-turn so the Lead's own Write lands.
/// Turn end re-arms the flag: escalation is per-turn parole.
#[tokio::test]
async fn fusion_steer_reworks_then_escalates() {
    let dir = crate::fresh_test_dir("fusion-esc");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix").unwrap();

    let lead = Arc::new(MockProvider {
        responses: queued(vec![
            tool_call(
                "f1",
                "FusionExecute",
                r#"{"spec":"create w.txt","files":["w.txt"],"verify_commands":["exit 7"],"model":"@sidekick"}"#,
            ),
            // rework the same child — verify still fails (the spec's
            // list stands), and `files` widens the grant add-only
            tool_call(
                "f2",
                "FusionExecute",
                r#"{"spec":"retry","files":["w2.txt"],"steer":"try harder","model":"@sidekick"}"#,
            ),
            // escalated: the Lead's own write must now pass the gate
            tool_call("f3", "Write", r#"{"path":"rescue.txt","content":"x"}"#),
            text("finished myself"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let sub = Arc::new(MockProvider {
        responses: queued(vec![
            text("attempt one"),
            // SAME provider, second turn = the steer continuation
            text("attempt two"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = fusion_ctx(&dir, SessionLog::ephemeral(), lead, sub.clone());
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_turn_mode(TurnMode::Fusion, &NullObserver)
        .await
        .unwrap();
    let outcome = agent.run_turn("build it", &NullObserver).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed));

    // one delegation, one Sidekick — steer reused the same sub_id
    assert_eq!(
        ctx.live_tasks.lock_or_recover().len(),
        1,
        "steer reopens the same roster row, not a new spawn"
    );
    assert_eq!(sub.calls.load(Ordering::Relaxed), 2);

    // the escalated Lead's own Write landed mid-turn — the whitelist is a
    // SIDEKICK grant, so it must not reach back and block the Lead
    assert!(dir.join("rescue.txt").exists(), "escalation unlocks writes");

    let evs = events(&ctx).await;
    let results: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolResult { name, output, .. } if name == "FusionExecute" => {
                Some(output.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(
        results[0].contains("exit 7"),
        "the real exit code feeds back"
    );
    assert!(results[1].contains("escalated"), "the second miss unlocks");
    assert!(
        evs.iter()
            .any(|e| matches!(e, SessionEvent::FusionEscalated { .. })),
        "the escalation is a durable fact"
    );
    assert!(
        evs.iter().any(|e| matches!(
            e,
            SessionEvent::Hook { event, .. } if event == "fusion.whitelist"
        )),
        "widening the grant is audited"
    );

    // per-turn parole: the flag re-arms at turn end, escalation cleared
    assert!(
        ctx.read_only.load(Ordering::Relaxed),
        "the next fusion turn starts locked again"
    );
    assert!(!ctx.fusion.lock_or_recover().escalated);

    // both verify runs are durable facts on the child's own log — a rework
    // turn reads the failure it was steered to fix
    let sub_id = ctx.fusion.lock_or_recover().sidekick_id.clone().unwrap();
    let child_log =
        SessionLog::open_path(&dir.join(".sunmao/sessions").join(format!("{sub_id}.jsonl")))
            .await
            .unwrap();
    let verify_runs = child_log
        .events()
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(e, SessionEvent::LocalShell { command, .. } if command == "exit 7"))
        .count();
    assert_eq!(verify_runs, 2, "every verify run lands in the child's log");
    std::fs::remove_dir_all(&dir).ok();
}
