//! Progress lane — subscribes the session's live bus, maintains one
//! editable status draft per pending chat, and routes the turn's final
//! text through the delivery ledger. Design (§3.4, `streaming: progress`):
//! the draft carries *activity* (elapsed time, current tool), never the
//! answer itself — a crash mid-turn can never strand a half-written
//! reply; the final answer is a fresh message.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use super::channels::ChannelAdapter;
use super::deliver::Delivery;
use super::messages;

/// Draft refresh cadence — editMessageText is rate-limited (flood control
/// kicks in around 1 edit/sec/chat); 3s keeps the draft live without
/// tripping it.
const EDIT_INTERVAL: Duration = Duration::from_secs(3);
/// Telegram's typing indicator expires after ~5s — refresh under that.
const TYPING_INTERVAL: Duration = Duration::from_secs(4);

/// Chats with an unanswered message in flight — a turn's final answer
/// fans out to all of them (dmScope=main: the sender that submitted and
/// anyone who steered into the same turn).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChatKey {
    pub channel: String,
    pub chat_id: String,
}

/// One chat's live draft — the status message's channel-side id and the
/// throttle cursor. `None` id = draft not yet posted (first event creates
/// it lazily — a turn may end before any activity is worth showing).
#[derive(Default)]
struct Draft {
    message_id: Option<String>,
    last_edit: Option<Instant>,
    last_typing: Option<Instant>,
}

/// The lane's shared state — ingress adds chats, the subscriber drains.
#[derive(Default)]
pub struct ProgressState {
    /// chats awaiting the current turn's reply
    pending: HashSet<ChatKey>,
    /// per-chat draft handles
    drafts: HashMap<ChatKey, Draft>,
    /// the latest assistant text segment — resets when a tool starts
    /// (inter-step narration is not the answer; the *last* segment is)
    latest_text: String,
    /// last tool seen, for the status line
    last_tool: String,
    /// turn start — drives the `{elapsed}` field
    turn_started: Option<Instant>,
    /// current outcome until TurnEnd lands
    running: bool,
}

/// The handle ingress and the subscriber both hold.
pub type Shared = Arc<Mutex<ProgressState>>;

pub fn new_shared() -> Shared {
    Arc::new(Mutex::new(ProgressState::default()))
}

/// Register a chat whose message entered the pipeline — its reply arrives
/// at the next `turn_end` regardless of steer-vs-queue path.
pub fn expect_reply(state: &Shared, key: ChatKey) {
    state.lock().unwrap().pending.insert(key);
}

/// The draft's body — status text only, no answer fragments.
fn draft_text(st: &ProgressState) -> String {
    let elapsed = st.turn_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let last = if st.last_tool.is_empty() {
        "thinking".to_string()
    } else {
        st.last_tool.clone()
    };
    messages::get("status_line")
        .replace("{elapsed}", &elapsed.to_string())
        .replace("{last}", &last)
}

