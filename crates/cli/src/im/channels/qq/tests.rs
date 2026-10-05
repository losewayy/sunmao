//! Offline tests for the QQ adapter: the pure event → `InboundMsg`
//! mapping (group traffic included, which must stay dropped), the
//! frame/heartbeat builders, the 2000-char cut, and the store-backed
//! target/passive bookkeeping. Nothing here touches the network.

use super::protocol::*;
use super::*;

fn spec() -> QqSpec {
    QqSpec {
        enabled: true,
        app_id: "1024".into(),
        app_secret_env: Some("SUNMAO_TEST_QQ_SECRET".into()),
        app_secret_file: None,
        owner: None,
        dm_policy: None,
        allowlist: Vec::new(),
    }
}

fn adapter(store: Arc<Store>) -> QqAdapter {
    unsafe { std::env::set_var("SUNMAO_TEST_QQ_SECRET", "shh") };
    QqAdapter::new(&spec(), store).unwrap()
}

fn store() -> (Arc<Store>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "sunmao-im-qq-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    (Arc::new(Store::open(&dir).unwrap()), dir)
}

fn c2c(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "msg_1",
        "content": content,
        "author": {"id": "u_1", "user_openid": "ou_c2c"},
    })
}

fn group(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "msg_2",
        "group_openid": "gp_1",
        "content": content,
        "author": {"member_openid": "ou_member"},
        "member": {"card": "Ada", "nickname": "user"},
    })
}

#[test]
fn c2c_event_maps_to_a_user_target() {
    let (msg, target) = extract_event("C2C_MESSAGE_CREATE", &c2c("  hi bot  ")).unwrap();
    assert_eq!(target, ChatTarget::User);
    assert_eq!(msg.source.channel, "qq");
    assert_eq!(msg.source.chat_id, "ou_c2c");
    assert_eq!(msg.source.sender_id, "u_1");
    assert_eq!(msg.source.sender_name, "u_1");
    assert_eq!(msg.text, "hi bot");
}

#[test]
fn c2c_falls_back_to_the_openid_when_the_author_has_no_id() {
    let d = serde_json::json!({
        "id": "msg_1",
        "content": "hey",
        "author": {"user_openid": "ou_only"},
    });
    let (msg, target) = extract_event("C2C_MESSAGE_CREATE", &d).unwrap();
    assert_eq!(target, ChatTarget::User);
    assert_eq!(msg.source.chat_id, "ou_only");
    assert_eq!(msg.source.sender_id, "ou_only");
}

/// Groups are out of scope for this version: a group reply would hand a
/// pairing code to an unpaired stranger in front of the whole room, so the
/// event is dropped even though the gateway still receives it.
#[test]
fn group_events_are_dropped() {
    assert!(
        extract_event("GROUP_AT_MESSAGE_CREATE", &group("<@!123> hello")).is_none(),
        "a group message must never enter the DM-only gateway"
    );
}

#[test]
fn placeholder_names_are_not_names() {
    let d = serde_json::json!({
        "content": "hi",
        "author": {"user_openid": "ou_1", "username": "user"},
        "member": {"card": "user"},
    });
    let (msg, _) = extract_event("C2C_MESSAGE_CREATE", &d).unwrap();
    assert_eq!(msg.source.sender_name, "ou_1");
}

#[test]
fn non_target_events_resolve_to_none() {
    let cases = [
        // guild paths are not subscribed
        ("AT_MESSAGE_CREATE", c2c("hi")),
        ("DIRECT_MESSAGE_CREATE", c2c("hi")),
        ("GROUP_ADD_ROBOT", c2c("hi")),
        // groups are dropped, with or without a readable body
        ("GROUP_AT_MESSAGE_CREATE", group("<@!123> hello")),
        ("GROUP_AT_MESSAGE_CREATE", group("")),
        // an event with no text is not a message
        ("C2C_MESSAGE_CREATE", c2c("")),
        ("C2C_MESSAGE_CREATE", c2c("   ")),
        // no author identity
        (
            "C2C_MESSAGE_CREATE",
            serde_json::json!({"content": "hi", "author": {}}),
        ),
        ("C2C_MESSAGE_CREATE", serde_json::json!({"content": "hi"})),
    ];
    for (kind, d) in cases {
        assert!(extract_event(kind, &d).is_none(), "{kind} {d}");
    }
}

