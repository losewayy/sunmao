//! Steer + resume fixtures — the sub-agent addressing layer: pushing into
//! a live child's turn (`steer`/`steer_sub`) and continuing a finished
//! child on its own log (`resume`, incl. `model` re-route).

use super::*;
use crate::session::SessionLog;
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

/// Recording scripted provider: queued responses replayed in order, plus
/// the user-facing texts each request carried (steer assertions need to
/// see what the model actually saw, not just what the log holds).
struct RecProvider {
    responses: std::sync::Mutex<std::collections::VecDeque<Vec<StreamDelta>>>,
    calls: std::sync::atomic::AtomicUsize,
    /// user-role `content_text`s per stream() call
    seen: std::sync::Mutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl ProviderAdapter for RecProvider {
    async fn stream(&self, req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.seen.lock().unwrap().push(
            req.messages
                .iter()
                .filter(|m| matches!(m.role, sunmao_llm::types::Role::User))
                .filter_map(|m| m.content_text())
                .collect(),
        );
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
                        usage: Some(Usage::default()),
                    },
                ]
            });
        Ok(Box::pin(stream::iter(deltas.into_iter().map(Ok))))
    }
}

fn tool_call_delta(id: &str, name: &str, args: &str) -> Vec<StreamDelta> {
    vec![
        StreamDelta::ToolCalls(vec![
            sunmao_llm::ToolCallFragment {
                index: 0,
                id: Some(id.into()),
                name: Some(name.into()),
                arguments: None,
            },
            sunmao_llm::ToolCallFragment {
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

fn text_delta(text: &str) -> Vec<StreamDelta> {
    vec![
        StreamDelta::Content(text.into()),
        StreamDelta::Finish {
            reason: Some("stop".into()),
            usage: Some(Usage::default()),
        },
    ]
}

/// Steer into a running sub-agent: the roster's steer handle pushes a user
/// message into the child's live turn — it lands at a request boundary as a
/// user fact in the child's own log, and the child's next request sees it.
#[tokio::test]
async fn steer_into_running_sub_turn() {
    let dir = crate::fresh_test_dir("steer");
    std::fs::create_dir_all(&dir).unwrap();
    // The child signals when request 1 finishes streaming; its second
    // request then parks on `release` — the test pushes the steer in that
    // window, so it lands mid-turn between the settled tool pair and the
    // next request, deterministically.
    let req1_done = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    struct GateProvider {
        req1_done: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        seen: std::sync::Mutex<Vec<Vec<String>>>,
        /// request ordinals, behind a Mutex so stream() can't double-lock
        nth: std::sync::Mutex<usize>,
    }
    #[async_trait::async_trait]
    impl ProviderAdapter for GateProvider {
        async fn stream(&self, req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
            {
                let users: Vec<String> = req
                    .messages
                    .iter()
                    .filter(|m| matches!(m.role, sunmao_llm::types::Role::User))
                    .filter_map(|m| m.content_text())
                    .collect();
                self.seen.lock().unwrap().push(users);
            }
            let nth = {
                let mut n = self.nth.lock().unwrap();
                *n += 1;
                *n
            };
            if nth == 1 {
                self.req1_done.notify_one();
                Ok(Box::pin(stream::iter(
                    tool_call_delta("g", "Glob", "{\"pattern\":\"*.rs\"}")
                        .into_iter()
                        .map(Ok),
                )))
            } else {
                // park until the test has pushed the steer
                self.release.notified().await;
                Ok(Box::pin(stream::iter(
                    text_delta("steered").into_iter().map(Ok),
                )))
            }
        }
    }
    let provider = Arc::new(GateProvider {
        req1_done: req1_done.clone(),
        release: release.clone(),
        seen: std::sync::Mutex::new(Vec::new()),
        nth: std::sync::Mutex::new(0),
    });
    let ctx = Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );

    let res = TaskTool
        .call(
            json!({"prompt": "work it", "run_in_background": true}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(res.ok);
    let sub_id = res
        .output
        .split_whitespace()
        .find(|w| w.starts_with("sub-") && w.as_bytes().get(4).is_some_and(|b| b.is_ascii_digit()))
        .expect("call returns the task id")
        .trim_end_matches(',')
        .to_string();
    // wait for the child's first request, then push the steer while the
    // child is mid-turn (its second request is parked on `release`)
    req1_done.notified().await;
    ctx.steer_sub(&sub_id, "switch to plan B".into())
        .expect("running sub-agent must accept a steer");
    release.notify_one();

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

    // the steer is a durable user fact in the child's own log, inside its
    // single turn — after the tool_result it was queued behind
    let text = std::fs::read_to_string(dir.join(format!(".sunmao/sessions/{sub_id}.jsonl")))
        .expect("child log exists");
    let steer_pos = text.find("switch to plan B").expect("steer in child log");
    let result_pos = text.find("\"tool_result\"").expect("tool result precedes");
    assert!(steer_pos > result_pos, "steer folds after the settled pair");
    // and the model's second request carried it
    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "gate releases exactly the second request");
    assert!(
        seen[1].iter().any(|u| u.contains("switch to plan B")),
        "the next request saw the steer — {seen:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A finished sub-agent resumes on its own log: same file grows (no new
/// session), the `resumed` hook fact marks the seam, the roster entry
/// reopens (fresh lane, done reset), and the continuation request inherits
/// the prior transcript.
#[tokio::test]
async fn resume_finished_sub_continues_log() {
    let dir = crate::fresh_test_dir("resume");
    std::fs::create_dir_all(&dir).unwrap();
    let provider = Arc::new(RecProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let ctx = Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );

    let res = TaskTool
        .call(json!({"prompt": "first leg"}), &ctx)
        .await
        .unwrap();
    assert!(res.ok, "{}", res.output);
    let (sub_id, lane1) = {
        let tasks = ctx.live_tasks.lock().unwrap();
        let e = tasks.iter().find(|t| t.done.is_some()).expect("spawn done");
        (e.id.clone(), e.lane)
    };
    let log_path = dir.join(format!(".sunmao/sessions/{sub_id}.jsonl"));
    let before = std::fs::read_to_string(&log_path).unwrap().lines().count();

    let res = TaskTool
        .call(json!({"resume": sub_id, "prompt": "keep going"}), &ctx)
        .await
        .unwrap();
    assert!(res.ok, "{}", res.output);

    let after_text = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        after_text.lines().count() > before,
        "the same log grew — no new session file"
    );
    assert_eq!(
        std::fs::read_dir(dir.join(".sunmao/sessions"))
            .unwrap()
            .count(),
        1,
        "resume must not mint a second session file"
    );
    assert!(
        after_text.contains("\"event\":\"resumed\""),
        "the resumed seam is a durable hook fact"
    );
    // roster: same entry reopened — new lane, done flipped again
    {
        let tasks = ctx.live_tasks.lock().unwrap();
        assert_eq!(tasks.len(), 1, "resume reopens, not duplicates");
        let e = &tasks[0];
        assert_ne!(e.lane, lane1, "the continuation claims a fresh lane");
        assert_eq!(e.done, Some(true));
        assert!(e.steer.is_some(), "a live handle is re-registered");
    }
    // transcript continuity: the continuation's request folded the first
    // leg's prompt — same log, same conversation.
    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[1].iter().any(|u| u.contains("first leg"))
            && seen[1].iter().any(|u| u.contains("keep going")),
        "the resumed turn saw both legs — {seen:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `Task{resume, model}` re-routes the continuation to another adapter —
/// the rate-limit escape: a provider-limited sub-agent can be continued on
/// a different route.
#[tokio::test]
async fn resume_with_model_reroutes_adapter() {
    let dir = crate::fresh_test_dir("resumemodel");
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
    .with_adapter(
        "@alt",
        Arc::new(RecProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![text_delta(
                "alt provider spoke",
            )])),
            calls: std::sync::atomic::AtomicUsize::new(0),
            seen: std::sync::Mutex::new(Vec::new()),
        }),
    );
    ctx.models = Some(Arc::new(resolver));

    let res = TaskTool
        .call(json!({"prompt": "first leg"}), &ctx)
        .await
        .unwrap();
    assert!(res.ok, "{}", res.output);
    let sub_id = ctx.live_tasks.lock().unwrap()[0].id.clone();

    let res = TaskTool
        .call(
            json!({"resume": sub_id, "prompt": "continue", "model": "@alt"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        res.output.contains("alt provider spoke"),
        "the continuation ran on the @alt adapter — {}",
        res.output
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Steering a finished (or unknown) sub-agent is a legible failure, not a
/// silent queue.
#[tokio::test]
async fn steer_to_finished_sub_errors() {
    let dir = crate::fresh_test_dir("steerdead");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    let res = TaskTool
        .call(json!({"prompt": "quick"}), &ctx)
        .await
        .unwrap();
    assert!(res.ok);
    let sub_id = ctx.live_tasks.lock().unwrap()[0].id.clone();

    let res = TaskTool
        .call(json!({"steer": sub_id, "message": "too late"}), &ctx)
        .await
        .unwrap();
    assert!(!res.ok, "finished sub-agent refuses the steer");
    assert!(res.output.contains("finished"), "{}", res.output);

    let res = TaskTool
        .call(json!({"steer": "sub-ghost", "message": "hello?"}), &ctx)
        .await
        .unwrap();
    assert!(!res.ok);
    assert!(res.output.contains("no such sub-agent"), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// `resume` without `prompt` is refused — the continuation instruction is
/// the point of the call.
#[tokio::test]
async fn resume_requires_prompt() {
    let dir = crate::fresh_test_dir("resumenoprompt");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    let err = TaskTool
        .call(json!({"resume": "sub-1"}), &ctx)
        .await
        .err()
        .expect("resume without prompt must fail");
    assert!(err.to_string().contains("prompt"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Resuming an id with no log on disk fails loudly — a typo'd id must not
/// silently spawn a fresh generic child under a borrowed name.
#[tokio::test]
async fn resume_missing_log_errors() {
    let dir = crate::fresh_test_dir("resumemiss");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    let err = TaskTool
        .call(json!({"resume": "sub-ghost", "prompt": "x"}), &ctx)
        .await
        .err()
        .expect("missing log must fail");
    assert!(err.to_string().contains("no such sub-agent log"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A still-running sub-agent can't be resumed — steer is the verb for the
/// living; resume is for the dead.
#[tokio::test]
async fn resume_running_sub_errors() {
    let dir = crate::fresh_test_dir("resumerun");
    std::fs::create_dir_all(&dir).unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    struct Park {
        gate: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl ProviderAdapter for Park {
        async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
            self.gate.notified().await;
            Ok(Box::pin(stream::iter(
                text_delta("finally").into_iter().map(Ok),
            )))
        }
    }
    let ctx = Context::new(
        Arc::new(Park { gate: gate.clone() }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    let res = TaskTool
        .call(json!({"prompt": "p", "run_in_background": true}), &ctx)
        .await
        .unwrap();
    let sub_id = res
        .output
        .split_whitespace()
        .find(|w| w.starts_with("sub-") && w.as_bytes().get(4).is_some_and(|b| b.is_ascii_digit()))
        .unwrap()
        .trim_end_matches(',')
        .to_string();
    let err = TaskTool
        .call(json!({"resume": sub_id, "prompt": "again"}), &ctx)
        .await
        .err()
        .expect("running sub refuses resume");
    assert!(err.to_string().contains("still running"), "{err}");
    gate.notify_one();
    std::fs::remove_dir_all(&dir).ok();
}
