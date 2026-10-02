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
            cancel: Some(crate::context::SubCancel::new(child)),
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
