//! `RunCode` — programmatic tool calling (PTC / codemode).
//!
//! The model writes one JS program instead of emitting tool calls one at a
//! time. The program runs in an embedded QuickJS sandbox with **no fs, no
//! network, no `import`/`require`/`process`** (the runtime simply doesn't
//! have them — security is structural, not policy): every side effect is a
//! call into the host-injected `tools.*` surface, dispatched through the
//! same hooks/permission/approval pipeline a model-emitted tool call takes.
//! Intermediate results stay inside the sandbox; only the script's return
//! value reaches the model's context.
//!
//! Engine notes (rquickjs 0.14, `futures` feature):
//! - `AsyncRuntime`/`AsyncContext` are `!Send` (Rc internals) — like
//!   `deno_task_shell` in `shell.rs`, everything is constructed and driven
//!   inside one `spawn_blocking` closure via `Handle::block_on`.
//! - Host functions must be `'static` closures (the `'js` bound can't hold
//!   a `&Context`), so `__ptc` enqueues requests on a channel and awaits a
//!   oneshot reply. The serving future lives outside `async_with` in the
//!   same `block_on` scope and borrows `ctx` directly — `Promise.all` fan-out
//!   lands on a `FuturesUnordered`, which is genuinely concurrent
//!   (smoke-tested: two parallel calls overlap).
//! - Two-layer cancellation: a spawned watchdog flips the flag the JS
//!   interrupt handler consults (covers CPU-bound scripts — the block_on's
//!   own select! arms can't be polled while sync JS occupies the thread),
//!   and `select!` arms cover scripts parked in a host await. The watchdog
//!   is aborted when the select returns, so nothing outlives the script.
//! - Nested calls produce `SessionEvent::PtcCall` audit facts, NOT
//!   `ToolCall`/`ToolResult` — those would fold into protocol tool messages
//!   without a matching assistant `tool_call` and corrupt the transcript.
//!   PtcCall stays out of the fold but replays as a nested transcript row.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use sunmao_llm::types::Tool;

use crate::agent::{LiveEvent, Observer};
use crate::context::Context;
use crate::tool::{ToolImpl, ToolResult};

/// JS heap cap per script — big enough for real fan-outs, small enough that
/// a `while(1) a.push("x")` dies fast instead of eating the process.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

/// One host-side request from the sandbox: op name + JSON args + reply slot.
type PtcMsg = (
    String,
    Value,
    tokio::sync::oneshot::Sender<Result<String, String>>,
);

pub struct RunCodeTool;

#[async_trait::async_trait]
impl ToolImpl for RunCodeTool {
    fn name(&self) -> &'static str {
        "RunCode"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "RunCode",
            "Run a JavaScript program in a sandbox that orchestrates this session's tools. \
             `await tools.<Name>(args)` calls any registered tool through the normal \
             permission/approval pipeline (arg shape = that tool's schema; result = \
             {ok, output}); `await describe()` lists the callable tools; `await store(key, \
             value)`/`await load(key)` persist JSON values across calls. Use Promise.all \
             for parallel calls and keep intermediate data in the script — only `return`ed \
             values enter the transcript. The sandbox has no fs/network/import: tools are \
             the only capabilities. The script itself is the return value of evaluating \
             `code` — end with an expression or `(async () => { ... })()`.",
            json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "JavaScript source; its completion value (a promise is awaited) becomes the result"},
                    "timeout_ms": {"type": "integer", "description": "Wall-clock budget for the whole script (default 120000, max 600000)"}
                },
                "required": ["code"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &Arc<Context>) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            code: String,
            timeout_ms: Option<u64>,
        }
        let a: Args = serde_json::from_value(args)?;
        let timeout = Duration::from_millis(
            a.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1_000, MAX_TIMEOUT_MS),
        );
        let ctx = ctx.clone();
        // !Send world: the runtime, context, and every JS value are born and
        // die inside this blocking closure — identical discipline to
        // `shell.rs::run_parsed` for deno_task_shell.
        let out = tokio::task::spawn_blocking(move || run_script(ctx, &a.code, timeout)).await;
        Ok(match out {
            Ok(Ok(text)) => ToolResult {
                output: text,
                ok: true,
            },
            Ok(Err(e)) => ToolResult {
                output: e,
                ok: false,
            },
            Err(e) => ToolResult {
                output: format!("script engine panicked: {e}"),
                ok: false,
            },
        })
    }
}

/// Forwards live events for nested calls through the session sink — a
/// RunCode script's tool calls surface in the transcript like a sub-agent's
/// would (they already carry the session's `depth`/`lane`).
struct PtcObserver(Option<Arc<dyn Observer>>);

impl Observer for PtcObserver {
    fn on_event(&self, ev: &LiveEvent) {
        if let Some(s) = &self.0 {
            s.on_event(ev);
        }
    }
}

