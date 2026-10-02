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
