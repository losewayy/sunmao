use super::*;

#[tokio::test]
async fn pre_tool_use_hook_blocks_via_exit2() {
    let dir = crate::fresh_test_dir("hook");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"cat > payload.json; echo nope >&2; exit 2"}]}]}}"#,
    )
    .unwrap();
    let engine = HookEngine::load(&dir, "test", &[]);
    engine
        .trust_all
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let out = engine
        .fire(
            HookEvent::PreToolUse,
            &dir,
            &HookInput {
                tool_name: Some("Bash"),
                tool_input: Some(&json!({"command": "ls"})),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(out.block_reason.as_deref(), Some("nope"));
    // payload landed on the hook's stdin — including the dialect fields
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("payload.json")).unwrap()).unwrap();
    assert_eq!(payload["hook_event_name"], "PreToolUse");
    assert_eq!(payload["tool_name"], "Bash");
    assert!(
        payload["transcript_path"]
            .as_str()
            .unwrap()
            .ends_with(".jsonl")
    );
}

/// The context-mode half of the SPEC fixture: a SessionStart hook receives
/// `source` on stdin and may answer with `additionalContext`. Our engine
/// must deliver the dialect fields verbatim — context-mode's plugin hinges
/// on seeing `source` to decide which sidecar state to heal/inject.
#[tokio::test]
async fn session_start_delivers_source_and_collects_context() {
    let dir = crate::fresh_test_dir("cm");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    // the hook echoes its stdin so we can inspect the payload, then appends
    // the context-mode-style JSON on a second line... keep it simple: emit
    // payload to a file, stdout carries the additionalContext response
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"cat > session-start.json; echo '{\"hookSpecificOutput\":{\"additionalContext\":\"cm warm\"}}'"}]}]}}"#,
    )
    .unwrap();
    let engine = HookEngine::load(&dir, "test", &[]);
    engine
        .trust_all
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let out = engine
        .fire(
            HookEvent::SessionStart,
            &dir,
            &HookInput {
                source: Some("startup"),
                ..Default::default()
            },
        )
        .await;
    assert!(out.block_reason.is_none());
    assert_eq!(out.extra_context, vec!["cm warm".to_string()]);
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("session-start.json")).unwrap())
            .unwrap();
    assert_eq!(payload["hook_event_name"], "SessionStart");
    assert_eq!(payload["source"], "startup");
    assert!(payload["session_id"].is_string());
    std::fs::remove_dir_all(&dir).ok();
}