/// Drive one script to completion. `ctx` is the *session's* Context — nested
/// calls share its permissions, grants, approval mode, live sink and log.
fn run_script(ctx: Arc<Context>, code: &str, timeout: Duration) -> Result<String, String> {
    let handle = tokio::runtime::Handle::current();
    handle.block_on(async {
        let rt = rquickjs::AsyncRuntime::new().map_err(|e| format!("quickjs init: {e}"))?;
        rt.set_memory_limit(MEMORY_LIMIT).await;
        // Watchdog: a separate task, not a select! arm — this block_on runs
        // on ONE thread, so while a CPU-bound `while(true)` sits inside an
        // async_with poll no other future here can ever be polled. The task
        // flips the flag the JS interrupt handler reads; an abort at the end
        // of the select keeps it from sleeping past the script's finish.
        // `notify_waiters` only wakes registered waiters — enable() pins the
        // registration before the select, and the cancelled flag is the
        // fallback for a cancel that landed before it.
        let stop = Arc::new(AtomicBool::new(false));
        let reason = Arc::new(AtomicU64::new(0)); // 0 none, 1 timeout, 2 cancel
        rt.set_interrupt_handler(Some({
            let stop = stop.clone();
            Box::new(move || stop.load(Ordering::Relaxed))
        }))
        .await;
        let jctx = rquickjs::AsyncContext::custom::<rquickjs::context::intrinsic::All>(&rt)
            .await
            .map_err(|e| format!("quickjs context: {e}"))?;
        let watchdog = {
            let stop = stop.clone();
            let reason = reason.clone();
            let cancelled = ctx.cancelled.clone();
            let cancel = ctx.cancel_notify.clone();
            tokio::spawn(async move {
                let n = cancel.notified();
                tokio::pin!(n);
                n.as_mut().enable();
                if cancelled.load(Ordering::Relaxed) {
                    stop.store(true, Ordering::Relaxed);
                    reason.store(2, Ordering::Relaxed);
                    return;
                }
                tokio::select! {
                    () = tokio::time::sleep(timeout) => reason.store(1, Ordering::Relaxed),
                    () = &mut n => reason.store(2, Ordering::Relaxed),
                }
                stop.store(true, Ordering::Relaxed);
            })
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<PtcMsg>();
        let observer = PtcObserver(ctx.live_sink.get().cloned());
        let serve = serve_requests(&ctx, rx, &observer);
        tokio::pin!(serve);
        let code = code.to_string();
        let cancel_wait = ctx.cancel_notify.notified();
        tokio::pin!(cancel_wait);
        cancel_wait.as_mut().enable();
        let res = if ctx.cancelled.load(Ordering::Relaxed) {
            // the cancel beat our waiter registration — the notified()
            // would wait forever on a wake that already happened
            Err("cancelled by user".into())
        } else {
            tokio::select! {
                r = jctx.async_with(async |jctx| -> Result<String, String> {
                    install_surface(&jctx, &ctx, tx).map_err(|e| format!("install: {e}"))?;
                    let p = jctx
                        .eval::<rquickjs::promise::MaybePromise, _>(code)
                        .map_err(|e| format!("eval error: {e} / {:?}", jctx.catch()))?;
                    let v = p
                        .into_future::<rquickjs::Value>()
                        .await
                        .map_err(|e| format!("script rejected: {e} / {:?}", jctx.catch()))?;
                    jctx.json_stringify(&v)
                        .map_err(|e| e.to_string())?
                        .map(|s| s.to_string().unwrap_or_default())
                        .ok_or_else(|| "script returned a non-JSON-serializable value".to_string())
                }) => match r {
                    Err(_) if reason.load(Ordering::Relaxed) == 1 => {
                        Err(format!("script exceeded its {timeout:?} budget — killed"))
                    }
                    Err(_) if reason.load(Ordering::Relaxed) == 2 => Err("cancelled by user".into()),
                    r => r,
                },
                () = &mut serve => Err("tool bridge ended before the script".into()),
                () = tokio::time::sleep(timeout) => {
                    Err(format!("script exceeded its {timeout:?} budget — killed"))
                }
                () = &mut cancel_wait => Err("cancelled by user".into()),
            }
        };
        // the script is over — a watchdog still parked on sleep(timeout)
        // would hold the interrupt flag closed until the budget lapses;
        // abort it so nothing outlives this call
        watchdog.abort();
        // Calls the script fired but never awaited (`Promise.all` without
        // a join, a fire-and-forget `tools.Write`) are still inflight in
        // `serve`. Dropping it mid-dispatch used to orphan the work AND
        // lose the durable PtcCall audit fact — drain the channel so every
        // started call lands its record. jctx drops first: its host fn
        // clones hold `tx`, and recv() only ends on the last drop.
        drop(jctx);
        drop(rt);
        serve.await;
        res
    })
}

mod install;
mod serve;

use install::install as install_surface;
use serve::serve_requests;

#[cfg(test)]
mod tests;
