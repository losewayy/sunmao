//! Spawn machinery — driving a child's turn (foreground await and the
//! detached `run_in_background` completion path that pushes results into
//! the parent's session log) plus roster bookkeeping. Child-context
//! assembly (id+lane+log minting, seam wiring) lives in `parts.rs`; the
//! TaskTool surface and spawn policy stay in `mod.rs`.
// arch: allow-god-file the child driver, foreground cancellation guard and detached completion delivery share one lifecycle boundary

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
            // the child's own state events must never touch the parent's
            // UI: a lane's `compacted` would clear the whole transcript,
            // its Todos/Goal would clobber the parent's chips. They carry
            // no lane field on the wire, so there's nothing to forward.
            LiveEvent::Compacted { .. }
            | LiveEvent::Todos { .. }
            | LiveEvent::Goal { .. }
            | LiveEvent::TurnBoundary { .. } => {}
            // Reasoning forwards verbatim — no lane field exists on the
            // wire shape; parallel lanes' think text interleaves into one
            // fold (a wire limitation, not a choice).
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
/// into the child's queue (`Task{steer:}`, `steer_sub`). `background` marks
/// an already-detached spawn; `detach` is the latch the GUI's
/// send-to-background button trips while the parent still awaits the call.
#[allow(clippy::too_many_arguments)]
pub(crate) fn register_task(
    ctx: &Context,
    sub_id: &str,
    lane: u16,
    prompt: &str,
    def: Option<&str>,
    steer: &crate::context::SteerQueue,
    cancel: &crate::context::CancelSignal,
    background: bool,
    detach: Option<crate::tool::KillSwitch>,
) -> std::sync::Arc<tokio::sync::Notify> {
    let done_notify = std::sync::Arc::new(tokio::sync::Notify::new());
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
        agent: def.map(String::from),
        steer: Some(steer.clone()),
        cancel: Some(cancel.clone()),
        detach,
        background,
        done_notify: done_notify.clone(),
        ..crate::context::TaskEntry::pending(
            sub_id.to_string(),
            lane,
            digest.split_whitespace().collect::<Vec<_>>().join(" "),
        )
    });
    drop(tasks);
    roster_changed(&ctx.live_sink.get().cloned(), sub_id);
    done_notify
}

/// Flip the roster entry to finished — the detached completion path and
/// the foreground return both route here — and wake any `WaitFor` sleepers.
pub(crate) fn finish_task(
    tasks: &std::sync::Mutex<Vec<crate::context::TaskEntry>>,
    sub_id: &str,
    ok: bool,
) {
    let mut tasks = tasks.lock_or_recover();
    if let Some(e) = tasks.iter_mut().find(|t| t.id == sub_id) {
        e.done = Some(ok);
        e.done_notify.notify_waiters();
    }
}

