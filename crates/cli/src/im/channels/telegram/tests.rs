//! Offline tests for the Telegram adapter: the chunk cut, the
//! update → DM mapping, the credential-scoped offset, the poll-timeout
//! clamp, and the redacted transport error. Nothing here touches the
//! network.

use super::*;
use crate::im::store::Store;
use std::sync::Arc;

const TOKEN: &str = "123456:AAHtesttoken";

fn store() -> (Arc<Store>, std::path::PathBuf) {
    let dir = crate::im::test_dir("telegram");
    (Arc::new(Store::open(&dir).unwrap()), dir)
}

/// A token by file, so the tests never touch process environment.
fn adapter_with(
    store: Arc<Store>,
    dir: &std::path::Path,
    token: &str,
    poll_secs: u64,
) -> TelegramAdapter {
    let secret = dir.join(format!("token-{}.txt", scope::credential_tag(token)));
    std::fs::write(&secret, token).unwrap();
    TelegramAdapter::new(
        &TelegramSpec {
            token_env: None,
            token_file: Some(secret),
            poll_timeout_secs: poll_secs,
            owner: None,
            dm_policy: None,
            allowlist: Vec::new(),
            enabled: true,
        },
        store,
    )
    .unwrap()
}

fn adapter(store: Arc<Store>, dir: &std::path::Path, poll_secs: u64) -> TelegramAdapter {
    adapter_with(store, dir, TOKEN, poll_secs)
}

/// A loopback port nothing listens on.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn update(chat_type: &str, from: serde_json::Value, text: Option<&str>) -> serde_json::Value {
    let mut update = serde_json::json!({
        "update_id": 41,
        "message": {"chat": {"id": 7, "type": chat_type}, "from": from},
    });
    if let Some(text) = text {
        update["message"]["text"] = serde_json::json!(text);
    }
    update
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

    // an emoji is four bytes: the cut has to land on a boundary
    let crab = "🦀".repeat(MSG_LIMIT / 4 + 3);
    let parts = chunk(&crab);
    assert_eq!(parts.concat(), crab);
    assert!(parts.iter().all(|p| p.len() <= MSG_LIMIT));
    assert!(
        parts.iter().all(|p| p.chars().all(|c| c == '🦀')),
        "{parts:?}"
    );
}

#[test]
fn a_private_text_update_maps_to_a_dm() {
    let msg = TelegramAdapter::extract_dm(&update(
        "private",
        serde_json::json!({"id": 9, "first_name": "Ada"}),
        Some("hi bot"),
    ))
    .unwrap();
    assert_eq!(msg.source.channel, "telegram");
    assert_eq!(msg.source.chat_id, "7");
    assert_eq!(msg.source.sender_id, "9");
    assert_eq!(msg.source.sender_name, "Ada");
    assert_eq!(msg.text, "hi bot");
}

#[test]
fn the_sender_label_prefers_the_username_then_the_real_name() {
    let named = TelegramAdapter::extract_dm(&update(
        "private",
        serde_json::json!({"id": 9, "username": "ada", "first_name": "Ada"}),
        Some("hi"),
    ))
    .unwrap();
    assert_eq!(named.source.sender_name, "@ada");

    // no name at all: the numeric id is the honest label
    let bare =
        TelegramAdapter::extract_dm(&update("private", serde_json::json!({"id": 9}), Some("hi")))
            .unwrap();
    assert_eq!(bare.source.sender_name, "9");
}

/// Non-private chats never map to a session, so a room can never be
/// answered — `allowed_updates` filters most of this, but the mapper is
/// the guarantee.
#[test]
fn group_and_channel_updates_are_dropped() {
    for chat_type in ["group", "supergroup", "channel"] {
        assert!(
            TelegramAdapter::extract_dm(&update(
                chat_type,
                serde_json::json!({"id": 9}),
                Some("hi")
            ))
            .is_none(),
            "{chat_type} must not enter the DM-only gateway"
        );
    }
}

#[test]
fn updates_without_a_readable_sender_or_body_are_dropped() {
    let cases = [
        update("private", serde_json::json!({"id": 9}), None),
        update("private", serde_json::json!({"id": 9}), Some("   ")),
        update("private", serde_json::json!({}), Some("hi")),
        update(
            "private",
            serde_json::json!({"id": "not-a-number"}),
            Some("hi"),
        ),
        serde_json::json!({"update_id": 1}),
    ];
    for case in cases {
        assert!(TelegramAdapter::extract_dm(&case).is_none(), "{case}");
    }
}

