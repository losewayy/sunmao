//! Admission control — who may drive the shared brain. DM-only (groups
//! are filtered upstream in the adapter); the policy surface mirrors
//! OpenClaw's key names (`dmPolicy`, `unauthorized_dm_behavior`) so the
//! docs read one dialect.

use super::config::{ChannelsConfig, DmPolicy, UnauthorizedBehavior};
use super::route::ImSource;
use super::store::{PairingRow, Store};

/// Pairing-code parameters — 8 chars over an unambiguous alphabet, 1h
/// TTL, ≤3 live codes per channel, one code per sender per 60s cooldown.
/// Numbers match the research baseline (Hermes/OpenClaw both picked the
/// same shape; it survives brute force because *approving* a code is an
/// owner action — a guessed code can't admit anyone).
const CODE_LEN: usize = 8;
const CODE_TTL_SECS: i64 = 3600;
const MAX_PENDING_PER_CHANNEL: usize = 3;
const ISSUE_COOLDOWN_SECS: i64 = 60;
/// Unambiguous alphabet — no 0/O/1/I/L.
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// What ingress does with an inbound message after authz.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Admitted — route and dispatch.
    Allow,
    /// Stranger + pairing policy → send this reply (the code offer or a
    /// rate-limit note). Reply text comes pre-rendered.
    Reply(String),
    /// Answer nothing — `ignore` behavior or `disabled` policy.
    Silent,
}

/// Is `sender` admitted on this channel? The effective policy is the
/// channel block's override else the top-level `dm_policy`; allowlist
/// checks read the channel's entries plus the top-level list, and the
/// store's approved senders. `open` requires `allowlist: ["*"]` —
/// config-level wildcard only, never a store row.
pub fn admitted(cfg: &ChannelsConfig, store: &Store, src: &ImSource) -> bool {
    let entry = format!("{}:{}", src.channel, src.sender_id);
    let in_list = |list: &[String]| list.iter().any(|e| e == "*" || e == &entry);
    let mut allowlist = in_list(&cfg.allowlist);
    let mut policy = cfg.dm_policy;
    if let Some(super::config::ChannelSpec::Telegram(tg)) = cfg
        .channels
        .iter()
        .find(|c| c.kind_name() == src.channel)
    {
        if tg.allowlist.iter().any(|e| e == &src.sender_id || e == &entry) {
            allowlist = true;
        }
        if let Some(p) = tg.dm_policy {
            policy = p;
        }
    }
    match policy {
        DmPolicy::Open => allowlist,
        DmPolicy::Disabled => false,
        // pairing policy behaves as allowlist once a code was approved —
        // the difference is only how a sender *gets* onto the list
        DmPolicy::Pairing | DmPolicy::Allowlist => {
            allowlist || store.allow_role(&src.channel, &src.sender_id).is_some()
        }
    }
}

/// Owner = config `owner:` or the store role. First approved sender lands
/// here — `pairing approve` bootstraps it (Store::allow_add).
pub fn is_owner(cfg: &ChannelsConfig, store: &Store, src: &ImSource) -> bool {
    cfg.owner
        .as_deref()
        .map(|o| o == src.sender_id || o == format!("{}:{}", src.channel, src.sender_id))
        .unwrap_or(false)
        || store.allow_role(&src.channel, &src.sender_id).as_deref() == Some("owner")
}

/// The effective DM policy for a channel — the channel block's
/// `dm_policy` wins over the top-level value.
fn effective_policy(cfg: &ChannelsConfig, channel: &str) -> DmPolicy {
    for c in &cfg.channels {
        if c.kind_name() == channel
            && let super::config::ChannelSpec::Telegram(tg) = c
            && let Some(p) = tg.dm_policy
        {
            return p;
        }
    }
    cfg.dm_policy
}

