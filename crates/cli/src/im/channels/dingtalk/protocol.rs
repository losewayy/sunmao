//! The DingTalk Stream wire shapes, kept apart from the connection: the
//! gateway registration body, the callback envelope classification, the
//! payload → `InboundMsg` mapping, the ACK, the robot message body, and the
//! UTF-8 byte cut. Every function here is pure and offline-testable.

use crate::im::channels::InboundMsg;
use crate::im::route::ImSource;

/// The one callback topic this bot subscribes to.
pub const CALLBACK_TOPIC: &str = "/v1.0/im/bot/messages/get";
/// Gateway registration path — `POST {api_base}/gateway/connections/open`.
pub const STREAM_OPEN_PATH: &str = "/gateway/connections/open";
/// One-to-one robot send path.
pub const DM_SEND_PATH: &str = "/robot/oToMessages/batchSend";
/// The `ua` the registration carries; the platform records it per connection.
pub const UA: &str = "sunmao-im-dingtalk/1.0.0";
/// Outbound text is cut at this many UTF-8 bytes — the platform's own gate
/// counts bytes, so a chunk is a byte budget and never splits a character.
pub const MAX_MSG_BYTES: usize = 12_000;
/// Inbound text beyond this many chars is truncated before it reaches a
/// session (a pathological payload must not become a pathological prompt).
pub const MAX_INBOUND_CHARS: usize = 100_000;
/// Markdown card title; a multi-chunk reply numbers itself off it.
pub const TITLE: &str = "sunmao";
/// `conversationType` value meaning a one-to-one chat. Anything else is a
/// group chat, which this DM-only gateway drops.
const CONVERSATION_DM: &str = "1";

/// The gateway registration body: the callback subscription this bot wants.
pub fn open_body(client_id: &str, client_secret: &str) -> serde_json::Value {
    serde_json::json!({
        "clientId": client_id,
        "clientSecret": client_secret,
        "subscriptions": [{"topic": CALLBACK_TOPIC, "type": "CALLBACK"}],
        "ua": UA,
    })
}

/// The ACK every callback must be answered with. The platform re-delivers an
/// unanswered event, so this is sent before the payload is even parsed.
pub fn ack(message_id: &str) -> serde_json::Value {
    serde_json::json!({
        "code": 200,
        "headers": {"contentType": "application/json", "messageId": message_id},
        "message": "OK",
        "data": "{}",
    })
}

/// `SYSTEM` + topic `disconnect` — the platform is closing this connection.
pub fn is_disconnect(envelope: &serde_json::Value) -> bool {
    frame_type(envelope) == Some("SYSTEM") && topic(envelope) == Some("disconnect")
}

/// A bot callback on the one topic this adapter carries.
pub fn is_callback(envelope: &serde_json::Value) -> bool {
    frame_type(envelope) == Some("CALLBACK") && topic(envelope) == Some(CALLBACK_TOPIC)
}

pub fn frame_type(envelope: &serde_json::Value) -> Option<&str> {
    envelope["type"].as_str()
}

pub fn topic(envelope: &serde_json::Value) -> Option<&str> {
    envelope["headers"]["topic"].as_str()
}

/// The id the ACK has to echo back.
pub fn message_id(envelope: &serde_json::Value) -> Option<&str> {
    envelope["headers"]["messageId"]
        .as_str()
        .filter(|s| !s.is_empty())
}

/// The callback body. `data` is a JSON *string* on the wire; an already
/// parsed object is tolerated so fixtures and future SDK-shaped callers both
/// work.
pub fn callback_data(envelope: &serde_json::Value) -> Option<serde_json::Value> {
    match &envelope["data"] {
        serde_json::Value::String(s) => serde_json::from_str(s).ok(),
        v if v.is_object() => Some(v.clone()),
        _ => None,
    }
}

/// Map one callback payload to an inbound DM. Anything that is not a
/// one-to-one text/audio/rich-text message — group traffic, media, another
/// robot's event — resolves to `None`.
pub fn extract_dm(payload: &serde_json::Value, robot_code: &str) -> Option<InboundMsg> {
    if !robot_code_matches(payload, robot_code) || !is_direct(payload) {
        return None;
    }
    let sender_id = principal_id(payload)?;
    let text = extract_text(payload)?;
    // The peer *is* the reply target on this channel: `/robot/oToMessages/
    // batchSend` takes the user id, not a conversation id.
    Some(InboundMsg {
        source: ImSource {
            channel: "dingtalk".into(),
            chat_id: sender_id.clone(),
            sender_id: sender_id.clone(),
            sender_name: display_name(payload).unwrap_or(sender_id),
        },
        text,
    })
}

