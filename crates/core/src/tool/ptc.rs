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
//! - Two-layer cancellation: a watchdog flips a flag the JS interrupt
//!   handler consults (covers CPU-bound scripts), and `select!` arms on
//!   timeout/cancel cover scripts parked in a host await.
//! - Nested calls produce `SessionEvent::Hook` audit facts, NOT
//!   `ToolCall`/`ToolResult` — those would fold into protocol tool messages
//!   without a matching assistant `tool_call` and corrupt the transcript.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use sunmao_llm::types::Tool;

use crate::agent::{LiveEvent, Observer, call_summary, specifier_for, tool_timeout_for};
use crate::context::{Context, MutexRecover};
use crate::hooks::{HookEvent, HookInput};
use crate::session::SessionEvent;
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
        // Watchdog: timeout or session cancel flips the flag the JS
        // interrupt handler reads — the only way out of a CPU-bound
        // `while(true)` is an uncatchable interrupt mid-eval.
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
        {
            let stop = stop.clone();
            let reason = reason.clone();
            let cancel = ctx.cancel_notify.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = tokio::time::sleep(timeout) => reason.store(1, Ordering::Relaxed),
                    () = cancel.notified() => reason.store(2, Ordering::Relaxed),
                }
                stop.store(true, Ordering::Relaxed);
            });
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<PtcMsg>();
        let observer = PtcObserver(ctx.live_sink.get().cloned());
        let serve = serve_requests(&ctx, rx, &observer);
        let code = code.to_string();
        tokio::select! {
            r = jctx.async_with(async |jctx| -> Result<String, String> {
                install(&jctx, &ctx, tx).map_err(|e| format!("install: {e}"))?;
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
            () = serve => Err("tool bridge ended before the script".into()),
            () = tokio::time::sleep(timeout) => {
                Err(format!("script exceeded its {timeout:?} budget — killed"))
            }
            () = ctx.cancel_notify.notified() => Err("cancelled by user".into()),
        }
    })
}

/// Register the JS surface: `tools.<Name>(args)` per callable tool plus the
/// `store`/`load`/`describe` builtins — all thin wrappers over `__ptc`,
/// the single host channel. `RunCode` itself is withheld: a sandboxed
/// script must not spawn nested sandboxes.
fn install<'js>(
    jctx: &rquickjs::Ctx<'js>,
    ctx: &Arc<Context>,
    tx: tokio::sync::mpsc::UnboundedSender<PtcMsg>,
) -> rquickjs::Result<()> {
    let globals = jctx.globals();
    globals.set(
        "__ptc",
        rquickjs::Function::new(
            jctx.clone(),
            rquickjs::prelude::Async(move |op: String, args: String| {
                let tx = tx.clone();
                async move {
                    let (rtx, rrx) = tokio::sync::oneshot::channel();
                    let args_v: Value = serde_json::from_str(&args).unwrap_or(Value::Null);
                    if tx.send((op, args_v, rtx)).is_err() {
                        return Err(rquickjs::Error::new_into_js_message(
                            "host",
                            "js",
                            "tool bridge is gone",
                        ));
                    }
                    match rrx.await {
                        Ok(Ok(out)) => Ok(out),
                        Ok(Err(e)) => Err(rquickjs::Error::new_into_js_message("host", "js", e)),
                        Err(_) => Err(rquickjs::Error::new_into_js_message(
                            "host",
                            "js",
                            "tool bridge dropped the reply",
                        )),
                    }
                }
            }),
        ),
    )?;
    let names: Vec<String> = ctx
        .tools
        .declarations()
        .iter()
        .map(|t| t.function.name.clone())
        .filter(|n| n != "RunCode")
        .collect();
    // Bracket-access assignments tolerate any tool name (mcp__x__y is a
    // valid identifier anyway, but `tools["..."]` needs no validation).
    let list = serde_json::to_string(&names).unwrap_or_else(|_| "[]".into());
    jctx.eval::<(), _>(format!(
        r#"
        globalThis.tools = {{}};
        for (const n of {list}) {{
            tools[n] = (args) => __ptc("tool", JSON.stringify({{name: n, args: args ?? {{}}}}))
                .then(JSON.parse);
        }}
        globalThis.store = (key, value) => __ptc("store", JSON.stringify({{key, value}}))
            .then(JSON.parse);
        globalThis.load = (key) => __ptc("load", JSON.stringify({{key}}))
            .then((r) => JSON.parse(r).value);
        globalThis.describe = (name) => __ptc("describe", JSON.stringify({{name: name ?? null}}))
            .then(JSON.parse);
        "#,
    ))
}

