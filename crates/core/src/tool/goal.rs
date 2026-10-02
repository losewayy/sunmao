//! `UpdateGoal` — the session's cross-turn objective, a first-class native
//! tool (SPEC §4.3). A goal is product semantics the same way the task list
//! is: `/goal` or the model writes it, the kernel persists it
//! (`SessionEvent::Goal`), and every completed turn under an `in_progress`
//! goal advances `rounds` and chains another turn until the model marks it
//! `complete`, declares it `blocked`, or `rounds` hits `max_rounds`.
//!
//! Blocked is earned, not declared: the same blocker must be reported in at
//! least `BLOCKED_MIN_ROUNDS` distinct rounds before a `blocked` write
//! sticks — a one-round stall keeps the goal `in_progress` and the report
//! itself counts toward the streak (the DSH rule this mirrors).

use crate::context::MutexRecover;
use crate::tool::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Consecutive distinct rounds the same blocker must be reported in before
/// `status:"blocked"` is accepted — a single stalled turn can't declare
/// the goal dead.
pub const BLOCKED_MIN_ROUNDS: u32 = 2;

/// Where an `in_progress` goal's own loop stops volunteering turns — a
/// safety ceiling, not a plan: raise it with `UpdateGoal.max_rounds` or
/// drop the goal with `/goal clear`.
pub const DEFAULT_MAX_ROUNDS: u32 = 32;

/// `read_dir`-style fast prefilter for log lines — the serializer emits
/// `"type"` first, so a `Goal` line always starts with this literal.
/// Used by the reseed scan that walks possibly-large logs.
pub(crate) const GOAL_LINE_PREFIX: &str = "{\"type\":\"goal\"";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    InProgress,
    Complete,
    Blocked,
    /// `/goal clear` — an explicit human stop, distinct from the model's
    /// verdicts so a cleared goal never reads as a resolved one.
    Abandoned,
}

/// The session's goal snapshot — durable as `SessionEvent::Goal` (last
/// write wins on resume), mirrored live as `LiveEvent::Goal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalState {
    pub objective: String,
    pub status: GoalStatus,
    /// Completed turns since the goal was (re)set — the loop's ceiling is
    /// `max_rounds`, so `rounds` doubles as the progress readout.
    pub rounds: u32,
    pub max_rounds: u32,
    /// The blocker the model last reported (`UpdateGoal.blocker`) — cleared
    /// the moment a report names nothing or the goal returns to progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
    /// Distinct rounds `blocker` has been reported in — resets when the
    /// reported blocker text changes.
    #[serde(default)]
    pub blocker_streak: u32,
    /// `rounds` at the streak's last report — same-round re-reports don't
    /// inflate the streak; a different round must carry it.
    #[serde(default)]
    blocker_last_round: u32,
}

impl GoalState {
    pub fn new(objective: String) -> Self {
        Self {
            objective,
            status: GoalStatus::InProgress,
            rounds: 0,
            max_rounds: DEFAULT_MAX_ROUNDS,
            blocker: None,
            blocker_streak: 0,
            blocker_last_round: 0,
        }
    }

    /// Model-facing render — appended (synthetic, never logged) to the
    /// folded messages of every request while a goal is in progress, so the
    /// objective survives compaction and steer noise. Short on purpose: the
    /// continuation prompt owns the full phrasing.
    pub fn inject_text(&self) -> String {
        format!(
            "[goal round {}/{} · {} — status {}]",
            self.rounds,
            self.max_rounds,
            status_name(self.status),
            self.objective,
        )
    }

    /// Human-facing one-liner — `/goal`, the tool's echo, and the frontends'
    /// status rows all render through this.
    pub fn render(&self) -> String {
        let extra = match self.status {
            GoalStatus::Blocked => self
                .blocker
                .as_deref()
                .map(|b| format!(" — {b}"))
                .unwrap_or_default(),
            _ => String::new(),
        };
        format!(
            "{} ({} · round {}/{}){}",
            self.objective,
            status_name(self.status),
            self.rounds,
            self.max_rounds,
            extra
        )
    }
}

/// Wire spelling of a status — one name shared by the tool echo, the
/// slash command's note, and every frontend's chip.
pub fn status_name(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::InProgress => "in progress",
        GoalStatus::Complete => "complete",
        GoalStatus::Blocked => "blocked",
        GoalStatus::Abandoned => "abandoned",
    }
}

/// Apply a blocker report to `goal` — a new report of the SAME blocker in
/// a LATER round grows the streak; a re-report inside one round doesn't
/// count twice, and a changed blocker restarts it.
pub(crate) fn apply_blocker(goal: &mut GoalState, blocker: Option<String>) {
    match blocker {
        Some(b) if !b.trim().is_empty() => {
            let b = b.trim().to_string();
            if goal.blocker.as_deref() == Some(b.as_str()) {
                if goal.blocker_last_round < goal.rounds {
                    goal.blocker_streak += 1;
                    goal.blocker_last_round = goal.rounds;
                }
            } else {
                goal.blocker = Some(b);
                goal.blocker_streak = 1;
                goal.blocker_last_round = goal.rounds;
            }
        }
        _ => {
            goal.blocker = None;
            goal.blocker_streak = 0;
        }
    }
}

