//! Turn modes (`/mode standard|fusion`) — which *shape* a turn takes,
//! distinct from the approval stance (`approval_mode`, how permissive the
//! gate is) and the loop driver (`loop_driver`, which circuit runs).
//! Standard is the single-adapter turn; Fusion splits a turn into a
//! read-only Lead (the session's model) plus a delegated Sidekick worker
//! (`agent/fusion.rs`).
//!
//! Durable as `SessionEvent::TurnModeChange` — a dedicated event rather
//! than a `ModeChange` payload: `ModeChange` carries an `ApprovalMode`,
//! and `fusion` is not an approval stance. The mode is per-Context (not
//! Arc-shared like `approval_mode`): a sub-agent context always runs
//! Standard — Sidekicks don't get to delegate another level.

use super::*;

/// How a turn executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnMode {
    /// The ordinary single-adapter turn — every tool call runs here.
    #[default]
    Standard,
    /// Lead/Sidekick split: this context becomes the read-only Lead —
    /// mutating calls short-circuit at the gate (`ctx.read_only`) and its
    /// declared surface sheds the write tools; work delegates to a
    /// fresh-context Sidekick through `FusionExecute`.
    Fusion,
}

impl TurnMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Fusion => "fusion",
        }
    }

    /// Every spelling the frontends may send.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "standard" | "default" | "单模型" => Some(Self::Standard),
            "fusion" | "fused" => Some(Self::Fusion),
            _ => None,
        }
    }

    /// The canonical selectors, in menu order.
    pub const ALL: [Self; 2] = [Self::Standard, Self::Fusion];
}

impl AgentLoop {
    /// The session's current turn mode.
    pub fn turn_mode(&self) -> TurnMode {
        *self.ctx.turn_mode.read_or_recover()
    }

    /// Switch the turn mode mid-session — `set_approval_mode`'s twin:
    /// the turn fence makes the flip land between turns, the
    /// `TurnModeChange` fact makes it durable, and the live Hook row
    /// announces it. Switching to Fusion arms the per-context
    /// `read_only` flag (the Lead's gate block); switching back disarms
    /// it and drops any live delegation state.
    ///
    /// Refuses `fusion` under the `ptc` driver — fusion ⊥ ptc: the Ptc
    /// advertised surface is RunCode alone, so a Lead could never emit
    /// the delegation call. (TODO: a fusion-aware Ptc surface could
    /// advertise FusionExecute alongside RunCode.)
    pub async fn set_turn_mode(
        &self,
        mode: TurnMode,
        observer: &dyn Observer,
    ) -> Result<(), String> {
        if mode == TurnMode::Fusion && self.ctx.loop_driver == LoopDriver::Ptc {
            return Err(
                "fusion needs the standard tool surface — the ptc loop advertises RunCode only"
                    .into(),
            );
        }
        let _turn_permit = self.ctx.turn_lock.lock().await;
        *self.ctx.turn_mode.write_or_recover() = mode;
        self.ctx.read_only.store(
            mode == TurnMode::Fusion,
            std::sync::atomic::Ordering::Relaxed,
        );
        // a mode flip dissolves the delegation: a dropped Sidekick's later
        // `steer` would be a steer into nothing, and the verify-fail streak
        // belongs to the abandoned session shape
        *self.ctx.fusion.lock_or_recover() = fusion::FusionState::default();
        {
            let mut log = self.ctx.sessions.lock().await;
            if let Err(e) = log.append(&SessionEvent::TurnModeChange { mode }).await {
                tracing::warn!("turn-mode change not durable: {e:#}");
            }
        }
        observer.on_event(&LiveEvent::Hook {
            event: "turn.mode".into(),
            detail: mode.as_str().to_string(),
        });
        Ok(())
    }
}
