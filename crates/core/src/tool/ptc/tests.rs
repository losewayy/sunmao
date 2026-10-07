// arch: allow-god-file one sandbox-behavior suite — splitting mid-flow
// would scatter tests that share the run() harness and session fixtures
use super::*;
use crate::session::{SessionEvent, SessionLog};
use crate::tool::builtin_registry;
use sunmao_llm::ProviderAdapter;

struct StubLlm;

#[async_trait::async_trait]
impl ProviderAdapter for StubLlm {
    async fn stream(
        &self,
        _req: sunmao_llm::ChatRequest<'_>,
    ) -> anyhow::Result<sunmao_llm::DeltaStream> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
}

fn test_ctx(dir: &std::path::Path) -> Arc<Context> {
    Arc::new(Context::new(
        Arc::new(StubLlm),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.to_path_buf(),
    ))
}

/// An approver that always says no — the gate's deny path for a nested
/// call without touching the interactive card machinery.
struct DenyAll;

#[async_trait::async_trait]
impl crate::approval::Approver for DenyAll {
    async fn approve(&self, _tool: &str, _detail: &str, _why: &str) -> crate::approval::Approval {
        crate::approval::Approval::Deny { reason: None }
    }
}

async fn run(ctx: &Arc<Context>, code: &str) -> ToolResult {
    ctx.tools
        .call(
            "RunCode",
            &json!({"code": code, "timeout_ms": 5000}).to_string(),
            ctx,
        )
        .await
}