/// A cursor is a position in one bot's update stream: two tokens must
/// never share the slot, so neither can read or move the other's.
#[test]
fn two_bot_tokens_never_share_an_offset_slot() {
    let (store, dir) = store();
    let a = adapter_with(store.clone(), &dir, "111:AAA", 30);
    let b = adapter_with(store.clone(), &dir, "222:BBB", 30);
    assert_ne!(
        a.offset_key, b.offset_key,
        "one shared offset slot replays or drops updates after a token swap"
    );

    // A advances; B starts clean and cannot see A's position
    assert_eq!(a.advance_offset(41).unwrap(), 42);
    assert_eq!(store.kv_get(&a.offset_key).as_deref(), Some("42"));
    assert_eq!(b.resume_offset(), 0, "B must not inherit A's position");
    assert_eq!(store.kv_get(&b.offset_key), None);

    // B advances; A's slot is untouched
    assert_eq!(b.advance_offset(7).unwrap(), 8);
    assert_eq!(store.kv_get(&b.offset_key).as_deref(), Some("8"));
    assert_eq!(
        store.kv_get(&a.offset_key).as_deref(),
        Some("42"),
        "B's poll must not move A's cursor"
    );

    // the same token keeps landing in the same slot across a restart
    assert_eq!(
        adapter_with(store.clone(), &dir, "111:AAA", 30).offset_key,
        a.offset_key
    );
    std::fs::remove_dir_all(dir).ok();
}

/// The unscoped `tg:offset` a pre-scoping build left behind is not
/// adopted — it cannot be attributed to a credential, and a foreign
/// offset would make the server forget a new bot's early updates.
#[test]
fn the_unscoped_legacy_offset_is_not_adopted() {
    let (store, dir) = store();
    store.kv_set("tg:offset", "99").unwrap();
    let adapter = adapter(store.clone(), &dir, 30);
    assert_eq!(adapter.offset_key, scope::scoped("tg:offset", TOKEN));
    assert_eq!(
        adapter.resume_offset(),
        0,
        "the legacy value must not be read"
    );
    assert_eq!(
        store.kv_get(&adapter.offset_key),
        None,
        "and it must not be copied into the scoped slot either"
    );
    assert_eq!(store.kv_get("tg:offset").as_deref(), Some("99"));
    std::fs::remove_dir_all(dir).ok();
}

/// Clamping is a visible decision, not a silent one: `0` is legal on the
/// Bot API wire (short polling) but wrong for this daemon, and `Some(asked)`
/// is exactly what the warning is emitted from. Asserting the decision
/// instead of scraping a subscriber keeps this deterministic under the whole
/// parallel suite (a thread-local capture here proved flaky).
#[test]
fn a_clamped_poll_timeout_is_moved_and_reported() {
    assert_eq!(clamp_poll_secs(0), (MIN_POLL_SECS, Some(0)));
    assert_eq!(clamp_poll_secs(1), (MIN_POLL_SECS, Some(1)));
    assert_eq!(clamp_poll_secs(30), (30, None));
    assert_eq!(clamp_poll_secs(MAX_POLL_SECS), (MAX_POLL_SECS, None));
    assert_eq!(clamp_poll_secs(10_000), (MAX_POLL_SECS, Some(10_000)));
    assert_eq!(clamp_poll_secs(u64::MAX), (MAX_POLL_SECS, Some(u64::MAX)));
}

#[test]
fn an_absurd_poll_timeout_is_clamped_not_overflowing() {
    let (store, dir) = store();
    for (asked, expected) in [
        (u64::MAX, MAX_POLL_SECS),
        (0, MIN_POLL_SECS),
        (30, 30),
        (10_000, MAX_POLL_SECS),
    ] {
        let adapter = adapter(store.clone(), &dir, asked);
        assert_eq!(adapter.hold_secs, expected, "poll_timeout_secs={asked}");
    }
    std::fs::remove_dir_all(dir).ok();
}

/// The bot token rides the request URL, and reqwest prints that URL into
/// its error `Display` — which is exactly what the poll loop logs and the
/// delivery ledger records.
#[tokio::test]
async fn a_transport_error_never_carries_the_bot_token() {
    let (store, dir) = store();
    let adapter = adapter(store, &dir, MIN_POLL_SECS)
        .with_api_base(format!("http://127.0.0.1:{}/bot{TOKEN}", closed_port()));
    let err = adapter
        .api_call(
            "sendMessage",
            &serde_json::json!({"chat_id": "1", "text": "hi"}),
        )
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(
        !text.contains(TOKEN),
        "the bot token reached the error: {text}"
    );
    assert!(
        !text.contains("http://"),
        "the URL reached the error: {text}"
    );
    assert!(
        text.contains("sendMessage"),
        "the diagnosis is gone: {text}"
    );
    std::fs::remove_dir_all(dir).ok();
}
