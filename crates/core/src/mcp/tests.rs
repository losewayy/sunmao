//! Live tests for the MCP client — stdio fixture roundtrips, crash
//! tolerance and the MCP Apps (SEP-1865) host surface. The fixture
//! binary is `tests/fixtures/mcp_server.rs` compiled once per run.

use super::*;

use serde_json::json;

/// Compile `tests/fixtures/mcp_server.rs` once; live tests reuse the
/// binary (`--die` selects the mid-session crash path). None when
/// rustc is absent — the test degrades to a skip.
fn fixture_bin() -> Option<std::path::PathBuf> {
    static BIN: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    BIN.get_or_init(|| crate::compile_fixture("mcp_server.rs", "sunmao-mcp-echo"))
        .clone()
}

/// A bad server entry must not brick the session — `connect_all`
/// warns and returns the working tools only.
#[tokio::test]
async fn connect_all_degrades_a_dead_server() {
    let dir = crate::fresh_test_dir("mcp-bad");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/mcp.json"),
        r#"{"mcpServers":{"ghost":{"command":"sunmao-no-such-binary-zz","args":[]}}}"#,
    )
    .unwrap();
    let conn = connect_all(&dir, &[]).await;
    assert!(conn.tools.is_empty(), "dead server contributes no tools");
    std::fs::remove_dir_all(&dir).ok();
}

/// Live roundtrip + crash tolerance: a real stdio server lists `ping`,
/// the call echoes back — then the `--die` variant exits right after
/// `tools/list`, and the next `tools/call` must degrade to a failed
/// ToolResult, never abort the loop (v0.2 acceptance: 子进程崩溃不炸
/// agent — this is the MCP half of that bar).
#[tokio::test]
async fn mcp_call_survives_then_fails_after_child_death() {
    let Some(bin) = fixture_bin() else {
        eprintln!("rustc not found — skipping live MCP test");
        return;
    };
    let ctx = crate::context::Context::new(
        std::sync::Arc::new(StubLlm),
        crate::session::SessionLog::ephemeral(),
        crate::tool::ToolRegistry::new(),
        std::env::temp_dir(),
    );

    // healthy path: real initialize → tools/list → tools/call
    let spec = ServerSpec {
        command: Some(bin.to_string_lossy().to_string()),
        args: vec![],
        env: Default::default(),
        url: None,
    };
    let (handle, tools) = connect_one("echo", &spec).await.unwrap();
    assert_eq!(tools.len(), 3, "model registry hides the app-only tool");
    assert_eq!(tools[0].name(), "mcp__echo__ping");
    assert_eq!(handle.tools.len(), 4, "catalog keeps every listed tool");
    let res = tools[0].call(json!({"msg": "hi"}), &ctx).await.unwrap();
    assert!(res.ok);
    assert!(res.output.contains("pong[ping]: hi"), "{}", res.output);

    // crash path: server exits after tools/list — connect succeeds,
    // the first call must error out, not hang or panic
    let dying = ServerSpec {
        command: Some(bin.to_string_lossy().to_string()),
        args: vec!["--die".into()],
        env: Default::default(),
        url: None,
    };
    let (_h, tools) = connect_one("die", &dying).await.unwrap();
    assert_eq!(tools.len(), 3, "listed before death");
    // the call must resolve to an error — never hang, never panic
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tools[0].call(json!({"msg": "x"}), &ctx),
    )
    .await
    {
        Err(_) => panic!("call against a dead child hung"),
        Ok(Err(e)) => {
            assert!(format!("{e:#}").contains("fail"), "{e:#}")
        }
        Ok(Ok(res)) => assert!(!res.ok, "dead server must not report ok"),
    }
}