/// A payload from another robot can land on the same connection — only this
/// robot's messages are ours. An absent `robotCode` is accepted (nothing to
/// disagree with).
pub fn robot_code_matches(payload: &serde_json::Value, robot_code: &str) -> bool {
    match payload["robotCode"].as_str().filter(|c| !c.is_empty()) {
        Some(code) => code == robot_code,
        None => true,
    }
}

/// A one-to-one chat. A missing `conversationType` is *not* a DM (the
/// reference reads it the same way) — an unknown shape must not become a
/// directive to reply to a group.
fn is_direct(payload: &serde_json::Value) -> bool {
    let v = &payload["conversationType"];
    v.as_str() == Some(CONVERSATION_DM) || v.as_i64() == Some(1)
}

/// The peer id: staff id first, then the corp-scoped ids.
fn principal_id(payload: &serde_json::Value) -> Option<String> {
    [
        &payload["senderStaffId"],
        &payload["senderId"],
        &payload["senderUnionId"],
    ]
    .into_iter()
    .find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string))
}

/// The sender's display label; a reader without one gets the id.
fn display_name(payload: &serde_json::Value) -> Option<String> {
    [
        &payload["senderNick"],
        &payload["senderName"],
        &payload["senderStaffName"],
        &payload["sender"]["nick"],
        &payload["sender"]["name"],
    ]
    .into_iter()
    .find_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
    .map(str::to_string)
}

/// The message body. `text` and `audio` (speech recognition) carry it
/// directly; a rich-text payload carries it as an array of text runs.
/// Everything else is dropped — attachments are out of scope for this
/// gateway, and the reference's "send text instead" notice is not protocol.
fn extract_text(payload: &serde_json::Value) -> Option<String> {
    let msgtype = payload["msgtype"]
        .as_str()
        .or_else(|| payload["msgType"].as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let direct = match msgtype.as_str() {
        "text" => first_str([
            &payload["text"]["content"],
            &payload["content"]["text"],
            &payload["content"],
        ]),
        "audio" => first_str([
            &payload["content"]["recognition"],
            &payload["audio"]["recognition"],
        ]),
        _ => None,
    };
    let text = match direct {
        Some(text) => text.to_string(),
        None => payload["content"]["richText"]
            .as_array()?
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect(),
    };
    (!text.trim().is_empty()).then(|| truncate(text))
}

fn first_str<const N: usize>(values: [&serde_json::Value; N]) -> Option<&str> {
    values
        .into_iter()
        .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
}

/// Cap an oversized body at a character boundary.
fn truncate(text: String) -> String {
    if text.chars().count() <= MAX_INBOUND_CHARS {
        return text;
    }
    text.chars().take(MAX_INBOUND_CHARS).collect()
}

/// A markdown robot message: `msgParam` is a JSON *string* the platform
/// renders as a card, so the title/text pair is serialized, not embedded.
pub fn send_body(
    robot_code: &str,
    chat_id: &str,
    text: &str,
    index: usize,
    total: usize,
) -> serde_json::Value {
    let title = if total > 1 {
        format!("{TITLE} {}/{}", index + 1, total)
    } else {
        TITLE.to_string()
    };
    let msg_param = serde_json::json!({"title": title, "text": text}).to_string();
    serde_json::json!({
        "robotCode": robot_code,
        "userIds": [chat_id],
        "msgKey": "sampleMarkdown",
        "msgParam": msg_param,
    })
}

/// Split into MAX_MSG_BYTES-sized pieces — a byte budget over characters, so
/// every chunk fits and no chunk splits a character. Returns at least one
/// (possibly empty) chunk.
pub fn chunk(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut bytes = 0usize;
    for ch in text.chars() {
        let len = ch.len_utf8();
        if !current.is_empty() && bytes + len > MAX_MSG_BYTES {
            out.push(std::mem::take(&mut current));
            bytes = 0;
        }
        current.push(ch);
        bytes += len;
    }
    out.push(current);
    out
}
