//! Offline tests for the DingTalk adapter: the pure payload → `InboundMsg`
//! mapping, the envelope classification + ACK, the byte-budget cut, the
//! token/registration request shapes, and the response envelope check.
//! Nothing here touches the network.

use super::api::*;
use super::protocol::*;

const ROBOT: &str = "ding_robot";

fn dm(text: &str) -> serde_json::Value {
    serde_json::json!({
        "msgtype": "text",
        "text": {"content": text},
        "conversationType": "1",
        "conversationId": "cid_1",
        "senderStaffId": "staff_1",
        "senderId": "sid_1",
        "senderNick": "Ada",
        "robotCode": ROBOT,
        "msgId": "msg_1",
    })
}

fn envelope(payload: &serde_json::Value, message_id: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "CALLBACK",
        "headers": {"topic": CALLBACK_TOPIC, "messageId": message_id},
        "data": payload.to_string(),
    })
}

#[test]
fn dm_callback_maps_to_inbound_dm() {
    let msg = extract_dm(&dm("  hello bot  "), ROBOT).unwrap();
    assert_eq!(msg.source.channel, "dingtalk");
    // the reply target is the *user* on this channel, not the conversation
    assert_eq!(msg.source.chat_id, "staff_1");
    assert_eq!(msg.source.sender_id, "staff_1");
    assert_eq!(msg.source.sender_name, "Ada");
    assert_eq!(msg.text, "  hello bot  ");
}

#[test]
fn group_traffic_is_not_a_dm() {
    for conversation_type in [serde_json::json!("2"), serde_json::json!(2)] {
        let mut payload = dm("hi");
        payload["conversationType"] = conversation_type.clone();
        assert!(
            extract_dm(&payload, ROBOT).is_none(),
            "conversationType {conversation_type} must not enter the DM-only gateway"
        );
    }
    // an absent conversationType is not a DM either — never guess a group
    let mut payload = dm("hi");
    payload.as_object_mut().unwrap().remove("conversationType");
    assert!(extract_dm(&payload, ROBOT).is_none());
}

#[test]
fn another_robots_callback_is_not_ours() {
    let mut payload = dm("hi");
    payload["robotCode"] = serde_json::json!("other_robot");
    assert!(extract_dm(&payload, ROBOT).is_none());
    assert!(!robot_code_matches(&payload, ROBOT));
    // a payload that names no robot cannot disagree with this one
    let mut anonymous = dm("hi");
    anonymous.as_object_mut().unwrap().remove("robotCode");
    assert!(robot_code_matches(&anonymous, ROBOT));
    assert!(extract_dm(&anonymous, ROBOT).is_some());
}

#[test]
fn sender_falls_back_through_the_id_fields() {
    let mut payload = dm("hi");
    payload.as_object_mut().unwrap().remove("senderStaffId");
    payload["senderNick"] = serde_json::json!("");
    let msg = extract_dm(&payload, ROBOT).unwrap();
    assert_eq!(msg.source.sender_id, "sid_1");
    // no display label → the id is the label (never a placeholder name)
    assert_eq!(msg.source.sender_name, "sid_1");
}

#[test]
fn audio_and_rich_text_carry_their_body() {
    let audio = serde_json::json!({
        "msgtype": "audio",
        "content": {"recognition": "spoken text"},
        "conversationType": "1",
        "senderStaffId": "staff_1",
        "robotCode": ROBOT,
    });
    assert_eq!(extract_dm(&audio, ROBOT).unwrap().text, "spoken text");

    let rich = serde_json::json!({
        "msgType": "richText",
        "content": {"richText": [{"text": "a"}, {"text": "b"}, {"image": "x"}]},
        "conversationType": 1,
        "senderStaffId": "staff_1",
    });
    assert_eq!(extract_dm(&rich, ROBOT).unwrap().text, "ab");
}

#[test]
fn non_target_payloads_resolve_to_none() {
    let cases = [
        // media and other message types are out of scope
        serde_json::json!({
            "msgtype": "picture",
            "content": {"pic": "x"},
            "conversationType": "1",
            "senderStaffId": "staff_1",
        }),
        // every text shape is empty
        serde_json::json!({
            "msgtype": "text",
            "text": {"content": "   "},
            "content": {"text": ""},
            "conversationType": "1",
            "senderStaffId": "staff_1",
        }),
        // no peer identity
        serde_json::json!({
            "msgtype": "text",
            "text": {"content": "hi"},
            "conversationType": "1",
        }),
        // no payload body at all
        serde_json::json!({"conversationType": "1", "senderStaffId": "staff_1"}),
    ];
    for case in cases {
        assert!(extract_dm(&case, ROBOT).is_none(), "case {case}");
    }
}

