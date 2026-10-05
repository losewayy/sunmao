//! Channel adapters — the transport edge of the gateway. An adapter owns
//! exactly four jobs: stay connected, translate inbound payloads into
//! `InboundMsg`, and answer `send_text`/`edit_text`/`send_typing` for the
//! delivery/progress lanes. Routing, authz, sessions — all upstream.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use super::route::ImSource;
use crate::im::config::ChannelSpec;
use crate::im::store::Store;

/// One inbound DM — what an adapter hands the gateway. Attachments are a
/// future concern (MVP is text-only; photo/caption maps to caption text).
#[derive(Debug)]
pub struct InboundMsg {
    pub source: ImSource,
    pub text: String,
}

/// The outcome of one `send_text` call.
pub type SendResult = std::result::Result<Option<String>, SendFailure>;

/// Why one `send_text` stopped, and how much of the text got out first.
///
/// Only the adapter can answer that: chunking happens inside it, so the
/// ledger sees a single opaque call. The counts are what make the difference
/// between a failure that is safe to retry and one that is not — text that
/// already reached the platform must never be sent again, and WeChat and
/// DingTalk mint a fresh `client_id` per attempt, so the platform cannot
/// de-duplicate on its side.
#[derive(Debug)]
pub struct SendFailure {
    delivered: usize,
    total: usize,
    source: anyhow::Error,
}

impl SendFailure {
    /// `delivered` of the `total` chunks reached the platform before the
    /// failure. There is deliberately no `From<anyhow::Error>`: every call
    /// site has to say how much it had already sent.
    pub fn new(delivered: usize, total: usize, source: anyhow::Error) -> Self {
        Self {
            delivered,
            total,
            source,
        }
    }

    /// The send stopped before the first chunk was attempted (a missing
    /// reply window, an unreachable token endpoint): nothing is on the
    /// platform and the total is not worth computing.
    pub fn before_send(source: anyhow::Error) -> Self {
        Self {
            delivered: 0,
            total: 0,
            source,
        }
    }

    /// Chunks that reached the platform before the failure.
    pub fn delivered(&self) -> usize {
        self.delivered
    }

    /// Chunks the text was split into; `0` when the failure happened before
    /// any chunking was attempted.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Part of the answer is already on the platform — resending the text
    /// would duplicate it.
    pub fn is_partial(&self) -> bool {
        self.delivered > 0
    }

    /// `delivered/total`, the shape an operator needs to tell a truncated
    /// answer from one that never went out.
    pub fn progress(&self) -> String {
        format!("{}/{}", self.delivered, self.total)
    }
    /// The underlying transport/adapter error, with its context chain.
    pub fn into_source(self) -> anyhow::Error {
        self.source
    }
}