/// The subscriber loop — one per session the gateway owns (dmScope=main:
/// exactly one). Runs until the bus closes (process shutdown).
/// `sess` filters the shared live bus to this session's frames.
pub async fn run(
    sess: String,
    mut rx: broadcast::Receiver<serde_json::Value>,
    state: Shared,
    adapter: Arc<dyn ChannelAdapter>,
    delivery: Arc<Delivery>,
) {
    loop {
        let frame = match rx.recv().await {
            Ok(v) => v,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if frame["sess"].as_str() != Some(sess.as_str()) {
            continue;
        }
        match frame["type"].as_str().unwrap_or("") {
            "live" => handle_live(&frame["event"], &state).await,
            // slash-command replies (`note` frames) belong to whoever has a
            // pending turn — deliver verbatim through the ledger
            "note" => {
                let text = frame["text"].as_str().unwrap_or_default().to_string();
                if !text.is_empty() {
                    let keys = pending_of(&state.lock().unwrap());
                    for key in keys {
                        let _ = delivery.send_final(&key.chat_id, &text).await;
                    }
                }
            }
            _ => {}
        }
        // any event is a reason to maybe-refresh drafts — the throttle
        // inside tick() caps it
        tick(&state, &adapter).await;
        flush_on_turn_end(&frame, &state, &adapter, &delivery).await;
    }
}

/// Read the pending set without holding the lock across awaits.
fn pending_of(st: &ProgressState) -> Vec<ChatKey> {
    st.pending.iter().cloned().collect()
}

/// Fold a live event into progress state. `tool_start` clears the
/// accumulated text — the segment before it was narration, not the
/// answer; the segment after it may be. Depth>0 events are sub-agent
/// traffic relayed through the live sink — the draft follows the parent
/// only.
async fn handle_live(ev: &serde_json::Value, state: &Shared) {
    let mut st = state.lock().unwrap();
    match ev["type"].as_str().unwrap_or("") {
        "content" => {
            st.latest_text.push_str(ev["text"].as_str().unwrap_or(""));
            if st.turn_started.is_none() {
                st.turn_started = Some(Instant::now());
            }
            st.running = true;
        }
        "tool_start" if ev["depth"].as_u64() == Some(0) => {
            st.latest_text.clear();
            st.last_tool = ev["name"].as_str().unwrap_or("").to_string();
            if st.turn_started.is_none() {
                st.turn_started = Some(Instant::now());
            }
            st.running = true;
        }
        "turn_end" => {
            st.running = false;
        }
        _ => {}
    }
}

/// Throttled draft maintenance + typing keep-alive for every pending chat.
async fn tick(state: &Shared, adapter: &Arc<dyn ChannelAdapter>) {
    let (text, keys) = {
        let st = state.lock().unwrap();
        if !st.running || st.pending.is_empty() {
            return;
        }
        (draft_text(&st), pending_of(&st))
    };
    for key in keys {
        // typing under every tick cadence — cheap and self-rearming
        let needs_typing = {
            let st = state.lock().unwrap();
            st.drafts
                .get(&key)
                .and_then(|d| d.last_typing)
                .is_none_or(|t| t.elapsed() >= TYPING_INTERVAL)
        };
        if needs_typing && key.channel == adapter.channel() {
            adapter.send_typing(&key.chat_id).await;
            if let Some(d) = state.lock().unwrap().drafts.get_mut(&key) {
                d.last_typing = Some(Instant::now());
            } else {
                state
                    .lock()
                    .unwrap()
                    .drafts
                    .entry(key.clone())
                    .or_default()
                    .last_typing = Some(Instant::now());
            }
        }
        if key.channel != adapter.channel() {
            continue;
        }
        let (id, due) = {
            let st = state.lock().unwrap();
            let d = st.drafts.get(&key);
            (
                d.and_then(|d| d.message_id.clone()),
                d.and_then(|d| d.last_edit)
                    .is_none_or(|t| t.elapsed() >= EDIT_INTERVAL),
            )
        };
        if !due {
            continue;
        }
        match id {
            // first frame → post the draft, keep the id for edits
            None => {
                if let Ok(Some(mid)) = adapter.send_text(&key.chat_id, &text).await {
                    let mut st = state.lock().unwrap();
                    let d = st.drafts.entry(key).or_default();
                    d.message_id = Some(mid);
                    d.last_edit = Some(Instant::now());
                }
            }
            Some(mid) => {
                if adapter.edit_text(&key.chat_id, &mid, &text).await.is_ok()
                    && let Some(d) = state.lock().unwrap().drafts.get_mut(&key)
                {
                    d.last_edit = Some(Instant::now());
                }
            }
        }
    }
}

/// Turn ended → the draft freezes, `latest_text` goes out as the final
/// reply through the ledger, and pending clears for the next turn.
async fn flush_on_turn_end(
    frame: &serde_json::Value,
    state: &Shared,
    adapter: &Arc<dyn ChannelAdapter>,
    delivery: &Arc<Delivery>,
) {
    let is_end = frame["type"] == "live" && frame["event"]["type"] == "turn_end";
    if !is_end {
        return;
    }
    let (keys, text, outcome) = {
        let mut st = state.lock().unwrap();
        let keys = pending_of(&st);
        let outcome = frame["event"]["outcome"].as_str().unwrap_or("").to_string();
        let text = std::mem::take(&mut st.latest_text);
        st.pending.clear();
        st.last_tool.clear();
        st.turn_started = None;
        // drafts stay keyed — a stale draft message just sits in the chat;
        // the next turn's first tick replaces it via a fresh send
        st.drafts.clear();
        (keys, text, outcome)
    };
    if keys.is_empty() {
        return;
    }
    // cancelled turns send the stop marker; other non-clean outcomes fall
    // through with whatever text (possibly empty) accumulated
    let final_text = match outcome.as_str() {
        "cancelled" if text.trim().is_empty() => messages::get("stopped"),
        _ => text,
    };
    for key in keys {
        if key.channel != adapter.channel() {
            continue;
        }
        if !final_text.trim().is_empty()
            && let Err(e) = delivery.send_final(&key.chat_id, &final_text).await
        {
            tracing::warn!("im final → {}: {e:#}", key.chat_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
