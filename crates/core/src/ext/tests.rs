use super::*;
use serde_json::json;
use std::sync::Arc;

// — pure protocol —

#[test]
fn frame_parse_roundtrip() {
    let frame = proto::request_frame(7, "ext/event", json!({"event": "PreToolUse"}));
    let line = serde_json::to_string(&frame).unwrap();
    let parsed = proto::parse_frame(&line).unwrap();
    assert_eq!(proto::reply_id(&parsed), Some(7));
    assert_eq!(parsed["method"], "ext/event");
    // noise must not parse as frames
    assert!(proto::parse_frame("").is_none());
    assert!(proto::parse_frame("   ").is_none());
    assert!(proto::parse_frame("not json").is_none());
    assert!(proto::parse_frame("[1,2]").is_none());
}

#[test]
fn reply_result_unwraps_result_and_errors() {
    let ok = proto::reply_result(json!({"id": 1, "result": {"a": 1}})).unwrap();
    assert_eq!(ok["a"], 1);
    let err = proto::reply_result(json!({"id": 1, "error": {"message": "boom"}}));
    assert!(err.unwrap_err().to_string().contains("boom"));
}

/// Reply folding: the ext reply shape maps onto the same outcome fields
/// command hooks produce — block veto, context accumulation, the
/// PreToolUse `updatedInput` rewrite, permission verdicts.
#[test]
fn ext_reply_folds_into_hook_outcome() {
    let mut out = crate::hooks::HookOutcome::default();
    crate::hooks::apply_ext_reply(
        &json!({"extra_context": ["warm", "warmer"], "updatedInput": {"command": "rtk ls"}}),
        &mut out,
    );
    assert_eq!(out.extra_context, vec!["warm", "warmer"]);
    assert_eq!(out.updated_input.as_ref().unwrap()["command"], "rtk ls");

    crate::hooks::apply_ext_reply(&json!({"block": "no such tool"}), &mut out);
    assert_eq!(out.block_reason.as_deref(), Some("no such tool"));

    crate::hooks::apply_ext_reply(&json!({"permissionDecision": "allow"}), &mut out);
    assert_eq!(
        out.permission_decision,
        Some(crate::hooks::HookPermission::Allow)
    );

    // a string extra_context and junk shapes degrade quietly
    let mut out2 = crate::hooks::HookOutcome::default();
    crate::hooks::apply_ext_reply(&json!({"extra_context": "single"}), &mut out2);
    assert_eq!(out2.extra_context, vec!["single"]);
    crate::hooks::apply_ext_reply(&json!({"extra_context": [1, {}]}), &mut out2);
    assert_eq!(out2.extra_context.len(), 1);
    crate::hooks::apply_ext_reply(&json!("nonsense"), &mut out2);
    assert!(out2.block_reason.is_none());
}

#[test]
fn plugin_names_are_sanitized() {
    let m = json!({});
    // dir names from convention roots contain dots — unsanitized they
    // would produce tool names model grammars reject.
    assert_eq!(
        plugin_name(&m, std::path::Path::new("/x/.sunmao")),
        "_sunmao"
    );
    let named = json!({"name": "my plugin!"});
    assert_eq!(
        plugin_name(&named, std::path::Path::new("/x/y")),
        "my_plugin_"
    );
}

/// Reply correlation: dispatch resolves exactly the parked id; stray ids
/// are dropped, wrong-id replies don't steal a pending slot.
#[tokio::test]
async fn dispatch_resolves_parked_id() {
    let state = registry::test_state();
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.lock().unwrap().pending.insert(3, tx);
    registry::test_dispatch(&state, json!({"id": 99, "result": "stray"}));
    registry::test_dispatch(&state, json!({"id": 3, "result": {"ok": true}}));
    let frame = rx.await.unwrap();
    assert_eq!(frame["result"]["ok"], true);
    assert!(state.lock().unwrap().pending.is_empty());
}

// — live, gated on `node` —