/// assert its `updatedInput` rewrite flows through our dispatcher intact.
/// Skips quietly when `rtk` isn't installed — CI exercises the contract
/// via the mock shape in tests.rs; this pins the real wire bytes.
#[tokio::test]
async fn real_rtk_hook_rewrites_command() {
    if which_rtk().is_none() {
        eprintln!("rtk not on PATH — skipping live conformance test");
        return;
    }
    let dir = crate::fresh_test_dir("rtk-live");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    // exactly the registration `rtk init` writes into Claude settings
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]}]}}"#,
    )
    .unwrap();
    let engine = HookEngine::load(&dir, "test", &[]);
    engine
        .trust_all
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let out = engine
        .fire(
            HookEvent::PreToolUse,
            &dir,
            &HookInput {
                tool_name: Some("Bash"),
                tool_input: Some(&json!({"command": "git status"})),
                ..Default::default()
            },
        )
        .await;
    let rewritten = out
        .updated_input
        .as_ref()
        .and_then(|v| v["command"].as_str())
        .unwrap_or_default();
    // rtk's single source of truth is its own `rewrite` subcommand — assert
    // the hook output carries the prefix, not a hardcoded command shape
    assert!(
        rewritten.starts_with("rtk "),
        "expected rtk rewrite, got: {rewritten}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

fn which_rtk() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        ["rtk", "rtk.exe"]
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// The cursor dialect end-to-end: a REAL `.cursor/hooks.json` in the flat
/// `{command, matcher}` shape (cursor docs' own layout). A `Shell` matcher
/// must hit our `Bash` tool, and a snake_case `updated_input` reply must
/// rewrite the dispatch args — the normalization chain, not just parsing.
#[tokio::test]
async fn cursor_hooks_file_fires_on_mapped_tool_and_rewrites() {
    let dir = crate::fresh_test_dir("cursor");
    std::fs::create_dir_all(dir.join(".cursor")).unwrap();
    std::fs::write(
        dir.join(".cursor/hooks.json"),
        r#"{"version":1,"hooks":{"preToolUse":[{"command":"cat > cursor-payload.json; echo '{\"permission\":\"allow\",\"updated_input\":{\"command\":\"echo cursor-rewrote\"}}'","matcher":"Shell"}],"beforeShellExecution":[{"command":"exit 9"}]}}"#,
    )
    .unwrap();
    let engine = HookEngine::load(&dir, "test", &[]);
    engine
        .trust_all
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let out = engine
        .fire(
            HookEvent::PreToolUse,
            &dir,
            &HookInput {
                tool_name: Some("Bash"),
                tool_input: Some(&json!({"command": "ls"})),
                ..Default::default()
            },
        )
        .await;
    // matcher "Shell" → hit our Bash; reply normalized
    assert_eq!(out.permission_decision, Some(HookPermission::Allow));
    assert_eq!(
        out.updated_input
            .as_ref()
            .and_then(|v| v["command"].as_str()),
        Some("echo cursor-rewrote")
    );
    // the payload spelled the cursor dialect
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("cursor-payload.json")).unwrap())
            .unwrap();
    assert_eq!(payload["hook_event_name"], "preToolUse");
    assert_eq!(payload["tool_name"], "Shell");
    assert!(payload["conversation_id"].is_string());
    std::fs::remove_dir_all(&dir).ok();
}

/// Codex's `.codex/hooks.json` is the Claude file shape at a different
/// path — a `rtk init --codex` bundle must work verbatim.
#[tokio::test]
async fn codex_hooks_file_loads_like_a_claude_file() {
    let dir = crate::fresh_test_dir("codex");
    std::fs::create_dir_all(dir.join(".codex")).unwrap();
    std::fs::write(
        dir.join(".codex/hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"cat > codex-payload.json"}]}]}}"#,
    )
    .unwrap();
    let engine = HookEngine::load(&dir, "test", &[]);
    engine
        .trust_all
        .store(true, std::sync::atomic::Ordering::Relaxed);
    engine
        .fire(
            HookEvent::PreToolUse,
            &dir,
            &HookInput {
                tool_name: Some("Bash"),
                tool_input: Some(&json!({"command": "ls"})),
                ..Default::default()
            },
        )
        .await;
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("codex-payload.json")).unwrap())
            .unwrap();
    assert_eq!(payload["hook_event_name"], "PreToolUse");
    assert_eq!(payload["tool_name"], "Bash");
    std::fs::remove_dir_all(&dir).ok();
}

