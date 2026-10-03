use super::dialect::apply_result;
use super::*;

#[test]
fn matcher_semantics() {
    // literals + wildcards
    assert!(matches("", "Bash"));
    assert!(matches("*", "Read"));
    assert!(matches("Bash", "Bash"));
    assert!(!matches("Bash", "Read"));
    // regex alternation — the ecosystem's multi-tool matcher
    assert!(matches("Bash|Read", "Bash"));
    assert!(matches("Bash|Read", "Read"));
    assert!(!matches("Bash|Read", "Write"));
    // prefix match — the mcp__ wildcard convention
    assert!(matches("mcp__", "mcp__fs__read"));
    assert!(matches(
        "mcp__plugin_context-mode.*",
        "mcp__plugin_context-mode_x__y"
    ));
    assert!(!matches("mcp__", "Bash"));
}

#[test]
fn exit2_blocks_with_stderr() {
    let mut o = HookOutcome::default();
    apply_result(2, "", "no deletes allowed", &mut o);
    assert_eq!(o.block_reason.as_deref(), Some("no deletes allowed"));
}

#[test]
fn json_continue_false_blocks() {
    let mut o = HookOutcome::default();
    apply_result(0, r#"{"continue":false,"stopReason":"halt"}"#, "", &mut o);
    assert_eq!(o.block_reason.as_deref(), Some("halt"));
}

#[test]
fn json_injects_context() {
    let mut o = HookOutcome::default();
    apply_result(
        0,
        r#"{"systemMessage":"hi","hookSpecificOutput":{"additionalContext":"ctx"}}"#,
        "",
        &mut o,
    );
    assert_eq!(o.extra_context, vec!["hi", "ctx"]);
    assert!(o.block_reason.is_none());
}

#[test]
fn permission_decision_maps() {
    let mut o = HookOutcome::default();
    apply_result(
        0,
        r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#,
        "",
        &mut o,
    );
    assert_eq!(o.permission_decision, Some(HookPermission::Allow));
    assert!(o.block_reason.is_none());

    let mut o = HookOutcome::default();
    apply_result(
        0,
        r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#,
        "",
        &mut o,
    );
    assert_eq!(o.permission_decision, Some(HookPermission::Ask));

    let mut o = HookOutcome::default();
    apply_result(
        0,
        r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"nope"}}"#,
        "",
        &mut o,
    );
    assert_eq!(o.permission_decision, Some(HookPermission::Deny));
    assert_eq!(o.block_reason.as_deref(), Some("nope"));
}

#[test]
fn updated_input_replaces_args() {
    let mut o = HookOutcome::default();
    apply_result(
        0,
        r#"{"hookSpecificOutput":{"updatedInput":{"command":"rtk git status"}}}"#,
        "",
        &mut o,
    );
    assert_eq!(
        o.updated_input
            .and_then(|v| v["command"].as_str().map(String::from)),
        Some("rtk git status".into())
    );
}

#[test]
fn plugin_root_expands() {
    let root = Path::new("C:/plugins/cm");
    assert_eq!(
        expand_plugin_root(r#"node "${CLAUDE_PLUGIN_ROOT}/hooks/x.mjs""#, Some(root)),
        r#"node "C:/plugins/cm/hooks/x.mjs""#
    );
    assert_eq!(
        expand_plugin_root("echo hi", Some(root)),
        "echo hi",
        "no placeholder → untouched"
    );
    assert_eq!(
        expand_plugin_root("echo $CLAUDE_PLUGIN_ROOT", None),
        "echo $CLAUDE_PLUGIN_ROOT",
        "non-plugin command untouched"
    );
}

/// A claude-shape `hooks.json` `timeout` field must reach the exec budget,
/// not deserialize to nothing (serde default ≠ skip).
#[test]
fn claude_shape_timeout_field_loads() {
    let dir = crate::fresh_test_dir("hook-timeout");
    let sd = dir.join(".sunmao");
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::write(
        sd.join("hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo x","timeout":7}]}]}}"#,
    )
    .unwrap();
    let engine = HookEngine::load(&dir, "s1", &[]);
    let group = &engine.groups["PreToolUse"][0];
    assert_eq!(group.hooks[0].timeout, Some(7));
    std::fs::remove_dir_all(&dir).ok();
}

/// A hook that detaches a grandchild (`start /b`) inherits our pipe ends —
/// cmd exits, the pipes never EOF, and the OLD code hung in the drain join
/// until the grandchild's own lifespan ended. The bounded drain must return
/// the hook's verdict in ~the drain grace, not the grandchild's runtime.
#[cfg(windows)]
#[tokio::test]
async fn detached_grandchild_cannot_hang_hook() {
    let dir = crate::fresh_test_dir("hook-drain");
    std::fs::create_dir_all(&dir).unwrap();
    let t0 = std::time::Instant::now();
    let res = super::exec::run_hook_command(
        "cmd /c start /b ping -n 20 127.0.0.1 >nul",
        &serde_json::json!({"x": 1}),
        &dir,
        Some(60),
    )
    .await
    .unwrap();
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(15),
        "drain must be bounded: {:?}",
        t0.elapsed()
    );
    assert_eq!(res.0, 0);
    std::fs::remove_dir_all(&dir).ok();
}
