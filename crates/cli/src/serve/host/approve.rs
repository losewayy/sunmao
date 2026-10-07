//! Per-session approval + observer seams — split out of `host.rs` when the
//! host registry hit the 600-line shape budget. `Pending` is the card map
//! the risky-call gate suspends on; `WsObserver` re-tags LiveEvents with
//! the session id before they hit the broadcast bus.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use sunmao_core::context::MutexRecover;

use sunmao_core::agent::{LiveEvent, Observer};
use sunmao_core::approval::{Approval, Approver};
use tokio::sync::oneshot;

/// One unanswered approval card — the reply oneshot plus the payload the
/// card was raised with (a tab switching into a session re-renders cards
/// from these, so a pending approval survives the view switch).
pub(crate) struct PendingCard {
    pub(crate) tx: oneshot::Sender<Approval>,
    tool: String,
    detail: String,
    why: String,
}

/// Approval state for ONE session — `Context.approval` installs the
/// approver holding this handle while the context is being built; the host
/// adopts the same instance. The id space is process-global so a verdict
/// can never hit the wrong session's card.
pub(crate) struct Pending {
    pub(crate) map: Mutex<HashMap<u64, PendingCard>>,
    /// shared across hosts — ids are already unique per process, a counter
    /// per session would collide cards from concurrent sessions
    next: Arc<AtomicU64>,
    /// approval requests go out over the same live bus as LiveEvents,
    /// tagged with this session's id
    live: super::LiveBus,
    sess: String,
}

impl Pending {
    pub(crate) fn new(live: super::LiveBus, next: Arc<AtomicU64>, sess: String) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            next,
            live,
            sess,
        }
    }

    /// Cards still awaiting a verdict — replayed to a tab that starts
    /// viewing this session.
    pub(crate) fn cards(&self) -> Vec<serde_json::Value> {
        self.map
            .lock()
            .unwrap()
            .iter()
            .map(|(id, c)| {
                serde_json::json!({"id": id, "tool": c.tool, "detail": c.detail, "why": c.why})
            })
            .collect()
    }
}

/// Approver seam — the risky-call gate suspends on a oneshot while the
/// browser shows the card. Same contract as TuiApprover.
pub(crate) struct ServeApprover {
    pub(crate) pending: Arc<Pending>,
}

#[async_trait::async_trait]
impl Approver for ServeApprover {
    async fn approve(&self, tool: &str, detail: &str, why: &str) -> Approval {
        let id = self.pending.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.map.lock_or_recover().insert(
            id,
            PendingCard {
                tx,
                tool: tool.to_string(),
                detail: detail.to_string(),
                why: why.to_string(),
            },
        );
        self.pending.live.send(serde_json::json!({
            "type": "approval", "id": id, "sess": self.pending.sess,
            "tool": tool, "detail": detail, "why": why,
        }));
        // verdicts route through the map by id — ws ordering never decides
        rx.await.unwrap_or(Approval::Deny { reason: None })
    }

    fn cancel_pending(&self) {
        // every parked card resolves Cancelled AND every tab hears
        // approval_done — the ask() suspension and the DOM card both close
        // in the same sweep
        let drained: Vec<u64> = {
            let mut map = self.pending.map.lock_or_recover();
            map.drain()
                .map(|(id, c)| {
                    let _ = c.tx.send(Approval::Cancelled);
                    id
                })
                .collect()
        };
        for id in drained {
            self.pending.live.send(serde_json::json!({
                "type": "approval_done", "sess": self.pending.sess,
                "id": id, "why": "cancelled",
            }));
        }
    }
}

/// Observer → broadcast, tagged with its session id. LiveEvent serializes
/// as the session-neutral wire shape (tagged enum) — frontends never see
/// a second dialect; `sess` is the tab's routing key.
pub(crate) struct WsObserver {
    live: super::LiveBus,
    sess: String,
}
impl WsObserver {
    pub(crate) fn new(live: super::LiveBus, sess: impl Into<String>) -> Self {
        Self {
            live,
            sess: sess.into(),
        }
    }
}
impl Observer for WsObserver {
    fn on_event(&self, ev: &LiveEvent) {
        self.live.send(serde_json::json!({
            "type": "live",
            "sess": self.sess,
            "event": serde_json::to_value(ev).unwrap_or_default(),
        }));
    }
}
