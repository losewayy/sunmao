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
//!
//! `SearchTools` is the discovery half of the pair. Under the `ptc` loop
//! driver the model's schema budget holds `RunCode` + `SearchTools` alone;
//! a search hit returns the tool's full declaration (name, description,
//! parameters) so the next script knows the exact arg shape to emit.
//! Discovery only — the `tools.*` bridge already reaches every registered
//! tool, so a match is information, not authorization.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use sunmao_llm::types::Tool;

use crate::agent::{LiveEvent, Observer};
use crate::context::{Context, MutexRecover};
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
             the only capabilities. The script's completion value is the result — end with \
             an expression; top-level `await`/`return` also work (a script that can't parse \
             plainly retries inside an async wrapper).",
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
                exit_code: None,
                output: text,
                ok: true,
            },
            Ok(Err(e)) => ToolResult {
                exit_code: None,
                output: e,
                ok: false,
            },
            Err(e) => ToolResult {
                exit_code: None,
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

/// Compile+run `code`. `eval` uses the Script goal — top-level `await`
/// and `return`, codemode's most natural spellings, are syntax errors
/// there, so a parse failure falls back inside an async wrapper: the
/// expression form first (`await expr` keeps its completion value), then
/// the statement form (`return` decides the result). A reported error
/// always describes the ORIGINAL source, never the wrapped retry's
/// shifted positions.
fn eval_code<'js>(
    jctx: &rquickjs::Ctx<'js>,
    code: &str,
) -> Result<rquickjs::promise::MaybePromise<'js>, String> {
    let first = match jctx.eval(code) {
        Ok(p) => return Ok(p),
        Err(_) => {
            // catch() CLEARS the pending exception — read it once and keep
            // the value; a second call sees nothing.
            let caught = jctx.catch();
            let desc = caught_desc(&caught, jctx);
            // Retry gate: a SyntaxError thrown before ANY tools.* call —
            // parse errors execute nothing. `__calls` counts invocations
            // in JS (the send inside __ptc polls later, so a Rust-side
            // counter can't see a queued call in time).
            let syntax = matches!(
                caught
                    .as_exception()
                    .and_then(|ex| ex
                        .as_object()
                        .get::<_, Option<String>>("name")
                        .ok()
                        .flatten())
                    .as_deref(),
                Some("SyntaxError")
            );
            let invoked = jctx.eval::<i32, _>("__calls").unwrap_or(1);
            if !syntax || invoked > 0 {
                return Err(format!("eval error: {desc}"));
            }
            desc
        }
    };
    if let Ok(p) = jctx.eval::<rquickjs::promise::MaybePromise, _>(format!(
        "(async () => {{ return (\n{code}\n); }})()"
    )) {
        return Ok(p);
    }
    jctx.eval::<rquickjs::promise::MaybePromise, _>(format!("(async () => {{\n{code}\n}})()"))
        .map_err(|_| format!("eval error: {first}"))
}