/// The host side of `__ptc`: receive requests, dispatch them against the
/// session's real pipeline, keep `Promise.all`-era calls concurrent via
/// `FuturesUnordered`. Ends when the script's side drops its senders.
async fn serve_requests(
    ctx: &Arc<Context>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<PtcMsg>,
    observer: &PtcObserver,
) {
    use futures_util::StreamExt;
    let mut inflight = futures_util::stream::FuturesUnordered::new();
    let seq = AtomicU64::new(0);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some((op, args, reply)) => {
                    let n = seq.fetch_add(1, Ordering::Relaxed);
                    inflight.push(dispatch(ctx, observer, op, args, reply, n));
                }
                None => {
                    while inflight.next().await.is_some() {}
                    break;
                }
            },
            Some(()) = inflight.next() => {}
        }
    }
}

/// One sandboxed request → host. `tool` ops take the full dispatch path
/// (PreToolUse → gate → run → PostToolUse); the `store`/`load`/`describe`
/// builtins are pure in-memory/durable bookkeeping, no gate.
async fn dispatch(
    ctx: &Arc<Context>,
    observer: &PtcObserver,
    op: String,
    args: Value,
    reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    seq: u64,
) {
    let res = match op.as_str() {
        "tool" => tool_call(ctx, observer, &args, seq).await,
        "store" => {
            let key = args["key"].as_str().unwrap_or_default().to_string();
            let value = serde_json::to_string(&args["value"]).unwrap_or_else(|_| "null".into());
            if key.is_empty() {
                Err("store: empty key".into())
            } else {
                ctx.ptc_store
                    .lock_or_recover()
                    .insert(key.clone(), value.clone());
                let mut log = ctx.sessions.lock().await;
                log.append_audit(&SessionEvent::PtcStore {
                    key: key.clone(),
                    value,
                })
                .await;
                Ok(format!("{{\"stored\":{}}}", json!(key)))
            }
        }
        "load" => {
            let key = args["key"].as_str().unwrap_or_default();
            let v = ctx.ptc_store.lock_or_recover().get(key).cloned();
            Ok(format!(
                "{{\"value\":{}}}",
                v.unwrap_or_else(|| "null".into())
            ))
        }
        "describe" => {
            let name = args["name"].as_str();
            let decls: Vec<Tool> = ctx
                .tools
                .declarations()
                .into_iter()
                .filter(|t| {
                    t.function.name != "RunCode" && name.is_none_or(|n| t.function.name == n)
                })
                .collect();
            Ok(serde_json::to_string(&decls).unwrap_or_else(|_| "[]".into()))
        }
        other => Err(format!("unknown op {other:?}")),
    };
    let _ = reply.send(res);
}