/// MCP Apps (SEP-1865) live run against the fixture server:
///   - `draw` carries `_meta.ui.resourceUri` — calling it lands the
///     `ui://` resource as `artifacts/mcp-echo-draw.html` + a `.ui.json`
///     sidecar, and emits a versioned Artifact event
///   - `internal` (visibility `["app"]`) is absent from the model-facing
///     registry but callable through the island bridge — which runs the
///     same dispatch gate (read_only refuses it; a model-only tool is
///     refused whatever the mode)
///   - `mcp_resource_read` proxies `resources/read` for the island
#[tokio::test]
async fn mcp_apps_lands_artifact_and_bridges_visibility() {
    use crate::agent::{AgentLoop, ApprovalMode, LiveEvent, Observer};
    struct Null;
    impl Observer for Null {
        fn on_event(&self, _ev: &LiveEvent) {}
    }
    let Some(bin) = fixture_bin() else {
        eprintln!("rustc not found — skipping live MCP apps test");
        return;
    };
    let dir = crate::fresh_test_dir("mcp-apps");
    let spec = ServerSpec {
        command: Some(bin.to_string_lossy().to_string()),
        args: vec![],
        env: Default::default(),
        url: None,
    };
    let (handle, tools) = connect_one("echo", &spec).await.unwrap();

    // catalog/visibility shape
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        [
            "mcp__echo__ping",
            "mcp__echo__draw",
            "mcp__echo__model_only"
        ],
        "app-only tools stay out of the model registry"
    );
    let internal = handle
        .tools
        .iter()
        .find(|t| t.server_tool == "internal")
        .expect("internal in catalog");
    assert!(internal.app_visible);
    let model_only = handle
        .tools
        .iter()
        .find(|t| t.server_tool == "model_only")
        .expect("model_only in catalog");
    assert!(!model_only.app_visible);
    assert!(model_only.ui.as_ref().unwrap().resource_uri.is_none());

    let mut ctx_raw = crate::context::Context::new(
        std::sync::Arc::new(StubLlm),
        crate::session::SessionLog::ephemeral(),
        crate::tool::ToolRegistry::new(),
        dir.clone(),
    );
    ctx_raw.mcp_servers = vec![handle];
    let ctx = std::sync::Arc::new(ctx_raw);
    let agent = AgentLoop::new(ctx.clone());

    // model-initiated `draw` call → ui artifact + sidecar + event
    let draw = tools
        .iter()
        .find(|t| t.name() == "mcp__echo__draw")
        .unwrap();
    let res = draw.call(json!({"msg": "hi"}), &ctx).await.unwrap();
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("pong[draw]: hi"), "{}", res.output);
    let html = dir.join(".sunmao/artifacts/mcp-echo-draw.html");
    assert!(
        std::fs::read_to_string(&html)
            .unwrap()
            .contains("mcp-app view")
    );
    let sidecar: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join(".sunmao/artifacts/mcp-echo-draw.ui.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(sidecar["uri"], "ui://echo/app");
    assert_eq!(
        sidecar["csp"]["connectDomains"][0], "https://api.example.com",
        "resource _meta.ui.csp rides the sidecar"
    );
    // second call archives rev 1 → .v1.html, new file is rev 2
    draw.call(json!({"msg": "again"}), &ctx).await.unwrap();
    assert!(dir.join(".sunmao/artifacts/mcp-echo-draw.v1.html").exists());
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    let revs: Vec<usize> = evs
        .iter()
        .filter_map(|e| match e {
            crate::session::SessionEvent::Artifact { name, rev, .. } if name == "mcp-echo-draw" => {
                Some(*rev)
            }
            _ => None,
        })
        .collect();
    assert_eq!(revs, [1, 2], "versioned artifact chain: {revs:?}");

    // island bridge: app-only tool callable through the gate
    let out = agent
        .mcp_app_call("echo", "internal", json!({"msg": "via island"}), &Null)
        .await
        .expect("internal callable from the island");
    let text = out["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("pong[internal]: via island"), "{out}");
    // a ["model"]-only tool is refused whatever the mode
    let err = agent
        .mcp_app_call("echo", "model_only", json!({}), &Null)
        .await
        .unwrap_err();
    assert!(err.contains("visibility"), "{err}");
    // the gate still applies: read_only refuses the island's mutation
    agent.set_approval_mode(ApprovalMode::ReadOnly, &Null).await;
    let err = agent
        .mcp_app_call("echo", "internal", json!({}), &Null)
        .await
        .unwrap_err();
    assert!(err.contains("read_only"), "{err}");
    agent.set_approval_mode(ApprovalMode::Auto, &Null).await;

    // resources/read proxy — the island's own fetch path
    let r = agent
        .mcp_resource_read("echo", "ui://echo/app", &Null)
        .await
        .expect("ui:// resource read");
    assert!(
        r["contents"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("mcp-app view"),
        "{r}"
    );
    assert!(
        agent
            .mcp_resource_read("echo", "ui://echo/missing", &Null)
            .await
            .is_err(),
        "unknown resource must error, not empty-200"
    );
    std::fs::remove_dir_all(&dir).ok();
}

struct StubLlm;
#[async_trait::async_trait]
impl sunmao_llm::ProviderAdapter for StubLlm {
    async fn stream(
        &self,
        _req: sunmao_llm::ChatRequest<'_>,
    ) -> anyhow::Result<sunmao_llm::DeltaStream> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
}
