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
    // server spelling is not case-stable — the detector lowercases
    let e =
        anyhow::anyhow!("provider 400 Bad Request: Previous response with id 'resp_x' not found");
    assert!(stale_chain(&e));
    let e = anyhow::anyhow!("provider 500 Internal Server Error");
    assert!(!stale_chain(&e));
}

/// Smallest possible HTTP/1.1 server step: read one request (headers +
/// content-length body), reply with `response`, return the request body.
async fn one_request(
    listener: &tokio::net::TcpListener,
    response: &'static str,
) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut sock, _) = listener.accept().await.unwrap();
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await.unwrap();
        assert!(n > 0, "connection closed before request completed");
        buf.extend_from_slice(&chunk[..n]);
        let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..head_end]);
        let len: usize = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap_or(0);
        if buf.len() >= head_end + 4 + len {
            sock.write_all(response.as_bytes()).await.unwrap();
            return serde_json::from_slice(&buf[head_end + 4..head_end + 4 + len]).unwrap();
        }
    }
}

/// A rejected `prev_id` must clear the dead link AND resend full input —
/// the old case-sensitive detector missed "Previous response …" and let
/// the error kill the request instead of recovering.
#[tokio::test]
async fn stale_prev_id_clears_chain_and_resends_full_input() {
    use futures_util::StreamExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client = ResponsesClient::new(&format!("http://{addr}"), "k", "m");

    // the full logical list = sent prefix + one tail item; build it from
    // real Messages so item shapes match map_items' output
    let msgs = [Message::user("hi"), Message::tool_result("call_1", "out")];
    // seed a chain the server will reject — `sent` must be the exact item
    // map_items emits for the prefix, or split_input won't chain
    {
        let mut chain = client.chain.lock().unwrap();
        chain.prev_id = Some("resp_dead".into());
        chain.sent = vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hi"}],
        })];
    }
    let req = ChatRequest {
        messages: &msgs,
        tools: None,
        max_tokens: None,
        temperature: None,
        reasoning_effort: None,
    };

    // serve both requests in order while the client future runs: conn 1
    // rejects the dead id (capital-P spelling the detector used to miss),
    // conn 2 is the rescue resend → clean SSE
    let bad = "HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"Previous response with id not found\"}}";
    let ok = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_ok\",\"status\":\"completed\"}}\n\n";
    let server = async {
        let first = one_request(&listener, bad).await;
        let second = one_request(&listener, ok).await;
        (first, second)
    };
    let (res, (first_body, second_body)) = tokio::join!(client.stream(req), server);

    let mut stream = res.unwrap();
    while let Some(d) = stream.next().await {
        d.unwrap();
    }
    assert_eq!(first_body["previous_response_id"], "resp_dead");
    assert!(second_body.get("previous_response_id").is_none());
    assert_eq!(second_body["input"].as_array().unwrap().len(), 2);
    let chain = client.chain.lock().unwrap();
    assert_eq!(chain.prev_id.as_deref(), Some("resp_ok"));
    assert_eq!(chain.sent.len(), 2);
}

/// The rescue itself failing must not leave the dead prev_id behind —
/// next request would otherwise inherit the same rejection forever.
#[tokio::test]
async fn failed_rescue_leaves_no_dead_link() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client = ResponsesClient::new(&format!("http://{addr}"), "k", "m");
    let msgs = [Message::user("hi"), Message::tool_result("call_1", "out")];
    {
        let mut chain = client.chain.lock().unwrap();
        chain.prev_id = Some("resp_dead".into());
        chain.sent = vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hi"}],
        })];
    }
    let req = ChatRequest {
        messages: &msgs,
        tools: None,
        max_tokens: None,
        temperature: None,
        reasoning_effort: None,
    };
    let bad = "HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"Previous response with id not found\"}}";
    // conn 1 rejects the chain; conn 2 (the rescue send) rejects too —
    // 400 is non-retryable so each is a single shot
    let server = async {
        let _ = one_request(&listener, bad).await;
        let _ = one_request(&listener, bad).await;
    };
    let (res, ()) = tokio::join!(client.stream(req), server);

    let err = res.err().expect("two rejections must surface as an error");
    assert!(err.to_string().contains("400"));
    let chain = client.chain.lock().unwrap();
    assert!(
        chain.prev_id.is_none(),
        "dead link must not survive a failed rescue"
    );
    assert!(chain.sent.is_empty());
}

/// `reasoning_effort` rides the `reasoning.effort` field — Responses'
/// spelling, not chat-completions' flat key — and stays absent when unset.
#[test]
fn effort_lands_on_reasoning_object() {
    let c = ResponsesClient::new("http://x", "k", "m");
    let msgs = [Message::user("hi")];
    let split = Split {
        input: vec![],
        prev_id: None,
    };
    let body = c.request_body(
        &ChatRequest {
            messages: &msgs,
            tools: None,
            max_tokens: None,
            temperature: None,
            reasoning_effort: Some("high"),
        },
        "",
        &split,
    );
    assert_eq!(body["reasoning"]["effort"], "high");
    assert!(body.get("reasoning_effort").is_none());
    let body = c.request_body(
        &ChatRequest {
            messages: &msgs,
            tools: None,
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
        },
        "",
        &split,
    );
    assert!(body.get("reasoning").is_none());
}