#[test]
fn oversized_bodies_are_truncated_at_a_character_boundary() {
    let long = "中".repeat(MAX_INBOUND_CHARS + 10);
    let text = extract_dm(&dm(&long), ROBOT).unwrap().text;
    assert_eq!(text.chars().count(), MAX_INBOUND_CHARS);
    assert!(text.chars().all(|c| c == '中'));
}

#[test]
fn envelope_classification() {
    let callback = envelope(&dm("hi"), "mid_1");
    assert!(is_callback(&callback));
    assert!(!is_disconnect(&callback));
    assert_eq!(message_id(&callback), Some("mid_1"));
    assert_eq!(callback_data(&callback).unwrap()["msgtype"], "text");

    let disconnect = serde_json::json!({
        "type": "SYSTEM",
        "headers": {"topic": "disconnect"},
    });
    assert!(is_disconnect(&disconnect));
    assert!(!is_callback(&disconnect));
    assert_eq!(message_id(&disconnect), None);

    // another topic on the same connection is not a callback
    let other = serde_json::json!({
        "type": "CALLBACK",
        "headers": {"topic": "/v1.0/im/other", "messageId": "mid_2"},
    });
    assert!(!is_callback(&other));
    // an unparsable `data` is not a payload
    let broken = serde_json::json!({
        "type": "CALLBACK",
        "headers": {"topic": CALLBACK_TOPIC, "messageId": "mid_3"},
        "data": "not json",
    });
    assert!(callback_data(&broken).is_none());
}

#[test]
fn ack_answers_ok_on_the_same_message_id() {
    let ack = ack("mid_7");
    assert_eq!(ack["code"], 200);
    assert_eq!(ack["headers"]["contentType"], "application/json");
    assert_eq!(ack["headers"]["messageId"], "mid_7");
    assert_eq!(ack["message"], "OK");
    assert_eq!(ack["data"], "{}");
}

#[test]
fn open_body_subscribes_to_the_bot_callback_topic() {
    let body = open_body("key", "sec");
    assert_eq!(body["clientId"], "key");
    assert_eq!(body["clientSecret"], "sec");
    assert_eq!(body["subscriptions"][0]["topic"], CALLBACK_TOPIC);
    assert_eq!(body["subscriptions"][0]["type"], "CALLBACK");
    assert_eq!(body["ua"], UA);
}

#[test]
fn send_body_is_a_markdown_robot_message() {
    let body = send_body(ROBOT, "staff_1", "hello", 0, 1);
    assert_eq!(body["robotCode"], ROBOT);
    assert_eq!(body["userIds"], serde_json::json!(["staff_1"]));
    assert_eq!(body["msgKey"], "sampleMarkdown");
    let param: serde_json::Value =
        serde_json::from_str(body["msgParam"].as_str().unwrap()).unwrap();
    assert_eq!(param["title"], TITLE);
    assert_eq!(param["text"], "hello");

    // a multi-chunk reply numbers itself off the title
    let numbered = send_body(ROBOT, "staff_1", "part two", 1, 3);
    let param: serde_json::Value =
        serde_json::from_str(numbered["msgParam"].as_str().unwrap()).unwrap();
    assert_eq!(param["title"], "sunmao 2/3");
}

#[test]
fn chunk_is_a_byte_budget_over_characters() {
    assert_eq!(chunk("short"), vec!["short".to_string()]);
    assert_eq!(chunk(""), vec![String::new()]);

    let ascii = "a".repeat(MAX_MSG_BYTES * 2 + 5);
    let parts = chunk(&ascii);
    assert_eq!(parts.len(), 3);
    assert!(parts.iter().all(|p| p.len() <= MAX_MSG_BYTES));
    assert_eq!(parts.concat(), ascii);

    // a 3-byte character still fits exactly 4000 to a chunk
    let cjk = "中".repeat(MAX_MSG_BYTES / 3 + 5);
    let parts = chunk(&cjk);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].chars().count(), MAX_MSG_BYTES / 3);
    assert!(parts.iter().all(|p| p.len() <= MAX_MSG_BYTES));
    assert_eq!(parts.concat(), cjk);

    // the cut lands before the character that would overflow the chunk
    let mixed = format!("{}{}", "x".repeat(MAX_MSG_BYTES - 1), "中中");
    let parts = chunk(&mixed);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], "x".repeat(MAX_MSG_BYTES - 1));
    assert_eq!(parts.concat(), mixed);
}

