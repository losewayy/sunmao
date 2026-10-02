//! The goal loop — `/goal`'s "keep working until done" semantic lives here.
//! `run_turn` chains a continuation turn after every Completed outcome
//! while the session goal is `in_progress`; the chain ends when the model
//! settles a verdict (UpdateGoal complete/blocked/abandoned), the round
//! budget runs out, the user cancels, or a frontend has pending input
//! (`ctx.input_pending` — a typed prompt interleaves instead of queueing
//! behind the whole chain).

use super::*;
use crate::context::MutexRecover;
use crate::tool::{GoalState, GoalStatus};

impl AgentLoop {
    /// The session's goal snapshot (`/goal` bare, frontends' status row).
    pub fn goal(&self) -> Option<GoalState> {
        self.ctx.goal.lock_or_recover().clone()
    }

    /// `/goal <objective>` — set (or replace) the session's standing
    /// objective. Takes the turn fence like `set_approval_mode`: a goal
    /// write mid-turn must not land between a ToolCall and its ToolResult.
    /// The continuation loop picks the new state up at the next boundary.
    pub async fn set_goal(&self, objective: &str, observer: &dyn Observer) -> anyhow::Result<()> {
        let _turn_permit = self.ctx.turn_lock.lock().await;
        self.ctx
            .apply_goal(GoalState::new(objective.trim().to_string()))
            .await?;
        observer.on_event(&LiveEvent::Hook {
            event: "goal".into(),
            detail: format!("set — {objective}"),
        });
        Ok(())
    }

    /// `/goal clear` — the explicit human stop. Records `abandoned` rather
    /// than deleting: the audit spine keeps *that* the loop was dropped,
    /// and the status stays visible instead of vanishing.
    pub async fn clear_goal(&self, observer: &dyn Observer) -> anyhow::Result<()> {
        let _turn_permit = self.ctx.turn_lock.lock().await;
        let cur = self.ctx.goal.lock_or_recover().clone();
        let Some(mut goal) = cur else {
            return Ok(());
        };
        goal.status = GoalStatus::Abandoned;
        self.ctx.apply_goal(goal).await?;
        observer.on_event(&LiveEvent::Hook {
            event: "goal".into(),
            detail: "cleared".into(),
        });
        Ok(())
    }

    /// The goal-loop accounting for one finished turn. `None` = the chain
    /// stops here (no goal, a terminal status, budget spent, or a queued
    /// user submission takes precedence). `Some` = the next round's
    /// continuation prompt — the durable Goal event and its live mirror
    /// have already been stamped with the bumped round.
    ///
    /// Only `TurnOutcome::Completed` advances the loop — LengthLimited,
    /// Cancelled and Other all park the chain rather than reward a bad
    /// turn with another round.
    pub(super) async fn goal_next(
        &self,
        outcome: &TurnOutcome,
        observer: &dyn Observer,
    ) -> anyhow::Result<Option<String>> {
        if !matches!(outcome, TurnOutcome::Completed) {
            return Ok(None);
        }
        // a queued submission interleaves — the chain yields to typed input
        // instead of holding the lock while the user waits out max_rounds
        if self
            .ctx
            .input_pending
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
        {
            return Ok(None);
        }
        let mut goal = match self.ctx.goal.lock_or_recover().clone() {
            Some(g) if g.status == GoalStatus::InProgress => g,
            _ => return Ok(None),
        };
        if goal.rounds + 1 >= goal.max_rounds {
            // the budget is spent — park in_progress (not blocked: nothing
            // failed, the ceiling just arrived). The audit row names why
            // the chain stopped so a replay reads it as a pause, not a crash.
            observer.on_event(&LiveEvent::Hook {
                event: "goal".into(),
                detail: format!("max rounds {} reached — paused", goal.max_rounds),
            });
            let mut log = self.ctx.sessions.lock().await;
            log.append_audit(&crate::session::SessionEvent::Hook {
                event: "goal.max_rounds".into(),
                detail: format!("{} rounds", goal.max_rounds),
            })
            .await;
            return Ok(None);
        }
        goal.rounds += 1;
        let next_round = goal.rounds;
        self.ctx.apply_goal(goal.clone()).await?;
        Ok(Some(crate::prompt::goal_continue_prompt(
            &self.ctx.cwd,
            &goal.objective,
            next_round,
            goal.max_rounds,
        )))
    }
}
