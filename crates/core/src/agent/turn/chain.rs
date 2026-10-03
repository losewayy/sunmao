//! The chain driver — `run_turn`/`run_turn_blocks` own the outer loop
//! that re-enters the per-round driver. While the session goal is
//! in_progress, a Completed round's tail asks `agent/goal.rs::goal_next`
//! for the next continuation prompt; the turn lock is held per round so a
//! queued `run_turn` interleaves between rounds instead of waiting out
//! the whole chain. TurnEnd fires exactly once per call — frontends
//! unwind "busy" at the end of the chain.

use super::*;

impl AgentLoop {
    /// Run one turn: `input` is the user's message; returns when the model
    /// stops calling tools or we hit the iteration ceiling.
    ///
    /// Turns serialize on `ctx.turn_lock` — a second concurrent run_turn
    /// queues instead of interleaving facts into the session log. That's
    /// the replay fence: one turn's ToolCall/ToolResult events can never
    /// straddle a predecessor's, so the fold the next request sees is
    /// always a well-formed transcript.
    ///
    /// Frontends depend on TurnEnd to unwind their "working" state — this
    /// wrapper emits it on every exit (success, veto, cancel, Err), so an
    /// early return inside a driver can never strand a frontend. The
    /// `cancelled` flag resets here too: a stale flag must not survive
    /// into the next turn regardless of how this one ended.
    ///
    /// `ctx.loop_driver` picks the driver (SPEC §4.5): `Full` runs the
    /// contract loop below; `Bare` runs `run_turn_bare` — same session log
    /// and observer, no hooks/gate/compaction.
    pub async fn run_turn(
        &self,
        input: &str,
        observer: &dyn Observer,
    ) -> anyhow::Result<TurnOutcome> {
        self.run_turn_blocks(input, &[], observer).await
    }

    /// A turn whose user message carries attachment blocks (images) after
    /// the prompt text. Empty `attachments` behaves exactly like
    /// `run_turn` — `Message::user_blocks` degenerates to one text block.
    ///
    /// Goal chaining: while the session goal is `in_progress`, every
    /// Completed round's tail re-enters the driver with a continuation
    /// prompt (`agent/goal.rs::goal_next` decides — budget, terminal
    /// status, queued input, and non-clean outcomes all end the chain).
    /// The turn lock is held per round, not per chain, so a queued
    /// `run_turn` interleaves between rounds instead of waiting out the
    /// whole loop. TurnEnd still fires exactly once per `run_turn_blocks`
    /// call — frontends unwind "busy" at the end of the chain.
    pub async fn run_turn_blocks(
        &self,
        input: &str,
        attachments: &[sunmao_llm::Content],
        observer: &dyn Observer,
    ) -> anyhow::Result<TurnOutcome> {
        let mut prompt = input.to_string();
        let mut atts = attachments.to_vec();
        let res = loop {
            let res = {
                let _turn_permit = self.ctx.turn_lock.lock().await;
                // MCP push traffic lands here — inside the fence, before the
                // driver runs, so a catalog bump can never swap the registry
                // between a turn's ToolCall and its ToolResult.
                self.drain_mcp(observer).await;
                let res = match self.ctx.loop_driver {
                    // PTC is the full loop with a RunCode+SearchTools
                    // advertised surface — `advertised_tools` does the trim;
                    // hooks, the gate and compaction all still run.
                    crate::agent::LoopDriver::Full | crate::agent::LoopDriver::Ptc => {
                        self.run_turn_full(&prompt, &atts, observer).await
                    }
                    crate::agent::LoopDriver::Bare => {
                        self.run_turn_bare(&prompt, &atts, observer).await
                    }
                };
                // cancelled resets at turn END on *every* exit path — an Err
                // or an early-returned outcome must not leak the flag into
                // the next turn (a stale flag would make the next turn
                // short-circuit forever).
                self.ctx
                    .cancelled
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                // A cancelled turn ends in partial messages only —
                // indistinguishable from a crash mid-stream on replay. Stamp
                // the terminal fact so the log can answer "this was stopped,
                // not broken."
                if matches!(res, Ok(TurnOutcome::Cancelled)) {
                    let mut log = self.ctx.sessions.lock().await;
                    log.append_audit(&SessionEvent::Hook {
                        event: "cancelled".into(),
                        detail: String::new(),
                    })
                    .await;
                }
                // the goal-loop decision rides the same permit — the round
                // bump and the turn's tail are one atomic unit, and a
                // concurrent turn can only interleave BETWEEN rounds
                let next = match &res {
                    Ok(o) => match self.goal_next(o, observer).await {
                        Ok(next) => next,
                        Err(e) => break Err(e),
                    },
                    Err(_) => None,
                };
                match next {
                    Some(continuation) => {
                        // live mirror of the durable user message the next
                        // round's head will append — the GUI paints the same
                        // bubble a replay would fold
                        observer.on_event(&LiveEvent::UserMessage {
                            content: vec![sunmao_llm::Content::Text {
                                text: continuation.clone(),
                            }],
                        });
                        prompt = continuation;
                        atts.clear();
                        None
                    }
                    None => Some(res),
                }
            };
            match res {
                Some(res) => break res,
                None => continue,
            }
        };
        // TurnEnd is the frontend's "unwind working state" signal — emit it
        // here so every inner exit (early returns included) produces exactly
        // one, with Stop/StopFailure already fired inside the driver.
        match &res {
            Ok(o) => observer.on_event(&LiveEvent::TurnEnd { outcome: o.clone() }),
            Err(e) => observer.on_event(&LiveEvent::TurnEnd {
                outcome: TurnOutcome::Other(format!("error: {e:#}")),
            }),
        }
        res
    }
}
