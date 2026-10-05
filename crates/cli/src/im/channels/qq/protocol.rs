//! The QQ wire shapes, kept apart from the connection: event → inbound
//! mapping, the WS frame builders, the reply body, the 2000-char cut, and
//! the mention stripper. Every function here is pure and offline-testable.

use crate::im::channels::InboundMsg;
use crate::im::route::ImSource;

/// QQ gates messages at 2000 chars; the reference hard-slices at the same
/// number (a byte gate, not a paragraph boundary).
pub const MSG_LIMIT: usize = 2000;
/// Intents: public guild messages + group/C2C. The group/C2C bit is shared,
/// so group events still arrive on the socket — they are dropped at the
/// mapper, not unsubscribed. Guild-direct messages (`1 << 12`) are
/// deliberately not subscribed: that path needs a guild map this DM gateway
/// has no use for.
pub const INTENTS: u64 = (1 << 30) | (1 << 25);

/// WS opcodes.
pub mod op {
    pub const DISPATCH: i64 = 0;
    pub const HEARTBEAT: i64 = 1;
    pub const IDENTIFY: i64 = 2;
    pub const RESUME: i64 = 6;
    pub const RECONNECT: i64 = 7;
    pub const INVALID_SESSION: i64 = 9;
    pub const HELLO: i64 = 10;
    pub const HEARTBEAT_ACK: i64 = 11;
}

/// Which endpoint a chat's replies go to. Only `C2C_MESSAGE_CREATE` is
/// carried, so `User` is the only target a live event produces; `Group` stays
/// because a store written by an earlier version still holds group entries
/// and the unknown-chat probe still tries both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatTarget {
    User,
    Group,
}

impl ChatTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Group => "group",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "group" => Some(Self::Group),
            _ => None,
        }
    }

    /// The v2 send path for this target.
    pub fn path(self, chat_id: &str) -> String {
        match self {
            Self::User => format!("/v2/users/{chat_id}/messages"),
            Self::Group => format!("/v2/groups/{chat_id}/messages"),
        }
    }
}

/// `POST /app/getAppAccessToken` — AppID + AppSecret for the token.
/// The token host is not the API host.
pub fn token_request(app_id: &str, app_secret: &str) -> (String, serde_json::Value) {
    let url = "https://bots.qq.com/app/getAppAccessToken".to_string();
    let body = serde_json::json!({"appId": app_id, "clientSecret": app_secret});
    (url, body)
}

/// IDENTIFY — the `QQBot ` token prefix is part of the protocol.
pub fn identify_frame(token: &str) -> serde_json::Value {
    serde_json::json!({
        "op": op::IDENTIFY,
        "d": {"token": token, "intents": INTENTS, "shard": [0, 1]},
    })
}

/// RESUME — same session, last sequence number seen.
pub fn resume_frame(token: &str, session_id: &str, seq: Option<i64>) -> serde_json::Value {
    serde_json::json!({
        "op": op::RESUME,
        "d": {"token": token, "session_id": session_id, "seq": seq},
    })
}

/// Heartbeat — `d` is the last sequence number, `null` before the first
/// dispatch.
pub fn heartbeat_frame(seq: Option<i64>) -> serde_json::Value {
    serde_json::json!({"op": op::HEARTBEAT, "d": seq})
}

/// A text reply: markdown content plus, while the passive window is open,
/// the `msg_id`/`msg_seq` pair QQ requires of a reply.
pub fn send_body(text: &str, passive: Option<(&str, i64)>) -> serde_json::Value {
    let mut body =
        serde_json::json!({"content": " ", "msg_type": 2, "markdown": {"content": text}});
    if let Some((msg_id, seq)) = passive {
        body["msg_id"] = serde_json::json!(msg_id);
        body["msg_seq"] = serde_json::json!(seq);
    }
    body
}

/// Split into MSG_LIMIT-sized pieces — a hard cut, like the reference.
pub fn chunk(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > MSG_LIMIT {
        let cut = rest.floor_char_boundary(MSG_LIMIT);
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    out.push(rest.to_string());
    out
}

/// Map one dispatch event to an inbound message + the target its replies
/// must use. `None` for every event this adapter does not carry — including
/// `GROUP_AT_MESSAGE_CREATE`: groups are out of scope for this version, and
/// answering in one would hand a pairing code to an unpaired stranger in a
/// room full of people.
pub fn extract_event(kind: &str, d: &serde_json::Value) -> Option<(InboundMsg, ChatTarget)> {
    let content = d["content"].as_str().unwrap_or("");
    let (chat_id, target, text) = match kind {
        "C2C_MESSAGE_CREATE" => (
            d["author"]["user_openid"]
                .as_str()
                .or_else(|| d["author"]["id"].as_str())?,
            ChatTarget::User,
            content.trim().to_string(),
        ),
        _ => return None,
    };
    let sender_id = principal_id(d)?;
    if chat_id.is_empty() || text.is_empty() {
        return None;
    }
    Some((
        InboundMsg {
            source: ImSource {
                channel: "qq".into(),
                chat_id: chat_id.to_string(),
                sender_id: sender_id.clone(),
                sender_name: display_name(d).unwrap_or(sender_id),
            },
            text,
        },
        target,
    ))
}

/// The sender's id — the reference's order: the author's own id fields
/// first, then the group member profile's.
fn principal_id(d: &serde_json::Value) -> Option<String> {
    let author = &d["author"];
    let profile = if d["member"].is_object() {
        &d["member"]
    } else {
        &d["member_info"]
    };
    [
        &author["id"],
        &author["user_openid"],
        &author["member_openid"],
        &profile["id"],
        &profile["user_openid"],
        &profile["member_openid"],
    ]
    .into_iter()
    .find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string))
}

/// A human label for the transcript. A bare "user" is a placeholder on
/// this platform, not a name (the reference filters the same value).
fn display_name(d: &serde_json::Value) -> Option<String> {
    let author = &d["author"];
    let profile = if d["member"].is_object() {
        &d["member"]
    } else {
        &d["member_info"]
    };
    [
        &profile["card"],
        &profile["member_card"],
        &profile["nick"],
        &profile["nickname"],
        &profile["name"],
        &author["nick"],
        &author["nickname"],
        &author["global_name"],
        &author["username"],
    ]
    .into_iter()
    .find_map(|v| {
        let s = v.as_str()?.trim();
        (!s.is_empty() && !s.eq_ignore_ascii_case("user")).then(|| s.to_string())
    })
}
