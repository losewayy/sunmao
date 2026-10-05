//! Offline tests for the Feishu adapter: the pure payload → `InboundMsg`
//! mapping, the frame/chunk plumbing, chunking, and the two REST request
//! shapes. Nothing here touches the network.

use super::*;

const APP: &str = "cli_app";

fn user_sender() -> serde_json::Value {
    serde_json::json!({
        "sender_type": "user",
        "sender_id": {"open_id": "ou_1", "user_id": "u_1", "union_id": "un_1"},
    })
}

fn envelope(
    chat_type: &str,
    message_type: &str,
    content: serde_json::Value,
    sender: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "header": {"event_type": EVENT_MESSAGE, "event_id": "ev_1"},
        "event": {
            "sender": sender,
            "message": {
                "message_id": "om_1",
                "chat_id": "oc_1",
                "chat_type": chat_type,
                "message_type": message_type,
                "content": content.to_string(),
            },
        },
    })
}

fn text_event(text: &str) -> serde_json::Value {
    envelope(
        "p2p",
        "text",
        serde_json::json!({"text": text}),
        user_sender(),
    )
}

#[test]
fn p2p_text_maps_to_inbound_dm() {
    let msg = extract_dm(&text_event("hello bot"), APP).unwrap();
    assert_eq!(msg.source.channel, "feishu");
    assert_eq!(msg.source.chat_id, "oc_1");
    assert_eq!(msg.source.sender_id, "ou_1");
    assert_eq!(msg.source.sender_name, "ou_1");
    assert_eq!(msg.text, "hello bot");
}

#[test]
fn group_traffic_is_not_a_dm() {
    let event = envelope(
        "group",
        "text",
        serde_json::json!({"text": "@bot hi"}),
        user_sender(),
    );
    assert!(
        extract_dm(&event, APP).is_none(),
        "chat_type group must not enter the DM-only gateway"
    );
}

#[test]
fn self_echo_is_dropped() {
    let echo = serde_json::json!({"sender_type": "bot", "sender_id": {"app_id": APP}});
    assert!(is_self_echo(&echo, APP));
    assert!(is_self_echo(
        &serde_json::json!({"sender_type": "app", "sender_id": {"app_id": APP}}),
        APP
    ));
    assert!(!is_self_echo(&user_sender(), APP));
    assert!(!is_self_echo(
        &serde_json::json!({"sender_type": "bot", "sender_id": {"app_id": "cli_other"}}),
        APP
    ));
    let event = envelope(
        "p2p",
        "text",
        serde_json::json!({"text": "echo"}),
        serde_json::json!({"sender_type": "bot", "sender_id": {"app_id": APP}}),
    );
    assert!(extract_dm(&event, APP).is_none());
}

#[test]
fn post_paragraphs_flatten_with_at_and_links() {
    let content = serde_json::json!({
        "zh_cn": {"content": [
            [{"tag": "text", "text": "line one"}],
            [{"tag": "md", "text": "bold "}, {"tag": "a", "text": "docs", "href": "https://x"}],
            [{"tag": "at", "user_id": "ou_9", "user_name": "ann"}],
            [{"tag": "img", "image_key": "img_1"}],
        ]},
    });
    let event = envelope("p2p", "post", content, user_sender());
    let text = extract_dm(&event, APP).unwrap().text;
    assert_eq!(text, "line one\nbold docs\n@ann");
}

#[test]
fn post_falls_back_to_the_first_locale_with_content() {
    let content = serde_json::json!({
        "ja_jp": {"content": [[{"tag": "text", "text": "konnichiwa"}]]},
    });
    let event = envelope("p2p", "post", content, user_sender());
    assert_eq!(extract_dm(&event, APP).unwrap().text, "konnichiwa");
}

