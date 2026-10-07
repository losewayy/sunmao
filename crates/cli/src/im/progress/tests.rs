use super::*;
use crate::im::channels::{ChannelAdapter, SendFailure, SendResult};
use crate::im::store::Store;

#[test]
fn draft_line_has_elapsed() {
    let st = ProgressState {
        turn_started: Some(Instant::now()),
        last_tool: "Bash".into(),
        ..Default::default()
    };
    let t = draft_text(&st);
    assert!(t.contains("Bash") && t.contains('s'));
}

/// Scripted adapter — records (chat_id, text) pairs, one per channel.
struct FakeAdapter {
    name: &'static str,
    sends: std::sync::Mutex<Vec<(String, String)>>,
}

impl FakeAdapter {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            sends: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn sent(&self, chat_id: &str, text: &str) -> bool {
        self.sends
            .lock()
            .unwrap()
            .iter()
            .any(|(c, t)| c == chat_id && t == text)
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for FakeAdapter {
    fn channel(&self) -> &'static str {
        self.name
    }
    async fn poll(&self, _tx: tokio::sync::mpsc::Sender<crate::im::channels::InboundMsg>) {}
    async fn send_text(&self, chat_id: &str, text: &str) -> SendResult {
        self.sends
            .lock()
            .unwrap()
            .push((chat_id.to_string(), text.to_string()));
        Ok(Some("1".into()))
    }
    async fn edit_text(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

fn store() -> (Arc<Store>, std::path::PathBuf) {
    let dir = crate::im::test_dir("prog");
    (Arc::new(Store::open(&dir).unwrap()), dir)
}

/// Scripted adapter whose first draft send delivers `delivered` of
/// `total` chunks and then fails; every later send succeeds. That is the
/// shape that used to make the next tick post the whole bubble again.
struct PartialDraftAdapter {
    delivered: usize,
    total: usize,
    sends: std::sync::Mutex<Vec<String>>,
    failed_once: std::sync::Mutex<bool>,
}

impl PartialDraftAdapter {
    fn new(delivered: usize, total: usize) -> Self {
        Self {
            delivered,
            total,
            sends: std::sync::Mutex::new(Vec::new()),
            failed_once: std::sync::Mutex::new(false),
        }
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for PartialDraftAdapter {
    fn channel(&self) -> &'static str {
        "test"
    }
    async fn poll(&self, _tx: tokio::sync::mpsc::Sender<crate::im::channels::InboundMsg>) {}
    async fn send_text(&self, _chat_id: &str, text: &str) -> SendResult {
        let mut failed = self.failed_once.lock().unwrap();
        if !*failed {
            *failed = true;
            if self.delivered > 0 {
                // the chunks that landed show up in the chat
                self.sends.lock().unwrap().push(text.to_string());
            }
            return Err(SendFailure::new(
                self.delivered,
                self.total,
                anyhow::anyhow!("a chunk hit a transient error"),
            ));
        }
        drop(failed);
        self.sends.lock().unwrap().push(text.to_string());
        Ok(Some("1".into()))
    }
    async fn edit_text(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

fn drafting_state(key: &ChatKey) -> Shared {
    let state = new_shared();
    let mut st = state.lock().unwrap();
    st.running = true;
    st.turn_started = Some(Instant::now());
    st.pending.insert(key.clone());
    drop(st);
    state
}

/// Adapter declaring `can_edit: false` (wechat/dingtalk/qq shape) — the
/// lane must post the draft once and then leave it alone: no edit calls
/// and no resend on later ticks.
struct NoEditAdapter {
    sends: std::sync::Mutex<Vec<String>>,
    edits: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ChannelAdapter for NoEditAdapter {
    fn channel(&self) -> &'static str {
        "test"
    }
    async fn poll(&self, _tx: tokio::sync::mpsc::Sender<crate::im::channels::InboundMsg>) {}
    async fn send_text(&self, _chat_id: &str, text: &str) -> SendResult {
        self.sends.lock().unwrap().push(text.to_string());
        // wechat/dingtalk have no message id to hand back — the bug this
        // test guards is the draft resending every tick on exactly that
        // shape, so the fake must return Ok(None) too
        Ok(None)
    }
    async fn edit_text(&self, _chat_id: &str, _message_id: &str, text: &str) -> anyhow::Result<()> {
        self.edits.lock().unwrap().push(text.to_string());
        Ok(())
    }
    fn can_edit(&self) -> bool {
        false
    }
}

/// A draft that delivered part of its bubble must not be posted again
/// from the top: the chat would show the first chunk twice. The lane has
/// no ledger to resume from, so the turn simply stops drafting and the
/// final answer still goes out through the ledger.
#[tokio::test]
async fn a_partial_draft_is_not_reposted() {
    let (s, dir) = store();
    let a = Arc::new(PartialDraftAdapter::new(1, 2));
    let delivery = Delivery::new(s.clone(), a.clone());
    let key = ChatKey {
        channel: "test".into(),
        chat_id: "42".into(),
    };
    let state = drafting_state(&key);
    let channels = [Arc::new(delivery)];
    tick(&state, &channels).await;
    assert_eq!(
        a.sends.lock().unwrap().len(),
        1,
        "the first draft attempt went out once"
    );
    // without the guard the next tick posts the whole bubble again: the
    // failure left the draft unthrottled (last_edit is still None)
    tick(&state, &channels).await;
    assert_eq!(
        a.sends.lock().unwrap().len(),
        1,
        "a partially delivered draft must not be reposted"
    );
    assert!(
        state.lock().unwrap().drafts[&key].abandoned,
        "the turn's drafting stops, it does not retry"
    );
    std::fs::remove_dir_all(dir).ok();
}

/// Channels without an edit call keep their first draft and never touch
/// it again — no edit calls and no resend, however many ticks follow.
#[tokio::test]
async fn a_no_edit_channel_keeps_its_first_draft() {
    let (s, dir) = store();
    let a = Arc::new(NoEditAdapter {
        sends: std::sync::Mutex::new(Vec::new()),
        edits: std::sync::Mutex::new(Vec::new()),
    });
    let delivery = Delivery::new(s.clone(), a.clone());
    let key = ChatKey {
        channel: "test".into(),
        chat_id: "42".into(),
    };
    let state = drafting_state(&key);
    let channels = [Arc::new(delivery)];
    tick(&state, &channels).await;
    assert_eq!(a.sends.lock().unwrap().len(), 1, "draft posted once");
    // backdate the throttle cursor so the next tick is genuinely due —
    // without can_edit this would be a resend/edit candidate
    state
        .lock()
        .unwrap()
        .drafts
        .get_mut(&key)
        .unwrap()
        .last_edit = Some(Instant::now() - EDIT_INTERVAL * 2);
    tick(&state, &channels).await;
    assert_eq!(a.sends.lock().unwrap().len(), 1, "no resend");
    assert!(a.edits.lock().unwrap().is_empty(), "no edit calls");
    std::fs::remove_dir_all(dir).ok();
}

/// Nothing landed, so the next tick is free to try again — the guard only
/// covers the duplicate case.
#[tokio::test]
async fn a_draft_that_delivered_nothing_is_retried_next_tick() {
    let (s, dir) = store();
    let a = Arc::new(PartialDraftAdapter::new(0, 2));
    let delivery = Delivery::new(s.clone(), a.clone());
    let key = ChatKey {
        channel: "test".into(),
        chat_id: "42".into(),
    };
    let state = drafting_state(&key);
    let channels = [Arc::new(delivery)];
    tick(&state, &channels).await;
    assert!(!state.lock().unwrap().drafts[&key].abandoned);
    assert!(
        a.sends.lock().unwrap().is_empty(),
        "nothing reached the chat"
    );
    // the retry posts it once, cleanly
    tick(&state, &channels).await;
    assert_eq!(a.sends.lock().unwrap().len(), 1);
    assert!(!state.lock().unwrap().drafts[&key].abandoned);
    std::fs::remove_dir_all(dir).ok();
}

async fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..300 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Discriminator: two adapters on one lane must BOTH receive the
/// turn_end final. The lane builds ONE shared state and ONE progress
/// subscriber for all its channels; whichever channel's chat sits
/// first must not consume the text out from under the other.
#[tokio::test]
async fn both_channels_get_turn_end_final() {
    let (s, dir) = store();
    let alpha = Arc::new(FakeAdapter::new("alpha"));
    let beta = Arc::new(FakeAdapter::new("beta"));
    let state = new_shared();
    expect_reply(
        &state,
        ChatKey {
            channel: "alpha".into(),
            chat_id: "1".into(),
        },
        false,
    );
    expect_reply(
        &state,
        ChatKey {
            channel: "beta".into(),
            chat_id: "2".into(),
        },
        false,
    );
    let (tx, _) = broadcast::channel(16);
    tokio::spawn(run(
        "s1".into(),
        tx.subscribe(),
        state.clone(),
        vec![
            Arc::new(Delivery::new(s.clone(), alpha.clone())),
            Arc::new(Delivery::new(s.clone(), beta.clone())),
        ],
    ));
    tx.send(crate::serve::host::LiveFrame::new(serde_json::json!({
        "sess": "s1", "type": "live",
        "event": {"type": "content", "text": "hello"},
    })))
    .unwrap();
    tx.send(crate::serve::host::LiveFrame::new(serde_json::json!({
        "sess": "s1", "type": "live",
        "event": {"type": "turn_end", "outcome": "ok"},
    })))
    .unwrap();
    wait_for(|| alpha.sent("1", "hello") && beta.sent("2", "hello")).await;
    assert!(
        alpha.sent("1", "hello"),
        "alpha never got the turn_end final: {:?}",
        alpha.sends.lock().unwrap()
    );
    assert!(
        beta.sent("2", "hello"),
        "beta never got the turn_end final: {:?}",
        beta.sends.lock().unwrap()
    );
    std::fs::remove_dir_all(dir).ok();
}
