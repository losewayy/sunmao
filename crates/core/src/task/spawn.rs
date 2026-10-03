//! Spawn machinery — driving a child's turn (foreground await and the
//! detached `run_in_background` completion path that pushes results into
//! the parent's session log) plus roster bookkeeping. Child-context
//! assembly (id+lane+log minting, seam wiring) lives in `parts.rs`; the
//! TaskTool surface and spawn policy stay in `mod.rs`.

use crate::context::MutexRecover;
use std::sync::Arc;

use serde_json::json;
use sunmao_llm::ProviderAdapter;

use super::parts::spawn_parts;
use crate::agent::{AgentLoop, LiveEvent, Observer};
use crate::context::Context;
use crate::session::SessionEvent;
use crate::tool::ToolResult;

/// Collects the sub-agent's final assistant text for the tool result AND
/// relays its tool lifecycle to the session's `live_sink` — the frontend
/// sees `Task` blocks working through real calls instead of a frozen row.
/// Relayed events are re-stamped with the child's `lane` so parallel
/// siblings don't collide on (name, depth). TurnEnd is swallowed: a
/// sub-agent's end must not unwind the outer turn.
pub(super) struct RelayObserver {
    pub(super) text: std::sync::Mutex<String>,
    pub(super) sink: Option<Arc<dyn Observer>>,
    pub(super) lane: u16,
}

impl Observer for RelayObserver {
    fn on_event(&self, ev: &LiveEvent) {
        match ev {
            LiveEvent::Content { text } => self.text.lock_or_recover().push_str(text),
            // sub-agent lifecycle is the parent's business, not the UI's —
            // forwarding TurnEnd would close the outer transcript early.
            LiveEvent::TurnEnd { .. } => {}
            LiveEvent::ToolStart {
                name,
                summary,
                depth,
                call_id,
                args,
                ..
            } => {
                if let Some(s) = &self.sink {
                    s.on_event(&LiveEvent::ToolStart {
                        name: name.clone(),
                        summary: summary.clone(),
                        depth: *depth,
                        lane: self.lane,
                        call_id: call_id.clone(),
                        args: args.clone(),
                    });
                }
            }
            LiveEvent::ToolDone {
                name,
                ok,
                output,
                depth,
                call_id,
                elapsed_ms,
                ..
            } => {
                if let Some(s) = &self.sink {
                    s.on_event(&LiveEvent::ToolDone {
                        name: name.clone(),
                        ok: *ok,
                        output: output.clone(),
                        depth: *depth,
                        lane: self.lane,
                        call_id: call_id.clone(),
                        elapsed_ms: *elapsed_ms,
                    });
                }
            }
            // Non-tool events forward verbatim — except `Hook`, whose detail
            // gets a lane prefix so the frontend can attribute the child's
            // audit line (the wire shape is locked; no lane field exists).
            // Content/Reasoning stay unprefixed: they're per-delta streams —
            // a tag inside them would corrupt the rendered text.
            LiveEvent::Hook { event, detail } => {
                if let Some(s) = &self.sink {
                    s.on_event(&LiveEvent::Hook {
                        event: event.clone(),
                        detail: format!("[l{}] {detail}", self.lane),
                    });
                }
            }
            _ => {
                if let Some(s) = &self.sink {
                    s.on_event(ev);
                }
            }
        }
    }
}

/// Tell the session's live sink the roster moved — frontends re-pull
/// `/tasks` on this hook (the roster is pull-state; this is its nudge).
pub(crate) fn roster_changed(sink: &Option<Arc<dyn Observer>>, sub_id: &str) {
    if let Some(s) = sink {
        s.on_event(&LiveEvent::Hook {
            event: "tasks.changed".into(),
            detail: sub_id.to_string(),
        });
    }
}

