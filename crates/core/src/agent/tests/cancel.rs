//! `cancel` vs `cancel_main` — the IM frontend's `/stop` stops only the
//! main agent; the full `cancel()` is the cascade TUI/serve issue (a
//! foreground Task's Bash would otherwise outlive the killed turn).

use super::*;

/// One context wired like a real session's — mock provider, ephemeral log.
fn ctx() -> Arc<Context> {
    Arc::new(Context::new(
        Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ))
}

/// A running sub-agent roster row whose cancel handle points at a child
/// context — same shape `task::spawn::register_task` writes.
fn register_child(parent: &Context, child: &Context) {
    parent
        .live_tasks
        .lock_or_recover()
        .push(crate::context::TaskEntry {
            id: "sub-test-l1".into(),
            lane: 1,
            agent: None,
            prompt: "probe".into(),
            done: None,
            steer: Some(child.steer.clone()),
            cancel: Some(child.cancel_signal()),
        });
}

fn flag(ctx: &Context) -> bool {
    ctx.cancelled.load(std::sync::atomic::Ordering::Relaxed)
}

#[test]
fn cancel_main_stops_only_the_parent() {
    let parent = ctx();
    let child = ctx();
    register_child(&parent, &child);
    let agent = AgentLoop::new(parent.clone());

    agent.cancel_main();
    assert!(flag(&parent), "main agent cancelled");
    assert!(
        !flag(&child),
        "cancel_main must not cascade — IM /stop kills one layer"
    );
}

#[test]
fn cancel_still_cascades() {
    let parent = ctx();
    let child = ctx();
    register_child(&parent, &child);
    let agent = AgentLoop::new(parent.clone());

    agent.cancel();
    assert!(flag(&parent));
    assert!(flag(&child), "the TUI/serve kill path still cascades");
}

/// A context rooted at `dir` — the shell backend has to be pinned posix so
/// these tests exercise the same path on every box (`pwsh` is auto-detected
/// on Windows and would otherwise take the run).
fn ctx_in(dir: &std::path::Path) -> Arc<Context> {
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix\n").unwrap();
    Arc::new(Context::new(
        Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.to_path_buf(),
    ))
}

/// A real child process (~29s), not the deno `sleep` builtin: a builtin
/// lives inside the exec future and ignores the SIGKILL, so it can't tell
/// "the kill reached the run" from "the run was abandoned".
const LONG_LIVED_CMD: &str = if cfg!(windows) {
    "ping -n 30 127.0.0.1"
} else {
    "ping -c 30 127.0.0.1"
};

/// The cancel signal must have *memory*. `Notify::notify_waiters` stores no
/// permit, so a cancel that landed before the run's own kill waiter was
/// registered was swallowed whole: the user had already stopped the turn
/// and the shell went on running its command to the end. An
/// already-requested cancel must abort the run at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_already_requested_cancel_kills_a_running_shell() {
    let dir = crate::fresh_test_dir("cancel-shell");
    let ctx = ctx_in(&dir);
    // exactly what `AgentLoop::cancel` does, landing BEFORE the shell arms
    // its waiter — the window the dispatch notify → spawn_blocking
    // scheduling gap opens for real
    ctx.cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    ctx.cancel_notify.notify_waiters();

    let t0 = std::time::Instant::now();
    let run = crate::tool::run_foreground(
        LONG_LIVED_CMD,
        dir.clone(),
        5,
        crate::tool::ShellBackend::Posix,
        Some(ctx.cancel_signal()),
    )
    .await
    .unwrap();
    let elapsed = t0.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "an already-requested cancel must end the run at once — took {elapsed:?}"
    );
    assert!(
        run.ended
            .as_deref()
            .unwrap_or_default()
            .contains("cancelled"),
        "the run must report the user stop, not its own timeout: {:?}",
        run.ended
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The same guarantee on the `pwsh` backend — the one Windows auto-detects,
/// so it is what a Windows user's `Bash` really runs. The child is spawned
/// (and killed on drop) by this path, so the test also pins that a cancel
/// fired before the select does not leave a live pwsh behind.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_already_requested_cancel_kills_a_running_pwsh_command() {
    if std::process::Command::new("pwsh")
        .arg("--version")
        .output()
        .is_err()
    {
        return; // no pwsh on this box — the path is untestable here
    }
    let dir = crate::fresh_test_dir("cancel-pwsh");
    let ctx = ctx_in(&dir);
    ctx.cancel_signal().cancel();

    let t0 = std::time::Instant::now();
    let run = crate::tool::run_foreground(
        "Start-Sleep 30",
        dir.clone(),
        5,
        crate::tool::ShellBackend::Pwsh,
        Some(ctx.cancel_signal()),
    )
    .await
    .unwrap();
    let elapsed = t0.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "pwsh must not run the command the user already stopped — took {elapsed:?}"
    );
    assert!(
        run.ended
            .as_deref()
            .unwrap_or_default()
            .contains("cancelled"),
        "the run must report the user stop, not its own timeout: {:?}",
        run.ended
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The hard stop. A turn parked on an await with no cancel arm (here: the
/// session log, held by someone else — that is what the backstop exists
/// for) must not hold the round forever: once the cooperative grace has
/// lapsed the round is force-ended, the outcome reads Cancelled, and the
/// stop is announced instead of happening silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_stuck_past_the_cancel_grace_is_force_ended() {
    let dir = crate::fresh_test_dir("cancel-hard");
    let ctx = ctx_in(&dir);
    // the log is held for the whole test — the turn parks on it and no
    // cancel arm reaches that await
    let holder = {
        let c = ctx.clone();
        tokio::spawn(async move {
            let _log = c.sessions.lock().await;
            std::future::pending::<()>().await;
        })
    };
    for _ in 0..500 {
        if ctx.sessions.try_lock().is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(4)).await;
    }
    assert!(ctx.sessions.try_lock().is_err(), "the log must be held");

    let obs = HookNames::default();
    let agent = AgentLoop::new(ctx.clone());
    let cancel_then_wait = async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        agent.cancel();
    };
    let deadline = crate::agent::cancel::HARD_STOP_GRACE + std::time::Duration::from_secs(5);
    let done = tokio::time::timeout(deadline, async {
        tokio::join!(cancel_then_wait, agent.run_turn("go", &obs))
    })
    .await;
    let (_, res) = done.expect("a stuck turn must be force-ended, not hang forever");
    assert!(
        matches!(res.unwrap(), TurnOutcome::Cancelled),
        "a force-ended turn is a user stop"
    );
    assert!(
        obs.names().iter().any(|n| n == "force_stop"),
        "the hard stop must be announced, never silent — saw {:?}",
        obs.names()
    );
    holder.abort();
    std::fs::remove_dir_all(&dir).ok();
}

