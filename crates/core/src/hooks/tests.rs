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