/// Register a spawn in the roster — both foreground and detached spawns
/// register; `done` flips when the result lands. `sub_ctx.lane` is the
/// claimed lane; the `steer` handle lets the parent push mid-run messages
/// into the child's queue (`Task{steer:}`, `steer_sub`).
pub(crate) fn register_task(
    ctx: &Context,
    sub_id: &str,
    lane: u16,
    prompt: &str,
    def: Option<&str>,
    steer: &crate::context::SteerQueue,
    cancel: &crate::context::SubCancel,
) {
    let mut digest: String = prompt.chars().take(60).collect();
    if prompt.chars().count() > 60 {
        digest.push('…');
    }
    let mut tasks = ctx.live_tasks.lock_or_recover();
    // the roster is never a history — drop settled entries past a small
    // tail so `/tasks` stays a live view and the Vec can't grow unboundedly
    // across a long session (running rows are never pruned).
    const KEEP_SETTLED: usize = 20;
    let settled = tasks.iter().filter(|t| t.done.is_some()).count();
    if settled >= KEEP_SETTLED {
        let mut drop_n = settled - KEEP_SETTLED + 1;
        tasks.retain(|t| {
            if t.done.is_some() && drop_n > 0 {
                drop_n -= 1;
                false
            } else {
                true
            }
        });
    }
    tasks.push(crate::context::TaskEntry {
        id: sub_id.to_string(),
        lane,
        agent: def.map(String::from),
        prompt: digest.split_whitespace().collect::<Vec<_>>().join(" "),
        done: None,
        steer: Some(steer.clone()),
        cancel: Some(cancel.clone()),
    });
    drop(tasks);
    roster_changed(&ctx.live_sink.get().cloned(), sub_id);
}

/// Flip the roster entry to finished — the detached completion path and
/// the foreground return both route here.
pub(crate) fn finish_task(
    tasks: &std::sync::Mutex<Vec<crate::context::TaskEntry>>,
    sub_id: &str,
    ok: bool,
) {
    let mut tasks = tasks.lock_or_recover();
    if let Some(e) = tasks.iter_mut().find(|t| t.id == sub_id) {
        e.done = Some(ok);
    }
}

/// Drop-guard on a roster entry: if the future driving the child is
/// cancelled (the turn's tool-call select aborts a foreground Task mid-run,
/// or the detached task dies), the entry would stay `done: None` forever —
/// /tasks lists a ghost "running" row and `Task{resume}` refuses it as
/// still-running. The guard fills `done` only when it's still unset — the
/// happy path's `finish_task` writes the real `ok` first and Drop no-ops.
pub(crate) struct RosterGuard {
    tasks: Arc<std::sync::Mutex<Vec<crate::context::TaskEntry>>>,
    id: String,
}

impl RosterGuard {
    pub(crate) fn new(
        tasks: &Arc<std::sync::Mutex<Vec<crate::context::TaskEntry>>>,
        id: &str,
    ) -> Self {
        Self {
            tasks: tasks.clone(),
            id: id.to_string(),
        }
    }
}

impl Drop for RosterGuard {
    fn drop(&mut self) {
        let mut tasks = self.tasks.lock_or_recover();
        if let Some(e) = tasks
            .iter_mut()
            .find(|t| t.id == self.id && t.done.is_none())
        {
            e.done = Some(false);
        }
    }
}

/// Run one sub-agent to completion in the foreground: registers, drives
/// `run_spawn` (`source` names the hook dialect — `"subagent"` for a fresh
/// spawn, `"subagent-resume"` for a continuation), then flips the roster.
async fn drive_foreground(
    ctx: &Context,
    sub_id: String,
    sub_ctx: Context,
    prompt: String,
    source: &'static str,
    agent: Option<&str>,
) -> ToolResult {
    let lane = sub_ctx.lane;
    let steer = sub_ctx.steer.clone();
    let cancel = crate::context::SubCancel::new(&sub_ctx);
    register_task(ctx, &sub_id, lane, &prompt, agent, &steer, &cancel);
    // a cancelled select arm drops this future mid-run_spawn — without the
    // guard the roster row stays "running" forever (and resume refuses it)
    let _roster = RosterGuard::new(&ctx.live_tasks, &sub_id);
    let mut res = run_spawn(
        Arc::new(sub_ctx),
        prompt,
        ctx.live_sink.get().cloned(),
        source,
    )
    .await;
    // the caller gets the child's stable handle — foreground `Task` used
    // to return bare final text, leaving `steer`/`resume` with nothing to
    // target; prefix the id so the next call has a name to point at
    res.output = format!("[task:{sub_id}]\n\n{}", res.output);
    finish_task(&ctx.live_tasks, &sub_id, res.ok);
    roster_changed(&ctx.live_sink.get().cloned(), &sub_id);
    res
}