fn which_node() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        ["node", "node.exe"]
            .iter()
            .map(|name| dir.join(name))
            .find(|c| c.is_file())
    })
}

fn fixture_path() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR = crates/core → workspace root is two up.
    // canonicalize() returns `\\?\` verbatim paths on Windows — node
    // can't load them (same reason hooks strip the prefix).
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/extensions/echo-ext.mjs")
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .replace("\\\\?\\", "")
        .into()
}

fn host_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/extension-host.mjs")
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .replace("\\\\?\\", "")
        .into()
}

fn init(dir: &std::path::Path) -> registry::ExtInit {
    registry::ExtInit {
        cwd: dir.display().to_string(),
        session_id: "test-session".into(),
        transcript_path: dir.join("t.jsonl").display().to_string(),
    }
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

/// End-to-end against the shipped example: initialize handshake, tool
/// registration under `ext__echo__*`, a real `ext/tools/call`, the
/// `ext/event` reply folding through `HookEngine::fire`, and subscription
/// filtering (fixture subscribes to SessionStart, not PreToolUse).
#[tokio::test]
async fn node_fixture_full_roundtrip() {
    let Some(node) = which_node() else {
        eprintln!("node not on PATH — skipping live extension test");
        return;
    };
    let dir = crate::fresh_test_dir("ext");
    std::fs::create_dir_all(&dir).unwrap();

    let reg = registry::ExtRegistry::new();
    let spec = ExtSpec {
        command: node.to_string_lossy().to_string(),
        args: vec![fixture_path().to_string_lossy().to_string()],
        env: Default::default(),
    };
    reg.connect(&spec, &init(&dir), "echo").await.unwrap();

    let tools = reg.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "ext__echo__ping");

    // a real ext/tools/call through the ToolImpl surface
    let ctx = crate::context::Context::new(
        Arc::new(StubLlm),
        crate::session::SessionLog::ephemeral(),
        crate::tool::ToolRegistry::new(),
        dir.clone(),
    );
    let res = tools[0].call(json!({"msg": "hi"}), &ctx).await.unwrap();
    assert!(res.ok);
    assert!(res.output.contains("hi"));

    // event delivery through the hooks seam — fixture answers SessionStart
    // with extra_context; PreToolUse is not subscribed → nothing arrives
    let mut engine = crate::hooks::HookEngine::load(&dir, "test", &[]);
    engine.attach_ext(Arc::new(reg));
    let out = engine
        .fire(
            crate::hooks::HookEvent::SessionStart,
            &dir,
            &crate::hooks::HookInput {
                source: Some("startup"),
                ..Default::default()
            },
        )
        .await;
    assert!(out.extra_context.iter().any(|c| c.contains("warm")));
    let out = engine
        .fire(
            crate::hooks::HookEvent::PreToolUse,
            &dir,
            &crate::hooks::HookInput {
                tool_name: Some("Bash"),
                ..Default::default()
            },
        )
        .await;
    assert!(out.extra_context.is_empty());
    assert!(out.block_reason.is_none());

    std::fs::remove_dir_all(&dir).ok();
}