/// `--preset` end-to-end: a preset dir shaped like a plugin bundle must
/// contribute its hooks when (and only when) it's handed to the context as
/// an extra plugin root.
#[tokio::test]
async fn preset_dir_fires_its_hooks() {
    let dir = crate::fresh_test_dir("preset-hook");
    let preset = dir.join(".sunmao/presets/strict");
    std::fs::create_dir_all(preset.join("hooks")).unwrap();
    std::fs::write(
        preset.join("plugin.json"),
        r#"{"name":"strict","description":"test preset"}"#,
    )
    .unwrap();
    std::fs::write(
        preset.join("hooks/hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"cat >/dev/null; echo '{\"systemMessage\":\"strict preset fired\"}'"}]}]}}"#,
    )
    .unwrap();
    let roots = crate::presets::resolve(&dir, &["+strict".to_string()]).unwrap();
    assert_eq!(roots.len(), 1);

    // without the preset enabled the engine sees nothing
    let bare = HookEngine::load(&dir, "test", &[]);
    let out = bare
        .fire(
            HookEvent::PreToolUse,
            &dir,
            &HookInput {
                tool_name: Some("Bash"),
                ..Default::default()
            },
        )
        .await;
    assert!(out.extra_context.is_empty());

    // through the Context builder the preset's hook must run
    let ctx = crate::context::Context::new(
        std::sync::Arc::new(MockProvider),
        crate::session::SessionLog::ephemeral(),
        crate::tool::builtin_registry(),
        dir.clone(),
    )
    .with_extra_plugin_roots(roots);
    ctx.hooks
        .trust_all
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let out = ctx
        .hooks
        .fire(
            HookEvent::PreToolUse,
            &dir,
            &HookInput {
                tool_name: Some("Bash"),
                tool_input: Some(&json!({"command": "ls"})),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(out.notices, vec!["strict preset fired".to_string()]);
    std::fs::remove_dir_all(&dir).ok();
}

/// An Interrupt hook receives its event name on stdin like any other — the
/// detached spawn in `AgentLoop::cancel` is a delivery detail; this proves
/// the event round-trips through load/match/fire like its siblings. No
/// tool_name is set: cancel happens outside tool dispatch, so the hook's
/// payload legitimately carries `"tool_name": null`.
#[tokio::test]
async fn interrupt_event_reaches_the_hook() {
    let dir = crate::fresh_test_dir("irq");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"Interrupt":[{"hooks":[{"type":"command","command":"cat > interrupt.json"}]}]}}"#,
    )
    .unwrap();
    let engine = HookEngine::load(&dir, "test", &[]);
    engine
        .trust_all
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let out = engine
        .fire(HookEvent::Interrupt, &dir, &HookInput::default())
        .await;
    assert!(out.block_reason.is_none());
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("interrupt.json")).unwrap())
            .unwrap();
    assert_eq!(payload["hook_event_name"], "Interrupt");
    assert!(payload["tool_name"].is_null());
    assert!(payload["tool_input"].is_null());
    std::fs::remove_dir_all(&dir).ok();
}

struct MockProvider;
#[async_trait::async_trait]
impl sunmao_llm::ProviderAdapter for MockProvider {
    async fn stream(
        &self,
        _req: sunmao_llm::ChatRequest<'_>,
    ) -> anyhow::Result<sunmao_llm::DeltaStream> {
        Ok(Box::pin(futures_util::stream::iter(vec![])))
    }
}

