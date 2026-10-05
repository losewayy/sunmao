//! `~/.sunmao/channels.json` — the IM gateway's declared surface. Same
//! file convention as every other config (CONFIG.md): missing = feature
//! off, invalid JSON = warning + off. Credentials follow the mcp.json
//! rule — the file names *where* the secret lives (`token_env` /
//! `token_file`), never the secret itself.

use std::path::{Path, PathBuf};

/// The resolved channel config. Parsed once at `sunmao im` startup —
/// cold-plug: edits take effect on the next daemon launch, never hot.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ChannelsConfig {
    /// `pairing` (default): strangers get a pairing code; `allowlist`:
    /// only configured/approved senders; `open` requires `allowlist: ["*"]`
    /// — a policy this permissive must be written twice to be believed;
    /// `disabled` answers nothing.
    #[serde(default = "default_dm_policy")]
    pub dm_policy: DmPolicy,
    /// `main` (default): every DM folds into one shared session;
    /// `per_channel_peer`: one session per channel+chat.
    #[serde(default)]
    pub dm_scope: DmScope,
    /// What an unauthorized DM gets back: `pair` (default) sends a pairing
    /// code; `ignore` stays silent — replying at all already leaks that
    /// a bot lives here.
    #[serde(default = "default_unauthorized")]
    pub unauthorized_dm_behavior: UnauthorizedBehavior,
    /// Config-time allowlist (`channel:sender` strings), merged with the
    /// store's approved senders at authorize time.
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// Sender id granted management commands (`/pairing` in IM). The first
    /// approved pairing also becomes owner when none exists yet.
    #[serde(default)]
    pub owner: Option<String>,
    /// Per-channel adapter blocks. Unknown kinds warn and skip — a config
    /// written for a newer sunmao must not kill the daemon.
    #[serde(default)]
    pub channels: Vec<ChannelSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmPolicy {
    Pairing,
    Allowlist,
    Open,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmScope {
    /// One shared session for every DM — "one brain", channel-agnostic.
    #[default]
    Main,
    /// A session per channel+chat (multi-user context isolation).
    PerChannelPeer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnauthorizedBehavior {
    Pair,
    Ignore,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelSpec {
    Telegram(TelegramSpec),
    /// Test-only kind — drives the kind-agnostic `enabled()`/`scoped()`
    /// path a new adapter (feishu/wechat/qq) takes without shipping a
    /// stub adapter for it. Compiled into test builds only.
    #[cfg(test)]
    Test(TestSpec),
    /// A kind this binary doesn't know — the doc comment promises
    /// warn-and-skip, so it must survive parse or one newer config entry
    /// would take down the whole daemon.
    #[serde(other)]
    Unknown,
}

/// A channel block's scoped policy overrides — the kind-agnostic shape
/// authz consumes, so admission never has to match on which adapter kind
/// produced the block.
#[derive(Debug, Clone, Copy)]
pub struct ChannelScoped<'a> {
    pub dm_policy: Option<DmPolicy>,
    pub allowlist: &'a [String],
}

impl ChannelSpec {
    /// The channel id for authz/policy lookups — matches `im:{channel}`.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Telegram(_) => "telegram",
            #[cfg(test)]
            Self::Test(_) => "test",
            Self::Unknown => "unknown",
        }
    }

    /// Is this block switched on? An unknown kind never is — a config
    /// written for a newer binary is skipped, not guessed at.
    pub fn enabled(&self) -> bool {
        match self {
            Self::Telegram(t) => t.enabled,
            #[cfg(test)]
            Self::Test(t) => t.enabled,
            Self::Unknown => false,
        }
    }

    /// The block's channel-scoped overrides. `None` when the kind has no
    /// policy surface (Unknown) — authz then keeps the top-level values.
    pub fn scoped(&self) -> Option<ChannelScoped<'_>> {
        match self {
            Self::Telegram(t) => Some(ChannelScoped {
                dm_policy: t.dm_policy,
                allowlist: &t.allowlist,
            }),
            #[cfg(test)]
            Self::Test(t) => Some(ChannelScoped {
                dm_policy: t.dm_policy,
                allowlist: &t.allowlist,
            }),
            Self::Unknown => None,
        }
    }
}