/// The pi dialect end-to-end through the REAL sidecar: a module written in
/// oh-my-pi's ExtensionAPI subset — `api.on("tool_call")` returning
/// `{block:true, reason}` must veto PreToolUse, `session_start` folds
/// extra_context, and a pi-spec `registerTool` ({parameters, execute})
/// surfaces and calls as `ext__pi__*`.
#[tokio::test]
async fn js_host_pi_dialect_roundtrip() {
    let Some(node) = which_node() else {
        eprintln!("node not on PATH — skipping pi-dialect host test");
        return;
    };
    let dir = crate::fresh_test_dir("ext-pi");
    let ext_dir = dir.join("ext");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("policy.mjs"),
        r#"export default function (api) {
  api.on("tool_call", (event) => {
    if (event.toolName === "Bash" && event.input?.command?.includes("rm")) {
      return { block: true, reason: "pi veto" };
    }
  });
  api.on("session_start", () => ({ extra_context: "pi warm" }));
  api.registerTool({
    name: "pi_ping",
    description: "pi-spec tool",
    parameters: { type: "object", properties: { msg: { type: "string" } } },
    async execute(_id, params) {
      return { content: [{ type: "text", text: `pong ${params.msg}` }] };
    },
  });
}
"#,
    )
    .unwrap();

    let reg = registry::ExtRegistry::new();
    let spec = ExtSpec {
        command: node.to_string_lossy().to_string(),
        args: vec![
            host_path().to_string_lossy().to_string(),
            ext_dir.display().to_string(),
        ],
        env: Default::default(),
    };
    reg.connect(&spec, &init(&dir), "pi").await.unwrap();

    // pi-spec tool registered under the plugin namespace
    let tools = reg.tools();
    assert!(tools.iter().any(|t| t.name() == "ext__pi__pi_ping"));
    let ctx = crate::context::Context::new(
        Arc::new(StubLlm),
        crate::session::SessionLog::ephemeral(),
        crate::tool::ToolRegistry::new(),
        dir.clone(),
    );
    let res = tools
        .iter()
        .find(|t| t.name() == "ext__pi__pi_ping")
        .unwrap()
        .call(json!({"msg": "via-pi"}), &ctx)
        .await
        .unwrap();
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("pong via-pi"), "{}", res.output);

    // events through the hooks seam — pi names normalized both directions
    let mut engine = crate::hooks::HookEngine::load(&dir, "test", &[]);
    engine.attach_ext(Arc::new(reg));
    let veto = engine
        .fire(
            crate::hooks::HookEvent::PreToolUse,
            &dir,
            &crate::hooks::HookInput {
                tool_name: Some("Bash"),
                tool_input: Some(&json!({"command": "rm -rf /"})),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(veto.block_reason.as_deref(), Some("pi veto"));
    let benign = engine
        .fire(
            crate::hooks::HookEvent::PreToolUse,
            &dir,
            &crate::hooks::HookInput {
                tool_name: Some("Bash"),
                tool_input: Some(&json!({"command": "ls"})),
                ..Default::default()
            },
        )
        .await;
    assert!(benign.block_reason.is_none());
    let start = engine
        .fire(
            crate::hooks::HookEvent::SessionStart,
            &dir,
            &crate::hooks::HookInput::default(),
        )
        .await;
    assert!(start.extra_context.iter().any(|c| c.contains("pi warm")));
    std::fs::remove_dir_all(&dir).ok();
}

/// Dead-child semantics: requests after the child exits fail fast as an
/// error — never panic, never hang the loop.
#[tokio::test]
async fn dead_child_fails_fast() {
    let Some(node) = which_node() else {
        eprintln!("node not on PATH — skipping live extension test");
        return;
    };
    let dir = crate::fresh_test_dir("ext-die");
    std::fs::create_dir_all(&dir).unwrap();

    // child that answers initialize with a SessionStart subscription,
    // then exits — connect succeeds, the next request must hit the dead
    // path and degrade, not panic or hang.
    let script = r#"
        const rl = require('readline').createInterface({input: process.stdin});
        rl.on('line', (l) => {
            const m = JSON.parse(l);
            if (m.method === 'ext/initialize') {
                process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:m.id,result:{name:'die',version:'0',capabilities:{tools:false,events:['SessionStart']}}})+'\n', () => process.exit(0));
            }
        });
    "#;
    let reg = registry::ExtRegistry::new();
    let spec = ExtSpec {
        command: node.to_string_lossy().to_string(),
        args: vec!["-e".into(), script.into()],
        env: Default::default(),
    };
    reg.connect(&spec, &init(&dir), "die").await.unwrap();

    let mut out = crate::hooks::HookOutcome::default();
    reg.fire_event("SessionStart", &json!({}), &mut out).await;
    assert!(out.block_reason.is_none()); // failure degraded to a warn
    std::fs::remove_dir_all(&dir).ok();
}