/// Run one sub-agent to completion: own context, own session file, own
/// lane; relays its tool lifecycle to the parent's live sink.
/// `llm_override` is the resolved call-site `model` selector — wins over
/// the def's `model:` frontmatter and over inheriting the parent.
pub(super) async fn spawn_one(
    ctx: &Context,
    prompt: &str,
    def: Option<&crate::agents::AgentDef>,
    llm_override: Option<Arc<dyn ProviderAdapter>>,
) -> ToolResult {
    let (sub_id, sub_ctx) = spawn_parts(ctx, def, llm_override, None).await;
    drive_foreground(
        ctx,
        sub_id,
        sub_ctx,
        prompt.to_string(),
        "subagent",
        def.map(|d| d.name.as_str()),
    )
    .await
}

/// Detached spawn (`run_in_background: true`): the tool returns an id at
/// once; the child runs on its own task and, when it finishes, appends a
/// `TaskDone` event straight into the *parent's* session log — push-style
/// delivery, no polling. The result lands in the session that launched it,
/// even across a /resume.
pub(super) async fn spawn_detached(
    ctx: &Context,
    prompt: &str,
    def: Option<&crate::agents::AgentDef>,
    llm_override: Option<Arc<dyn ProviderAdapter>>,
) -> String {
    let (sub_id, sub_ctx) = spawn_parts(ctx, def, llm_override, None).await;
    // roster entry — `/tasks` reads this; the completion path flips `done`
    let cancel = crate::context::SubCancel::new(&sub_ctx);
    register_task(
        ctx,
        &sub_id,
        sub_ctx.lane,
        prompt,
        def.map(|d| d.name.as_str()),
        &sub_ctx.steer,
        &cancel,
    );
    detach(ctx, sub_id.clone(), sub_ctx, prompt.to_string(), "subagent").await;
    sub_id
}

/// The detached driver task shared by fresh spawns and resumes: run the
/// turn on its own tokio task, append `TaskDone` into the parent log under
/// the parent's turn fence, flip the roster, notify the live sink.
pub(super) async fn detach(
    ctx: &Context,
    sub_id: String,
    sub_ctx: Context,
    prompt: String,
    source: &'static str,
) {
    // Fork the writer BEFORE spawning: `ctx.sessions` is a shared Arc the
    // parent's swap_session can re-point mid-run — appending through it
    // would land this TaskDone on a session the child never ran in.
    // The forked handle pins the append to the spawning log (fresh file
    // handle on the same path / shared buffer for ephemeral); a failed
    // fork falls back to the shared Arc — wrong-attribution beats silence.
    let parent_log: Arc<tokio::sync::Mutex<crate::session::SessionLog>> =
        match ctx.sessions.lock().await.fork_writer().await {
            Ok(w) => Arc::new(tokio::sync::Mutex::new(w)),
            Err(e) => {
                tracing::warn!(
                    "sub-agent log fork failed ({e}) — TaskDone attribution may follow a later swap"
                );
                ctx.sessions.clone()
            }
        };
    let parent_tasks = ctx.live_tasks.clone();
    // The parent's turn fence: a mid-turn completion must not append the
    // TaskDone user-message between a ToolCall and its ToolResult — that
    // would break provider pairing (tool_result must immediately follow
    // its tool_use) and hard-400 the next request. Waiting for the turn
    // boundary keeps the pushed fact well-formed.
    let parent_fence = ctx.turn_lock.clone();
    let sink = ctx.live_sink.get().cloned();
    let notify_sink = sink.clone();
    let id = sub_id;
    tokio::spawn(async move {
        let _roster = RosterGuard::new(&parent_tasks, &id);
        let res = run_spawn(Arc::new(sub_ctx), prompt, sink, source).await;
        // The child's own log already holds the full transcript — the
        // parent record stays lean (capped), with `id` pointing there.
        let output = crate::agent::truncate_output(&res.output);
        {
            let _turn_permit = parent_fence.lock().await;
            let mut log = parent_log.lock().await;
            log.append_audit(&SessionEvent::TaskDone {
                id: id.clone(),
                ok: res.ok,
                output,
            })
            .await;
        }
        {
            finish_task(&parent_tasks, &id, res.ok);
        }
        if let Some(s) = notify_sink {
            // the durable TaskDone fact renders a 子代理 notice on replay —
            // the live mirror makes a watching session see the same row
            s.on_event(&LiveEvent::TaskDone {
                id: id.clone(),
                ok: res.ok,
                output: String::new(),
            });
            roster_changed(&Some(s.clone()), &id);
            s.on_event(&LiveEvent::Hook {
                event: "task.bg.done".into(),
                detail: format!("{} — {}", id, if res.ok { "done" } else { "failed" }),
            });
        }
    });
}

