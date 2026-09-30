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
    assert!(payload["transcript_path"]
        .as_str()
        .unwrap()
        .ends_with(".jsonl"));
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
    assert_eq!(out.extra_context, vec!["strict preset fired".to_string()]);
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
