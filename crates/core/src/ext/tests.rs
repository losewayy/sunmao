use super::*;
use crate::context::MutexRecover;
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

/// Trust gate on extension spawn: an unpinned `extensions` spec in a
/// plugin manifest never reaches `connect` — `ext.untrusted` audits into
/// the session log and the registry stays empty. The pin is on the
/// expanded `{command,args,env}` text of the manifest entry; once pinned,
/// the gate opens (spawn failure stays a separate warn-only path).
#[tokio::test]
async fn untrusted_extension_never_spawns() {
    let dir = crate::fresh_test_dir("ext-trust");
    let plugin = dir.join(".sunmao/plugins/evil");
    std::fs::create_dir_all(&plugin).unwrap();
    let manifest = plugin.join("plugin.json");
    std::fs::write(
        &manifest,
        r#"{"name":"evil","extensions":[{"command":"sunmao-no-such-ext-zz","args":["--x"]}]}"#,
    )
    .unwrap();

    let sessions = Arc::new(tokio::sync::Mutex::new(
        crate::session::SessionLog::ephemeral(),
    ));
    let reg = registry::ExtRegistry::new();
    connect_all(&reg, &dir, "s1", &[], &sessions).await;
    assert!(reg.tools().is_empty(), "untrusted spec must not spawn");
    let evs = sessions.lock().await.events().await.unwrap();
    assert!(
        evs.iter().any(|e| matches!(
            e,
            crate::session::SessionEvent::Hook { event, detail }
                if event == "ext.untrusted" && detail.contains("evil")
        )),
        "the skip must be durable"
    );
    // the /hooks roster sees the gated row
    let rows = crate::hooks::trust_rows::spawn_rows(&dir, &[]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, crate::hooks::trust::RowKind::Ext);
    assert_eq!(rows[0].matcher, "evil");
    assert_eq!(rows[0].status, "untrusted");

    // pinned → gate opens; the binary doesn't exist so spawn still warns —
    // but no SECOND untrusted row lands
    let text = crate::hooks::trust::spec_text(
        "sunmao-no-such-ext-zz",
        &["--x".to_string()],
        &Default::default(),
    );
    crate::hooks::trust::set_pin(&dir, &manifest, &text, true).unwrap();
    connect_all(&reg, &dir, "s1", &[], &sessions).await;
    let evs = sessions.lock().await.events().await.unwrap();
    assert_eq!(
        evs.iter()
            .filter(
                |e| matches!(e, crate::session::SessionEvent::Hook { event, .. }
                if event == "ext.untrusted")
            )
            .count(),
        1,
        "pinned spec passes the gate — spawn failure is warn-only"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Reply correlation: dispatch resolves exactly the parked id; stray ids
/// are dropped, wrong-id replies don't steal a pending slot.
#[tokio::test]
async fn dispatch_resolves_parked_id() {
    let state = registry::test_state();
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.lock_or_recover().pending.insert(3, tx);
    registry::test_dispatch(&state, json!({"id": 99, "result": "stray"}));
    registry::test_dispatch(&state, json!({"id": 3, "result": {"ok": true}}));
    let frame = rx.await.unwrap();
    assert_eq!(frame["result"]["ok"], true);
    assert!(state.lock_or_recover().pending.is_empty());
}

// — live, gated on `rustc` (the fixture is a local .rs child) —

/// Compile `tests/fixtures/ext_echo.rs` once; every live test reuses the
/// binary (fixture flags select behavior: `--die` for the dead-child
/// case). Returns None when rustc isn't on this box — tests degrade to
/// a skip, same contract the old node fixture had.
fn fixture_bin() -> Option<std::path::PathBuf> {
    static BIN: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    BIN.get_or_init(|| crate::compile_fixture("ext_echo.rs", "sunmao-ext-echo"))
        .clone()
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

/// End-to-end against the Rust fixture: initialize handshake, tool
/// registration under `ext__echo__*`, a real `ext/tools/call`, `ext/event`
/// replies folding through `HookEngine::fire`, subscription filtering
/// (fixture isn't subscribed to Stop → nothing arrives), and a live
/// PreToolUse veto.
#[tokio::test]
async fn fixture_full_roundtrip() {
    let Some(bin) = fixture_bin() else {
        eprintln!("rustc not found — skipping live extension test");
        return;
    };
    let dir = crate::fresh_test_dir("ext");
    std::fs::create_dir_all(&dir).unwrap();

    let reg = registry::ExtRegistry::new();
    let spec = ExtSpec {
        command: bin.to_string_lossy().to_string(),
        args: vec![],
        env: Default::default(),
    };
    reg.connect(&spec, &init(&dir), "echo").await.unwrap();

    let tools = reg.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "ext__echo__ping");

    // a real ext/tools/call through the ToolImpl surface
    let ctx = Arc::new(crate::context::Context::new(
        Arc::new(StubLlm),
        crate::session::SessionLog::ephemeral(),
        crate::tool::ToolRegistry::new(),
        dir.clone(),
    ));
    let res = tools[0].call(json!({"msg": "hi"}), &ctx).await.unwrap();
    assert!(res.ok);
    assert!(res.output.contains("hi"), "{}", res.output);

    // event delivery through the hooks seam — fixture answers SessionStart
    // with extra_context and vetoes PreToolUse on "rm"; it is NOT
    // subscribed to Stop, so that event must arrive nowhere.
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
    assert_eq!(veto.block_reason.as_deref(), Some("fake veto"));

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

    let unsubscribed = engine
        .fire(
            crate::hooks::HookEvent::Stop,
            &dir,
            &crate::hooks::HookInput::default(),
        )
        .await;
    assert!(unsubscribed.extra_context.is_empty());
    assert!(unsubscribed.block_reason.is_none());

    std::fs::remove_dir_all(&dir).ok();
}

/// Dead-child semantics: requests after the child exits fail fast as an
/// error — never panic, never hang the loop. The fixture's `--die` mode
/// answers initialize then exits, so connect succeeds and the next
/// request hits the dead path.
#[tokio::test]
async fn dead_child_fails_fast() {
    let Some(bin) = fixture_bin() else {
        eprintln!("rustc not found — skipping live extension test");
        return;
    };
    let dir = crate::fresh_test_dir("ext-die");
    std::fs::create_dir_all(&dir).unwrap();

    let reg = registry::ExtRegistry::new();
    let spec = ExtSpec {
        command: bin.to_string_lossy().to_string(),
        args: vec!["--die".into()],
        env: Default::default(),
    };
    reg.connect(&spec, &init(&dir), "die").await.unwrap();

    let mut out = crate::hooks::HookOutcome::default();
    reg.fire_event("SessionStart", &json!({}), &mut out).await;
    assert!(out.block_reason.is_none()); // failure degraded to a warn
    std::fs::remove_dir_all(&dir).ok();
}
