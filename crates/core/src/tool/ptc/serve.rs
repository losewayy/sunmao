//! The host half of the `__ptc` bridge: the request-serving loop, the
//! per-op dispatch, the doom-loop guard, and the one `tools.<Name>(args)`
//! call's full pipeline trip (PreToolUse → gate → run → PostToolUse).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use sunmao_llm::types::Tool;

use super::{PtcMsg, PtcObserver};
use crate::agent::{LiveEvent, Observer, call_summary, specifier_for, tool_timeout_for};
use crate::context::{Context, MutexRecover};
use crate::hooks::{HookEvent, HookInput};
use crate::session::SessionEvent;
use crate::tool::ToolResult;

/// The host side of `__ptc`: receive requests, dispatch them against the
/// session's real pipeline, keep `Promise.all`-era calls concurrent via
/// `FuturesUnordered`. Ends when the script's side drops its senders —
/// timeout/cancel belong to the spawned watchdog (a CPU-bound script never
/// calls into us, and this loop shares the blocking thread's executor: its
/// own select arms can't be polled while JS spins).
pub(super) async fn serve_requests(
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
    // nested calls render one depth in from the owning RunCode call —
    // live frontends indent them the way a replay does
    let nested_depth = ctx.depth.saturating_add(1);
    observer.on_event(&LiveEvent::ToolStart {
        name: name.clone(),
        summary: call_summary(&name, &args_value),
        depth: nested_depth,
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
        depth: nested_depth,
        lane: ctx.lane,
        call_id: Some(call_id.clone()),
        elapsed_ms: t0.elapsed().as_millis() as u64,
    });
    // durable audit fact — replays render the nested row, --dataflow and
    // --export-md can attribute the call, the message fold ignores it
    {
        let mut log = ctx.sessions.lock().await;
        log.append(&SessionEvent::PtcCall {
            call_id: call_id.clone(),
            name: name.clone(),
            args: args_value.to_string(),
            ok: result.ok,
            output: result.output.clone(),
            depth: ctx.depth,
            lane: ctx.lane,
        })
        .await
        .map_err(|e| format!("ptc audit append failed: {e:#}"))?;
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
