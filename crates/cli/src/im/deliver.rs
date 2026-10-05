//! Outbound delivery ledger — at-least-once final replies. Design (§3.6):
//! a `final` text is recorded `pending` before the first send, flips
//! `attempting` while the API call is in flight, lands `delivered` on
//! success. `pending`/`attempting` rows at startup get replayed — the
//! `attempting` ones already reached the wire (possibly), so the replay
//! carries the ♻️ "may repeat" prefix: honest about the ambiguity.
//!
//! Retry discipline. Text is chunked *inside* an adapter, so the ledger sees
//! one opaque `send_text` call, and a failure in the middle of it means part
//! of the answer is already on the platform. Resending the text would
//! duplicate those chunks, and WeChat/DingTalk mint a fresh `client_id` per
//! attempt, so the platform cannot de-duplicate on its side. The reference's
//! rule is the same one: a send that may have landed is never retried
//! (`outbound-http.ts` only retries `idempotent` requests). So a retry
//! happens only while the adapter reports that nothing was delivered; once a
//! chunk is out, the row is dead-lettered instead of resent, which also
//! keeps the next startup from replaying the whole text.

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

    /// Send a final reply through the ledger. Errors surface to the caller
    /// (progress lane logs them). A row that delivered nothing stays
    /// outstanding for a later `resend_outstanding`; one that already
    /// delivered part of the answer is dead-lettered, because resending it
    /// would duplicate the chunks that landed.
    pub async fn send_final(&self, chat_id: &str, text: &str) -> Result<()> {
        let id = self
            .store
            .deliver_pending(self.adapter.channel(), chat_id, text)?;
        self.attempt(id, chat_id, text).await
    }

    /// Re-deliver a ledger row — `text` may carry the ♻️ prefix when the
    /// row was `attempting` at replay time.
    ///
    /// The retry loop stops for good as soon as the adapter reports that a
    /// chunk reached the platform: re-sending would duplicate it, so the row
    /// is dead-lettered (`deliver_dead`) rather than left outstanding, which
    /// would have the next startup replay the whole text.
    async fn attempt(&self, id: i64, chat_id: &str, text: &str) -> Result<()> {
        let mut last_err = None;
        for _ in 0..SEND_RETRIES {
            self.store.deliver_attempting(id)?;
            match self.adapter.send_text(chat_id, text).await {
                Ok(_) => return self.store.deliver_done(id),
                Err(e) if e.is_partial() => {
                    let (delivered, total) = (e.delivered(), e.total());
                    let note = format!("partial: {} chunks delivered", e.progress());
                    self.store.deliver_dead(id, &note)?;
                    tracing::warn!(
                        "im delivery {} stopped after {}/{} chunk(s): dead-lettered, \
                         the remaining chunks are not resent because that would \
                         duplicate the ones that landed",
                        id,
                        delivered,
                        total
                    );
                    return Err(e.into_source());
                }
                Err(e) => {
                    last_err = Some(e.into_source());
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
                let _ = self
                    .store
                    .deliver_dead(row.id, "retry budget exhausted, nothing delivered");
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
    use crate::im::channels::{SendFailure, SendResult};
    use tokio::sync::Mutex;

    /// Scripted adapter — records sends, fails a configurable streak.
    /// Nothing is ever delivered before its failures, so it stands for a
    /// channel that died before the first chunk went out.
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
        async fn send_text(&self, chat_id: &str, text: &str) -> SendResult {
            let mut fails = self.fail_next.lock().await;
            if *fails > 0 {
                *fails -= 1;
                return Err(SendFailure::before_send(anyhow::anyhow!("flaky")));
            }
            drop(fails);
            self.sends
                .lock()
                .await
                .push((chat_id.to_string(), text.to_string()));
            Ok(Some("1".into()))
        }
    }

    /// Scripted adapter that chunks like the real ones: `pieces` chunks per
    /// send, and on its first call it delivers `fail_after` chunks before
    /// failing. The failure script is consumed by that first call, so a retry
    /// would deliver the whole text from the top — exactly the duplicate the
    /// ledger has to prevent.
    struct ChunkingAdapter {
        pieces: usize,
        fail_after: Mutex<Option<usize>>,
        delivered: Mutex<Vec<usize>>,
    }

    impl ChunkingAdapter {
        fn new(pieces: usize, fail_after: Option<usize>) -> Self {
            Self {
                pieces,
                fail_after: Mutex::new(fail_after),
                delivered: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl ChannelAdapter for ChunkingAdapter {
        fn channel(&self) -> &'static str {
            "test"
        }
        async fn poll(&self, _tx: tokio::sync::mpsc::Sender<crate::im::channels::InboundMsg>) {}
        async fn send_text(&self, _chat_id: &str, _text: &str) -> SendResult {
            let fail_after = self.fail_after.lock().await.take();
            for index in 0..self.pieces {
                if fail_after == Some(index) {
                    return Err(SendFailure::new(
                        index,
                        self.pieces,
                        anyhow::anyhow!("chunk {index} hit a transient error"),
                    ));
                }
                self.delivered.lock().await.push(index);
            }
            Ok(Some("1".into()))
        }
    }

    fn store() -> (Arc<Store>, std::path::PathBuf) {
        let dir = crate::im::test_dir("dlv");
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

    /// Case 1 of the retry rule: nothing was delivered, so the existing
    /// retry semantics stand. Past the budget the row stays outstanding —
    /// replaying it later is a *first* send, not a duplicate.
    #[tokio::test]
    async fn a_send_that_delivered_nothing_is_still_retried() {
        let (s, dir) = store();
        let a = Arc::new(FakeAdapter {
            sends: Mutex::new(Vec::new()),
            fail_next: Mutex::new(SEND_RETRIES),
        });
        let d = Delivery::new(s.clone(), a.clone());
        assert!(d.send_final("42", "hello").await.is_err());
        assert!(
            a.sends.lock().await.is_empty(),
            "nothing reached the channel"
        );
        let rows = s.deliver_outstanding();
        assert_eq!(rows.len(), 1, "a nothing-delivered row stays replayable");
        assert_eq!(rows[0].state, "attempting");
        assert_eq!(
            rows[0].attempts,
            i64::from(SEND_RETRIES),
            "the whole retry budget was spent"
        );
        assert_eq!(
            s.delivery_state_note(rows[0].id).unwrap().1,
            "",
            "no dead-letter note: it never became a dead letter"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// Case 2, the bug this task exists for: chunk 0 is on the platform and
    /// chunk 1 failed. Retrying the send would put chunk 0 there twice, and
    /// WeChat/DingTalk mint a fresh `client_id` per attempt, so the platform
    /// cannot de-duplicate. The ledger must not retry, must record why, and
    /// must not leave the row outstanding either (the next startup would
    /// replay the same whole text).
    #[tokio::test]
    async fn a_partial_send_is_never_replayed_from_the_top() {
        let (s, dir) = store();
        let a = Arc::new(ChunkingAdapter::new(3, Some(1)));
        let d = Delivery::new(s.clone(), a.clone());
        let id = s.deliver_pending("test", "42", "a long answer").unwrap();
        let outcome = d.attempt(id, "42", "a long answer").await;
        // the duplicate check comes first: if this ever regresses, the failure
        // message is the record of what actually reached the platform
        assert_eq!(
            *a.delivered.lock().await,
            vec![0],
            "chunk 0 must reach the platform exactly once"
        );
        assert!(outcome.is_err());
        let (state, note) = s.delivery_state_note(id).unwrap();
        assert_eq!(state, "dead");
        assert_eq!(
            note, "partial: 1/3 chunks delivered",
            "an operator has to tell a truncated answer from one that never went out"
        );
        assert!(
            s.deliver_outstanding().is_empty(),
            "a partially delivered row must not be replayed"
        );
        // a restart must not resend it either
        d.resend_outstanding().await;
        assert_eq!(*a.delivered.lock().await, vec![0]);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Case 3: every chunk lands — one clean send, nothing outstanding and no
    /// dead-letter note.
    #[tokio::test]
    async fn a_complete_chunked_send_lands_delivered() {
        let (s, dir) = store();
        let a = Arc::new(ChunkingAdapter::new(3, None));
        let d = Delivery::new(s.clone(), a.clone());
        let id = s.deliver_pending("test", "42", "three chunks").unwrap();
        d.attempt(id, "42", "three chunks").await.unwrap();
        assert_eq!(*a.delivered.lock().await, vec![0, 1, 2]);
        assert_eq!(
            s.delivery_state_note(id).unwrap(),
            ("delivered".to_string(), String::new())
        );
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