#[test]
fn non_text_payloads_resolve_to_none() {
    let cases = [
        envelope(
            "p2p",
            "image",
            serde_json::json!({"image_key": "img_1"}),
            user_sender(),
        ),
        envelope(
            "p2p",
            "sticker",
            serde_json::json!({"file_key": "f_1"}),
            user_sender(),
        ),
        envelope(
            "p2p",
            "text",
            serde_json::json!({"text": "   "}),
            user_sender(),
        ),
        envelope("p2p", "text", serde_json::json!({}), user_sender()),
        // content is not JSON at all — dropped, never a panic
        envelope(
            "p2p",
            "text",
            serde_json::Value::String("not json".into()),
            user_sender(),
        ),
        // a different event on the same connection
        serde_json::json!({
            "header": {"event_type": "im.chat.updated"},
            "event": {"sender": user_sender(), "message": {"chat_type": "p2p"}},
        }),
        // no open_id → no peer identity
        envelope(
            "p2p",
            "text",
            serde_json::json!({"text": "hi"}),
            serde_json::json!({"sender_type": "user", "sender_id": {"user_id": "u_1"}}),
        ),
    ];
    for case in cases {
        assert!(extract_dm(&case, APP).is_none(), "case {case}");
    }
}

#[test]
fn region_picks_the_api_domain() {
    assert_eq!(domain(FeishuRegion::FeishuCn), "https://open.feishu.cn");
    assert_eq!(
        domain(FeishuRegion::LarkGlobal),
        "https://open.larksuite.com"
    );
}

#[test]
fn token_request_shape() {
    let (url, body) = token_request("https://open.feishu.cn", "cli_x", "sec");
    assert_eq!(
        url,
        "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal"
    );
    assert_eq!(body["app_id"], "cli_x");
    assert_eq!(body["app_secret"], "sec");
}

#[test]
fn send_request_uses_chat_id_and_a_post_body() {
    let (url, body) = send_request("https://open.feishu.cn", "oc_9", "hi\nthere");
    assert_eq!(
        url,
        "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id"
    );
    assert_eq!(body["receive_id"], "oc_9");
    assert_eq!(body["msg_type"], "post");
    let content: serde_json::Value =
        serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["zh_cn"]["content"][0][0]["tag"], "md");
    assert_eq!(content["zh_cn"]["content"][0][0]["text"], "hi");
    assert_eq!(content["zh_cn"]["content"][1][0]["text"], "there");
}

#[test]
fn edit_request_targets_the_message_id() {
    let (url, body) = edit_request("https://open.feishu.cn", "om_7", "draft");
    assert_eq!(url, "https://open.feishu.cn/open-apis/im/v1/messages/om_7");
    assert_eq!(body["msg_type"], "post");
    assert!(body.get("receive_id").is_none());
}

#[test]
fn service_id_comes_off_the_endpoint_query() {
    let url = "wss://example.feishu.cn/connect?device_id=d1&service_id=33445566&app_id=x";
    assert_eq!(service_id(url), 33_445_566);
    assert_eq!(service_id("wss://example.feishu.cn/connect"), 0);
    assert_eq!(service_id("not a url"), 0);
}

#[test]
fn chunks_under_limit() {
    assert_eq!(chunk("short"), vec!["short".to_string()]);
    let long = "a".repeat(MSG_LIMIT * 2 + 5);
    let parts = chunk(&long);
    assert_eq!(parts.len(), 3);
    assert!(parts.iter().all(|p| p.len() <= MSG_LIMIT));
    assert_eq!(parts.concat(), long);
}

#[test]
fn chunks_prefer_paragraph_breaks() {
    let text = format!("{}\n\n{}", "x".repeat(3000), "y".repeat(2000));
    let parts = chunk(&text);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], "x".repeat(3000) + "\n");
}