/// The caught JS error the way a model can act on: the engine's own
/// message plus its first stack line (`eval_script:L:C`), without the
/// `{:?}` Debug dump (`Exception { message: Some(…) }`) it used to carry.
fn caught_desc<'js>(v: &rquickjs::Value<'js>, jctx: &rquickjs::Ctx<'js>) -> String {
    if let Some(e) = v.as_exception() {
        let msg = e.message().unwrap_or_else(|| "unknown exception".into());
        return match e
            .stack()
            .and_then(|s| s.lines().next().map(str::trim).map(str::to_string))
        {
            Some(loc) if !loc.is_empty() => format!("{msg} ({})", loc.trim_start_matches("at ")),
            _ => msg,
        };
    }
    // a thrown non-Error (`throw "boom"`) carries no stack — its JSON is
    // still the best description the model can get
    jctx.json_stringify(v)
        .ok()
        .flatten()
        .and_then(|s| s.to_string().ok())
        .unwrap_or_else(|| "unknown error".into())
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
            let cancel = ctx.cancel_signal();
            tokio::spawn(async move {
                // `wait` carries the flag, so a cancel that landed before
                // this task was scheduled still stops the script
                tokio::select! {
                    () = tokio::time::sleep(timeout) => reason.store(1, Ordering::Relaxed),
                    () = cancel.wait() => reason.store(2, Ordering::Relaxed),
                }
                stop.store(true, Ordering::Relaxed);
            })
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<PtcMsg>();
        let observer = PtcObserver(ctx.live_sink.get().cloned());
        let serve = serve_requests(&ctx, rx, &observer);
        tokio::pin!(serve);
        let code = code.to_string();
        let cancel = ctx.cancel_signal();
        let res = if cancel.is_cancelled() {
            // the cancel beat our waiter registration — a bare notified()
            // would wait forever on a wake that already happened
            Err("cancelled by user".into())
        } else {
            tokio::select! {
                r = jctx.async_with(async |jctx| -> Result<String, String> {
                    install_surface(&jctx, &ctx, tx).map_err(|e| format!("install: {e}"))?;
                    // console.log rides back appended to whatever the script
                    // produced — on error paths it is often the whole reason
                    // the run can be diagnosed at all
                    let take_logs = |jctx: &rquickjs::Ctx| -> String {
                        jctx.eval::<rquickjs::String, _>("__logs.join('\\n')")
                            .ok()
                            .and_then(|s| s.to_string().ok())
                            .unwrap_or_default()
                    };
                    let p = match eval_code(&jctx, &code) {
                        Ok(p) => p,
                        Err(e) => {
                            let logs = take_logs(&jctx);
                            return Err(if logs.is_empty() { e } else { format!("{e}\n[console]\n{logs}") });
                        }
                    };
                    let v = match p.into_future::<rquickjs::Value>().await {
                        Ok(v) => v,
                        Err(_) => {
                            let mut e = format!("script rejected: {}", caught_desc(&jctx.catch(), &jctx));
                            let logs = take_logs(&jctx);
                            if !logs.is_empty() { e += &format!("\n[console]\n{logs}"); }
                            return Err(e);
                        }
                    };
                    let mut out = jctx.json_stringify(&v)
                        .map_err(|e| e.to_string())?
                        .map(|s| s.to_string().unwrap_or_default())
                        .ok_or_else(|| "script returned a non-JSON-serializable value".to_string())?;
                    let logs = take_logs(&jctx);
                    if !logs.is_empty() { out += &format!("\n\n[console]\n{logs}"); }
                    Ok(out)
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
                () = cancel.wait() => Err("cancelled by user".into()),
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

/// The script-callable catalog — every declaration `tools.<Name>` can
/// reach. `RunCode` is withheld (a sandboxed script must not spawn nested
/// sandboxes); `SearchTools` stays — `tools.SearchTools` is a legitimate
/// discovery call, and MCP/extension tools land here by registration.
fn callable_catalog(ctx: &Context) -> Vec<Tool> {
    ctx.tools
        .declarations()
        .into_iter()
        .filter(|t| t.function.name != "RunCode")
        .collect()
}

/// Term-wise AND match over a tool's name + description, case-insensitive.
/// An empty/whitespace query returns the whole catalog — the same "list
/// everything" fallback `describe()` gives.
fn matching_tools(decls: Vec<Tool>, query: &str) -> Vec<Tool> {
    let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    if terms.is_empty() {
        return decls;
    }
    decls
        .into_iter()
        .filter(|t| {
            let hay = format!("{}\n{}", t.function.name, t.function.description).to_lowercase();
            terms.iter().all(|term| hay.contains(term))
        })
        .collect()
}

/// `SearchTools` — catalog lookup for the borrowed-tools surface. The
/// model asks for "something that greps" and gets `Grep`'s full schema
/// back instead of guessing; the call is a model-emitted tool like any
/// other, so its `ToolResult` IS the durable record a replay reads.
pub struct SearchToolsTool;

#[async_trait::async_trait]
impl ToolImpl for SearchToolsTool {
    fn name(&self) -> &'static str {
        "SearchTools"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "SearchTools",
            "Search the session's tool catalog — returns the declarations of \
             every tool whose name or description matches the query terms, \
             including MCP (`mcp__*`) and extension (`ext__*`) tools, and any \
             tool not currently in your advertised set (deferred tools are \
             callable — call them by name once found). `detail` picks the \
             payload: `names` (a bare name list), `desc` (name + \
             description), `schema` (full declarations — the default). An \
             empty or omitted query lists the whole catalog. Discovery only \
             — a match is callable information, not a permission grant.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Case-insensitive terms; a tool matches when every term appears in its name or description. Empty lists all."},
                    "detail": {"type": "string", "enum": ["names", "desc", "schema"], "description": "Payload detail — `names` for a compact list, `desc` for name+description, `schema` (default) for full declarations"}
                }
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &Arc<Context>) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            query: Option<String>,
            detail: Option<String>,
        }
        let a: Args = serde_json::from_value(args).unwrap_or(Args {
            query: None,
            detail: None,
        });
        let hits = matching_tools(callable_catalog(ctx), a.query.as_deref().unwrap_or(""));
        // a surfaced tool joins the advertised set on later requests — the
        // lazy surface (catalog > LAZY_ADVERTISE_AT) promotes what the model
        // actually went looking for instead of keeping the whole catalog hot
        ctx.promoted_tools
            .lock_or_recover()
            .extend(hits.iter().map(|t| t.function.name.clone()));
        let out = match a.detail.as_deref().unwrap_or("schema") {
            "names" => {
                serde_json::to_string(&hits.iter().map(|t| &t.function.name).collect::<Vec<_>>())
            }
            "desc" => serde_json::to_string(
                &hits
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.function.name,
                            "description": t.function.description,
                        })
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => serde_json::to_string(&hits),
        }
        .unwrap_or_else(|_| "[]".into());
        Ok(ToolResult {
            exit_code: None,
            output: out,
            ok: true,
        })
    }
}

mod install;
mod serve;

use install::install as install_surface;
use serve::serve_requests;

#[cfg(test)]
mod tests;