/// The whole point of trust pinning: a project-carried SessionStart hook
/// must NOT execute until the user pins it — and the skip must be a durable
/// `hook.untrusted` fact, not a silent drop.
#[tokio::test]
async fn untrusted_project_hook_skips_and_audits() {
    let dir = crate::fresh_test_dir("untrusted");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo ran > should-not-exist.txt"}]}]}}"#,
    )
    .unwrap();
    let mut engine = HookEngine::load(&dir, "test", &[]);
    let log = crate::session::SessionLog::open(dir.join(".sunmao/sessions"), "s-t1")
        .await
        .unwrap();
    let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(log));
    engine.attach_sessions(sessions.clone());
    // roster shows OUR project hook as untrusted before any pin — user-level
    // rows (~/.claude etc. on a real machine) may coexist, so locate by command
    let rows = engine.roster();
    let row = rows
        .iter()
        .find(|r| r.command.contains("should-not-exist.txt"))
        .expect("the project hook must be loaded");
    assert_eq!(row.status, "untrusted", "row: {row:?}");

    engine
        .fire(
            HookEvent::SessionStart,
            &dir,
            &HookInput {
                source: Some("startup"),
                ..Default::default()
            },
        )
        .await;
    // the command never ran — no side-effect file
    assert!(!dir.join("should-not-exist.txt").exists());
    // the skip IS durable: a hook.untrusted row names command + source
    let events = sessions.lock().await.events().await.unwrap();
    let skip = events.iter().find_map(|e| match e {
        crate::session::SessionEvent::Hook { event, detail } if event == "hook.untrusted" => {
            Some(detail.clone())
        }
        _ => None,
    });
    let detail = skip.expect("a skip must land in the session log");
    assert!(detail.contains("SessionStart"), "{detail}");
    assert!(detail.contains("should-not-exist.txt"), "{detail}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Pin → run → revoke → skip again: the round-trip through
/// `set_row_trust` + the ledger file is the `/hooks` contract.
#[tokio::test]
async fn pinned_hook_executes_and_revocation_stops_it() {
    let dir = crate::fresh_test_dir("pinned");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo ok >> pin-ran.txt"}]}]}}"#,
    )
    .unwrap();
    let engine = HookEngine::load(&dir, "test", &[]);
    let input = || HookInput {
        source: Some("startup"),
        ..Default::default()
    };
    // user-level rows (~/.claude etc.) may coexist on a real machine —
    // locate our command's roster index rather than assuming row 1
    let idx_of = |engine: &HookEngine| {
        engine
            .roster()
            .iter()
            .position(|r| r.command.contains("pin-ran.txt"))
            .map(|i| i + 1)
    };
    engine.fire(HookEvent::SessionStart, &dir, &input()).await;
    assert!(!dir.join("pin-ran.txt").exists(), "untrusted must not run");

    // pin OUR row — the ledger write alone is what unlocks it
    let idx = idx_of(&engine).expect("the project hook must be loaded");
    engine.set_row_trust(idx, true).unwrap();
    let row = engine
        .roster()
        .into_iter()
        .find(|r| r.command.contains("pin-ran.txt"))
        .unwrap();
    assert_eq!(row.status, "pinned");
    engine.fire(HookEvent::SessionStart, &dir, &input()).await;
    assert_eq!(
        std::fs::read_to_string(dir.join("pin-ran.txt")).unwrap(),
        "ok\n"
    );

    // revoke → back to skipped (a second fire adds nothing)
    engine.set_row_trust(idx, false).unwrap();
    let row = engine
        .roster()
        .into_iter()
        .find(|r| r.command.contains("pin-ran.txt"))
        .unwrap();
    assert_eq!(row.status, "untrusted");
    engine.fire(HookEvent::SessionStart, &dir, &input()).await;
    assert_eq!(
        std::fs::read_to_string(dir.join("pin-ran.txt")).unwrap(),
        "ok\n"
    );
    // out-of-range and zero indices are errors, not silent no-ops
    assert!(engine.set_row_trust(0, true).is_err());
    assert!(engine.set_row_trust(9, true).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

/// `hook.trust` decisions are audit facts too — the session log must
/// answer "who pinned this, when" without asking the ledger file.
#[tokio::test]
async fn trust_decisions_are_logged() {
    let dir = crate::fresh_test_dir("trustlog");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
    )
    .unwrap();
    let log = crate::session::SessionLog::open(dir.join(".sunmao/sessions"), "s-t2")
        .await
        .unwrap();
    let provider = std::sync::Arc::new(MockProvider);
    let ctx = std::sync::Arc::new(crate::context::Context::new(
        provider,
        log,
        crate::tool::builtin_registry(),
        dir.clone(),
    ));
    let agent = crate::agent::AgentLoop::new(ctx.clone());
    // index by command — user-level rows may occupy earlier slots
    let idx = ctx
        .hooks
        .roster()
        .iter()
        .position(|r| r.command == "true")
        .map(|i| i + 1)
        .expect("the project hook must be loaded");
    agent.set_hook_trust(idx, true).await.unwrap();
    let events = ctx.sessions.lock().await.events().await.unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        crate::session::SessionEvent::Hook { event, .. } if event == "hook.trust"
    )));
    std::fs::remove_dir_all(&dir).ok();
}
