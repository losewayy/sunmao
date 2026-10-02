//! Pointed cancel — `cancel_sub` kills one running child through the
//! AgentLoop surface, flips its roster row to `done=false`, and leaves a
//! `task.cancel` audit row on the PARENT's log so a roster kill is
//! attributable, not silent.

use super::*;
use crate::context::MutexRecover;
use crate::session::SessionLog;
use crate::tool::builtin_registry;
use sunmao_llm::{ChatRequest, DeltaStream, ProviderAdapter};

/// Cancel ONE running sub-agent (the roster's kill control): the parked
/// child exits its pending request, its roster row flips to `done=false`,
/// and a second cancel is a legible Finished refusal — not a panic.
#[tokio::test]
async fn cancel_sub_kills_one_child() {
    let dir = crate::fresh_test_dir("cancel-sub");
    std::fs::create_dir_all(&dir).unwrap();
    // The provider parks forever on its first request — the only way out
    // is the cancel_notify `select!` arm in the turn loop.
    struct ParkProvider {
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl ProviderAdapter for ParkProvider {
        async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
            self.started.notify_one();
            std::future::pending::<()>().await;
            unreachable!()
        }
    }
    let started = Arc::new(tokio::sync::Notify::new());
    let ctx = Arc::new(Context::new(
        Arc::new(ParkProvider {
            started: started.clone(),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));

    let res = TaskTool
        .call(
            json!({"prompt": "park forever", "run_in_background": true}),
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
    started.notified().await;
    // go through AgentLoop (not bare ctx) so the `task.cancel` audit row
    // lands — the parent's log is where a roster kill gets attributed
    let agent = crate::agent::AgentLoop::new(ctx.clone());
    agent
        .cancel_sub(&sub_id)
        .await
        .expect("running child must cancel");

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
    let done = ctx.live_tasks.lock_or_recover()[0].done;
    assert_eq!(
        done,
        Some(false),
        "cancelled child records a failure — {done:?}"
    );
    // the parent log attributes the kill — a roster click is auditable,
    // not silent
    {
        let events = ctx
            .sessions
            .lock()
            .await
            .events()
            .await
            .expect("read parent log");
        assert!(
            events.iter().any(|e| matches!(
                e,
                crate::session::SessionEvent::Hook { event, detail }
                    if event == "task.cancel" && detail == &sub_id
            )),
            "task.cancel audit row must name the killed child"
        );
    }
    // and a second cancel reports Finished, not Ok — the row is done
    let err = ctx.cancel_sub(&sub_id).unwrap_err().to_string();
    assert!(err.contains("finished"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A foreground Task whose owning turn is cancelled gets its call future
/// dropped mid-run — the roster entry must still settle, or it reads as
/// "running" forever and `Task{resume}` refuses the child for good.
#[tokio::test]
async fn cancelled_foreground_spawn_does_not_ghost_the_roster() {
    let dir = crate::fresh_test_dir("cancel-fg");
    std::fs::create_dir_all(&dir).unwrap();
    // first request parks forever (the turn's cancel drops the call
    // future — no reply ever arrives); a resumed child answers fast
    struct ParkOnce {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl ProviderAdapter for ParkOnce {
        async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
            if self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                == 0
            {
                std::future::pending::<()>().await;
            }
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(sunmao_llm::StreamDelta::Content("done".into())),
                Ok(sunmao_llm::StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                }),
            ])))
        }
    }
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ctx = Arc::new(Context::new(
        Arc::new(ParkOnce {
            calls: calls.clone(),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));

    let spawned = tokio::spawn({
        let ctx = ctx.clone();
        async move { TaskTool.call(json!({"prompt": "park forever"}), &ctx).await }
    });
    // wait until the child is actually parked on its first request — the
    // roster row exists earlier than that (registration precedes run_turn),
    // and aborting too early would leave the provider's park-once armed
    // for the RESUME call instead
    for _ in 0..500 {
        if calls.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    spawned.abort();
    let _ = spawned.await;
    let entry = ctx.live_tasks.lock_or_recover()[0].clone();
    assert_eq!(
        entry.done,
        Some(false),
        "a dropped foreground spawn must settle its roster row — {:?}",
        entry.done
    );
    // resume must not refuse a ghost as "still running"
    let res = TaskTool
        .call(json!({"resume": entry.id, "prompt": "continue"}), &ctx)
        .await
        .expect("resume must accept a cancelled child");
    assert!(res.ok, "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}
