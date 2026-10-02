//! Resume machinery — `Task{resume}` continues a finished sub-agent on its
//! own session log. The caller (`TaskTool::call`) resolved the roster def
//! name and the `model` override; here we locate the log, build a fresh
//! context over it (id kept, lane re-claimed), stamp the `resumed` audit
//! fact, and drive one turn — foreground or detached, same as a spawn.

use crate::context::MutexRecover;
use std::sync::Arc;

use sunmao_llm::ProviderAdapter;

use crate::context::Context;
use crate::session::{SessionEvent, SessionLog};
use crate::tool::ToolResult;

use super::parts::build_sub_ctx;
use super::spawn::{detach, finish_task, run_spawn};

/// Resume bookkeeping: a finished entry reopens (new lane + steer handle,
/// `done` reset — the agent def name survives so a later resume can
/// re-resolve it); a restart with an empty roster inserts a fresh row
/// tagged `…-r` to mark the -resume lineage.
fn roster_reopen(
    ctx: &Context,
    sub_id: &str,
    lane: u16,
    prompt: &str,
    agent: Option<&str>,
    steer: &crate::context::SteerQueue,
    cancel: &crate::context::SubCancel,
) {
    let mut digest: String = prompt.chars().take(60).collect();
    if prompt.chars().count() > 60 {
        digest.push('…');
    }
    let digest = digest.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut tasks = ctx.live_tasks.lock_or_recover();
    if let Some(e) = tasks.iter_mut().find(|t| t.id == sub_id) {
        e.lane = lane;
        e.done = None;
        e.steer = Some(steer.clone());
        if let Some(a) = agent {
            e.agent = Some(a.to_string());
        }
        e.prompt = digest;
        e.cancel = Some(cancel.clone());
    } else {
        tasks.push(crate::context::TaskEntry {
            id: sub_id.to_string(),
            lane,
            agent: Some(format!("{}-r", agent.unwrap_or(sub_id))),
            prompt: digest,
            done: None,
            steer: Some(steer.clone()),
            cancel: Some(cancel.clone()),
        });
    }
    drop(tasks);
    super::spawn::roster_changed(&ctx.live_sink.get().cloned(), sub_id);
}

/// Continue a finished sub-agent on its own log (`Task{resume}`): the
/// caller already resolved the def — this locates `<sub_id>.jsonl`,
/// refuses a still-running child, builds a fresh context onto the existing
/// log (transcript continuity, no second system prompt), stamps the
/// `resumed` marker, and drives one turn. `detached` honours the
/// `run_in_background` flag on the resume call.
pub(super) async fn resume_sub(
    ctx: &Context,
    sub_id: &str,
    prompt: &str,
    def: Option<&crate::agents::AgentDef>,
    llm_override: Option<Arc<dyn ProviderAdapter>>,
    detached: bool,
) -> anyhow::Result<ToolResult> {
    let path = crate::session::session_log_path(&ctx.cwd, sub_id);
    if !path.exists() {
        anyhow::bail!(
            "no such sub-agent log: {sub_id} (expected {})",
            path.display()
        );
    }
    {
        let tasks = ctx.live_tasks.lock_or_recover();
        if tasks.iter().any(|t| t.id == sub_id && t.done.is_none()) {
            anyhow::bail!("{sub_id} is still running — steer it, don't resume");
        }
    }
    let log = SessionLog::open_path(&path)
        .await
        .map_err(|e| anyhow::anyhow!("can't open {sub_id} log: {e:#}"))?;
    let events = log.events().await.unwrap_or_default();
    let sub_ctx = resume_parts(ctx, sub_id, def, llm_override, log).await;
    // the resumed log's facts reseed this continuation's state — checkpoint
    // ordinals and the task list follow the log, not a fresh session's
    sub_ctx.reseed_checkpoints(&events);
    sub_ctx.reseed_todos(&events);
    // audit seam: who picked up the baton at this breakpoint stays durable
    {
        let mut l = sub_ctx.sessions.lock().await;
        l.append_audit(&SessionEvent::Hook {
            event: "resumed".into(),
            detail: format!("{sub_id} continued: {prompt}"),
        })
        .await;
    }
    let agent_name = def.map(|d| d.name.as_str());
    let steer = sub_ctx.steer.clone();
    let cancel = crate::context::SubCancel::new(&sub_ctx);
    roster_reopen(
        ctx,
        sub_id,
        sub_ctx.lane,
        prompt,
        agent_name,
        &steer,
        &cancel,
    );
    if detached {
        detach(
            ctx,
            sub_id.to_string(),
            sub_ctx,
            prompt.to_string(),
            "subagent-resume",
        )
        .await;
        Ok(ToolResult {
            output: format!("resumed {sub_id} in the background"),
            ok: true,
        })
    } else {
        // same drop-guard as drive_foreground — a cancelled foreground
        // resume would otherwise leave the reopened row stuck at
        // done: None, refusing every later resume as "still running"
        let _roster = super::spawn::RosterGuard::new(&ctx.live_tasks, sub_id);
        let res = run_spawn(
            Arc::new(sub_ctx),
            prompt.to_string(),
            ctx.live_sink.get().cloned(),
            "subagent-resume",
        )
        .await;
        finish_task(&ctx.live_tasks, sub_id, res.ok);
        super::spawn::roster_changed(&ctx.live_sink.get().cloned(), sub_id);
        Ok(res)
    }
}

/// Continuation build for `Task{resume}`: the caller opened the existing
/// `<sub_id>.jsonl` — its transcript (system prompt and all) is the base,
/// so no second system message is written. The id is kept (the file stem
/// stays the identity); only the lane is freshly claimed — lane is event
/// attribution, not identity, and two runs of one sub_id each get their own.
async fn resume_parts(
    ctx: &Context,
    sub_id: &str,
    def: Option<&crate::agents::AgentDef>,
    llm_override: Option<Arc<dyn ProviderAdapter>>,
    log: SessionLog,
) -> Context {
    let lane = ctx
        .lane_counter
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    build_sub_ctx(ctx, sub_id.to_string(), lane, def, llm_override, log).await
}
