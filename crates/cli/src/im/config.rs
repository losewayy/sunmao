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

/// Feishu's data residency picks the API host: `feishu_cn` = open.feishu.cn,
/// `lark_global` = open.larksuite.com. Credentials are not portable between
/// the two — an app registered on one domain does not authenticate on the
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeishuRegion {
    #[default]
    FeishuCn,
    LarkGlobal,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelSpec {
    Telegram(TelegramSpec),
    Feishu(FeishuSpec),
    Qq(QqSpec),
    /// DingTalk enterprise internal bot over Stream: the registration
    /// endpoint hands back a WebSocket the platform pushes callbacks down.
    Dingtalk(DingtalkSpec),
    /// WeChat personal account over iLink: one long poll for messages plus a
    /// windowed send per peer.
    Wechat(WechatSpec),
    /// Test-only kind — drives the kind-agnostic `enabled()`/`scoped()`
    /// path a new adapter takes without shipping a stub adapter for it.
    /// Compiled into test builds only.
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
    /// Channel-level owner (`channel:sender` or a bare sender id) — the
    /// per-block row the settings UI writes.
    pub owner: Option<&'a str>,
}

impl ChannelSpec {
    /// The channel id for authz/policy lookups — matches `im:{channel}`.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Telegram(_) => "telegram",
            Self::Feishu(_) => "feishu",
            Self::Qq(_) => "qq",
            Self::Dingtalk(_) => "dingtalk",
            Self::Wechat(_) => "wechat",
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
            Self::Feishu(t) => t.enabled,
            Self::Qq(t) => t.enabled,
            Self::Dingtalk(t) => t.enabled,
            Self::Wechat(t) => t.enabled,
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
                owner: t.owner.as_deref(),
            }),
            Self::Feishu(t) => Some(ChannelScoped {
                dm_policy: t.dm_policy,
                allowlist: &t.allowlist,
                owner: t.owner.as_deref(),
            }),
            Self::Qq(t) => Some(ChannelScoped {
                dm_policy: t.dm_policy,
                allowlist: &t.allowlist,
                owner: t.owner.as_deref(),
            }),
            Self::Dingtalk(t) => Some(ChannelScoped {
                dm_policy: t.dm_policy,
                allowlist: &t.allowlist,
                owner: t.owner.as_deref(),
            }),
            Self::Wechat(t) => Some(ChannelScoped {
                dm_policy: t.dm_policy,
                allowlist: &t.allowlist,
                owner: t.owner.as_deref(),
            }),
            #[cfg(test)]
            Self::Test(t) => Some(ChannelScoped {
                dm_policy: t.dm_policy,
                allowlist: &t.allowlist,
                owner: None,
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
    /// Config-level owner for this channel (the UI writes one per block).
    #[serde(default)]
    pub owner: Option<String>,
    /// Channel-scoped overrides — inherit the top-level values when unset.
    #[serde(default)]
    pub dm_policy: Option<DmPolicy>,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub enabled: bool,
}

/// Feishu/Lark custom app — App ID + App Secret buy a
/// `tenant_access_token`; inbound rides a WebSocket long connection
/// subscribed to `im.message.receive_v1` (no callback URL, no public
/// endpoint).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FeishuSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret_env: Option<String>,
    #[serde(default)]
    pub app_secret_file: Option<PathBuf>,
    #[serde(default)]
    pub region: FeishuRegion,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub dm_policy: Option<DmPolicy>,
    #[serde(default)]
    pub allowlist: Vec<String>,
}

/// QQ 开放平台 bot (v2 API) — AppID + AppSecret buy an `access_token`,
/// the gateway URL comes from `/gateway`, and messages arrive over that
/// bot's own WebSocket.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct QqSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret_env: Option<String>,
    #[serde(default)]
    pub app_secret_file: Option<PathBuf>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub dm_policy: Option<DmPolicy>,
    #[serde(default)]
    pub allowlist: Vec<String>,
}

/// DingTalk Stream bot — an enterprise internal app: corp id + client
/// id/secret buy the access token, and `robot_code` names the bot that
/// answers. Inbound rides a reverse WebSocket (`gateway/connections/open`),
/// so no public callback URL is needed.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DingtalkSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub corp_id: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret_env: Option<String>,
    #[serde(default)]
    pub client_secret_file: Option<PathBuf>,
    #[serde(default)]
    pub robot_code: String,
    #[serde(default = "default_dingtalk_api_base")]
    pub api_base_url: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub dm_policy: Option<DmPolicy>,
    #[serde(default)]
    pub allowlist: Vec<String>,
}

/// WeChat personal account over iLink — the `bot_token` is the whole
/// credential. QR-code login is not part of this version, so the token comes
/// from `bot_token_env`/`bot_token_file` like every other one.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WechatSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bot_token_env: Option<String>,
    #[serde(default)]
    pub bot_token_file: Option<PathBuf>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub dm_policy: Option<DmPolicy>,
    #[serde(default)]
    pub allowlist: Vec<String>,
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
fn default_dingtalk_api_base() -> String {
    "https://api.dingtalk.com/v1.0".to_string()
}

/// Resolve a secret by reference — `<field>_env` first, then
/// `<field>_file`; neither set is a config error, not a runtime surprise.
/// The secret itself never reaches the store or a log line.
fn resolve_secret(
    env: Option<&str>,
    file: Option<&Path>,
    channel: &str,
    field: &str,
) -> anyhow::Result<String> {
    if let Some(env) = env {
        if let Ok(v) = std::env::var(env)
            && !v.trim().is_empty()
        {
            return Ok(v.trim().to_string());
        }
        anyhow::bail!("{field}_env {env} is unset or empty");
    }
    if let Some(file) = file {
        let t = std::fs::read_to_string(file)
            .map_err(|e| anyhow::anyhow!("{field}_file {}: {e}", file.display()))?;
        if t.trim().is_empty() {
            anyhow::bail!("{field}_file {} is empty", file.display());
        }
        return Ok(t.trim().to_string());
    }
    anyhow::bail!("{channel} channel needs {field}_env or {field}_file")
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
    /// Neither set → Err naming the field pair (config explains itself).
    pub fn token(&self) -> anyhow::Result<String> {
        resolve_secret(
            self.token_env.as_deref(),
            self.token_file.as_deref(),
            "telegram",
            "token",
        )
    }
}

impl FeishuSpec {
    /// `app_secret_env` first, then `app_secret_file`.
    pub fn app_secret(&self) -> anyhow::Result<String> {
        resolve_secret(
            self.app_secret_env.as_deref(),
            self.app_secret_file.as_deref(),
            "feishu",
            "app_secret",
        )
    }
}

impl QqSpec {
    /// `app_secret_env` first, then `app_secret_file`.
    pub fn app_secret(&self) -> anyhow::Result<String> {
        resolve_secret(
            self.app_secret_env.as_deref(),
            self.app_secret_file.as_deref(),
            "qq",
            "app_secret",
        )
    }
}

impl DingtalkSpec {
    /// `client_secret_env` first, then `client_secret_file`.
    pub fn client_secret(&self) -> anyhow::Result<String> {
        resolve_secret(
            self.client_secret_env.as_deref(),
            self.client_secret_file.as_deref(),
            "dingtalk",
            "client_secret",
        )
    }
}

impl WechatSpec {
    /// `bot_token_env` first, then `bot_token_file`.
    pub fn bot_token(&self) -> anyhow::Result<String> {
        resolve_secret(
            self.bot_token_env.as_deref(),
            self.bot_token_file.as_deref(),
            "wechat",
            "bot_token",
        )
    }
}

#[cfg(test)]
mod tests;
