//! Offline tests for the WeChat adapter: the pure message → `InboundMsg`
//! mapping, the request bodies, the 4000-char cut, the client-generated
//! identity fields, the business-error type, and the store-backed cursor +
//! reply-window bookkeeping. Nothing here touches the network.

use base64::Engine as _;

use super::protocol::*;
use super::*;

fn spec() -> WechatSpec {
    WechatSpec {
        enabled: true,
        bot_token_env: Some("SUNMAO_TEST_WECHAT_TOKEN".into()),
        bot_token_file: None,
        owner: None,
        dm_policy: None,
        allowlist: Vec::new(),
    }
}

fn adapter(store: Arc<Store>) -> WechatAdapter {
    unsafe { std::env::set_var("SUNMAO_TEST_WECHAT_TOKEN", "shh") };
    WechatAdapter::new(&spec(), store).unwrap()
}

fn store() -> (Arc<Store>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "sunmao-im-wechat-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    (Arc::new(Store::open(&dir).unwrap()), dir)
}

fn text_message(from: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "from_user_id": from,
        "context_token": "ctx_1",
        "item_list": [{"type": 1, "text_item": {"text": text}}],
    })
}

#[test]
fn inbound_text_maps_to_a_dm() {
    let msg = extract_dm(&text_message("u_1@im.wechat", "hello bot")).unwrap();
    assert_eq!(msg.source.channel, "wechat");
    assert_eq!(msg.source.chat_id, "u_1@im.wechat");
    assert_eq!(msg.source.sender_id, "u_1@im.wechat");
    // the local part is the only label iLink gives
    assert_eq!(msg.source.sender_name, "u_1");
    assert_eq!(msg.text, "hello bot");
}

#[test]
fn the_bots_own_echo_is_not_a_dm() {
    assert!(extract_dm(&text_message("bot@im.bot", "hi")).is_none());
    assert_eq!(peer_id(&text_message("bot@im.bot", "hi")), None);
    // the reply-window token is still readable off an echo, but the echo is
    // never mapped to a session
    assert_eq!(
        context_token(&text_message("bot@im.bot", "hi")),
        Some("ctx_1")
    );
}

#[test]
fn non_target_payloads_resolve_to_none() {
    let cases = [
        // no sender at all
        serde_json::json!({"item_list": [{"type": 1, "text_item": {"text": "hi"}}]}),
        serde_json::json!({"from_user_id": "", "item_list": []}),
        // text that is empty or whitespace only
        text_message("u_1", ""),
        text_message("u_1", "   "),
        // a media-only message: nothing to read (its reply window still opens)
        serde_json::json!({
            "from_user_id": "u_1",
            "item_list": [{"type": 2, "image_item": {"media": {"encrypt_query_param": "x"}}}],
        }),
        // an item list that is not a list
        serde_json::json!({"from_user_id": "u_1", "item_list": "nope"}),
    ];
    for case in cases {
        assert!(extract_dm(&case).is_none(), "case {case}");
    }
}

#[test]
fn voice_transcription_is_the_body() {
    let voice = serde_json::json!({
        "from_user_id": "u_1",
        "item_list": [{"type": 3, "voice_item": {"text": "spoken"}}],
    });
    assert_eq!(extract_dm(&voice).unwrap().text, "spoken");
    // an untranscribed voice item has no body
    let bare = serde_json::json!({
        "from_user_id": "u_1",
        "item_list": [{"type": 3, "voice_item": {}}],
    });
    assert!(extract_dm(&bare).is_none());
}

#[test]
fn a_quoted_message_is_prefixed() {
    let quoted = serde_json::json!({
        "from_user_id": "u_1",
        "item_list": [{
            "type": 1,
            "text_item": {"text": "my reply"},
            "ref_msg": {"title": "Ada", "message_item": {"type": 1, "text_item": {"text": "original"}}},
        }],
    });
    assert_eq!(
        extract_dm(&quoted).unwrap().text,
        "[引用: Ada | original]\nmy reply"
    );

    // quoting a media item contributes no text — the body stays clean
    let media_quote = serde_json::json!({
        "from_user_id": "u_1",
        "item_list": [{
            "type": 1,
            "text_item": {"text": "my reply"},
            "ref_msg": {"title": "Ada", "message_item": {"type": 2, "image_item": {}}},
        }],
    });
    assert_eq!(extract_dm(&media_quote).unwrap().text, "my reply");
}

#[test]
fn get_updates_body_carries_the_cursor_and_the_version() {
    let body = get_updates_body("buf_7");
    assert_eq!(body["get_updates_buf"], "buf_7");
    assert_eq!(body["base_info"]["channel_version"], CHANNEL_VERSION);
    // a first run asks with an empty cursor
    assert_eq!(get_updates_body("")["get_updates_buf"], "");
}

