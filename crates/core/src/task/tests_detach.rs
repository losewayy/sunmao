//! Foreground waiter hand-over must preserve the detached driver.

use super::spawn::{Driven, await_result};
use super::tests_steer_resume::MockProvider;
use crate::context::Context;
use crate::session::SessionLog;
use crate::tool::{KillSwitch, ToolResult, builtin_registry};
use std::sync::Arc;

#[tokio::test]
async fn detached_waiter_keeps_driver_alive() {
    let ctx = Context::new(
        Arc::new(MockProvider),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    );
    let cancel = ctx.cancel_signal();
    let detach = KillSwitch::new();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let (finished, completion) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        released.await.unwrap();
        let result = ToolResult {
            exit_code: None,
            output: "done".into(),
            ok: true,
        };
        assert!(
            tx.send(result).is_err(),
            "detached waiter releases its receiver"
        );
        finished.send(()).unwrap();
    });
    detach.kill();
    let result = await_result(
        &ctx,
        "sub-detach",
        Driven { rx, join },
        &detach,
        cancel.clone(),
    )
    .await;
    assert!(result.ok);
    assert!(!cancel.is_cancelled(), "detach must not cancel the child");
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), completion)
        .await
        .expect("detached driver must finish")
        .expect("detached driver must not be aborted");
}