/// Mark a roster entry as user-detached — the parent's `Task` call stopped
/// waiting on the GUI's button; the card keeps the "前台转后台" tag from
/// here on.
pub(crate) fn mark_detached(
    tasks: &std::sync::Mutex<Vec<crate::context::TaskEntry>>,
    sub_id: &str,
) {
    let mut tasks = tasks.lock_or_recover();
    if let Some(e) = tasks.iter_mut().find(|t| t.id == sub_id) {
        e.detached = true;
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

/// What `drive` hands back: the result channel for the waiter plus the
/// driver task's handle. The handle exists so `await_result` can abort the
/// driver the way a dropped foreground future used to — kill, settle the
/// roster row — instead of only signalling and racing the cleanup.
pub(super) struct Driven {
    pub rx: tokio::sync::oneshot::Receiver<ToolResult>,
    pub join: tokio::task::JoinHandle<()>,
}

/// Run a spawned child on its own tokio task, delivering the ToolResult
/// through a oneshot to whoever stays subscribed: the foreground `Task`
/// call first, nobody after a user detach (a dropped receiver pushes the
/// result as a `TaskDone` fact instead — the same delivery a
/// `run_in_background` spawn always had). `spawn_detached` drops its
/// receiver from the start, so both paths share this one driver.
#[allow(clippy::too_many_arguments)]
pub(super) async fn drive(
    parent_tasks: Arc<std::sync::Mutex<Vec<crate::context::TaskEntry>>>,
    parent_log: Arc<tokio::sync::Mutex<crate::session::SessionLog>>,
    parent_fence: Arc<tokio::sync::Mutex<()>>,
    sub_id: String,
    sub_ctx: Context,
    prompt: String,
    source: &'static str,
    sink: Option<Arc<dyn Observer>>,
) -> Driven {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let notify_sink = sink.clone();
    let id = sub_id;
    let join = tokio::spawn(async move {
        let _roster = RosterGuard::new(&parent_tasks, &id);
        let res = run_spawn(Arc::new(sub_ctx), prompt, sink, source).await;
        finish_task(&parent_tasks, &id, res.ok);
        // a waiter takes the result as the tool result; a dropped receiver
        // (user detach, `run_in_background` spawn) gets the pushed TaskDone.
        let res = match tx.send(res) {
            Ok(()) => {
                roster_changed(&notify_sink, &id);
                return;
            }
            Err(res) => res,
        };
        // Nobody waited: the caller detached (GUI button, `run_in_background`
        // spawn) or died waiting. Push the result into the session that
        // launched the child — the TaskDone fact lands on the forked log
        // under the parent's turn fence so it can't splice between a
        // ToolCall and its ToolResult.
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
        if let Some(s) = &notify_sink {
            // the durable TaskDone fact renders a 子代理 notice on replay —
            // the live mirror makes a watching session see the same row
            s.on_event(&LiveEvent::TaskDone {
                id: id.clone(),
                ok: res.ok,
                output: String::new(),
            });
            s.on_event(&LiveEvent::Hook {
                event: "task.bg.done".into(),
                detail: format!("{} — {}", id, if res.ok { "done" } else { "failed" }),
            });
        }
        roster_changed(&notify_sink, &id);
    });
    Driven { rx, join }
}

/// The parent log handle a detached completion writes into. Forked BEFORE
/// the drive task spawns: `ctx.sessions` is a shared Arc the parent's
/// swap_session can re-point mid-run — appending through it would land the
/// TaskDone on a session the child never ran in. A failed fork falls back
/// to the shared Arc — wrong-attribution beats silence.
pub(super) async fn parent_log_writer(
    ctx: &Context,
) -> Arc<tokio::sync::Mutex<crate::session::SessionLog>> {
    match ctx.sessions.lock().await.fork_writer().await {
        Ok(w) => Arc::new(tokio::sync::Mutex::new(w)),
        Err(e) => {
            tracing::warn!(
                "sub-agent log fork failed ({e}) — TaskDone attribution may follow a later swap"
            );
            ctx.sessions.clone()
        }
    }
}

/// The wait's cancellation contract, restored after the driver moved to its
/// own task: a caller dropped mid-wait (turn cancel, abort) used to drop
/// `run_spawn` with it, which died the child and settled the roster row
/// synchronously. Now the drop only orphans the receiver — the guard
/// aborts the driver task, which drops `run_spawn` exactly the way the old
/// direct-await did, and fires the child's cancel signal for the engines
/// (the child's own Bash runs) that watch it rather than the task.
struct CancelOnAbort {
    join: tokio::task::JoinHandle<()>,
    cancel: crate::context::CancelSignal,
    armed: bool,
}

impl Drop for CancelOnAbort {
    fn drop(&mut self) {
        if self.armed {
            self.cancel.cancel();
            self.join.abort();
        }
    }
}

/// Wait on a driven child: the receiver delivers its ToolResult, the
/// `detach` latch (GUI send-to-background) ends the wait WITHOUT ending the
/// child — the driver above notices the dropped receiver and pushes the
/// result as a TaskDone. A wait that dies unwatched (select arm dropped by
/// a cancel) kills the child through `CancelOnAbort`, so "cancel" keeps
/// meaning cancel.
pub(super) async fn await_result(
    ctx: &Context,
    sub_id: &str,
    driven: Driven,
    detach: &crate::tool::KillSwitch,
    child_cancel: crate::context::CancelSignal,
) -> ToolResult {
    let mut on_abort = CancelOnAbort {
        join: driven.join,
        cancel: child_cancel,
        armed: true,
    };
    let rx = driven.rx;
    tokio::pin!(rx);
    let out = tokio::select! {
        res = &mut rx => {
            let mut res = res.unwrap_or_else(|_| ToolResult {
                exit_code: None,
                output: "[sub-agent ended without reporting]".into(),
                ok: false,
            });
            // the caller gets the child's stable handle — foreground `Task`
            // used to return bare final text, leaving `steer`/`resume` with
            // nothing to target; prefix the id so the next call has a name
            // to point at
            res.output = format!("[task:{sub_id}]\n\n{}", res.output);
            res
        }
        () = detach.wait() => {
            mark_detached(&ctx.live_tasks, sub_id);
            roster_changed(&ctx.live_sink.get().cloned(), sub_id);
            ToolResult {
                exit_code: None,
                output: format!(
                    "sub-agent {sub_id} moved to the background at the user's request — still running.\n\
                     its result will arrive as a <task-result> message when it finishes; do not wait or poll.\n\
                     transcript: .sunmao/sessions/{sub_id}.jsonl"
                ),
                ok: true,
            }
        }
    };
    // either normal exit is a clean hand-over: the result arrived, or the
    // detach sent the child to the background path — disarm the abort kill
    on_abort.armed = false;
    out
}

/// Run one sub-agent to completion in the foreground: registers (armed with
/// the GUI's detach latch), drives the child on its own task, and waits on
/// the reported result — or on the user sending it to the background.
pub(super) async fn spawn_one(
    ctx: &Context,
    prompt: &str,
    def: Option<&crate::agents::AgentDef>,
    llm_override: Option<Arc<dyn ProviderAdapter>>,
) -> ToolResult {
    let (sub_id, sub_ctx) = spawn_parts(ctx, def, llm_override, None).await;
    let detach = crate::tool::KillSwitch::new();
    let cancel = sub_ctx.cancel_signal();
    register_task(
        ctx,
        &sub_id,
        sub_ctx.lane,
        prompt,
        def.map(|d| d.name.as_str()),
        &sub_ctx.steer,
        &cancel,
        false,
        Some(detach.clone()),
    );
    let driven = drive(
        ctx.live_tasks.clone(),
        parent_log_writer(ctx).await,
        ctx.turn_lock.clone(),
        sub_id.clone(),
        sub_ctx,
        prompt.to_string(),
        "subagent",
        ctx.live_sink.get().cloned(),
    )
    .await;
    await_result(ctx, &sub_id, driven, &detach, cancel).await
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
    let cancel = sub_ctx.cancel_signal();
    register_task(
        ctx,
        &sub_id,
        sub_ctx.lane,
        prompt,
        def.map(|d| d.name.as_str()),
        &sub_ctx.steer,
        &cancel,
        true,
        None,
    );
    detach(ctx, sub_id.clone(), sub_ctx, prompt.to_string(), "subagent").await;
    sub_id
}

/// The detached driver task shared by fresh spawns and resumes: same
/// `drive` as the foreground path with the receiver dropped up front — a
/// child nobody waits on delivers itself as a TaskDone fact.
pub(super) async fn detach(
    ctx: &Context,
    sub_id: String,
    sub_ctx: Context,
    prompt: String,
    source: &'static str,
) {
    let _driven = drive(
        ctx.live_tasks.clone(),
        parent_log_writer(ctx).await,
        ctx.turn_lock.clone(),
        sub_id,
        sub_ctx,
        prompt,
        source,
        ctx.live_sink.get().cloned(),
    )
    .await;
    // `_driven` drops here — the receiver's fall is what tells the driver
    // nobody is waiting (a JoinHandle drop never aborts the task itself)
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
    // capture hook can tell it apart from startup/resume. Their
    // additionalContext is real context: prefix it onto the child's first
    // user message under the same `[hook context]` convention a main
    // session's UserPromptSubmit injection uses.
    let mut prompt = prompt;
    let task_json = json!({"prompt": prompt});
    for (event, input) in [
        (
            crate::hooks::HookEvent::SessionStart,
            crate::hooks::HookInput {
                source: Some(source),
                mcp_servers: Some(sub_ctx.mcp_servers.iter().map(|s| s.name.clone()).collect()),
                ..Default::default()
            },
        ),
        (
            crate::hooks::HookEvent::SubagentStart,
            crate::hooks::HookInput {
                source: Some(source),
                agent_name: sub_ctx.agent_name.as_deref(),
                tool_input: Some(&task_json),
                ..Default::default()
            },
        ),
    ] {
        let out = sub_ctx.hooks.fire(event, &sub_ctx.cwd, &input).await;
        for ctx_text in out.extra_context {
            prompt = format!("[hook context] {ctx_text}\n\n{prompt}");
        }
    }
    let agent = AgentLoop::new(sub_ctx.clone()).with_max_iterations(24);
    let obs = std::sync::Arc::new(RelayObserver {
        text: std::sync::Mutex::new(String::new()),
        sink,
        lane,
    });
    // the child's hook audit lane (untrusted skips, exec failures) relays
    // to the parent's UI with the lane prefix — otherwise a skipped child
    // hook is invisible everywhere but its own log
    sub_ctx.hooks.set_live(obs.clone());
    let outcome = agent.run_turn(&prompt, &*obs).await;
    // advisory events — a wedged capture hook must not hold the child's
    // result (or the parent's turn) hostage
    crate::hooks::HookEngine::fire_detached(
        &sub_ctx.hooks,
        crate::hooks::HookEvent::SubagentStop,
        &sub_ctx.cwd,
        &crate::hooks::HookInput {
            agent_name: sub_ctx.agent_name.as_deref(),
            ..Default::default()
        },
    );
    crate::hooks::HookEngine::fire_detached(
        &sub_ctx.hooks,
        crate::hooks::HookEvent::SessionEnd,
        &sub_ctx.cwd,
        &crate::hooks::HookInput::default(),
    );
    // child's extension children die with its session — graceful path
    // before the context drop falls back to the detached reaper.
    sub_ctx.ext.shutdown().await;
    // steers pushed after the last drain (or raced against the child's
    // death — `steer_sub` can win the roster check and still orphan) are
    // undelivered by definition. Drop them with an audit row instead of
    // letting the parent's `task.steer` fact stand alone — the trail
    // then says "pushed AND lost", not just "pushed".
    {
        let orphans: Vec<String> = sub_ctx
            .steer
            .lock_or_recover()
            .drain(..)
            .map(|(_, t)| t)
            .collect();
        if !orphans.is_empty() {
            sub_ctx
                .sessions
                .lock()
                .await
                .append_audit(&SessionEvent::Hook {
                    event: "task.steer.dropped".into(),
                    detail: format!("{} undelivered: {}", orphans.len(), orphans.join(" | ")),
                })
                .await;
        }
    }
    let text = obs.text.lock_or_recover().clone();
    match outcome {
        // a killed child is a clean exit but NOT a success — the roster
        // must not mark it `done`. `Cancelled` is its own variant precisely
        // so this arm can discriminate without string-matching.
        Ok(crate::agent::TurnOutcome::Cancelled) => {
            // the child's detached spawns (grandchildren) register on ITS
            // roster — a parent cancel reaches them only through here, or
            // an orphan keeps writing into a dead session's log
            for entry in sub_ctx.live_tasks.lock_or_recover().iter() {
                if let Some(c) = &entry.cancel {
                    c.cancel();
                }
            }
            ToolResult {
                exit_code: None,
                output: "[sub-agent cancelled]".into(),
                ok: false,
            }
        }
        Ok(_) => ToolResult {
            exit_code: None,
            output: if text.is_empty() {
                "[sub-agent finished with no text output]".into()
            } else {
                text
            },
            ok: true,
        },
        Err(e) => ToolResult {
            exit_code: None,
            output: format!("sub-agent failed: {e:#}"),
            ok: false,
        },
    }
}
