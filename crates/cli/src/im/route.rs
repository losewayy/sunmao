//! Session-key derivation — the ONE place an inbound message becomes a
//! session identity. Everything downstream (store, deliver, progress)
//! consumes `ImSource`/`SessionRoute`; nothing else builds keys by hand.

/// An inbound message's source identity — resolved once at ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImSource {
    /// Channel id — "telegram", later "feishu"/"wechat".
    pub channel: String,
    /// Channel-native DM chat id (Telegram `chat.id` as string).
    pub chat_id: String,
    /// Channel-native sender id (Telegram `from.id`) — authz works on this.
    pub sender_id: String,
    /// Human label for attribution — username or display name.
    pub sender_name: String,
}

/// Which session a message lands in — derived, never stored in the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoute {
    /// `im:{channel}:dm:{chat_id}` — or `im:main` under dmScope=main.
    pub session_key: String,
    /// Where the reply goes: the chat that sent.
    pub channel: String,
    pub chat_id: String,
}

/// dmScope is the only policy input — under `main` every DM converges on
/// the shared `im:main` key; `per_channel_peer` keeps one session per
/// channel+chat. Group/thread segments (`:u{user}`, `:t{thread}`) are the
/// reserved expansion the key shape leaves room for.
pub fn build_session_key(scope: super::config::DmScope, src: &ImSource) -> String {
    match scope {
        super::config::DmScope::Main => "im:main".to_string(),
        super::config::DmScope::PerChannelPeer => {
            format!("im:{}:dm:{}", src.channel, src.chat_id)
        }
    }
}

/// Route = key + the reply address it came from.
pub fn route_for(scope: super::config::DmScope, src: &ImSource) -> SessionRoute {
    SessionRoute {
        session_key: build_session_key(scope, src),
        channel: src.channel.clone(),
        chat_id: src.chat_id.clone(),
    }
}

/// The prompt text the agent sees — sender attribution rides a small tag
/// so the shared main session can tell senders apart (multi-user DM
/// traffic merges into one transcript; unattributed lines would read as
/// one speaker).
pub fn prompt_text(src: &ImSource, text: &str) -> String {
    let name = if src.sender_name.is_empty() {
        src.sender_id.as_str()
    } else {
        src.sender_name.as_str()
    };
    format!("[im:{} from {}] {}", src.channel, name, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::config::DmScope;

    fn src() -> ImSource {
        ImSource {
            channel: "telegram".into(),
            chat_id: "42".into(),
            sender_id: "7".into(),
            sender_name: "ada".into(),
        }
    }

    #[test]
    fn main_scope_converges_everything() {
        let a = build_session_key(DmScope::Main, &src());
        let mut other = src();
        other.chat_id = "99".into();
        let b = build_session_key(DmScope::Main, &other);
        assert_eq!(a, "im:main");
        assert_eq!(a, b, "dmScope=main folds every DM into one session");
    }

    #[test]
    fn per_channel_peer_separates() {
        let a = build_session_key(DmScope::PerChannelPeer, &src());
        assert_eq!(a, "im:telegram:dm:42");
        let mut other = src();
        other.chat_id = "99".into();
        assert_ne!(a, build_session_key(DmScope::PerChannelPeer, &other));
    }

    #[test]
    fn prompt_carries_attribution() {
        assert_eq!(
            prompt_text(&src(), "hi"),
            "[im:telegram from ada] hi"
        );
    }
}