/// Records `LiveEvent::Hook` names — the audit trail a frontend renders.
#[derive(Default)]
struct HookNames(std::sync::Mutex<Vec<String>>);

impl HookNames {
    fn names(&self) -> Vec<String> {
        self.0.lock_or_recover().clone()
    }
}

impl Observer for HookNames {
    fn on_event(&self, ev: &LiveEvent) {
        if let LiveEvent::Hook { event, .. } = ev {
            self.0.lock_or_recover().push(event.clone());
        }
    }
}

/// End to end, the user's exact complaint: a turn running a long shell
/// command, stop pressed, and the round must unwind promptly. Then the
/// invariants: a second stop is a no-op, and the flag reset at turn END
/// leaves the next turn able to start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_ends_a_long_tool_turn_and_the_next_turn_starts_clean() {
    let dir = crate::fresh_test_dir("cancel-turn");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(dir.join(".sunmao/shell.txt"), "posix\n").unwrap();
    let provider = Arc::new(MockProvider {
        responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![
                StreamDelta::ToolCalls(vec![
                    ToolCallFragment {
                        index: 0,
                        id: Some("c".into()),
                        name: Some("Bash".into()),
                        arguments: None,
                    },
                    ToolCallFragment {
                        index: 0,
                        arguments: Some(format!("{{\"command\":\"{LONG_LIVED_CMD}\"}}")),
                        ..Default::default()
                    },
                ]),
                StreamDelta::Finish {
                    reason: Some("tool_calls".into()),
                    usage: None,
                },
            ],
            vec![
                StreamDelta::Content("done".into()),
                StreamDelta::Finish {
                    reason: Some("stop".into()),
                    usage: None,
                },
            ],
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let ctx = Arc::new(Context::new(
        provider.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    let agent = AgentLoop::new(ctx.clone());

    let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let cancel_then_wait = async {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            agent.cancel();
            // a second click must be a no-op, not an error or a wedge
            agent.cancel();
        };
        tokio::join!(cancel_then_wait, agent.run_turn("go", &NullObserver)).1
    })
    .await;
    assert!(
        matches!(
            first.expect("stop must unwind the round").unwrap(),
            TurnOutcome::Cancelled
        ),
        "the user stop must win"
    );

    // the next message starts a fresh turn: the flag reset at the previous
    // turn's END, so this one streams instead of short-circuiting
    let second = agent.run_turn("again", &NullObserver).await.unwrap();
    assert!(
        matches!(second, TurnOutcome::Completed),
        "a message after a stop starts a normal turn — got {second:?}"
    );
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "the follow-up turn really reached the provider"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Auto-compaction is a provider round trip with no tool call behind it —
/// the longest await in a turn that is not a tool, and it used to run to
/// the model's own end while the user's stop waited. A provider that never
/// yields the summary pins the arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_interrupts_an_in_flight_compaction() {
    /// Streams nothing, forever — only the cancel arm can end the summary.
    struct HangingSummary;

    #[async_trait::async_trait]
    impl ProviderAdapter for HangingSummary {
        async fn stream(&self, _req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
            Ok(Box::pin(futures_util::stream::pending()))
        }
    }

    let dir = crate::fresh_test_dir("cancel-compact");
    let ctx = Arc::new(Context::new(
        Arc::new(HangingSummary),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    // one durable message, so the head-of-turn estimate is non-zero and the
    // auto-compact tripwire (threshold 0) actually fires
    ctx.sessions
        .lock()
        .await
        .append(&crate::session::SessionEvent::Message {
            message: sunmao_llm::types::Message::user("seed"),
        })
        .await
        .unwrap();
    let agent = AgentLoop::new(ctx.clone()).with_compact_threshold(0);

    let cancel_then_wait = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        agent.cancel();
    };
    let done = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(cancel_then_wait, agent.run_turn("go", &NullObserver))
    })
    .await;
    let (_, res) = done.expect("a cancel must end a summary in flight, not wait it out");
    assert!(
        matches!(res.unwrap(), TurnOutcome::Cancelled),
        "the stop must win over the summary"
    );
    std::fs::remove_dir_all(&dir).ok();
}
