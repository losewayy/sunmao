//! The WeChat iLink wire shapes, kept apart from the poll loop: the request
//! bodies, the item → text rendering, the payload → `InboundMsg` mapping,
//! the 4000-char cut, the client-generated identity fields, the reply-window
//! bookkeeping, and the business-error type. Every function here is pure and
//! offline-testable.

use std::fmt;

use crate::im::channels::InboundMsg;
use crate::im::route::ImSource;

/// The iLink API host.
pub const BASE_URL: &str = "https://ilinkai.weixin.qq.com";
/// Incremental pull — a 40s server-side hold.
pub const GET_UPDATES_PATH: &str = "ilink/bot/getupdates";
/// One outbound message per call.
pub const SEND_PATH: &str = "ilink/bot/sendmessage";
/// The protocol version the API demands on every body.
pub const CHANNEL_VERSION: &str = "1.0.0";
/// iLink gates a message at 4000 characters (a character budget, not bytes).
pub const MSG_LIMIT: usize = 4000;
/// `getupdates` holds the connection this long; the HTTP timeout rides above
/// it.
pub const LONG_POLL_SECS: u64 = 40;
/// A `context_token` is the reply window for one peer: 24 hours.
pub const CONTEXT_TTL_SECS: i64 = 24 * 60 * 60;
/// The `get_updates_buf` cursor prefix — persisted, because losing it
/// replays or drops the peer's messages after a restart. Namespaced per bot
/// token by `cursor_key`: a cursor is a position in one bot's update stream.
pub const CURSOR_PREFIX: &str = "wx:cursor";
/// One reply window per peer, keyed under this prefix (and per bot token by
/// `context_key`).
pub const CONTEXT_PREFIX: &str = "wx:ctx:";
/// The bot's own messages come back addressed from this domain — an inbound
/// message from it is our echo, not the peer's.
const BOT_DOMAIN: &str = "@im.bot";
/// `item_list[].type`: text and voice-transcription are the two bodies this
/// gateway renders.
const ITEM_TYPE_TEXT: i64 = 1;
const ITEM_TYPE_VOICE: i64 = 3;
/// The media item types — rendered by their caption, never downloaded.
const ITEM_TYPES_MEDIA: [i64; 4] = [2, 3, 4, 5];
/// The bot is message type 2, and a finished message state is 2.
const MESSAGE_TYPE_BOT: i64 = 2;
const MESSAGE_STATE_FINISH: i64 = 2;

/// `POST ilink/bot/getupdates` — the cursor and the protocol version. An
/// empty cursor asks for the current tail (a fresh bot has no history).
pub fn get_updates_body(cursor: &str) -> serde_json::Value {
    serde_json::json!({
        "get_updates_buf": cursor,
        "base_info": {"channel_version": CHANNEL_VERSION},
    })
}

/// `POST ilink/bot/sendmessage` — one text message inside an open reply
/// window. `client_id` is generated per send; the platform de-duplicates on
/// it, so it must not be reused across chunks.
pub fn send_body(
    chat_id: &str,
    text: &str,
    client_id: &str,
    context_token: &str,
) -> serde_json::Value {
    serde_json::json!({
        "msg": {
            "from_user_id": "",
            "to_user_id": chat_id,
            "client_id": client_id,
            "message_type": MESSAGE_TYPE_BOT,
            "message_state": MESSAGE_STATE_FINISH,
            "item_list": [{"type": ITEM_TYPE_TEXT, "text_item": {"text": text}}],
            "context_token": context_token,
        },
        "base_info": {"channel_version": CHANNEL_VERSION},
    })
}

/// The peer behind one inbound message, or `None` when the message is the
/// bot's own echo or carries no sender.
pub fn peer_id(msg: &serde_json::Value) -> Option<&str> {
    msg["from_user_id"]
        .as_str()
        .filter(|id| !id.is_empty() && !id.ends_with(BOT_DOMAIN))
}

/// The reply window this message opens (or refreshes).
pub fn context_token(msg: &serde_json::Value) -> Option<&str> {
    msg["context_token"].as_str().filter(|t| !t.is_empty())
}

/// Map one inbound message to a DM. Only direct messages exist on this
/// channel, so there is no group case to drop; a message with nothing to
/// read resolves to `None`.
pub fn extract_dm(msg: &serde_json::Value) -> Option<InboundMsg> {
    let peer = peer_id(msg)?;
    let text = extract_text(&msg["item_list"]);
    if text.trim().is_empty() {
        return None;
    }
    let name = peer.split('@').next().filter(|s| !s.is_empty());
    Some(InboundMsg {
        source: ImSource {
            channel: "wechat".into(),
            chat_id: peer.to_string(),
            sender_id: peer.to_string(),
            sender_name: name.unwrap_or(peer).to_string(),
        },
        text,
    })
}

/// The body of the first text-bearing item: a text item's text, or a voice
/// item's transcription. A quoted message is prefixed with what it quoted;
/// a quote of a *media* item contributes no text and the body stays clean.
pub fn extract_text(item_list: &serde_json::Value) -> String {
    item_list
        .as_array()
        .and_then(|items| items.iter().find_map(item_text))
        .unwrap_or_default()
}