#[test]
fn token_request_targets_the_corp_scoped_endpoint() {
    let (url, body) = token_request("https://api.dingtalk.com/v1.0", "corp 1/x", "key", "sec");
    assert_eq!(
        url,
        "https://api.dingtalk.com/v1.0/oauth2/corp%201%2Fx/token"
    );
    assert_eq!(body["client_id"], "key");
    assert_eq!(body["client_secret"], "sec");
    assert_eq!(body["grant_type"], "client_credentials");
}

#[test]
fn urls_join_without_doubling_the_slash() {
    assert_eq!(
        open_url("https://api.dingtalk.com/v1.0/"),
        "https://api.dingtalk.com/v1.0/gateway/connections/open"
    );
    assert_eq!(
        dm_send_url("https://api.dingtalk.com/v1.0"),
        "https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend"
    );
}

#[test]
fn append_ticket_keeps_the_endpoints_own_query() {
    let url = append_ticket("wss://x.y/connect?device_id=d1", "tk").unwrap();
    assert!(url.contains("device_id=d1"), "{url}");
    assert!(url.contains("ticket=tk"), "{url}");
    assert!(append_ticket("not a url", "tk").is_err());
}

#[test]
fn token_answer_parses_and_clamps() {
    let ok = serde_json::json!({"access_token": "tok", "expires_in": 7200});
    assert_eq!(access_token(&ok).unwrap(), ("tok".to_string(), 7200));
    // a TTL the platform cannot mean is clamped, not trusted
    let long = serde_json::json!({"access_token": "tok", "expires_in": 999_999});
    assert_eq!(access_token(&long).unwrap().1, 86_400);
    let short = serde_json::json!({"access_token": "tok", "expires_in": 1});
    assert_eq!(access_token(&short).unwrap().1, 60);
    // no TTL at all falls back to the documented default
    assert_eq!(
        access_token(&serde_json::json!({"access_token": "tok"}))
            .unwrap()
            .1,
        7200
    );

    let bad = serde_json::json!({"code": "invalidParameter", "message": "bad secret"});
    let err = access_token(&bad).unwrap_err().to_string();
    assert!(
        err.contains("invalidParameter") && err.contains("bad secret"),
        "{err}"
    );
}

#[test]
fn registration_answer_needs_both_halves() {
    let ok = serde_json::json!({"endpoint": "wss://x.y/connect", "ticket": "tk"});
    assert_eq!(
        stream_registration(&ok).unwrap(),
        ("wss://x.y/connect".to_string(), "tk".to_string())
    );
    assert!(stream_registration(&serde_json::json!({"endpoint": "wss://x.y"})).is_err());
    assert!(stream_registration(&serde_json::json!({"ticket": "tk"})).is_err());
}

#[test]
fn the_response_envelope_carries_both_error_surfaces() {
    // no code and a 2xx is a success
    assert!(check_response("send", reqwest::StatusCode::OK, &serde_json::json!({})).is_ok());
    assert!(
        check_response(
            "send",
            reqwest::StatusCode::OK,
            &serde_json::json!({"code": 0})
        )
        .is_ok()
    );
    // a non-zero code under HTTP 200 is still a failure
    let err = check_response(
        "send",
        reqwest::StatusCode::OK,
        &serde_json::json!({"code": 400, "message": "nope"}),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("code=400") && err.contains("nope"), "{err}");
    // and the v1.0 API spells the code as a name
    let named = check_response(
        "send",
        reqwest::StatusCode::OK,
        &serde_json::json!({"code": "invalidParameter", "message": "nope"}),
    )
    .unwrap_err()
    .to_string();
    assert!(named.contains("invalidParameter"), "{named}");
    // a 4xx with no business code is a failure too
    assert!(
        check_response(
            "send",
            reqwest::StatusCode::BAD_REQUEST,
            &serde_json::json!({})
        )
        .is_err()
    );
}
