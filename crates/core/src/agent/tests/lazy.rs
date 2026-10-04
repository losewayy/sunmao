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
