//! Outbound delivery ledger — at-least-once final replies. Design (§3.6):
//! a `final` text is recorded `pending` before the first send, flips
//! `attempting` while the API call is in flight, lands `delivered` on
//! success. `pending`/`attempting` rows at startup get replayed — the
//! `attempting` ones already reached the wire (possibly), so the replay
//! carries the ♻️ "may repeat" prefix: honest about the ambiguity.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use super::channels::ChannelAdapter;
use super::store::Store;

/// Per-send retry ceiling inside one call — past it the row stays
/// `attempting` and the next process start replays it.
const SEND_RETRIES: u32 = 3;
const RETRY_BACKOFF_MS: u64 = 600;

/// The ledger + the adapter it sends through — one instance per channel.
pub struct Delivery {
    store: Arc<Store>,
    adapter: Arc<dyn ChannelAdapter>,
}

impl Delivery {
    pub fn new(store: Arc<Store>, adapter: Arc<dyn ChannelAdapter>) -> Self {
        Self { store, adapter }
    }

    /// The adapter's channel id — lane selection keys off it.
    pub fn channel(&self) -> &'static str {
        self.adapter.channel()
    }

    /// The adapter this ledger sends through. A live channel is one value
    /// (adapter + ledger), so a lane never zips two positional vectors.
    pub fn adapter(&self) -> &Arc<dyn ChannelAdapter> {
        &self.adapter
    }

    /// Send a final reply through the ledger. Errors surface to the
    /// caller (progress lane logs them); the row stays outstanding and a
    /// later `resend_outstanding` picks it up.
    pub async fn send_final(&self, chat_id: &str, text: &str) -> Result<()> {
        let id = self
            .store
            .deliver_pending(self.adapter.channel(), chat_id, text)?;
        self.attempt(id, chat_id, text).await
    }

    /// Re-deliver a ledger row — `text` may carry the ♻️ prefix when the
    /// row was `attempting` at replay time.
    async fn attempt(&self, id: i64, chat_id: &str, text: &str) -> Result<()> {
        let mut last_err = None;
        for _ in 0..SEND_RETRIES {
            self.store.deliver_attempting(id)?;
            match self.adapter.send_text(chat_id, text).await {
                Ok(_) => return self.store.deliver_done(id),
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_MS)).await;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("send failed")))
    }

    /// Startup replay — every outstanding row goes again. `attempting`
    /// rows get the ♻️ prefix: their send was in flight when the process
    /// died, so the message may already have arrived once.
    pub async fn resend_outstanding(&self) {
        for row in self.store.deliver_outstanding() {
            if row.channel != self.adapter.channel() {
                continue;
            }
            // a poison row would retry its full budget on every restart
            // forever — enough physical attempts → dead-letter it
            if row.attempts >= 15 {
                let _ = self.store.deliver_dead(row.id);
                tracing::warn!(
                    "im delivery {} dead-lettered after {} attempts",
                    row.id,
                    row.attempts
                );
                continue;
            }
            let text = if row.state == "attempting" {
                format!("{}{}", super::messages::get("redelivery_prefix"), row.text)
            } else {
                row.text
            };
            if let Err(e) = self.attempt(row.id, &row.chat, &text).await {
                tracing::warn!(
                    "im delivery replay {} (attempt {}): {e:#}",
                    row.id,
                    row.attempts
                );
            }
        }
    }
}

/// The live endpoint for a channel id — the delivery list is the single
/// source of truth, so the lookup never depends on two vectors staying
/// index-aligned.
pub(crate) fn for_channel<'a>(
    list: &'a [Arc<Delivery>],
    channel: &str,
) -> Option<&'a Arc<Delivery>> {
    list.iter().find(|d| d.channel() == channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    /// Scripted adapter — records sends, fails a configurable streak.
    struct FakeAdapter {
        sends: Mutex<Vec<(String, String)>>,
        fail_next: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl ChannelAdapter for FakeAdapter {
        fn channel(&self) -> &'static str {
            "test"
        }
        async fn poll(&self, _tx: tokio::sync::mpsc::Sender<crate::im::channels::InboundMsg>) {}
        async fn send_text(&self, chat_id: &str, text: &str) -> Result<Option<String>> {
            let mut fails = self.fail_next.lock().await;
            if *fails > 0 {
                *fails -= 1;
                anyhow::bail!("flaky");
            }
            drop(fails);
            self.sends
                .lock()
                .await
                .push((chat_id.to_string(), text.to_string()));
            Ok(Some("1".into()))
        }
    }

    fn store() -> (Arc<Store>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "sunmao-im-dlv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (Arc::new(Store::open(&dir).unwrap()), dir)
    }

    #[tokio::test]
    async fn final_send_lands_delivered() {
        let (s, dir) = store();
        let a = Arc::new(FakeAdapter {
            sends: Mutex::new(Vec::new()),
            fail_next: Mutex::new(0),
        });
        let d = Delivery::new(s.clone(), a.clone());
        d.send_final("42", "hello").await.unwrap();
        assert!(s.deliver_outstanding().is_empty());
        assert_eq!(a.sends.lock().await.len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn retries_then_recovers() {
        let (s, dir) = store();
        let a = Arc::new(FakeAdapter {
            sends: Mutex::new(Vec::new()),
            fail_next: Mutex::new(2),
        });
        let d = Delivery::new(s.clone(), a);
        d.send_final("42", "hi").await.unwrap();
        assert!(s.deliver_outstanding().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn outstanding_attempting_replays_marked() {
        let (s, dir) = store();
        // a send that died mid-attempt
        let id = s.deliver_pending("test", "42", "out").unwrap();
        s.deliver_attempting(id).unwrap();
        let a = Arc::new(FakeAdapter {
            sends: Mutex::new(Vec::new()),
            fail_next: Mutex::new(0),
        });
        Delivery::new(s.clone(), a.clone())
            .resend_outstanding()
            .await;
        let sent = a.sends.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0]
                .1
                .starts_with(&crate::im::messages::get("redelivery_prefix")),
            "attempting rows replay with the ♻️ marker"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn outstanding_pending_replays_clean() {
        let (s, dir) = store();
        s.deliver_pending("test", "42", "out").unwrap();
        let a = Arc::new(FakeAdapter {
            sends: Mutex::new(Vec::new()),
            fail_next: Mutex::new(0),
        });
        Delivery::new(s.clone(), a.clone())
            .resend_outstanding()
            .await;
        assert_eq!(a.sends.lock().await[0].1, "out");
        std::fs::remove_dir_all(dir).ok();
    }
}
