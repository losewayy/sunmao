//! Live tests for the MCP client — stdio fixture roundtrips, crash
//! tolerance and the MCP Apps (SEP-1865) host surface. The fixture
//! binary is `tests/fixtures/mcp_server.rs` compiled once per run.

use super::spec::ServerSpec;
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
/// warns and returns the working tools only. (An *untrusted* stdio spec
/// never reaches spawn either — the trust test below covers that lane.)
#[tokio::test]
async fn connect_all_degrades_a_dead_server() {
    let dir = crate::fresh_test_dir("mcp-bad");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/mcp.json"),
        r#"{"mcpServers":{"ghost":{"command":"sunmao-no-such-binary-zz","args":[]}}}"#,
    )
    .unwrap();
    // pin the spec so the gate isn't the reason it fails — this test is
    // about spawn tolerance, not trust
    let text = crate::hooks::trust::spec_text("sunmao-no-such-binary-zz", &[], &Default::default());
    crate::hooks::trust::set_pin(&dir, &dir.join(".sunmao/mcp.json"), &text, true).unwrap();
    let conn = connect_all(&dir, &[]).await;
    assert!(conn.tools.is_empty(), "dead server contributes no tools");
    assert!(conn.skipped.is_empty(), "pinned specs reach the spawn");
    std::fs::remove_dir_all(&dir).ok();
}

/// Fail-closed trust on stdio servers: an unpinned `command:` spec never
/// spawns — the skip lands in `McpConnected.skipped` for the caller to
/// audit, and the roster shows the row as `untrusted`. Pinning the same
/// `(source, spec)` digest lets the connect proceed (it may still fail
/// at spawn — a skipped row and a failed row are distinct outcomes).
#[tokio::test]
async fn untrusted_stdio_server_never_spawns() {
    let dir = crate::fresh_test_dir("mcp-trust");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    let mcp_file = dir.join(".sunmao/mcp.json");
    std::fs::write(
        &mcp_file,
        r#"{"mcpServers":{"ghost":{"command":"sunmao-no-such-binary-zz","args":[]},
           "remote":{"url":"http://127.0.0.1:9/never-dialed"}}}"#,
    )
    .unwrap();
    let mut log = crate::session::SessionLog::ephemeral();

    let conn = connect_all(&dir, &[]).await;
    assert_eq!(conn.skipped.len(), 1, "the stdio spec must be gated");
    assert!(conn.skipped[0].contains("ghost"), "{}", conn.skipped[0]);
    assert!(
        conn.servers.is_empty(),
        "untrusted spec contributes nothing"
    );
    audit_skips(&conn.skipped, &mut log).await;
    let evs = log.events().await.unwrap();
    assert!(
        evs.iter().any(|e| matches!(
            e,
            crate::session::SessionEvent::Hook { event, detail }
                if event == "mcp.untrusted" && detail.contains("ghost")
        )),
        "the skip must be durable"
    );

    // roster: the gated row lists under kind=mcp — `url` servers don't
    // spawn and produce no row at all
    let rows = crate::hooks::trust::spawn_rows(&dir, &[]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, crate::hooks::trust::RowKind::Mcp);
    assert_eq!(rows[0].status, "untrusted");
    assert_eq!(rows[0].matcher, "ghost");

    // pin → the gate opens (the binary still doesn't exist — spawn
    // failure is a separate, already-degraded path)
    crate::hooks::trust::set_pin(
        &dir,
        &mcp_file,
        &crate::hooks::trust::spec_text("sunmao-no-such-binary-zz", &[], &Default::default()),
        true,
    )
    .unwrap();
    let conn = connect_all(&dir, &[]).await;
    assert!(conn.skipped.is_empty(), "pinned spec is not gated");
    let rows = crate::hooks::trust::spawn_rows(&dir, &[]);
    assert_eq!(rows[0].status, "pinned");
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
    let ctx = Arc::new(crate::context::Context::new(
        std::sync::Arc::new(StubLlm),
        crate::session::SessionLog::ephemeral(),
        crate::tool::ToolRegistry::new(),
        std::env::temp_dir(),
    ));

    // healthy path: real initialize → tools/list → tools/call
    let spec = ServerSpec {
        command: Some(bin.to_string_lossy().to_string()),
        args: vec![],
        env: Default::default(),
        url: None,
        headers: Default::default(),
        auth_env: None,
        token_file: None,
        timeout_secs: None,
    };
    let (handle, tools) = connect_one("echo", &spec, std::time::Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(tools.len(), 3, "model registry hides the app-only tool");
    assert_eq!(tools[0].name(), "mcp__echo__ping");
    assert_eq!(handle.tools().len(), 4, "catalog keeps every listed tool");
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
        headers: Default::default(),
        auth_env: None,
        token_file: None,
        timeout_secs: None,
    };
    let (_h, tools) = connect_one("die", &dying, std::time::Duration::from_secs(10))
        .await
        .unwrap();
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
    use crate::agent::{AgentLoop, ApprovalMode};
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
        headers: Default::default(),
        auth_env: None,
        token_file: None,
        timeout_secs: None,
    };
    let (handle, tools) = connect_one("echo", &spec, std::time::Duration::from_secs(10))
        .await
        .unwrap();

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
    let catalog = handle.tools();
    let internal = catalog
        .iter()
        .find(|t| t.server_tool == "internal")
        .expect("internal in catalog");
    assert!(internal.app_visible);
    let model_only = catalog
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

