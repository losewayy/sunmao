//! Steering — the frontends' mid-turn input surface. `push_steer` queues a
//! line the running turn drains at its next request boundary;
//! `steer_sub`/`cancel_sub` point the same idea at ONE roster row, and both
//! leave an audit row on the PARENT's log — a steer or a kill that leaves
//! no trace can't be attributed.

use super::AgentLoop;
use crate::context::MutexRecover;

impl AgentLoop {
    /// Queue user steering for the running turn — the loop drains it at the
    /// next request boundary and folds the text in as a user message, so it
    /// steers THIS turn instead of becoming a queued next submission.
    pub fn push_steer(&self, client: u64, text: String) {
        self.ctx.steer.lock_or_recover().push_back((client, text));
    }

    /// Drop a queued steer before the turn consumes it (GUI chip ×). The
    /// index is positional over the current queue; consumed items have
    /// already left it, so a stale index is a harmless no-op.
    pub fn cancel_steer(&self, idx: usize) {
        self.ctx.steer.lock_or_recover().remove(idx);
    }

    /// Current steering backlog — replay/hello report it so a joining tab
    /// shows the same queued chips.
    pub fn steer_queue(&self) -> Vec<String> {
        self.ctx
            .steer
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.clone())
            .collect()
    }

    /// Steer a *sub-agent* mid-turn: roster lookup, then push onto the
    /// child's steer queue — its own turn loop folds the text as a user
    /// message at the next request boundary. Errors are legible: a finished
    /// child must be `resume`d, an unknown id never registered (or predates
    /// this process — resume covers that too). Async because a delivered
    /// steer is an audit fact on the PARENT's log — the child's log keeps
    /// the text as a user message, the parent's keeps *who* pushed it.
    pub async fn steer_sub(
        &self,
        sub_id: &str,
        text: String,
    ) -> Result<(), crate::context::SubSteerError> {
        self.ctx.steer_sub(sub_id, text.clone())?;
        let mut log = self.ctx.sessions.lock().await;
        log.append_audit(&crate::session::SessionEvent::Hook {
            event: "task.steer".into(),
            detail: format!("{sub_id}: {text}"),
        })
        .await;
        Ok(())
    }

    /// Cancel ONE running sub-agent by id — the pointed version of the
    /// `cancel()` cascade, for the GUI roster's kill control. The roster
    /// row's `done` is set by the child's own finish path, not here.
    /// Async because a successful kill is an audit fact on the PARENT's
    /// log — a roster kill that leaves no trace can't be attributed.
    pub async fn cancel_sub(&self, sub_id: &str) -> Result<(), crate::context::SubSteerError> {
        self.ctx.cancel_sub(sub_id)?;
        let mut log = self.ctx.sessions.lock().await;
        log.append_audit(&crate::session::SessionEvent::Hook {
            event: "task.cancel".into(),
            detail: sub_id.to_string(),
        })
        .await;
        Ok(())
    }

    /// Claim every queued steer — the turn boundary drains into the log;
    /// the driver drains leftovers after a turn ends (a steer that arrived
    /// mid-shutdown becomes the next submission, never dropped).
    pub fn drain_steer(&self) -> Vec<(u64, String)> {
        self.ctx.steer.lock_or_recover().drain(..).collect()
    }
}