/// Issue a pairing code for a stranger — returns the reply text to send
/// (`Reply`) or a silent verdict when rate-limited. The code rows are
/// the store's; expiry and caps are enforced here, not in the adapter.
fn pair_reply(cfg: &ChannelsConfig, store: &Store, src: &ImSource) -> Verdict {
    if cfg.unauthorized_dm_behavior == UnauthorizedBehavior::Ignore {
        return Verdict::Silent;
    }
    // cooldown — a sender spamming codes doesn't get a fresh one each time
    if let Some(last) = store.pairing_sender_created(&src.channel, &src.sender_id)
        && super::store::now() - last < ISSUE_COOLDOWN_SECS
    {
        return Verdict::Reply(super::messages::get("pairing_cooldown"));
    }
    if store.pairing_live(&src.channel).len() >= MAX_PENDING_PER_CHANNEL {
        return Verdict::Reply(super::messages::get("pairing_cooldown"));
    }
    let code = new_code();
    let _ = store.pairing_insert(&PairingRow {
        channel: src.channel.clone(),
        sender: src.sender_id.clone(),
        code: code.clone(),
        created: super::store::now(),
        expires: super::store::now() + CODE_TTL_SECS,
    });
    Verdict::Reply(
        super::messages::get("pairing_offer").replace("{code}", &code),
    )
}

/// Inbound admission: admitted senders pass, strangers get the pairing
/// reply (or silence). Control commands (`/pairing` mgmt) are decided
/// before this is called — authz guards *conversation*, not mgmt.
pub fn authorize(cfg: &ChannelsConfig, store: &Store, src: &ImSource) -> Verdict {
    if admitted(cfg, store, src) {
        return Verdict::Allow;
    }
    match effective_policy(cfg, &src.channel) {
        DmPolicy::Disabled | DmPolicy::Open => Verdict::Silent,
        DmPolicy::Allowlist => {
            // allowlist policy has no code path — strangers get silence;
            // "pair" behavior only makes sense under pairing
            Verdict::Silent
        }
        DmPolicy::Pairing => pair_reply(cfg, store, src),
    }
}

/// 8-char code over the unambiguous alphabet. `xorshift` seeded from
/// time+pid — codes are one-time, short-lived and capped; a CSPRNG would
/// cost a `rand` dep this file doesn't need.
fn new_code() -> String {
    let seed = super::store::now() as u64
        ^ (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64)
        ^ ((std::process::id() as u64) << 32);
    let mut x = seed.max(1);
    (0..CODE_LEN)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ALPHABET[(x % ALPHABET.len() as u64) as usize] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::config::ChannelsConfig;

    fn cfg(policy: DmPolicy) -> ChannelsConfig {
        serde_json::from_str::<ChannelsConfig>(&format!(
            r#"{{"dm_policy":"{}","channels":[]}}"#,
            match policy {
                DmPolicy::Pairing => "pairing",
                DmPolicy::Allowlist => "allowlist",
                DmPolicy::Open => "open",
                DmPolicy::Disabled => "disabled",
            }
        ))
        .unwrap()
    }

    fn store() -> (Store, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("sunmao-im-authz-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let _ = std::fs::remove_dir_all(&dir);
        (Store::open(&dir).unwrap(), dir)
    }

    fn src(sender: &str) -> ImSource {
        ImSource {
            channel: "telegram".into(),
            chat_id: "42".into(),
            sender_id: sender.into(),
            sender_name: String::new(),
        }
    }

    #[test]
    fn stranger_pairs_admitted_passes() {
        let (s, dir) = store();
        let c = cfg(DmPolicy::Pairing);
        // stranger gets the offer
        match authorize(&c, &s, &src("7")) {
            Verdict::Reply(t) => assert!(t.contains("sunmao pairing approve")),
            _ => panic!("stranger should get a code offer"),
        }
        // owner approves → admitted
        let row = s.pairing_all()[0].clone();
        s.allow_add(&row.channel, &row.sender).unwrap();
        assert_eq!(authorize(&c, &s, &src("7")), Verdict::Allow);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn disabled_is_silent() {
        let (s, dir) = store();
        assert_eq!(
            authorize(&cfg(DmPolicy::Disabled), &s, &src("7")),
            Verdict::Silent
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn open_needs_wildcard() {
        let (s, dir) = store();
        let mut c = cfg(DmPolicy::Open);
        assert_eq!(authorize(&c, &s, &src("7")), Verdict::Silent);
        c.allowlist = vec!["*".into()];
        assert_eq!(authorize(&c, &s, &src("7")), Verdict::Allow);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn config_allowlist_short_circuits() {
        let (s, dir) = store();
        let mut c = cfg(DmPolicy::Allowlist);
        c.allowlist = vec!["telegram:7".into()];
        assert_eq!(authorize(&c, &s, &src("7")), Verdict::Allow);
        assert_eq!(authorize(&c, &s, &src("8")), Verdict::Silent);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn code_shape() {
        let code = new_code();
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.chars().all(|c| ALPHABET.contains(&(c as u8))));
    }
}
