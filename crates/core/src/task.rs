//! Task tool — spawn nested agent loops for self-contained subtasks.
//!
//! Each sub-agent gets its own Context (fresh session file, own read
//! ledger, hooks fire in its scope) but shares the provider wiring —
//! unless its `agents/*.md` def pins a `model:` selector, in which case
//! the session's ModelResolver swaps the adapter. Depth-capped so agents
//! can't recurse into themselves; every spawn claims a `lane` so parallel
//! children never alias each other's tool events.
//!
//! Two call shapes: flat `{prompt, subagent_type?}` for one task, or
//! `{context, tasks[]}` to fan out a batch — items run concurrently and
//! results merge in order. `run_in_background: true` detaches instead: the
//! call returns task ids immediately and each finished child pushes its
//! result into the parent session as a `TaskDone` fact — no polling.

use std::sync::Arc;

use anyhow::bail;
use serde::Deserialize;
use serde_json::{json, Value};
use sunmao_llm::types::Tool;

use crate::agent::{AgentLoop, LiveEvent, Observer};
use crate::context::Context;
use crate::session::{SessionEvent, SessionLog};
use crate::tool::{builtin_registry, ToolImpl, ToolResult};

const MAX_DEPTH: u8 = 2;

pub struct TaskTool;

/// Collects the sub-agent's final assistant text for the tool result AND
/// relays its tool lifecycle to the session's `live_sink` — the frontend
/// sees `Task` blocks working through real calls instead of a frozen row.
/// Relayed events are re-stamped with the child's `lane` so parallel
/// siblings don't collide on (name, depth). TurnEnd is swallowed: a
/// sub-agent's end must not unwind the outer turn.
struct RelayObserver {
    text: std::sync::Mutex<String>,
    sink: Option<Arc<dyn Observer>>,
    lane: u8,
}