#[test]
fn chunked_events_assemble_in_seq_order() {
    let mut bufs = HashMap::new();
    assert!(matches!(
        merge_chunk(&mut bufs, "om_1", 1, 0, b"whole"),
        ChunkOutcome::Complete(p) if p == b"whole"
    ));
    // first half arrives → nothing yet, and the slot is held
    assert!(matches!(
        merge_chunk(&mut bufs, "om_2", 2, 0, b"A"),
        ChunkOutcome::Pending
    ));
    assert!(bufs.contains_key("om_2"), "half an event is not an event");
    // arrival order does not matter, seq order does
    assert!(matches!(
        merge_chunk(&mut bufs, "om_2", 2, 1, b"B"),
        ChunkOutcome::Complete(p) if p == b"AB"
    ));
    assert!(!bufs.contains_key("om_2"), "a completed event is released");
    // an out-of-range seq can never assemble: refused, not held
    assert!(matches!(
        merge_chunk(&mut bufs, "om_4", 2, 5, b"X"),
        ChunkOutcome::Refused
    ));
    assert!(!bufs.contains_key("om_4"));
}

#[test]
fn stale_chunks_are_abandoned() {
    let mut bufs = HashMap::new();
    bufs.insert(
        "om_old".to_string(),
        (Instant::now() - CHUNK_TTL, vec![Some(b"A".to_vec()), None]),
    );
    assert!(
        matches!(
            merge_chunk(&mut bufs, "om_old", 2, 1, b"B"),
            ChunkOutcome::Pending
        ),
        "the stale slot must not complete the event"
    );
}

/// The `sum` header sizes an allocation before any payload arrives, so an
/// absurd value has to be refused rather than trusted — and refused is not
/// "still waiting", because the caller has to act on the difference (an
/// unacked frame is redelivered forever).
#[test]
fn an_absurd_chunk_count_is_refused_not_allocated() {
    let mut bufs = HashMap::new();
    assert!(matches!(
        merge_chunk(&mut bufs, "om_big", usize::MAX, 0, b"A"),
        ChunkOutcome::Refused
    ));
    assert!(bufs.is_empty(), "no slot vector may be allocated for it");
    assert!(matches!(
        merge_chunk(&mut bufs, "om_big", MAX_CHUNK_SUM + 1, 0, b"A"),
        ChunkOutcome::Refused
    ));
    assert!(bufs.is_empty());
    // the other headers that can never assemble are refused the same way,
    // and none of them leaves state behind for the next frame
    assert!(matches!(
        merge_chunk(&mut bufs, "", 2, 0, b"A"),
        ChunkOutcome::Refused
    ));
    assert!(matches!(
        merge_chunk(&mut bufs, "om_big", 2, 9, b"A"),
        ChunkOutcome::Refused
    ));
    assert!(bufs.is_empty(), "a refused frame must not park a slot");

    // a refused frame does not stick the state machine: a legal event for
    // the same id after it still assembles
    assert!(matches!(
        merge_chunk(&mut bufs, "om_big", 2, 0, b"A"),
        ChunkOutcome::Pending
    ));
    assert!(matches!(
        merge_chunk(&mut bufs, "om_big", 2, 1, b"B"),
        ChunkOutcome::Complete(p) if p == b"AB"
    ));
    assert!(bufs.is_empty());
}

/// `Instant + Duration` panics on an absurd TTL, and a sub-minute one spins
/// the token endpoint.
#[test]
fn the_token_ttl_is_clamped_to_a_sane_window() {
    assert_eq!(
        token_ttl(&serde_json::json!({"expire": u64::MAX})),
        MAX_TTL_SECS
    );
    assert_eq!(token_ttl(&serde_json::json!({"expire": 1})), MIN_TTL_SECS);
    assert_eq!(token_ttl(&serde_json::json!({"expire": 7200})), 7200);
    assert_eq!(token_ttl(&serde_json::json!({"expire": "3600"})), 3600);
    assert_eq!(token_ttl(&serde_json::json!({})), DEFAULT_TTL_SECS);
}

