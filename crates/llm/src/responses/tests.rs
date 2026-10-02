use super::*;
use crate::types::{FunctionCall, ToolCall};

#[tokio::test]
async fn maps_roles_to_items() {
    let msgs = vec![
        Message::system("be terse"),
        Message::user("hi"),
        Message::assistant(
            Some("checking".into()),
            vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "Bash".into(),
                    arguments: "{\"cmd\":\"ls\"}".into(),
                },
            }],
        ),
        Message::tool_result("call_1", "a.txt"),
    ];
    let (inst, items) = map_items(&msgs).await;
    assert_eq!(inst, "be terse");
    assert_eq!(items.len(), 4);
    assert_eq!(items[0]["content"][0]["type"], "input_text");
    assert_eq!(items[1]["content"][0]["type"], "output_text");
    assert_eq!(items[2]["type"], "function_call");
    assert_eq!(items[2]["call_id"], "call_1");
    assert_eq!(items[3]["type"], "function_call_output");
    assert_eq!(items[3]["output"], "a.txt");
}

#[test]
fn deltas_map_text_args_and_finish() {
    let mut id = None;
    let text = map_event(
        r#"{"type":"response.output_text.delta","delta":"hi"}"#,
        &mut id,
    )
    .unwrap();
    assert_eq!(text, vec![StreamDelta::Content("hi".into())]);

    let args = map_event(
        r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"x\""}"#,
        &mut id,
    )
    .unwrap();
    assert_eq!(
        args,
        vec![StreamDelta::ToolCalls(vec![ToolCallFragment {
            index: 1,
            id: None,
            name: None,
            arguments: Some("{\"x\"".into()),
        }])]
    );

    let added = map_event(
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_9","call_id":"call_9","name":"Edit"}}"#,
            &mut id,
        )
        .unwrap();
    assert_eq!(
        added,
        vec![StreamDelta::ToolCalls(vec![ToolCallFragment {
            index: 2,
            id: Some("call_9".into()),
            name: Some("Edit".into()),
            arguments: None,
        }])]
    );

    let done = map_event(
            r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cached_tokens":6}}}}"#,
            &mut id,
        )
        .unwrap();
    match &done[0] {
        StreamDelta::Finish { reason, usage } => {
            assert_eq!(reason.as_deref(), Some("completed"));
            let u = usage.as_ref().unwrap();
            assert_eq!(u.prompt_tokens, 10);
            assert_eq!(u.cache_read_input_tokens, 6);
        }
        other => panic!("expected Finish, got {other:?}"),
    }
    assert_eq!(id.as_deref(), Some("resp_1"));
}

#[test]
fn split_chains_only_on_strict_prefix() {
    let a = json!({"type": "message", "role": "user"});
    let b = json!({"type": "message", "role": "assistant"});
    let c = json!({"type": "function_call_output"});

    let chain = Chain {
        prev_id: Some("resp_1".into()),
        sent: vec![a.clone(), b.clone()],
    };
    // append-only → tail send
    let s = split_input(&chain, &[a.clone(), b.clone(), c.clone()]);
    assert_eq!(s.prev_id.as_deref(), Some("resp_1"));
    assert_eq!(s.input, vec![c.clone()]);

    // divergence mid-list (edit/compact) → full resend
    let other = json!({"type": "message", "role": "user", "content": "edited"});
    let s = split_input(&chain, &[other, b.clone()]);
    assert!(s.prev_id.is_none());
    assert_eq!(s.input.len(), 2);

    // shrink (rewind) → full resend
    let s = split_input(&chain, std::slice::from_ref(&a));
    assert!(s.prev_id.is_none());

    // no prev → always full
    let cold = Chain::default();
    let s = split_input(&cold, std::slice::from_ref(&a));
    assert!(s.prev_id.is_none());
    assert_eq!(s.input.len(), 1);
}

#[test]
fn stale_chain_detection() {
    let e = anyhow::anyhow!("provider 400 Bad Request: previous_response_id 'resp_x' not found");
    assert!(stale_chain(&e));
    let e = anyhow::anyhow!("provider 500 Internal Server Error");
    assert!(!stale_chain(&e));
}