#[test]
fn chunk_is_a_hard_cut_at_the_limit() {
    let long = "a".repeat(MSG_LIMIT * 2 + 5);
    let parts = chunk(&long);
    assert_eq!(parts.len(), 3);
    assert!(parts.iter().all(|p| p.len() <= MSG_LIMIT));
    assert_eq!(parts.concat(), long);
    // no paragraph preference: the reference slices at exactly the limit
    let text = format!("{}\n\n{}", "x".repeat(1500), "y".repeat(1000));
    let parts = chunk(&text);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].len(), MSG_LIMIT);
}

#[test]
fn token_request_shape() {
    let (url, body) = token_request("1024", "sec");
    assert_eq!(url, "https://bots.qq.com/app/getAppAccessToken");
    assert_eq!(body["appId"], "1024");
    assert_eq!(body["clientSecret"], "sec");
}

#[test]
fn identify_and_resume_frames() {
    let identify = identify_frame("QQBot tok");
    assert_eq!(identify["op"], op::IDENTIFY);
    assert_eq!(identify["d"]["token"], "QQBot tok");
    assert_eq!(identify["d"]["intents"], INTENTS);
    assert_eq!(identify["d"]["shard"], serde_json::json!([0, 1]));

    let resume = resume_frame("QQBot tok", "sess-1", Some(42));
    assert_eq!(resume["op"], op::RESUME);
    assert_eq!(resume["d"]["session_id"], "sess-1");
    assert_eq!(resume["d"]["seq"], 42);
}

#[test]
fn heartbeat_carries_the_last_sequence() {
    assert_eq!(heartbeat_frame(None)["op"], op::HEARTBEAT);
    assert!(heartbeat_frame(None)["d"].is_null());
    assert_eq!(heartbeat_frame(Some(9))["d"], 9);
}

#[test]
fn send_body_carries_markdown_and_passive_fields() {
    let plain = send_body("hello", None);
    assert_eq!(plain["msg_type"], 2);
    assert_eq!(plain["markdown"]["content"], "hello");
    assert_eq!(plain["content"], " ");
    assert!(plain.get("msg_id").is_none());

    let passive = send_body("hello", Some(("msg_1", 2)));
    assert_eq!(passive["msg_id"], "msg_1");
    assert_eq!(passive["msg_seq"], 2);
    assert_eq!(passive["markdown"]["content"], "hello");
}

#[test]
fn target_paths() {
    assert_eq!(ChatTarget::User.path("ou_1"), "/v2/users/ou_1/messages");
    assert_eq!(ChatTarget::Group.path("gp_1"), "/v2/groups/gp_1/messages");
    assert_eq!(ChatTarget::parse("user"), Some(ChatTarget::User));
    assert_eq!(ChatTarget::parse("group"), Some(ChatTarget::Group));
    assert_eq!(ChatTarget::parse("channel"), None);
}

/// The reply target is learned from the inbound event and kept in the
/// store under the adapter's own prefix — never written back to config.
#[test]
fn target_map_round_trips_under_the_qq_prefix() {
    let (store, dir) = store();
    let a = adapter(store.clone());
    assert_eq!(a.target_of("gp_1"), None);
    a.remember_target("gp_1", ChatTarget::Group);
    assert_eq!(a.target_of("gp_1"), Some(ChatTarget::Group));
    assert_eq!(store.kv_get("qq:target:gp_1").as_deref(), Some("group"));
    std::fs::remove_dir_all(dir).ok();
}

