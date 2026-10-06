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
                let mut forced = false;
                let fusion_problem = if self.turn_mode() == crate::agent::TurnMode::Fusion {
                    self.fusion_model_problem()
                } else {
                    None
                };
                let res = if let Some(problem) = fusion_problem {
                    Err(anyhow::anyhow!(problem))
                } else {
                    // MCP push traffic lands here — inside the fence, before the
                    // driver runs, so a catalog bump can never swap the registry
                    // between a turn's ToolCall and its ToolResult.
                    self.drain_mcp(observer).await;
                    // The driver answers a cancel at every await that armed a
                    // `CancelSignal`. What it cannot answer is an await with no
                    // arm at all (a hook process, the log's lock, a tool that
                    // ignores the signal) — this arm is the backstop. Past the
                    // grace the round is dropped where it stands, which is the
                    // only way out of a stuck await, and the drop is what kills
                    // the in-flight tool futures.
                    let cancel = self.ctx.cancel_signal();
                    tokio::select! {
                        r = async {
                            match self.ctx.loop_driver {
                                // PTC is the full loop with a RunCode+SearchTools
                                // advertised surface — `advertised_tools` does the
                                // trim; hooks, the gate and compaction all still run.
                                crate::agent::LoopDriver::Full | crate::agent::LoopDriver::Ptc => {
                                    self.run_turn_full(&prompt, &atts, observer).await
                                }
                                crate::agent::LoopDriver::Bare => {
                                    self.run_turn_bare(&prompt, &atts, observer).await
                                }
                            }
                        } => r,
                        () = async {
                            cancel.wait().await;
                            tokio::time::sleep(crate::agent::cancel::HARD_STOP_GRACE).await;
                        } => {
                            // A kill path that armed its waiter *after* the click
                            // missed the first wake (notify_waiters stores no
                            // permit) — this second one reaches it, so the shell
                            // still SIGKILLs its process tree before the round is
                            // abandoned. The flag is already set; re-waking is
                            // all a cooperative waiter needs.
                            self.ctx.cancel_notify.notify_waiters();
                            forced = true;
                            tracing::warn!(
                                "cancel grace ({}s) lapsed — force-ending the turn",
                                crate::agent::cancel::HARD_STOP_GRACE.as_secs()
                            );
                            observer.on_event(&LiveEvent::Hook {
                                event: "force_stop".into(),
                                detail: "cancel grace lapsed".into(),
                            });
                            Ok(TurnOutcome::Cancelled)
                        }
                    }
                };
                // cancelled resets at turn END on *every* exit path — an Err
                // or an early-returned outcome must not leak the flag into
                // the next turn (a stale flag would make the next turn
                // short-circuit forever).
                self.ctx
                    .cancelled
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                // fusion escalation is per-TURN parole: an escalated Lead
                // got its write tools back only to finish this turn — the
                // next turn under Fusion re-arms read_only and a fresh
                // delegation. A mode flip back to Standard already disarmed
                // the flag in set_turn_mode; this only touches Fusion stays.
                {
                    let mut f = self.ctx.fusion.lock_or_recover();
                    if f.escalated {
                        f.escalated = false;
                        f.verify_fails = 0;
                        let armed =
                            *self.ctx.turn_mode.read_or_recover() == crate::agent::TurnMode::Fusion;
                        self.ctx
                            .read_only
                            .store(armed, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                // A cancelled turn ends in partial messages only —
                // indistinguishable from a crash mid-stream on replay. Stamp
                // the terminal fact so the log can answer "this was stopped,
                // not broken." Skipped on the forced path: the abandoned
                // future may have been parked on the very lock this append
                // needs, and a hard stop must not hang on its own audit
                // write (the live `force_stop` event is its record).
                if matches!(res, Ok(TurnOutcome::Cancelled)) && !forced {
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