/// Drive a built child context through one turn — SubagentStart/Stop hooks
/// wrap the run and the final assistant text becomes the ToolResult.
/// `source` stamps the lifecycle-hook dialect: `"subagent"` for a fresh
/// spawn, `"subagent-resume"` for a continuation (a capture hook can tell
/// which path fired it).
pub(crate) async fn run_spawn(
    sub_ctx: Arc<Context>,
    prompt: String,
    sink: Option<Arc<dyn Observer>>,
    source: &'static str,
) -> ToolResult {
    let lane = sub_ctx.lane;
    // the child runs a real session (own JSONL) — lifecycle hooks fire the
    // same way the main session's do, source names the spawn path so a
    // capture hook can tell it apart from startup/resume
    let _ = sub_ctx
        .hooks
        .fire(
            crate::hooks::HookEvent::SessionStart,
            &sub_ctx.cwd,
            &crate::hooks::HookInput {
                source: Some(source),
                ..Default::default()
            },
        )
        .await;
    let _ = sub_ctx
        .hooks
        .fire(
            crate::hooks::HookEvent::SubagentStart,
            &sub_ctx.cwd,
            &crate::hooks::HookInput {
                source: Some(source),
                tool_input: Some(&json!({"prompt": prompt})),
                ..Default::default()
            },
        )
        .await;
    let agent = AgentLoop::new(sub_ctx.clone()).with_max_iterations(24);
    let obs = RelayObserver {
        text: std::sync::Mutex::new(String::new()),
        sink,
        lane,
    };
    let outcome = agent.run_turn(&prompt, &obs).await;
    let _ = sub_ctx
        .hooks
        .fire(
            crate::hooks::HookEvent::SubagentStop,
            &sub_ctx.cwd,
            &crate::hooks::HookInput::default(),
        )
        .await;
    let _ = sub_ctx
        .hooks
        .fire(
            crate::hooks::HookEvent::SessionEnd,
            &sub_ctx.cwd,
            &crate::hooks::HookInput::default(),
        )
        .await;
    // child's extension children die with its session — graceful path
    // before the context drop falls back to the detached reaper.
    sub_ctx.ext.shutdown().await;
    let text = obs.text.lock_or_recover().clone();
    match outcome {
        // a killed child is a clean exit but NOT a success — the roster
        // must not mark it `done`. `Cancelled` is its own variant precisely
        // so this arm can discriminate without string-matching.
        Ok(crate::agent::TurnOutcome::Cancelled) => ToolResult {
            output: "[sub-agent cancelled]".into(),
            ok: false,
        },
        Ok(_) => ToolResult {
            output: if text.is_empty() {
                "[sub-agent finished with no text output]".into()
            } else {
                text
            },
            ok: true,
        },
        Err(e) => ToolResult {
            output: format!("sub-agent failed: {e:#}"),
            ok: false,
        },
    }
}