/// The passive reply cursor increments per reply and stops answering once
/// QQ's window has closed.
#[test]
fn passive_cursor_bumps_then_expires() {
    let (store, dir) = store();
    let a = adapter(store.clone());
    assert!(a.next_passive("ou_1").is_none(), "no inbound message yet");
    a.remember_passive("ou_1", "msg_1");
    assert_eq!(a.next_passive("ou_1"), Some(("msg_1".into(), 1)));
    assert_eq!(a.next_passive("ou_1"), Some(("msg_1".into(), 2)));
    store
        .kv_set(
            "qq:reply:ou_1",
            &format!("msg_1\t2\t{}", crate::im::store::now() - 3600),
        )
        .unwrap();
    assert!(
        a.next_passive("ou_1").is_none(),
        "a closed window sends unattached"
    );
    std::fs::remove_dir_all(dir).ok();
}

/// One long answer is several replies to the same `msg_id`, and QQ
/// de-duplicates on `(msg_id, msg_seq)`: a sequence shared across the chunks
/// has the platform drop every chunk after the first.
#[test]
fn every_chunk_of_one_reply_gets_its_own_seq() {
    let (store, dir) = store();
    let a = adapter(store);
    a.remember_passive("ou_1", "msg_1");
    let bodies = a.reply_bodies("ou_1", &"a".repeat(MSG_LIMIT * 2 + 5));
    assert_eq!(bodies.len(), 3);
    let seqs: Vec<i64> = bodies
        .iter()
        .map(|b| b["msg_seq"].as_i64().unwrap())
        .collect();
    assert_eq!(
        seqs,
        vec![1, 2, 3],
        "duplicate (msg_id, msg_seq) pairs are dropped by the platform"
    );
    assert!(bodies.iter().all(|b| b["msg_id"] == "msg_1"), "{bodies:?}");
    assert!(
        bodies.iter().all(|b| b["markdown"]["content"]
            .as_str()
            .is_some_and(|c| !c.is_empty())),
        "{bodies:?}"
    );
    std::fs::remove_dir_all(dir).ok();
}

/// No open passive window: every chunk still goes out, just unattached.
#[test]
fn every_chunk_of_a_closed_window_sends_unattached() {
    let (store, dir) = store();
    let a = adapter(store);
    let bodies = a.reply_bodies("ou_1", &"b".repeat(MSG_LIMIT + 1));
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies.iter().all(|b| b.get("msg_id").is_none()),
        "{bodies:?}"
    );
    assert!(
        bodies.iter().all(|b| b.get("msg_seq").is_none()),
        "{bodies:?}"
    );
    std::fs::remove_dir_all(dir).ok();
}

/// `Instant + Duration` panics on an absurd TTL, and a sub-minute one spins
/// the token endpoint, so both ends are clamped.
#[test]
fn the_token_ttl_is_clamped_to_a_sane_window() {
    assert_eq!(
        token_ttl(&serde_json::json!({"expires_in": u64::MAX})),
        MAX_TTL_SECS
    );
    assert_eq!(
        token_ttl(&serde_json::json!({"expires_in": 1})),
        MIN_TTL_SECS
    );
    assert_eq!(
        token_ttl(&serde_json::json!({"expires_in": 0})),
        MIN_TTL_SECS
    );
    assert_eq!(token_ttl(&serde_json::json!({"expires_in": 7200})), 7200);
    // the platform sometimes spells it as a string
    assert_eq!(token_ttl(&serde_json::json!({"expires_in": "3600"})), 3600);
    assert_eq!(token_ttl(&serde_json::json!({})), DEFAULT_TTL_SECS);
}

/// The unknown-chat probe must never guess "group": this version carries no
/// group events, so a guessed group endpoint is a message posted into a room.
#[test]
fn an_unknown_chat_is_only_ever_probed_as_a_dm() {
    assert_eq!(send_candidates(None), [ChatTarget::User].as_slice());
    assert_eq!(
        send_candidates(Some(ChatTarget::User)),
        [ChatTarget::User].as_slice()
    );
    // a target written by an earlier version still routes where it pointed
    assert_eq!(
        send_candidates(Some(ChatTarget::Group)),
        [ChatTarget::Group].as_slice()
    );
}