/// Top-level `await`/`return` — the spellings the Script goal can't take —
/// work through the async-wrapper fallback: `await expr` keeps its
/// completion value, `const r = await …; return r` runs as statements.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_top_level_await_and_return() {
    let dir = crate::fresh_test_dir("ptc-tla");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("t.txt"), "tla").unwrap();
    let ctx = test_ctx(&dir);
    let res = run(&ctx, "await 1").await;
    assert_eq!(res.output, "1", "{}", res.output);
    let res = run(
        &ctx,
        r#"const r = await tools.Read({path: "t.txt"}); return r.output"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("tla"), "{}", res.output);
    let res = run(&ctx, "return typeof tools").await;
    assert_eq!(res.output, "\"object\"", "{}", res.output);
    let res = run(&ctx, "await tools.Read({path: \"t.txt\"})").await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("\"ok\":true"), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// The wrapper fallback must never re-run a script that already reached a
/// tool call: `tools.X(); eval("(")` throws a runtime SyntaxError AFTER the
/// invocation. A retry would produce a wrapped "script rejected" — the
/// un-retried original failure keeps the plain "eval error:" shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_no_retry_after_side_effects() {
    let dir = crate::fresh_test_dir("ptc-noretry");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = test_ctx(&dir);
    let res = run(&ctx, r#"tools.Glob({pattern: "x"}); eval("(");"#).await;
    assert!(!res.ok, "{}", res.output);
    assert!(
        res.output.starts_with("eval error:"),
        "a retried run rejects inside the wrapper instead: {}",
        res.output
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A plain parse failure reaches the model as a legible line — engine
/// message + position — not the Debug dump of the CaughtError.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_syntax_error_is_legible() {
    let dir = crate::fresh_test_dir("ptc-syn");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = test_ctx(&dir);
    let res = run(&ctx, "const x = ;").await;
    assert!(!res.ok);
    assert!(
        res.output.contains("unexpected token") || res.output.contains("expecting"),
        "{}",
        res.output
    );
    assert!(res.output.contains("eval_script:1:"), "{}", res.output);
    assert!(!res.output.contains("Some("), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// A script that awaits one tool call end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_single_call() {
    let dir = crate::fresh_test_dir("ptc1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "hello ptc").unwrap();
    let ctx = test_ctx(&dir);
    let res = run(&ctx, r#"tools.Read({path: "a.txt"}).then(r => r.output)"#).await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("hello ptc"), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// Promise.all fans out: both tool calls resolve through the bridge —
/// real-concurrency is proven by the smoke test upstream; here we assert
/// the fan-out shape works against real tools.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_parallel_fanout() {
    let dir = crate::fresh_test_dir("ptc2");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a1.txt"), "a").unwrap();
    std::fs::write(dir.join("b1.txt"), "b").unwrap();
    let ctx = test_ctx(&dir);
    let res = run(
        &ctx,
        r#"Promise.all([
            tools.Glob({pattern: "a*.txt"}),
            tools.Glob({pattern: "b*.txt"}),
        ]).then(rs => rs.map(r => r.output.trim()).join("|"))"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("a1"), "{}", res.output);
    assert!(res.output.contains("b1"), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// A thrown JS error settles as a failed ToolResult the model can read —
/// never an Err that aborts the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_script_error() {
    let dir = crate::fresh_test_dir("ptc3");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = test_ctx(&dir);
    let res = run(&ctx, "throw new Error('boom')").await;
    assert!(!res.ok);
    assert!(res.output.contains("boom"), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// A nested call denied at the approval gate comes back to the script as
/// `{ok:false}` — the sandbox can route around refusals, not crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_denied_subcall() {
    let dir = crate::fresh_test_dir("ptc4");
    std::fs::create_dir_all(&dir).unwrap();
    let mut inner = Context::new(
        Arc::new(StubLlm),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    inner.approval = Arc::new(DenyAll);
    let ctx = Arc::new(inner);
    let res = run(
        &ctx,
        r#"tools.Bash({command: "rm -rf x"}).then(r => r.ok + ":" + r.output)"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("false"), "{}", res.output);
    assert!(res.output.contains("denied"), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// A CPU-bound infinite loop dies on the interrupt watchdog, not on the
/// host process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_timeout_kills_loop() {
    let dir = crate::fresh_test_dir("ptc5");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = test_ctx(&dir);
    let t0 = std::time::Instant::now();
    let res = ctx
        .tools
        .call(
            "RunCode",
            &json!({"code": "while(true){}", "timeout_ms": 1200}).to_string(),
            &ctx,
        )
        .await;
    assert!(!res.ok);
    assert!(res.output.contains("budget"), "{}", res.output);
    assert!(t0.elapsed() < Duration::from_secs(5));
    std::fs::remove_dir_all(&dir).ok();
}

/// store() lands a durable PtcStore fact; a reopened log reseeds the
/// snapshot so a later RunCode load() sees it — replay persistence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_store_persists_across_resume() {
    let dir = crate::fresh_test_dir("ptc6");
    std::fs::create_dir_all(&dir).unwrap();
    let sdir = dir.join("sess");
    let log = SessionLog::open(&sdir, "s1").await.unwrap();
    let ctx = Arc::new(Context::new(
        Arc::new(StubLlm),
        log,
        builtin_registry(),
        dir.clone(),
    ));
    let res = run(
        &ctx,
        r#"store("k", {n: 41}).then(() => load("k")).then(v => v.n)"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("41"), "{}", res.output);
    drop(ctx);
    let log2 = SessionLog::open(&sdir, "s1").await.unwrap();
    let ctx2 = Arc::new(Context::new(
        Arc::new(StubLlm),
        log2,
        builtin_registry(),
        dir.clone(),
    ));
    let res = run(&ctx2, r#"load("k").then(v => v && v.n)"#).await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("41"), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// The sandbox exposes no ambient authority: import/require/fetch are
/// absent, and tools.* only carries real registered tools (no RunCode).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_sandbox_surface() {
    let dir = crate::fresh_test_dir("ptc7");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = test_ctx(&dir);
    let res = run(
        &ctx,
        r#"(() => JSON.stringify({
            req: typeof require, imp: typeof fetch, proc: typeof process,
            self: typeof tools.RunCode, glob: typeof tools.Glob,
        }))()"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    assert!(
        res.output.contains("\\\"req\\\":\\\"undefined\\\""),
        "{}",
        res.output
    );
    assert!(
        res.output.contains("\\\"self\\\":\\\"undefined\\\""),
        "{}",
        res.output
    );
    assert!(
        res.output.contains("\\\"glob\\\":\\\"function\\\""),
        "{}",
        res.output
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A nested call lands a durable PtcCall fact — replay-visible, carrying
/// args so summaries/dataflow can be re-derived — and stays OUT of the
/// message fold (no orphan ToolResult in the provider transcript).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_nested_calls_are_durable_ptc_facts() {
    let dir = crate::fresh_test_dir("ptc8");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "hi").unwrap();
    let ctx = test_ctx(&dir);
    let res = run(&ctx, r#"tools.Read({path: "a.txt"}).then(r => r.ok)"#).await;
    assert!(res.ok, "{}", res.output);
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    let ptc: Vec<_> = evs
        .iter()
        .filter(|e| matches!(e, SessionEvent::PtcCall { name, .. } if name == "Read"))
        .collect();
    assert_eq!(ptc.len(), 1, "one nested call, one fact: {evs:?}");
    // the fold must not fabricate a protocol tool_result for the script's
    // call — the transcript would carry a tool message with no tool_use
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    assert!(
        !msgs.iter().any(|m| m.role == sunmao_llm::types::Role::Tool),
        "nested calls never fold into protocol messages: {msgs:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The script-side doom-loop guard: the same (tool, args) for the third
/// call in a row resolves {ok:false} with a legible reason — a runaway
/// `while` over a static call dies instead of burning the budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_doom_loop_refuses_identical_streak() {
    let dir = crate::fresh_test_dir("ptc9");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = test_ctx(&dir);
    let res = run(
        &ctx,
        r#"(async () => {
            const r = [];
            for (let i = 0; i < 5; i++) r.push((await tools.Glob({pattern: "x"})).ok);
            return r.join(",");
        })()"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    // first two calls ran, calls 3..5 refused by the guard
    assert_eq!(
        res.output, "\"true,true,false,false,false\"",
        "{}",
        res.output
    );
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    let refused = evs
        .iter()
        .filter(|e| {
            matches!(
                e,
                SessionEvent::PtcCall { ok: false, output, .. } if output.contains("doom-loop")
            )
        })
        .count();
    assert_eq!(refused, 3, "each refused call is a durable fact: {evs:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A different call interleaved between repeats resets the streak —
/// `glob(x) → glob(y) → glob(x)` is real control flow, not a stuck loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_doom_loop_resets_on_varied_calls() {
    let dir = crate::fresh_test_dir("ptc10");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = test_ctx(&dir);
    let res = run(
        &ctx,
        r#"(async () => {
            const r = [];
            for (let i = 0; i < 5; i++) {
                r.push((await tools.Glob({pattern: "x"})).ok);
                r.push((await tools.Glob({pattern: "y" + i})).ok);
            }
            return r.join(",");
        })()"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    assert_eq!(
        res.output, "\"true,true,true,true,true,true,true,true,true,true\"",
        "{}",
        res.output
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `SearchTools` as the model emits it: a query returns matching tools'
/// full declarations — name + parameters schema — and MCP/extension
/// (`mcp__*`) entries are discoverable from the same catalog.
#[tokio::test]
async fn search_tools_returns_matching_schemas() {
    let dir = crate::fresh_test_dir("st1");
    std::fs::create_dir_all(&dir).unwrap();
    struct FakeMcp;
    #[async_trait::async_trait]
    impl ToolImpl for FakeMcp {
        fn name(&self) -> &'static str {
            "mcp__docs__lookup"
        }
        fn decl(&self) -> Tool {
            Tool::function(
                "mcp__docs__lookup",
                "look up documentation",
                json!({"type": "object"}),
            )
        }
        async fn call(&self, _a: Value, _c: &Arc<Context>) -> anyhow::Result<ToolResult> {
            anyhow::bail!("unused")
        }
    }
    let reg = builtin_registry();
    reg.register(FakeMcp);
    let ctx = Arc::new(Context::new(
        Arc::new(StubLlm),
        SessionLog::ephemeral(),
        reg,
        dir.clone(),
    ));
    let res = ctx
        .tools
        .call("SearchTools", &json!({"query": "grep"}).to_string(), &ctx)
        .await;
    assert!(res.ok, "{}", res.output);
    let hits: Vec<Value> = serde_json::from_str(&res.output).unwrap();
    assert_eq!(hits.len(), 1, "only Grep matches 'grep': {hits:?}");
    assert_eq!(hits[0]["function"]["name"], "Grep");
    assert!(
        hits[0]["function"]["parameters"].is_object(),
        "the hit must carry the full schema a script needs: {hits:?}"
    );
    // every term must match — 'glob pattern' still lands Glob
    let res = ctx
        .tools
        .call(
            "SearchTools",
            &json!({"query": "glob pattern"}).to_string(),
            &ctx,
        )
        .await;
    let hits: Vec<Value> = serde_json::from_str(&res.output).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["function"]["name"], "Glob");
    // MCP tools are in the same catalog
    let res = ctx
        .tools
        .call(
            "SearchTools",
            &json!({"query": "documentation"}).to_string(),
            &ctx,
        )
        .await;
    let hits: Vec<Value> = serde_json::from_str(&res.output).unwrap();
    assert!(
        hits.iter()
            .any(|h| h["function"]["name"] == "mcp__docs__lookup"),
        "mcp__* tools must be searchable: {hits:?}"
    );
    // empty query lists the catalog — RunCode withheld (not script-callable)
    let res = ctx
        .tools
        .call("SearchTools", "{}".to_string().as_str(), &ctx)
        .await;
    let hits: Vec<Value> = serde_json::from_str(&res.output).unwrap();
    assert!(
        hits.iter().all(|h| h["function"]["name"] != "RunCode"),
        "RunCode is never a borrowed tool: {hits:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The borrowed surface inside the sandbox: `tools.SearchTools` resolves
/// through the real bridge (a durable PtcCall fact), `describe()` still
/// lists the catalog, and a tool found by search is callable under its
/// discovered name — the whole borrow loop in one script.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_searchtools_and_describe() {
    let dir = crate::fresh_test_dir("ptc-st");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hit.rs"), "fn main() {}").unwrap();
    let ctx = test_ctx(&dir);
    let res = run(
        &ctx,
        r#"(async () => {
            const found = JSON.parse((await tools.SearchTools({query: "glob"})).output);
            const one = await describe("Glob");
            // found[0].function.name IS a callable tools.* name — search is
            // discovery of the same catalog the bridge dispatches
            const hit = await tools[found[0].function.name]({pattern: "*.rs"});
            return found.length + ":" + one[0].function.name + ":" + hit.output.trim();
        })()"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    assert_eq!(res.output, "\"1:Glob:hit.rs\"", "{}", res.output);
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    assert!(
        evs.iter().any(|e| matches!(
            e,
            SessionEvent::PtcCall { name, .. } if name == "SearchTools"
        )),
        "a nested SearchTools call is the same auditable fact as any tools.* call: {evs:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A call the script starts but never awaits (fire-and-forget, or a
/// `Promise.all` whose join was dropped) still lands its durable PtcCall
/// fact — the host bridge drains inflight requests after the script
/// returns instead of dropping them mid-dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_unawaited_call_is_drained_into_the_log() {
    let dir = crate::fresh_test_dir("ptc-drain");
    std::fs::create_dir_all(&dir).unwrap();
    struct SlowMark;
    #[async_trait::async_trait]
    impl ToolImpl for SlowMark {
        fn name(&self) -> &'static str {
            "SlowMark"
        }
        fn decl(&self) -> Tool {
            Tool::function("SlowMark", "test tool", json!({}))
        }
        async fn call(&self, _a: Value, _c: &Arc<Context>) -> anyhow::Result<ToolResult> {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(ToolResult {
                exit_code: None,
                output: "slow-ok".into(),
                ok: true,
            })
        }
    }
    let reg = builtin_registry();
    reg.register(SlowMark);
    let ctx = Arc::new(Context::new(
        Arc::new(StubLlm),
        SessionLog::ephemeral(),
        reg,
        dir.clone(),
    ));
    // the script returns while SlowMark is still in flight — the select
    // resolves on the script and the OLD code dropped the dispatch here.
    // (the `await` tick pumps the __ptc send before the script settles)
    let res = run(
        &ctx,
        r#"(async () => { tools.SlowMark({}); await Promise.resolve(); return "done"; })()"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    let ptc: Vec<_> = evs
        .iter()
        .filter(|e| matches!(e, SessionEvent::PtcCall { name, .. } if name == "SlowMark"))
        .collect();
    assert_eq!(
        ptc.len(),
        1,
        "the unawaited call must land its fact: {evs:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// console.log collects in-sandbox and rides back appended to the result —
/// on a rejected script the lines up to the throw still surface, which is
/// the whole point of having a debug channel at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_console_log_round_trip() {
    let dir = crate::fresh_test_dir("ptc-console");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = test_ctx(&dir);
    let res = run(
        &ctx,
        r#"console.log("a", 1, {x: 2}); console.warn("w"); return 7"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.starts_with("7"), "{}", res.output);
    assert!(res.output.contains("[console]"), "{}", res.output);
    assert!(res.output.contains("a 1 {\"x\":2}"), "{}", res.output);
    assert!(res.output.contains("[warn] w"), "{}", res.output);
    // a rejected script still yields the lines it printed
    let res = run(&ctx, r#"console.log("before"); throw new Error("boom")"#).await;
    assert!(!res.ok);
    assert!(res.output.contains("boom"), "{}", res.output);
    assert!(res.output.contains("[console]"), "{}", res.output);
    assert!(res.output.contains("before"), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// The bridge surfaces a real `exit_code` on shell results — a script can
/// retry on it without scraping `[exit code N]` out of the output text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_tool_reply_carries_exit_code() {
    let dir = crate::fresh_test_dir("ptc-exit");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = test_ctx(&dir);
    let res = run(
        &ctx,
        r#"const r = await tools.Bash({command: "exit 7"}); return r.exit_code"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    assert_eq!(res.output, "7", "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// Lossy values surface as `warn` on the store reply instead of silently
/// hollowing out; `load` distinguishes a never-set key (undefined) from a
/// stored null — the old round-trip collapsed both into null.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runcode_store_warns_and_load_marks_found() {
    let dir = crate::fresh_test_dir("ptc-store");
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = test_ctx(&dir);
    let res = run(
        &ctx,
        r#"const r = await store("k", {s: new Set([1]), n: NaN, b: 5n});
           return [r.warn || "NO_WARN", typeof await load("nope"), typeof await load("k")]"#,
    )
    .await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("Set"), "{}", res.output);
    assert!(res.output.contains("NaN"), "{}", res.output);
    assert!(res.output.contains("BigInt"), "{}", res.output);
    assert!(res.output.contains("undefined"), "{}", res.output);
    std::fs::remove_dir_all(&dir).ok();
}