#[test]
fn send_body_is_a_finished_bot_text_message() {
    let body = send_body("u_1", "hi", "cid_1", "ctx_1");
    assert_eq!(body["msg"]["from_user_id"], "");
    assert_eq!(body["msg"]["to_user_id"], "u_1");
    assert_eq!(body["msg"]["client_id"], "cid_1");
    assert_eq!(body["msg"]["message_type"], 2);
    assert_eq!(body["msg"]["message_state"], 2);
    assert_eq!(body["msg"]["item_list"][0]["type"], 1);
    assert_eq!(body["msg"]["item_list"][0]["text_item"]["text"], "hi");
    assert_eq!(body["msg"]["context_token"], "ctx_1");
    assert_eq!(body["base_info"]["channel_version"], CHANNEL_VERSION);
}

#[test]
fn chunk_counts_characters_not_bytes() {
    assert_eq!(chunk("short"), vec!["short".to_string()]);
    assert_eq!(chunk(""), vec![String::new()]);
    assert_eq!(chunk(&"a".repeat(MSG_LIMIT)).len(), 1);
    assert_eq!(chunk(&"a".repeat(MSG_LIMIT + 1)).len(), 2);

    let long = "a".repeat(MSG_LIMIT * 2 + 5);
    let parts = chunk(&long);
    assert_eq!(parts.len(), 3);
    assert!(parts.iter().all(|p| p.chars().count() <= MSG_LIMIT));
    assert_eq!(parts.concat(), long);

    // a 3-byte character costs one of the 4000, so a chunk may be 12000 bytes
    let cjk = "中".repeat(MSG_LIMIT + 1);
    let parts = chunk(&cjk);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].chars().count(), MSG_LIMIT);
    assert_eq!(parts[0].len(), MSG_LIMIT * 3);
    assert_eq!(parts.concat(), cjk);
}

#[test]
fn the_client_identity_fields_have_the_wire_shape() {
    let uuid = uuid_v4();
    let parts: Vec<&str> = uuid.split('-').collect();
    assert_eq!(parts.len(), 5);
    assert_eq!(
        parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12]
    );
    assert_eq!(parts[2].chars().next(), Some('4'), "v4 marker");
    assert!(
        matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b')),
        "variant marker in {uuid}"
    );
    assert!(uuid.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    assert_ne!(uuid_v4(), uuid, "two sends must not share a client id");

    // X-WECHAT-UIN is base64 of the decimal random uint32 — decoding it has
    // to give a decimal integer back
    let raw = base64::engine::general_purpose::STANDARD
        .decode(random_uin())
        .unwrap();
    let text = String::from_utf8(raw).unwrap();
    assert!(text.parse::<u32>().is_ok(), "{text} is not a uint32");
}

#[test]
fn the_cursor_round_trips_through_the_store() {
    let (store, dir) = store();
    let a = adapter(store.clone());
    assert_eq!(a.load_cursor(), "", "a first run has no cursor");
    a.remember_cursor("buf_1");
    assert_eq!(a.load_cursor(), "buf_1");
    assert_eq!(
        store.kv_get(&cursor_key("shh")).as_deref(),
        Some("buf_1"),
        "the cursor slot carries the bot token's tag"
    );
    assert_eq!(
        store.kv_get("wx:cursor"),
        None,
        "one shared cursor slot hands another bot the wrong position"
    );
    // the cursor survives a rebuilt adapter — that is the whole point
    let b = adapter(store.clone());
    assert_eq!(b.load_cursor(), "buf_1");
    std::fs::remove_dir_all(dir).ok();
}

/// A cursor is a position in one bot's update stream: two tokens must never
/// share the slot, and the same token must keep landing in the same one.
#[test]
fn cursors_are_scoped_to_the_bot_token() {
    assert_ne!(cursor_key("bot-a"), cursor_key("bot-b"));
    assert_ne!(
        context_key("bot-a", "u_1"),
        context_key("bot-b", "u_1"),
        "one peer's reply window belongs to the token that opened it"
    );
    assert_eq!(cursor_key("bot-a"), cursor_key("bot-a"));
    assert!(cursor_key("bot-a").starts_with("wx:cursor:"));
    assert!(context_key("bot-a", "u_1").starts_with("wx:ctx:"));
    assert!(context_key("bot-a", "u_1").ends_with(":u_1"));
    // the token itself is never part of a key
    assert!(!cursor_key("secret-token").contains("secret-token"));
}