impl Observer for RelayObserver {
    fn on_event(&self, ev: &LiveEvent) {
        match ev {
            LiveEvent::Content(c) => self.text.lock().unwrap().push_str(c),
            // sub-agent lifecycle is the parent's business, not the UI's —
            // forwarding TurnEnd would close the outer transcript early.
            LiveEvent::TurnEnd { .. } => {}
            LiveEvent::ToolStart {
                name,
                summary,
                depth,
                ..
            } => {
                if let Some(s) = &self.sink {
                    s.on_event(&LiveEvent::ToolStart {
                        name: name.clone(),
                        summary: summary.clone(),
                        depth: *depth,
                        lane: self.lane,
                    });
                }
            }
            LiveEvent::ToolDone {
                name,
                ok,
                output,
                depth,
                ..
            } => {
                if let Some(s) = &self.sink {
                    s.on_event(&LiveEvent::ToolDone {
                        name: name.clone(),
                        ok: *ok,
                        output: output.clone(),
                        depth: *depth,
                        lane: self.lane,
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

/// One task in a batch spawn (`tasks[]` items).
#[derive(Deserialize)]
struct TaskItem {
    prompt: String,
    subagent_type: Option<String>,
}

#[async_trait::async_trait]
impl ToolImpl for TaskTool {
    fn name(&self) -> &'static str {
        "Task"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "Task",
            "Delegate self-contained subtask(s) to sub-agents with the same toolset. \
             Returns the sub-agent's final message(s). Flat form: one `prompt`. \
             Batch form: `context` (shared background) + `tasks[]` — items run \
             concurrently. `run_in_background: true` detaches each spawn: returns \
             task ids now, and each finished sub-agent pushes its result into this \
             session as a tagged message. Use for parallelizable or scope-isolated \
             work; sub-agents cannot spawn further sub-agents beyond the depth cap.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Complete instructions for the subtask (flat form)"},
                    "subagent_type": {"type": "string", "description": "Named agent def from .sunmao/agents/*.md or .claude/agents/*.md"},
                    "context": {"type": "string", "description": "Shared background prepended to every task (batch form)"},
                    "tasks": {
                        "type": "array",
                        "description": "Batch form: run all items concurrently",
                        "items": {
                            "type": "object",
                            "properties": {
                                "prompt": {"type": "string"},
                                "subagent_type": {"type": "string"}
                            },
                            "required": ["prompt"]
                        }
                    },
                    "run_in_background": {"type": "boolean", "description": "Detach: returns task ids; results arrive as tagged session messages when each sub-agent finishes"}
                }
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &Context) -> anyhow::Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            prompt: Option<String>,
            subagent_type: Option<String>,
            context: Option<String>,
            tasks: Option<Vec<TaskItem>>,
            run_in_background: Option<bool>,
        }
        let a: Args = serde_json::from_value(args)?;

        if ctx.depth >= MAX_DEPTH {
            bail!("sub-agent depth limit reached ({MAX_DEPTH})");
        }

        // normalize both shapes into (prompt, subagent_type) items
        let items: Vec<TaskItem> = match (a.prompt, a.tasks) {
            (Some(p), None) => vec![TaskItem {
                prompt: p,
                subagent_type: a.subagent_type,
            }],
            (None, Some(tasks)) if !tasks.is_empty() => {
                let ctx_preamble = a.context.unwrap_or_default();
                tasks
                    .into_iter()
                    .map(|t| TaskItem {
                        prompt: if ctx_preamble.is_empty() {
                            t.prompt
                        } else {
                            format!("{ctx_preamble}\n\n# Task\n{}", t.prompt)
                        },
                        subagent_type: t.subagent_type,
                    })
                    .collect()
            }
            (None, None) => bail!("Task needs `prompt` or a non-empty `tasks[]`"),
            _ => bail!("Task takes `prompt` (flat) or `tasks[]` (batch), not both"),
        };

        // spawn policy: resolve each requested type against the defs *and*
        // the parent agent's `spawns:` whitelist (ctx.agent_name names the
        // def that owns this context; the interactive agent is unrestricted).
        let defs: Vec<Option<crate::agents::AgentDef>> = items
            .iter()
            .map(|it| resolve_spawn_def(ctx, it.subagent_type.as_deref()))
            .collect::<Result<_, _>>()?;

        // detached lane: ids now, results pushed into the session log later
        if a.run_in_background.unwrap_or(false) {
            let mut ids = Vec::with_capacity(items.len());
            for (it, def) in items.iter().zip(&defs) {
                ids.push(spawn_detached(ctx, &it.prompt, def.as_ref()).await);
            }
            return Ok(ToolResult {
                output: format!(
                    "{} background sub-agent(s) launched: {}\nresults arrive as <task-result> messages in this session; \
                     full transcripts live at .sunmao/sessions/<id>.jsonl",
                    ids.len(),
                    ids.join(", ")
                ),
                ok: true,
            });
        }

        // fan out — each spawn gets its own lane + session file
        let futs: Vec<_> = items
            .iter()
            .zip(&defs)
            .map(|(it, def)| spawn_one(ctx, &it.prompt, def.as_ref()))
            .collect();
        let results = futures_util::future::join_all(futs).await;

        if results.len() == 1 {
            return Ok(results.into_iter().next().unwrap());
        }
        // batch: per-item verdict + merged output so the parent sees which
        // child produced what
        let mut merged = String::new();
        let mut all_ok = true;
        for (i, r) in results.into_iter().enumerate() {
            all_ok &= r.ok;
            let mark = if r.ok { "✓" } else { "✗" };
            merged.push_str(&format!("## task {} {mark}\n{}\n\n", i + 1, r.output));
        }
        Ok(ToolResult {
            output: merged.trim_end().to_string(),
            ok: all_ok,
        })
    }
}

/// Spawn policy: which def a `subagent_type` request resolves to, gated by
/// the parent agent's `spawns:` whitelist. `ctx.agent_name` is `None` for
/// the interactive agent — unrestricted. A restricted parent defaults an
/// omitted type to the first whitelist entry (omp semantics). Self-recursion
/// is blocked outright; unknown names fail with the known list instead of
/// silently spawning the generic agent.
fn resolve_spawn_def(
    ctx: &Context,
    requested: Option<&str>,
) -> anyhow::Result<Option<crate::agents::AgentDef>> {
    let all = crate::agents::load_all(&ctx.cwd, &ctx.extra_plugin_roots);
    let known: Vec<String> = all.iter().map(|d| d.name.clone()).collect();
    // extract what the parent def contributes (whitelist + name), then drop
    // the borrow — `all` moves into the lookup below.
    let (parent_name, allowed): (Option<String>, Option<Vec<String>>) = ctx
        .agent_name
        .as_deref()
        .and_then(|n| all.iter().find(|d| d.name == n))
        .map(|p| (Some(p.name.clone()), p.spawns.clone()))
        .unwrap_or((None, None));

    let requested = match (requested, allowed.as_deref()) {
        (None, Some([])) => {
            bail!("{} may not spawn sub-agents", parent_name.unwrap())
        }
        (None, Some(list)) => Some(list[0].as_str()),
        (r, _) => r,
    };
    let Some(name) = requested else {
        return Ok(None); // generic sub-agent, unrestricted parent
    };
    if let Some(list) = allowed.as_deref() {
        if !list.iter().any(|n| n == name) {
            bail!(
                "{} may not spawn `{name}` (allowed: {})",
                parent_name.unwrap(),
                if list.is_empty() {
                    "none".into()
                } else {
                    list.join(", ")
                }
            );
        }
    }
    if parent_name.as_deref() == Some(name) {
        bail!("{name} may not spawn itself (self-recursion)");
    }
    match all.into_iter().find(|d| d.name == name) {
        Some(d) => Ok(Some(d)),
        None => {
            bail!(
                "unknown subagent_type `{name}`{}",
                if known.is_empty() {
                    " — no agent defs under .sunmao/agents".to_string()
                } else {
                    format!(" — known: {}", known.join(", "))
                }
            )
        }
    }
}

/// Register a spawn in the roster — both foreground and detached spawns
/// register; `done` flips when the result lands. `sub_ctx.lane` is the
/// claimed lane.
fn register_task(ctx: &Context, sub_id: &str, lane: u8, prompt: &str, def: Option<&str>) {
    let mut digest: String = prompt.chars().take(60).collect();
    if prompt.chars().count() > 60 {
        digest.push('…');
    }
    ctx.live_tasks
        .lock()
        .unwrap()
        .push(crate::context::TaskEntry {
            id: sub_id.to_string(),
            lane,
            agent: def.map(String::from),
            prompt: digest.split_whitespace().collect::<Vec<_>>().join(" "),
            done: None,
        });
}

/// Flip the roster entry to finished — the detached completion path and
/// the foreground return both route here.
fn finish_task(tasks: &std::sync::Mutex<Vec<crate::context::TaskEntry>>, sub_id: &str, ok: bool) {
    let mut tasks = tasks.lock().unwrap();
    if let Some(e) = tasks.iter_mut().find(|t| t.id == sub_id) {
        e.done = Some(ok);
    }
}

/// Run one sub-agent to completion: own context, own session file, own
/// lane; relays its tool lifecycle to the parent's live sink.
async fn spawn_one(
    ctx: &Context,
    prompt: &str,
    def: Option<&crate::agents::AgentDef>,
) -> ToolResult {
    let (sub_id, sub_ctx) = spawn_parts(ctx, def).await;
    let lane = sub_ctx.lane;
    register_task(ctx, &sub_id, lane, prompt, def.map(|d| d.name.as_str()));
    let res = run_spawn(
        Arc::new(sub_ctx),
        prompt.to_string(),
        ctx.live_sink.get().cloned(),
    )
    .await;
    finish_task(&ctx.live_tasks, &sub_id, res.ok);
    res
}

/// Detached spawn (`run_in_background: true`): the tool returns an id at
/// once; the child runs on its own task and, when it finishes, appends a
/// `TaskDone` event straight into the *parent's* session log — push-style
/// delivery, no polling. The result lands in the session that launched it,
/// even across a /resume.
async fn spawn_detached(
    ctx: &Context,
    prompt: &str,
    def: Option<&crate::agents::AgentDef>,
) -> String {
    let (sub_id, sub_ctx) = spawn_parts(ctx, def).await;
    // roster entry — `/tasks` reads this; the completion path flips `done`
    register_task(
        ctx,
        &sub_id,
        sub_ctx.lane,
        prompt,
        def.map(|d| d.name.as_str()),
    );
    let parent_log = ctx.sessions.clone();
    let parent_tasks = ctx.live_tasks.clone();
    let sink = ctx.live_sink.get().cloned();
    let notify_sink = sink.clone();
    let id = sub_id.clone();
    let prompt = prompt.to_string();
    tokio::spawn(async move {
        let res = run_spawn(Arc::new(sub_ctx), prompt, sink).await;
        // The child's own log already holds the full transcript — the
        // parent record stays lean (capped), with `id` pointing there.
        let output = crate::agent::truncate_output(&res.output);
        {
            let mut log = parent_log.lock().await;
            let _ = log
                .append(&SessionEvent::TaskDone {
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
            s.on_event(&LiveEvent::Hook {
                event: "task.bg.done".into(),
                detail: format!("{} — {}", id, if res.ok { "done" } else { "failed" }),
            });
        }
    });
    sub_id
}

/// Build the child's context: lane claimed *first* so it doubles as the
/// session-file dedup suffix — two spawns in the same millisecond used to
/// collide on `sub-<ms>` and share one log file. `def` is pre-resolved by
/// `resolve_spawn_def` (spawn policy already applied).
async fn spawn_parts(ctx: &Context, def: Option<&crate::agents::AgentDef>) -> (String, Context) {
    let lane = ctx
        .lane_counter
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    let sub_id = format!(
        "sub-{}-l{lane}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let dir = ctx.cwd.join(".sunmao").join("sessions");
    let mut log = SessionLog::open(&dir, &sub_id)
        .await
        .unwrap_or_else(|_| SessionLog::ephemeral());
    // agents/*.md named def wins; else the `subagent-default` prompt
    // section — assembled by the same PromptAssembler as everything else.
    let sys_prompt = def
        .as_ref()
        .map(|d| d.system_prompt.clone())
        .unwrap_or_else(|| {
            crate::prompt::PromptAssembler::new(&ctx.cwd)
                .with_extra_roots(&ctx.extra_plugin_roots)
                .assemble_subagent(def.map(|d| d.name.as_str()))
        });
    {
        let _ = log
            .append(&SessionEvent::Message {
                message: sunmao_llm::types::Message::system(sys_prompt),
            })
            .await;
    }

    // model routing: the def's `model:` selector resolves through the
    // session's resolver; unresolvable/absent means inherit the parent's
    // *active* adapter — a `/model` swap mid-session carries into children.
    let llm = def
        .as_ref()
        .and_then(|d| d.model.as_deref())
        .and_then(|sel| ctx.models.as_ref().and_then(|m| m.adapter_for(sel)))
        .unwrap_or_else(|| ctx.active_llm());

    // tool surface: the def's `tools:` whitelist trims the registry; a
    // declared `spawns:` whitelist needs Task present to mean anything
    // (auto-added), and the depth cap strips Task from leaf children so the
    // model never sees a spawner it can't legally use.
    let allow_names = def.and_then(|d| d.tools.as_ref()).map(|allow| {
        let mut names = (*allow).clone();
        if def
            .and_then(|d| d.spawns.as_ref())
            .is_some_and(|s| !s.is_empty())
            && !names.iter().any(|n| n == "Task")
        {
            names.push("Task".into());
        }
        names
    });
    let mut tools = match &allow_names {
        Some(names) => builtin_registry().filtered(names),
        None => builtin_registry(),
    };
    if ctx.depth + 1 >= MAX_DEPTH {
        tools.remove("Task");
    }

    // fresh context, one depth deeper, on its own lane
    let mut sub_ctx = Context {
        llm,
        llm_override: std::sync::RwLock::new(None),
        sessions: Arc::new(tokio::sync::Mutex::new(log)),
        tools,
        permissions: crate::permissions::Permissions::load(&ctx.cwd, &ctx.extra_plugin_roots),
        approval: ctx.approval.clone(),
        // inherit the parent's resolved table (presets already folded in —
        // re-parsing here would append them twice)
        risk_table: ctx.risk_table.clone(),
        // sub-agents inherit the parent's preset layers — a preset is a
        // session-level property, not per-agent
        hooks: crate::hooks::HookEngine::load(&ctx.cwd, &sub_id, &ctx.extra_plugin_roots),
        cwd: ctx.cwd.clone(),
        session_id: sub_id.clone(),
        depth: ctx.depth + 1,
        lane,
        lane_counter: ctx.lane_counter.clone(),
        cancelled: std::sync::atomic::AtomicBool::new(false),
        read_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
        session_grants: ctx.session_grants.clone(),
        live_sink: std::sync::OnceLock::new(),
        models: ctx.models.clone(),
        agent_name: def.map(|d| d.name.clone()),
        extra_plugin_roots: ctx.extra_plugin_roots.clone(),
        // the child spawns its own extension children against its own
        // session id — parent's processes are never shared
        ext: Arc::new(crate::ext::ExtRegistry::new()),
        // the parent's driver applies — a preset-named loop is
        // session-level, not per-agent
        loop_driver: ctx.loop_driver,
        live_tasks: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        // the child's list is its own plan, not a copy of the parent's —
        // sub-session logs only carry their own Todos events.
        todos: std::sync::Mutex::new(Vec::new()),
    };
    // extension children come up in the child's scope; their tools follow
    // the same `tools:` whitelist rule as native ones — an ext tool not on
    // the list is filtered out with everything else.
    sub_ctx.connect_extensions().await;
    if let Some(names) = &allow_names {
        sub_ctx.tools = sub_ctx.tools.filtered(names);
    }
    if ctx.depth + 1 >= MAX_DEPTH {
        sub_ctx.tools.remove("Task");
    }
    (sub_id, sub_ctx)
}

/// Drive a built child context through one turn — SubagentStart/Stop hooks
/// wrap the run and the final assistant text becomes the ToolResult.
async fn run_spawn(
    sub_ctx: Arc<Context>,
    prompt: String,
    sink: Option<Arc<dyn Observer>>,
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
                source: Some("subagent"),
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
    let text = obs.text.lock().unwrap().clone();
    match outcome {
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

#[cfg(test)]
mod tests;