fn item_text(item: &serde_json::Value) -> Option<String> {
    match item["type"].as_i64() {
        Some(ITEM_TYPE_TEXT) => {
            let text = item["text_item"]["text"].as_str()?;
            let quoted = &item["ref_msg"];
            if !quoted.is_object() || is_media_item(&quoted["message_item"]) {
                return Some(text.to_string());
            }
            let mut parts = Vec::new();
            if let Some(title) = quoted["title"].as_str().filter(|t| !t.is_empty()) {
                parts.push(title.to_string());
            }
            if let Some(body) = item_text(&quoted["message_item"]).filter(|b| !b.is_empty()) {
                parts.push(body);
            }
            if parts.is_empty() {
                return Some(text.to_string());
            }
            Some(format!("[引用: {}]\n{text}", parts.join(" | ")))
        }
        Some(ITEM_TYPE_VOICE) => item["voice_item"]["text"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

fn is_media_item(item: &serde_json::Value) -> bool {
    matches!(item["type"].as_i64(), Some(t) if ITEM_TYPES_MEDIA.contains(&t))
}

/// Split into MSG_LIMIT-sized pieces by character, so a chunk never splits a
/// character and never exceeds the platform's count. Returns at least one
/// (possibly empty) chunk.
pub fn chunk(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in text.chars() {
        if count == MSG_LIMIT {
            out.push(std::mem::take(&mut current));
            count = 0;
        }
        current.push(ch);
        count += 1;
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

/// The `X-WECHAT-UIN` header: base64 of the decimal random uint32, exactly
/// the reference's shape. Not a secret, only a per-request client marker.
pub fn random_uin() -> String {
    use base64::Engine as _;

    let n = (rand_u64() & 0xffff_ffff) as u32;
    base64::engine::general_purpose::STANDARD.encode(n.to_string())
}

/// The `client_id` the platform de-duplicates on: a v4-shaped UUID. It only
/// has to be unique per send, so `RandomState` keys are a sufficient source —
/// no RNG dependency for an identifier.
pub fn uuid_v4() -> String {
    let hi = (rand_u64() & 0xffff_ffff_ffff_0fff) | 0x0000_0000_0000_4000;
    let lo = (rand_u64() & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
    let hex = format!("{hi:016x}{lo:016x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// A random u64 without an RNG crate: the hasher's keys are seeded from the
/// OS per `RandomState`, and the salt spreads calls within a process.
fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let salt = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs() << 32) | d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(salt);
    hasher.finish()
}

/// The store key for the `get_updates_buf` cursor of one bot token. The
/// cursor is a position in that bot's update stream: replaying it under a
/// different token drops the messages issued in between, and the platform
/// may reject a foreign cursor outright.
pub fn cursor_key(bot_token: &str) -> String {
    crate::im::scope::scoped(CURSOR_PREFIX, bot_token)
}

/// The store key for one peer's reply window, under the bot token that
/// opened it — a `context_token` is only valid for the bot it was issued to.
pub fn context_key(bot_token: &str, chat_id: &str) -> String {
    format!(
        "{CONTEXT_PREFIX}{}:{chat_id}",
        crate::im::scope::credential_tag(bot_token)
    )
}

/// One reply window as stored: `token<TAB>ts`. Tab-separated like the QQ
/// adapter's cursors — the token itself may contain anything else.
pub fn encode_context(token: &str, ts: i64) -> String {
    format!("{token}\t{ts}")
}

/// A stored reply window, or `None` once it is empty or older than
/// `CONTEXT_TTL_SECS`. A timestamp from the future is a clock skew, not an
/// expiry — it is treated as live.
pub fn decode_context(raw: &str, now: i64) -> Option<(String, i64)> {
    let (token, ts) = raw.split_once('\t')?;
    let ts: i64 = ts.parse().ok()?;
    if token.is_empty() || now - ts > CONTEXT_TTL_SECS {
        return None;
    }
    Some((token.to_string(), ts))
}

/// A business failure under HTTP 200: iLink answers `ret != 0` with the
/// reason in `errcode`/`errmsg`. `-14` means the session is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IlinkError {
    pub endpoint: String,
    pub ret: i64,
    pub errcode: Option<i64>,
    pub errmsg: String,
}

impl IlinkError {
    /// The bot token was revoked or the login expired — retrying cannot fix
    /// it, so the poll loop stops instead of hammering the endpoint.
    pub fn is_session_expired(&self) -> bool {
        self.ret == -14 || self.errcode == Some(-14)
    }
}

impl fmt::Display for IlinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ret={} errcode={} errmsg={}",
            self.endpoint,
            self.ret,
            self.errcode
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string()),
            if self.errmsg.is_empty() {
                "-"
            } else {
                &self.errmsg
            }
        )
    }
}

impl std::error::Error for IlinkError {}

/// The business error of a response body, if it carries one. An absent `ret`
/// is not an error (only the endpoints that define it use it). iLink spells
/// `ret`/`errcode` as a number *or* a string, so reading only the numeric
/// form silently turns a dead session (`"ret": "-14"`) into a success and the
/// expired-session path never fires.
pub fn business_error(endpoint: &str, v: &serde_json::Value) -> Option<IlinkError> {
    let ret = ret_of(&v["ret"])?;
    if ret == 0 {
        return None;
    }
    Some(IlinkError {
        endpoint: endpoint.to_string(),
        ret,
        errcode: ret_of(&v["errcode"]),
        errmsg: v["errmsg"].as_str().unwrap_or("").to_string(),
    })
}

/// A `ret`/`errcode` as an integer, from either spelling. A value that is
/// neither a number nor a numeric string is no value at all.
fn ret_of(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}