struct Null;
impl crate::agent::Observer for Null {
    fn on_event(&self, _ev: &crate::agent::LiveEvent) {}
}

/// `/srv:prompt` resolves through prompts/get — the fixture's `summarize`
/// takes the `topic` arg positionally; a non-colon name falls through to
/// file commands (None), and a colon name no server owns is also None.
#[tokio::test]
async fn mcp_prompt_resolves_via_get_prompt() {
    let Some(bin) = fixture_bin() else {
        eprintln!("rustc not found — skipping live MCP prompt test");
        return;
    };
    let dir = crate::fresh_test_dir("mcp-prompt");
    let spec = ServerSpec {
        command: Some(bin.to_string_lossy().to_string()),
        args: vec![],
        env: Default::default(),
        url: None,
        headers: Default::default(),
        auth_env: None,
        token_file: None,
        timeout_secs: None,
    };
    let (handle, _tools) = connect_one("echo", &spec, std::time::Duration::from_secs(10))
        .await
        .unwrap();
    // prompts/list landed at connect — the roster surface sees it
    let prompts = handle.prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].name, "summarize");
    assert_eq!(prompts[0].arg_names, ["topic"]);

    let mut ctx_raw = crate::context::Context::new(
        std::sync::Arc::new(StubLlm),
        crate::session::SessionLog::ephemeral(),
        crate::tool::ToolRegistry::new(),
        dir.clone(),
    );
    ctx_raw.mcp_servers = vec![handle];
    let agent = crate::agent::AgentLoop::new(std::sync::Arc::new(ctx_raw));

    // completion surface: `echo:summarize` is offered
    assert_eq!(agent.mcp_prompt_names(), ["echo:summarize"]);
    // positional arg mapping → the prompt's declared `topic`
    let text = agent
        .mcp_prompt_text("echo:summarize", "the diff")
        .await
        .expect("echo:summarize resolves")
        .unwrap();
    assert_eq!(text, "Summarize: the diff");
    // a plain name never touches MCP — file commands keep the slot
    assert!(agent.mcp_prompt_text("summarize", "x").await.is_none());
    // a colon name no server claims falls through too
    assert!(
        agent
            .mcp_prompt_text("ghost:summarize", "x")
            .await
            .is_none()
    );
    // a colon name the server knows but doesn't list errors — the caller
    // gets Some(Err), never a silent "unknown command"
    assert!(
        agent.mcp_prompt_text("echo:nosuch", "").await.is_none(),
        "unlisted prompt falls through to file commands"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A pushed `notifications/tools/list_changed` refreshes the shared
/// catalog; `drain_mcp` swaps the session registry at the turn boundary —
/// the new tool is declared and callable after the drain, and the bump is
/// durable (an `mcp.refresh` audit fact lands in the log).
#[tokio::test]
async fn list_changed_refreshes_registry_at_turn_boundary() {
    let Some(bin) = fixture_bin() else {
        eprintln!("rustc not found — skipping live MCP list_changed test");
        return;
    };
    let dir = crate::fresh_test_dir("mcp-push");
    let spec = ServerSpec {
        command: Some(bin.to_string_lossy().to_string()),
        args: vec!["--push".into()],
        env: Default::default(),
        url: None,
        headers: Default::default(),
        auth_env: None,
        token_file: None,
        timeout_secs: None,
    };
    let (handle, tools) = connect_one("echo", &spec, std::time::Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(tools.len(), 3, "initial catalog predates the push");

    let mut ctx_raw = crate::context::Context::new(
        std::sync::Arc::new(StubLlm),
        crate::session::SessionLog::ephemeral(),
        crate::tool::builtin_registry(),
        dir.clone(),
    );
    // move the handle — Clone resets seen_version, the live connection's
    // own catalog bump must be what the drain observes
    ctx_raw.mcp_servers = vec![handle];
    for t in tools {
        ctx_raw.tools.register_boxed(t);
    }
    let ctx = std::sync::Arc::new(ctx_raw);
    let agent = crate::agent::AgentLoop::new(ctx.clone());

    // wait for the push to land — notification delivery + the handler's
    // re-list are async off the serve task
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while ctx.mcp_servers[0].version() == 0 {
        if std::time::Instant::now() > deadline {
            panic!("list_changed never arrived");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // registry is stale until the drain — the push must not mutate a
    // session's tool surface mid-turn by itself
    assert!(
        !ctx.tools
            .declarations()
            .iter()
            .any(|t| t.function.name == "mcp__echo__after_push"),
        "refresh waits for the turn-boundary drain"
    );

    agent.drain_mcp(&Null).await;

    let names: Vec<String> = ctx
        .tools
        .declarations()
        .iter()
        .map(|t| t.function.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "mcp__echo__after_push"),
        "drained catalog registered: {names:?}"
    );
    // the refreshed impl actually calls through
    let res = ctx
        .tools
        .call("mcp__echo__after_push", "{\"msg\":\"x\"}", &ctx)
        .await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("pong[after_push]: x"));
    // the drain is durable — replay shows when the catalog moved
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    assert!(
        evs.iter().any(|e| matches!(
            e,
            crate::session::SessionEvent::Hook { event, .. } if event == "mcp.refresh"
        )),
        "mcp.refresh audit fact missing"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `${VAR}` must not leak its closing brace into the header value —
/// the name-end offset was measured inside the braces but applied to the
/// un-braced remainder, so every `${VAR}` expansion emitted `…}`.
#[test]
fn expand_env_consumes_the_closing_brace() {
    unsafe { std::env::set_var("SUNMAO_TEST_TOK", "sekrit") };
    assert_eq!(
        super::spec::expand_env("Bearer ${SUNMAO_TEST_TOK}"),
        "Bearer sekrit"
    );
    assert_eq!(
        super::spec::expand_env("k=${SUNMAO_TEST_TOK}&x=1"),
        "k=sekrit&x=1"
    );
    assert_eq!(super::spec::expand_env("$SUNMAO_TEST_TOK-x"), "sekrit-x");
    // unset collapses to empty, unterminated braces expand to end,
    // non-name `$` stays literal
    assert_eq!(super::spec::expand_env("${NOPE_VAR_ZZ}"), "");
    assert_eq!(super::spec::expand_env("${SUNMAO_TEST_TOK"), "sekrit");
    assert_eq!(super::spec::expand_env("a$b c"), "a c");
    unsafe { std::env::remove_var("SUNMAO_TEST_TOK") };
}

/// A server that completes `initialize` then never answers `tools/list`
/// must not stall session startup — connect used to await the listing
/// unbounded. `connect_all_with_timeout` gives the test a short fuse; the
/// outer timeout is the old-code tripwire (it would hang forever).
#[tokio::test]
async fn connect_all_bounds_a_hung_server() {
    let Some(bin) = fixture_bin() else {
        eprintln!("no rustc — skipping live fixture test");
        return;
    };
    let dir = crate::fresh_test_dir("mcp-hang");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    let cmd = bin.display().to_string();
    std::fs::write(
        dir.join(".sunmao/mcp.json"),
        format!(
            r#"{{"mcpServers":{{"hung":{{"command":{},"args":["--hang"]}}}}}}"#,
            serde_json::to_string(&cmd).unwrap()
        ),
    )
    .unwrap();
    let text = crate::hooks::trust::spec_text(&cmd, &["--hang".to_string()], &Default::default());
    crate::hooks::trust::set_pin(&dir, &dir.join(".sunmao/mcp.json"), &text, true).unwrap();
    let conn = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        super::connect::connect_all_with_timeout(&dir, &[], std::time::Duration::from_millis(250)),
    )
    .await
    .expect("a hung server must time out, not stall startup");
    assert!(conn.tools.is_empty());
    assert!(conn.servers.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}