#[test]
fn a_reply_window_is_cached_and_expires() {
    let (store, dir) = store();
    let a = adapter(store.clone());
    let key = context_key("shh", "u_1");
    assert!(a.context_of("u_1").is_none(), "no inbound message yet");

    a.remember_context("u_1", "ctx_1");
    assert_eq!(a.context_of("u_1").as_deref(), Some("ctx_1"));
    let stored = store.kv_get(&key).unwrap();
    assert!(stored.starts_with("ctx_1\t"), "{stored}");
    assert_eq!(
        decode_context(&stored, crate::im::store::now()).unwrap().0,
        "ctx_1"
    );

    // 25 hours later the platform window is closed
    let stale = encode_context("ctx_1", crate::im::store::now() - 25 * 60 * 60);
    store.kv_set(&key, &stale).unwrap();
    assert!(a.context_of("u_1").is_none());
    assert_eq!(
        store.kv_get(&key).as_deref(),
        Some(""),
        "an expired window is cleared, not re-expired on every send"
    );

    // just inside the window it is still live
    let fresh = encode_context("ctx_2", crate::im::store::now() - 60);
    store.kv_set(&key, &fresh).unwrap();
    assert_eq!(a.context_of("u_1").as_deref(), Some("ctx_2"));
    assert_eq!(decode_context("no-tab", 0), None);
    assert_eq!(decode_context("\t5", 5), None, "an empty token is no token");
    std::fs::remove_dir_all(dir).ok();
}

/// A message with nothing to read still carries the reply window — the next
/// reply must be able to answer it.
#[test]
fn an_unreadable_message_still_opens_the_reply_window() {
    let (store, dir) = store();
    let a = adapter(store.clone());
    let image_only = serde_json::json!({
        "from_user_id": "u_1",
        "context_token": "ctx_1",
        "item_list": [{"type": 2, "image_item": {"media": {"encrypt_query_param": "x"}}}],
    });
    assert!(a.absorb(&image_only).is_none(), "nothing to read");
    assert_eq!(a.context_of("u_1").as_deref(), Some("ctx_1"));

    let reply = text_message("u_1", "hi");
    assert_eq!(a.absorb(&reply).unwrap().text, "hi");
    std::fs::remove_dir_all(dir).ok();
}

/// Without a live `context_token` there is nothing to answer into, and the
/// platform would drop the message: the send refuses instead of going quiet.
#[tokio::test]
async fn sending_without_a_reply_window_is_an_error() {
    let (store, dir) = store();
    let a = adapter(store.clone());
    let err = a
        .send_message("u_1", "hello")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("reply window") && err.contains("u_1"), "{err}");

    a.remember_context("u_1", "ctx_1");
    let stale = encode_context("ctx_1", crate::im::store::now() - 25 * 60 * 60);
    store.kv_set(&context_key("shh", "u_1"), &stale).unwrap();
    assert!(
        a.send_message("u_1", "hello").await.is_err(),
        "an expired window is not a window"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_revoked_session_is_told_apart_from_a_transient_failure() {
    let expired = IlinkError {
        endpoint: GET_UPDATES_PATH.into(),
        ret: -14,
        errcode: Some(-14),
        errmsg: "session expired".into(),
    };
    assert!(expired.is_session_expired());
    assert!(expired.to_string().contains("ret=-14"));
    assert!(expired.to_string().contains("errmsg=session expired"));

    // the poll loop recovers the same verdict through the anyhow chain
    let boxed = anyhow::Error::from(expired.clone());
    let recovered = boxed.downcast_ref::<IlinkError>().unwrap();
    assert!(recovered.is_session_expired());

    let transient = business_error("ilink/bot/getupdates", &serde_json::json!({"ret": 1})).unwrap();
    assert!(!transient.is_session_expired());
    assert!(transient.to_string().contains("errcode=-"));
}

#[test]
fn only_a_non_zero_ret_is_a_business_error() {
    assert!(business_error("ep", &serde_json::json!({})).is_none());
    assert!(business_error("ep", &serde_json::json!({"ret": 0})).is_none());
    assert!(business_error("ep", &serde_json::json!({"ret": null})).is_none());

    let err = business_error(
        "ep",
        &serde_json::json!({"ret": -14, "errcode": -14, "errmsg": "gone"}),
    )
    .unwrap();
    assert_eq!(err.ret, -14);
    assert_eq!(err.errcode, Some(-14));
    assert!(err.is_session_expired());
}

/// iLink spells `ret` as a number or a string; reading only the numeric form
/// turns a dead session into a success and the expired-session stop never
/// fires.
#[test]
fn a_string_ret_reads_the_same_as_a_number() {
    let expired =
        business_error("ep", &serde_json::json!({"ret": "-14", "errmsg": "gone"})).unwrap();
    assert_eq!(expired.ret, -14);
    assert!(expired.is_session_expired());
    assert!(expired.to_string().contains("ret=-14"), "{expired}");

    // a string zero is still a success, and a non-numeric one is no ret
    assert!(business_error("ep", &serde_json::json!({"ret": "0"})).is_none());
    assert!(business_error("ep", &serde_json::json!({"ret": "nope"})).is_none());

    // errcode may be spelled as a string too
    let by_errcode =
        business_error("ep", &serde_json::json!({"ret": 1, "errcode": "-14"})).unwrap();
    assert_eq!(by_errcode.errcode, Some(-14));
    assert!(by_errcode.is_session_expired());
}