/// The test-only channel block — carries the same policy surface a real
/// adapter block does (`enabled`, `dm_policy`, `allowlist`). A production
/// kind gets its own spec struct next to this one; `TelegramSpec` is the
/// model.
#[cfg(test)]
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TestSpec {
    #[serde(default)]
    pub dm_policy: Option<DmPolicy>,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TelegramSpec {
    /// Bot token supply — env var name or a file path carrying the token.
    /// `token` (a literal secret in the config file) is deliberately not a
    /// field: it would break the credentials convention.
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    #[serde(default = "default_poll_timeout")]
    pub poll_timeout_secs: u64,
    /// Channel-scoped overrides — inherit the top-level values when unset.
    #[serde(default)]
    pub dm_policy: Option<DmPolicy>,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub enabled: bool,
}

fn default_dm_policy() -> DmPolicy {
    DmPolicy::Pairing
}
fn default_unauthorized() -> UnauthorizedBehavior {
    UnauthorizedBehavior::Pair
}
fn default_poll_timeout() -> u64 {
    30
}

/// `~` home dir the same way the rest of the codebase resolves it.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// `~/.sunmao/` — the user-level state root; `~/.sunmao/im/` is the
/// gateway's state dir (store, workspace, status.json).
pub(crate) fn sunmao_home() -> PathBuf {
    home_dir()
        .map(|h| h.join(".sunmao"))
        .unwrap_or_else(|| PathBuf::from(".sunmao"))
}

/// `~/.sunmao/im/` — routing index, pairing, delivery ledger, workspace.
pub fn state_dir() -> PathBuf {
    sunmao_home().join("im")
}

/// `~/.sunmao/channels.json` — the channel config the daemon reads.
pub fn config_path() -> PathBuf {
    sunmao_home().join("channels.json")
}

impl ChannelsConfig {
    /// Load `~/.sunmao/channels.json`. `Ok(None)` = no config / nothing
    /// enabled — callers report it and exit, not crash.
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(None);
        };
        let cfg: ChannelsConfig =
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(Some(cfg))
    }

    /// Every declared channel block, in config order — the one
    /// enumeration point for adapter construction and policy lookups.
    pub fn specs(&self) -> impl Iterator<Item = &ChannelSpec> {
        self.channels.iter()
    }

    /// The blocks the daemon actually runs.
    pub fn enabled_specs(&self) -> impl Iterator<Item = &ChannelSpec> {
        self.specs().filter(|c| c.enabled())
    }
}

impl TelegramSpec {
    /// Resolve the bot token: `token_env` first, then `token_file`.
    /// Neither set → Err naming the file (config explains itself).
    pub fn token(&self) -> anyhow::Result<String> {
        if let Some(env) = &self.token_env {
            if let Ok(v) = std::env::var(env)
                && !v.trim().is_empty()
            {
                return Ok(v.trim().to_string());
            }
            anyhow::bail!("token_env {env} is unset or empty");
        }
        if let Some(file) = &self.token_file {
            let t = std::fs::read_to_string(file)
                .map_err(|e| anyhow::anyhow!("token_file {}: {e}", file.display()))?;
            if t.trim().is_empty() {
                anyhow::bail!("token_file {} is empty", file.display());
            }
            return Ok(t.trim().to_string());
        }
        anyhow::bail!("telegram channel needs token_env or token_file")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let cfg = serde_json::from_str::<ChannelsConfig>(
            r#"{"channels":[{"kind":"telegram","enabled":true,"token_env":"TG_TOKEN"}]}"#,
        )
        .unwrap();
        assert_eq!(cfg.dm_policy, DmPolicy::Pairing);
        assert_eq!(cfg.dm_scope, DmScope::Main);
        assert_eq!(cfg.enabled_specs().count(), 1);
    }

    #[test]
    fn disabled_channel_skips() {
        let cfg = serde_json::from_str::<ChannelsConfig>(
            r#"{"channels":[{"kind":"telegram","enabled":false,"token_env":"T"}]}"#,
        )
        .unwrap();
        assert_eq!(cfg.enabled_specs().count(), 0);
    }

    #[test]
    fn missing_is_none_not_err() {
        assert!(
            ChannelsConfig::load(Path::new("/nonexistent/channels.json"))
                .unwrap()
                .is_none()
        );
    }

    /// A kind that is not telegram lands on its own variant and keeps its
    /// scoped policy — the generic path every new adapter rides.
    #[test]
    fn new_kind_parses_with_its_own_scope() {
        let cfg = serde_json::from_str::<ChannelsConfig>(
            r#"{"channels":[{"kind":"test","enabled":true,
                "dm_policy":"open","allowlist":["test:7"]}]}"#,
        )
        .unwrap();
        let spec = cfg.specs().next().unwrap();
        assert_eq!(spec.kind_name(), "test");
        assert!(spec.enabled());
        let scoped = spec.scoped().unwrap();
        assert_eq!(scoped.dm_policy, Some(DmPolicy::Open));
        assert_eq!(scoped.allowlist, ["test:7"]);
        assert_eq!(cfg.enabled_specs().count(), 1);
    }

    /// A config written for a newer binary must parse and be skipped —
    /// never an error, never an enabled channel.
    #[test]
    fn unknown_kind_parses_and_skips() {
        let cfg = serde_json::from_str::<ChannelsConfig>(
            r#"{"channels":[{"kind":"future_channel","enabled":true,"whatever":1}]}"#,
        )
        .unwrap();
        let spec = cfg.specs().next().unwrap();
        assert!(matches!(spec, ChannelSpec::Unknown));
        assert_eq!(spec.kind_name(), "unknown");
        assert!(!spec.enabled());
        assert!(spec.scoped().is_none());
        assert_eq!(cfg.enabled_specs().count(), 0);
    }
}