/// One `tools.<Name>(args)` call through the real dispatch seam: PreToolUse
/// hooks (veto/rewrite count) → the shared gate (rules, grants, modes,
/// classifier, approval card) → `tools.call` under the same per-tool
/// watchdog the turn loop applies → PostToolUse. Durable record lands as
/// `Hook` audit facts, never as transcript ToolCall/ToolResult.
async fn tool_call(
    ctx: &Arc<Context>,
    observer: &PtcObserver,
    args: &Value,
    seq: u64,
) -> Result<String, String> {
    let name = args["name"].as_str().unwrap_or_default().to_string();
    if name.is_empty() || name == "RunCode" {
        return Ok(json!({"ok": false, "output": format!("no such tool: {name:?}")}).to_string());
    }
    let call_id = format!("ptc-{seq}");
    let mut args_value = args.get("args").cloned().unwrap_or(Value::Null);

    // PreToolUse: hooks may veto, rewrite input, or hand the gate a verdict.
    let pre = ctx
        .hooks
        .fire(
            HookEvent::PreToolUse,
            &ctx.cwd,
            &HookInput {
                tool_name: Some(&name),
                tool_use_id: Some(&call_id),
                tool_input: Some(&args_value),
                ..Default::default()
            },
        )
        .await;
    if let Some(updated) = pre.updated_input {
        crate::agent::audit_fact(
            ctx,
            "PreToolUse.updatedInput",
            &format!("{name}: {args_value} → {updated}"),
            observer,
        )
        .await;
        args_value = updated;
    }
    observer.on_event(&LiveEvent::ToolStart {
        name: name.clone(),
        summary: call_summary(&name, &args_value),
        depth: ctx.depth,
        lane: ctx.lane,
        call_id: Some(call_id.clone()),
        args: args_value.clone(),
    });
    let t0 = std::time::Instant::now();
    let result = if let Some(reason) = pre.block_reason {
        crate::agent::audit_fact(
            ctx,
            "PreToolUse.block",
            &format!("{name}: {reason}"),
            observer,
        )
        .await;
        ToolResult {
            output: format!("blocked by hook: {reason}"),
            ok: false,
        }
    } else {
        let specifier = specifier_for(&name, &args_value);
        match crate::agent::gate_call(
            ctx,
            &name,
            &args_value,
            &specifier,
            pre.permission_decision,
            observer,
        )
        .await
        {
            Ok(()) => {
                // Same watchdog discipline as the turn loop: tools listed in
                // tool-timeouts.txt are abandoned past their budget.
                let args_json = args_value.to_string();
                let call_fut = ctx.tools.call(&name, &args_json, ctx);
                match tool_timeout_for(ctx, &name) {
                    Some(s) => match tokio::time::timeout(Duration::from_secs(s), call_fut).await {
                        Ok(r) => r,
                        Err(_) => ToolResult {
                            output: format!(
                                "tool {name} exceeded its {s}s timeout — raise or remove its row in .sunmao/tool-timeouts.txt"
                            ),
                            ok: false,
                        },
                    },
                    None => call_fut.await,
                }
            }
            Err(denial) => ToolResult {
                output: denial,
                ok: false,
            },
        }
    };
    observer.on_event(&LiveEvent::ToolDone {
        name: name.clone(),
        ok: result.ok,
        output: crate::agent::truncate_output(&result.output),
        depth: ctx.depth,
        lane: ctx.lane,
        call_id: Some(call_id.clone()),
        elapsed_ms: t0.elapsed().as_millis() as u64,
    });
    // durable audit fact — the log can answer "what did that script do"
    {
        let mut log = ctx.sessions.lock().await;
        log.append_audit(&SessionEvent::Hook {
            event: "ptc.tool".into(),
            detail: format!(
                "{name} {} {} → {}",
                if result.ok { "ok" } else { "failed" },
                call_summary(&name, &args_value),
                crate::agent::truncate_output(&result.output)
            ),
        })
        .await;
    }
    let post = ctx
        .hooks
        .fire(
            HookEvent::PostToolUse,
            &ctx.cwd,
            &HookInput {
                tool_name: Some(&name),
                tool_use_id: Some(&call_id),
                tool_input: Some(&args_value),
                tool_response: Some(&result.output),
                ..Default::default()
            },
        )
        .await;
    if !result.ok {
        crate::hooks::HookEngine::fire_detached(
            &ctx.hooks,
            HookEvent::PostToolUseFailure,
            &ctx.cwd,
            &HookInput {
                tool_name: Some(&name),
                tool_use_id: Some(&call_id),
                tool_input: Some(&args_value),
                tool_response: Some(&result.output),
                ..Default::default()
            },
        );
    }
    {
        let mut log = ctx.sessions.lock().await;
        for extra in post.extra_context {
            log.append(&SessionEvent::Message {
                message: sunmao_llm::types::Message::user(format!("[hook context] {extra}")),
            })
            .await
            .unwrap_or_else(|e| tracing::warn!("hook context append failed: {e:#}"));
        }
    }
    Ok(json!({"ok": result.ok, "output": result.output}).to_string())
}

#[cfg(test)]
mod tests;
