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
    async fn send_text(&self, chat_id: &str, text: &str) -> Result<Option<String>>;
    /// Edit a previously sent message — the progress draft's refresh path.
    /// `None` on channels without edit support is a no-op upstream.
    async fn edit_text(&self, _chat_id: &str, _message_id: &str, _text: &str) -> Result<()> {
        Ok(())
    }
    /// Typing indicator — best effort, errors are swallowed upstream.
    async fn send_typing(&self, _chat_id: &str) {}
}

mod telegram;
pub use telegram::TelegramAdapter;

/// Build the adapter for one enabled channel block — the single wiring
/// point for a new kind: add its `ChannelSpec` variant, its
/// `ChannelSpec::scoped()` arm, and one arm here; nothing else moves.
/// `Err` is per channel — the daemon logs it and keeps running the rest.
pub fn build(spec: &ChannelSpec, store: Arc<Store>) -> Result<Arc<dyn ChannelAdapter>> {
    match spec {
        ChannelSpec::Telegram(tg) => Ok(Arc::new(TelegramAdapter::new(tg, store)?)),
        // never built: `enabled_specs()` drops unknown kinds, and a
        // config-only kind has no adapter in this binary
        ChannelSpec::Unknown => anyhow::bail!("no adapter for channel kind {}", spec.kind_name()),
        #[cfg(test)]
        ChannelSpec::Test(_) => anyhow::bail!("no adapter for test channel"),
    }
}