pub struct UpdateGoalTool;

#[async_trait::async_trait]
impl ToolImpl for UpdateGoalTool {
    fn name(&self) -> &'static str {
        "UpdateGoal"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "UpdateGoal",
            "Set or update the session goal — a cross-turn objective this \
             agent keeps working on until it is complete or blocked. When the \
             user asks you to keep going toward an outcome (or to set your own \
             goal), write it here: after every turn that ends while the goal \
             is in progress, the harness feeds you another turn. Report \
             progress with `blocker` when something stalls you; a goal may \
             only be marked `blocked` after the SAME blocker was reported in \
             two or more consecutive rounds. Mark `complete` only when the \
             objective is genuinely met.",
            json!({
                "type": "object",
                "properties": {
                    "objective": {
                        "type": "string",
                        "description": "the standing objective; required when no goal exists, a changed objective restarts round counting"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["in_progress", "complete", "blocked", "abandoned"],
                        "description": "terminal verdicts (complete/blocked/abandoned) end the self-continuation loop"
                    },
                    "blocker": {
                        "type": "string",
                        "description": "what is currently blocking progress; report it honestly each round it applies"
                    },
                    "max_rounds": {
                        "type": "integer",
                        "description": "turn budget for self-continuation (default 32)"
                    }
                }
            }),
        )
    }

    async fn call(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            objective: Option<String>,
            status: Option<GoalStatus>,
            blocker: Option<String>,
            max_rounds: Option<u32>,
        }
        let a: Args = serde_json::from_value(args)?;
        let mut goal = ctx.goal.lock_or_recover().clone();

        // an objective rewrite = a new goal — the round/streak accounting
        // of the old objective must not carry over
        if let Some(obj) = a.objective {
            if obj.trim().is_empty() {
                return Ok(ToolResult {
                    output: "UpdateGoal rejected: objective is empty".into(),
                    ok: false,
                });
            }
            let keep = goal
                .as_ref()
                .is_some_and(|g| g.objective == obj && g.status == GoalStatus::InProgress);
            if !keep {
                goal = Some(GoalState::new(obj));
            }
        }
        let Some(goal) = goal else {
            return Ok(ToolResult {
                output: "UpdateGoal rejected: no goal exists — pass `objective` to set one".into(),
                ok: false,
            });
        };
        let mut goal = goal;

        if let Some(mr) = a.max_rounds
            && mr > 0
        {
            goal.max_rounds = mr;
        }
        apply_blocker(&mut goal, a.blocker);

        let mut note = String::new();
        match a.status {
            Some(GoalStatus::Complete) => {
                goal.status = GoalStatus::Complete;
                goal.blocker = None;
                goal.blocker_streak = 0;
                note = "goal marked complete".into();
            }
            Some(GoalStatus::Abandoned) => {
                goal.status = GoalStatus::Abandoned;
                note = "goal abandoned".into();
            }
            Some(GoalStatus::Blocked) => {
                if goal.blocker_streak >= BLOCKED_MIN_ROUNDS {
                    goal.status = GoalStatus::Blocked;
                    note = "goal marked blocked".into();
                } else {
                    // the report stands (it grew the streak); the verdict
                    // doesn't land — a one-round stall is not yet "blocked"
                    note = format!(
                        "blocker recorded ({}/{} consecutive rounds) — goal stays in progress; report the same blocker next round to mark it blocked",
                        goal.blocker_streak, BLOCKED_MIN_ROUNDS
                    );
                }
            }
            Some(GoalStatus::InProgress) => {
                if goal.status != GoalStatus::InProgress {
                    goal.status = GoalStatus::InProgress;
                    goal.rounds = 0;
                }
                note = "goal in progress".into();
            }
            None => {}
        }
        ctx.apply_goal(goal.clone()).await?;
        Ok(ToolResult {
            output: format!("goal: {}\n{note}", goal.render()),
            ok: true,
        })
    }
}

impl crate::context::Context {
    /// The single write path for goal state — `/goal` (frontends) and
    /// `UpdateGoal` (the model) both land here so the durable event, the
    /// live mirror, and the ctx snapshot can never disagree.
    pub(crate) async fn apply_goal(&self, goal: GoalState) -> anyhow::Result<GoalState> {
        {
            let mut log = self.sessions.lock().await;
            log.append_audit(&crate::session::SessionEvent::Goal { goal: goal.clone() })
                .await;
        }
        *self.goal.lock_or_recover() = Some(goal.clone());
        // live mirror of the durable Goal fact — a watching frontend renders
        // the same state a replay would fold (the Todos precedent)
        if let Some(sink) = self.live_sink.get() {
            sink.on_event(&crate::agent::LiveEvent::Goal { goal: goal.clone() });
        }
        Ok(goal)
    }
}