impl std::fmt::Display for SendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.source)?;
        if self.is_partial() {
            write!(
                f,
                " ({} of {} chunk(s) already delivered)",
                self.delivered, self.total
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for SendFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// The adapter contract — deliberately this small (the research baseline:
/// Hermes' adapter proves ~6 methods carry the whole channel). Async so a
/// second adapter (feishu) can carry its own client without wrappers.
#[async_trait::async_trait]
pub trait ChannelAdapter: Send + Sync {
    /// Channel id — matches `im:{channel}:…` session keys.
    fn channel(&self) -> &'static str;
    /// Run the inbound stream until the process dies. Connection churn is
    /// the adapter's problem — this call owns reconnect/backoff.
    async fn poll(&self, tx: mpsc::Sender<InboundMsg>);
    /// Deliver text. Returns the channel's message id when it has one —
    /// the progress lane needs it to edit drafts.
    ///
    /// The error is a [`SendFailure`], which carries the number of chunks
    /// that already reached the platform: the delivery ledger decides from
    /// that whether a retry is safe (see `deliver.rs`).
    async fn send_text(&self, chat_id: &str, text: &str) -> SendResult;
    /// Edit a previously sent message — the progress draft's refresh path.
    /// `None` on channels without edit support is a no-op upstream.
    async fn edit_text(&self, _chat_id: &str, _message_id: &str, _text: &str) -> Result<()> {
        Ok(())
    }
    /// Typing indicator — best effort, errors are swallowed upstream.
    async fn send_typing(&self, _chat_id: &str) {}
}

mod dingtalk;
mod feishu;
mod qq;
mod telegram;
mod wechat;
pub use dingtalk::DingtalkAdapter;
pub use feishu::FeishuAdapter;
pub use qq::QqAdapter;
pub use telegram::TelegramAdapter;
pub use wechat::WechatAdapter;

/// Build the adapter for one enabled channel block — the single wiring
/// point for a new kind: add its `ChannelSpec` variant, its
/// `ChannelSpec::scoped()` arm, and one arm here; nothing else moves.
/// `Err` is per channel — the daemon logs it and keeps running the rest.
pub fn build(spec: &ChannelSpec, store: Arc<Store>) -> Result<Arc<dyn ChannelAdapter>> {
    match spec {
        ChannelSpec::Telegram(tg) => Ok(Arc::new(TelegramAdapter::new(tg, store)?)),
        // feishu needs no store: the WS carries no resume offset
        ChannelSpec::Feishu(fs) => Ok(Arc::new(FeishuAdapter::new(fs)?)),
        // qq keeps its reply-target map and passive cursor in `qq:*`
        ChannelSpec::Qq(qq) => Ok(Arc::new(QqAdapter::new(qq, store)?)),
        // dingtalk needs no store: the Stream registration carries no cursor
        ChannelSpec::Dingtalk(dt) => Ok(Arc::new(DingtalkAdapter::new(dt)?)),
        // wechat keeps its `get_updates_buf` cursor and reply windows in `wx:*`
        ChannelSpec::Wechat(wx) => Ok(Arc::new(WechatAdapter::new(wx, store)?)),
        ChannelSpec::Unknown => anyhow::bail!("no adapter for channel kind {}", spec.kind_name()),
        #[cfg(test)]
        ChannelSpec::Test(_) => anyhow::bail!("no adapter for test channel"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::config::{DingtalkSpec, FeishuRegion, FeishuSpec, QqSpec, TestSpec, WechatSpec};

    fn store(dir: &std::path::Path) -> Arc<Store> {
        let _ = std::fs::remove_dir_all(dir);
        Arc::new(Store::open(dir).unwrap())
    }

    /// The factory is the single wiring point: every shipped adapter builds
    /// without touching the network (construction only resolves
    /// credentials — polling is what connects), and a kind without an
    /// adapter in this binary fails with its kind named.
    #[test]
    fn factory_wires_every_shipped_kind() {
        let dir = crate::im::test_dir("factory");
        let store = store(&dir);
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, "shh").unwrap();

        let cases = [
            (
                ChannelSpec::Feishu(FeishuSpec {
                    enabled: true,
                    app_id: "cli_x".into(),
                    app_secret_env: None,
                    app_secret_file: Some(secret.clone()),
                    region: FeishuRegion::default(),
                    owner: None,
                    dm_policy: None,
                    allowlist: Vec::new(),
                }),
                "feishu",
            ),
            (
                ChannelSpec::Qq(QqSpec {
                    enabled: true,
                    app_id: "1024".into(),
                    app_secret_env: None,
                    app_secret_file: Some(secret.clone()),
                    owner: None,
                    dm_policy: None,
                    allowlist: Vec::new(),
                }),
                "qq",
            ),
            (
                ChannelSpec::Dingtalk(DingtalkSpec {
                    enabled: true,
                    corp_id: "corp".into(),
                    client_id: "key".into(),
                    client_secret_env: None,
                    client_secret_file: Some(secret.clone()),
                    robot_code: "robot".into(),
                    api_base_url: "https://api.dingtalk.com/v1.0".into(),
                    owner: None,
                    dm_policy: None,
                    allowlist: Vec::new(),
                }),
                "dingtalk",
            ),
            (
                ChannelSpec::Wechat(WechatSpec {
                    enabled: true,
                    bot_token_env: None,
                    bot_token_file: Some(secret.clone()),
                    owner: None,
                    dm_policy: None,
                    allowlist: Vec::new(),
                }),
                "wechat",
            ),
        ];
        for (spec, expected) in cases {
            assert_eq!(build(&spec, store.clone()).unwrap().channel(), expected);
        }

        // kinds with no adapter in this binary name themselves in the error
        let test = ChannelSpec::Test(TestSpec {
            enabled: true,
            dm_policy: None,
            allowlist: Vec::new(),
        });
        for spec in [ChannelSpec::Unknown, test] {
            let err = match build(&spec, store.clone()) {
                Ok(_) => panic!("{} built an adapter", spec.kind_name()),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(spec.kind_name()), "{err}");
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