/// The cut is by byte, so a multi-byte character at the limit is where a
/// naive slice would panic or split a glyph.
#[test]
fn chunks_never_split_a_character() {
    let cjk = "中".repeat(MSG_LIMIT);
    let parts = chunk(&cjk);
    assert_eq!(parts.concat(), cjk);
    assert!(parts.iter().all(|p| p.len() <= MSG_LIMIT));
    assert!(
        parts.iter().all(|p| p.chars().all(|c| c == '中')),
        "{parts:?}"
    );

    let crab = "🦀".repeat(MSG_LIMIT / 4 + 3);
    let parts = chunk(&crab);
    assert_eq!(parts.concat(), crab);
    assert!(parts.iter().all(|p| p.len() <= MSG_LIMIT));
    assert!(
        parts.iter().all(|p| p.chars().all(|c| c == '🦀')),
        "{parts:?}"
    );
}

/// The message-edit path is `/open-apis/im/v1/messages/{message_id}`, and
/// reqwest prints the URL of a failed request in its `Display`.
#[tokio::test]
async fn a_transport_error_never_carries_the_message_id() {
    let dir = crate::im::test_dir("feishu-transport");
    std::fs::create_dir_all(&dir).unwrap();
    let secret = dir.join("secret.txt");
    std::fs::write(&secret, "shh").unwrap();
    let adapter = FeishuAdapter::new(&FeishuSpec {
        enabled: true,
        app_id: "cli_x".into(),
        app_secret_env: None,
        app_secret_file: Some(secret),
        region: FeishuRegion::default(),
        owner: None,
        dm_policy: None,
        allowlist: Vec::new(),
    })
    .unwrap();
    // skip the token exchange: the failure under test is the transport one
    *adapter.token.lock().unwrap() =
        Some(("tok".into(), Instant::now() + Duration::from_secs(600)));

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let message_id = "om_1f2e3d4c5b6a";
    let url = format!("http://127.0.0.1:{port}/open-apis/im/v1/messages/{message_id}");
    let err = adapter
        .api_call(reqwest::Method::PATCH, &url, None, "message update")
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(
        !text.contains(message_id),
        "the message id reached the error: {text}"
    );
    assert!(
        !text.contains("http://"),
        "the URL reached the error: {text}"
    );
    assert!(
        text.contains("message update"),
        "the diagnosis is gone: {text}"
    );
    std::fs::remove_dir_all(dir).ok();
}

/// The tenant-token and long-connection endpoints carry no credential in
/// their URL, so this is discipline rather than a leak: every reqwest error
/// on this adapter's credential-bearing paths is stripped of its URL, the
/// same way `api_call` does it for the calls whose URL does carry an id.
#[tokio::test]
async fn neither_the_token_nor_the_ws_endpoint_echoes_its_url() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener); // nothing listens there now

    let dir = crate::im::test_dir("feishu-endpoints");
    std::fs::create_dir_all(&dir).unwrap();
    let secret = dir.join("secret.txt");
    std::fs::write(&secret, "shh").unwrap();
    let mut adapter = FeishuAdapter::new(&FeishuSpec {
        enabled: true,
        app_id: "cli_x".into(),
        app_secret_env: None,
        app_secret_file: Some(secret),
        region: FeishuRegion::default(),
        owner: None,
        dm_policy: None,
        allowlist: Vec::new(),
    })
    .unwrap();
    adapter.base = Box::leak(format!("http://127.0.0.1:{port}").into_boxed_str());
    let authority = format!("127.0.0.1:{port}");

    let err = format!("{:#}", adapter.tenant_token().await.unwrap_err());
    assert!(err.contains("feishu tenant_access_token"), "{err}");
    assert!(!err.contains("http://"), "the URL survived: {err}");
    assert!(!err.contains(&authority), "{err}");

    let err = format!("{:#}", adapter.ws_endpoint().await.unwrap_err());
    assert!(err.contains("feishu ws endpoint"), "{err}");
    assert!(!err.contains("http://"), "the URL survived: {err}");
    assert!(!err.contains(&authority), "{err}");

    std::fs::remove_dir_all(dir).ok();
}
