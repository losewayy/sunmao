use super::*;

#[tokio::test]
async fn pre_tool_use_hook_blocks_via_exit2() {
    let dir = std::env::temp_dir().join(format!("sunmao-hook-{}", std::process::id()));
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"cat > payload.json; echo nope >&2; exit 2"}]}]}}"#,
    )
    .unwrap();
    let engine = HookEngine::load(&dir, "test");
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
