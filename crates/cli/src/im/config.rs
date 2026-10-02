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
}

impl ChannelSpec {
    /// The channel id for authz/policy lookups — matches `im:{channel}`.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Telegram(_) => "telegram",
        }
    }
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
        let cfg: ChannelsConfig = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(Some(cfg))
    }

    pub fn telegram(&self) -> Option<&TelegramSpec> {
        self.channels.iter().find_map(|c| match c {
            ChannelSpec::Telegram(t) if t.enabled => Some(t),
            _ => None,
        })
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
        assert!(cfg.telegram().is_some());
    }

    #[test]
    fn disabled_channel_skips() {
        let cfg = serde_json::from_str::<ChannelsConfig>(
            r#"{"channels":[{"kind":"telegram","enabled":false,"token_env":"T"}]}"#,
        )
        .unwrap();
        assert!(cfg.telegram().is_none());
    }

    #[test]
    fn missing_is_none_not_err() {
        assert!(
            ChannelsConfig::load(Path::new("/nonexistent/channels.json"))
                .unwrap()
                .is_none()
        );
    }
}
