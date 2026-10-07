//! Lazy tool surface: catalogs over `LAZY_ADVERTISE_AT` defer cold tools
//! behind `SearchTools` instead of shipping every schema per request.

use super::*;

struct FakeTool(&'static str);
#[async_trait::async_trait]
impl crate::tool::ToolImpl for FakeTool {
    fn name(&self) -> &'static str {
        self.0
    }
    fn decl(&self) -> sunmao_llm::types::Tool {
        sunmao_llm::types::Tool::function(self.0, "fake", serde_json::json!({}))
    }
    async fn call(
        &self,
        _a: serde_json::Value,
        _c: &std::sync::Arc<Context>,
    ) -> anyhow::Result<crate::tool::ToolResult> {
        Ok(crate::tool::ToolResult {
            exit_code: None,
            output: "ok".into(),
            ok: true,
        })
    }
}

fn ctx_with(registry: crate::tool::ToolRegistry) -> std::sync::Arc<Context> {
    let dir = crate::fresh_test_dir("lazy");
    std::sync::Arc::new(Context::new(
        std::sync::Arc::new(MockProvider {
            responses: std::sync::Mutex::new(Default::default()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        SessionLog::ephemeral(),
        registry,
        dir,
    ))
}

fn names(ctx: &Context) -> std::collections::BTreeSet<String> {
    ctx.advertised_tools()
        .into_iter()
        .map(|t| t.function.name)
        .collect()
}

#[test]
fn small_catalog_stays_eager() {
    // builtin-only registry sits under the lazy threshold — every decl
    // (minus the hidden pair) ships, exactly like before the lazy surface
    let ctx = ctx_with(builtin_registry());
    let n = names(&ctx);
    assert!(n.contains("WebFetch") && n.contains("HtmlArtifact"));
    // RunCode is a real standard-mode capability — the ptc driver owns the
    // "advertised nowhere else" carve-out, not the standard surface
    assert!(n.contains("RunCode"));
    assert!(!n.contains("SearchTools") && !n.contains("FusionExecute"));
}

#[tokio::test]
async fn fat_catalog_defers_and_searchtools_promotes() {
    let reg = builtin_registry();
    for i in 0..8 {
        // leak a 'static name — test registry lives for the test's span
        let n: &'static str = Box::leak(format!("mcp__srv__tool{i}").into_boxed_str());
        reg.register(FakeTool(n));
    }
    let ctx = ctx_with(reg);
    let n = names(&ctx);
    // hot set only: everyday tools + SearchTools; cold decls deferred
    for hot in [
        "Read",
        "Write",
        "Edit",
        "Bash",
        "Glob",
        "Grep",
        "Task",
        "SearchTools",
        // RunCode must NOT fall off the wire when the catalog grows — the
        // eager surface already ships it, so the lazy one must too
        "RunCode",
    ] {
        assert!(n.contains(hot), "hot tool {hot} must stay advertised");
    }
    assert!(!n.contains("WebFetch"), "cold builtin must defer");
    assert!(!n.contains("mcp__srv__tool0"), "cold mcp tool must defer");

    // SearchTools returns the full schema for a deferred tool and promotes
    // it into the advertised set on the next request
    let res = ctx
        .tools
        .call("SearchTools", r#"{"query":"tool3"}"#, &ctx)
        .await;
    assert!(res.ok && res.output.contains("mcp__srv__tool3"));
    let after = names(&ctx);
    assert!(after.contains("mcp__srv__tool3"), "searched tool promotes");

    // detail levels shape the payload
    let res = ctx
        .tools
        .call("SearchTools", r#"{"query":"tool","detail":"names"}"#, &ctx)
        .await;
    assert!(res.output.contains("mcp__srv__tool4") && !res.output.contains("parameters"));
}

#[test]
fn crossing_the_threshold_never_drops_an_advertised_tool() {
    // the failure shape that motivated advertised_pins: a session starts
    // under the threshold with an MCP tool visible, the catalog then grows
    // past it (second server connects mid-turn) — the tool must NOT vanish
    let ctx = ctx_with(builtin_registry());
    ctx.tools.register(FakeTool("mcp__srv__visible"));
    assert!(names(&ctx).contains("mcp__srv__visible"));

    for i in 0..10 {
        let n: &'static str = Box::leak(format!("mcp__srv2__tool{i}").into_boxed_str());
        ctx.tools.register(FakeTool(n));
    }
    let after = names(&ctx);
    assert!(
        after.contains("mcp__srv__visible"),
        "a once-advertised tool must survive the eager→lazy flip"
    );
    // but a tool never advertised still defers — the lazy budget holds
    assert!(!after.contains("mcp__srv2__tool0"));
    // and growth is monotonic: the pinned set can only widen
    let before: Vec<_> = names(&ctx).into_iter().collect();
    ctx.promoted_tools
        .lock_or_recover()
        .insert("mcp__srv2__tool7".to_string());
    let later = names(&ctx);
    assert!(before.iter().all(|n| later.contains(n)) && later.contains("mcp__srv2__tool7"));
}

#[tokio::test]
async fn tool_surface_persists_and_reseeds_across_restart() {
    // the restart half of the monotonic promise: pins live in memory, so
    // a fresh Context over the same session log must come back seeing
    // every name the model saw before the restart
    let dir = crate::fresh_test_dir("surface-persist");
    let log = SessionLog::open(&dir, "s1").await.unwrap();
    let reg = builtin_registry();
    reg.register(FakeTool("mcp__srv__visible"));
    let ctx = std::sync::Arc::new(Context::new(
        std::sync::Arc::new(MockProvider {
            responses: std::sync::Mutex::new(Default::default()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        log,
        reg,
        dir.clone(),
    ));
    assert!(names(&ctx).contains("mcp__srv__visible"));
    ctx.persist_tool_surface().await;

    // restart: brand-new context, same session id, catalog now fat enough
    // to flip lazy — the reseeded pin must keep the tool on the surface
    let log2 = SessionLog::open(&dir, "s1").await.unwrap();
    let events = log2.events().await.unwrap();
    let reg2 = builtin_registry();
    reg2.register(FakeTool("mcp__srv__visible"));
    for i in 0..10 {
        let n: &'static str = Box::leak(format!("mcp__srv2__tool{i}").into_boxed_str());
        reg2.register(FakeTool(n));
    }
    let ctx2 = ctx_with(reg2);
    ctx2.reseed_tool_surface(&events);
    let after = names(&ctx2);
    assert!(
        after.contains("mcp__srv__visible"),
        "restart-shrink: a once-advertised tool must survive resume"
    );
    assert!(
        !after.contains("mcp__srv2__tool0"),
        "a tool never advertised still defers — the lazy budget holds"
    );

    // an unchanged surface appends nothing — no one-line-per-request spam
    ctx.persist_tool_surface().await;
    let count = ctx
        .sessions
        .lock()
        .await
        .events()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|e| {
            serde_json::to_value(e).unwrap()["type"].as_str() == Some("tool_surface")
        })
        .count();
    assert_eq!(count, 1, "persist diffs; it does not append per request");
}
