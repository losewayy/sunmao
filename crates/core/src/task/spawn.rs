//! Spawn machinery — building a child's Context, driving its turn, and
//! the detached (`run_in_background`) completion path that pushes results
//! into the parent's session log. The TaskTool surface and spawn policy
//! (`resolve_spawn_def`) stay in `mod.rs`.

use std::sync::Arc;

use serde_json::json;
use sunmao_llm::ProviderAdapter;

use crate::agent::{AgentLoop, LiveEvent, Observer};
use crate::context::Context;
use crate::session::{SessionEvent, SessionLog};
use crate::tool::{ToolResult, builtin_registry};

/// Collects the sub-agent's final assistant text for the tool result AND
/// relays its tool lifecycle to the session's `live_sink` — the frontend
/// sees `Task` blocks working through real calls instead of a frozen row.
/// Relayed events are re-stamped with the child's `lane` so parallel
/// siblings don't collide on (name, depth). TurnEnd is swallowed: a
/// sub-agent's end must not unwind the outer turn.
pub(super) struct RelayObserver {
    pub(super) text: std::sync::Mutex<String>,
    pub(super) sink: Option<Arc<dyn Observer>>,
    pub(super) lane: u8,
}

impl Observer for RelayObserver {
    fn on_event(&self, ev: &LiveEvent) {
        match ev {
            LiveEvent::Content { text } => self.text.lock().unwrap().push_str(text),
            // sub-agent lifecycle is the parent's business, not the UI's —
            // forwarding TurnEnd would close the outer transcript early.
            LiveEvent::TurnEnd { .. } => {}
            LiveEvent::ToolStart {
                name,
                summary,
                depth,
                call_id,
                ..
            } => {
                if let Some(s) = &self.sink {
                    s.on_event(&LiveEvent::ToolStart {
                        name: name.clone(),
                        summary: summary.clone(),
                        depth: *depth,
                        lane: self.lane,
                        call_id: call_id.clone(),
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
            _ => {
                if let Some(s) = &self.sink {
                    s.on_event(ev);
                }
            }
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
/// `llm_override` is the resolved call-site `model` selector — wins over
/// the def's `model:` frontmatter and over inheriting the parent.
pub(super) async fn spawn_one(
    ctx: &Context,
    prompt: &str,
    def: Option<&crate::agents::AgentDef>,
    llm_override: Option<Arc<dyn ProviderAdapter>>,
) -> ToolResult {
    let (sub_id, sub_ctx) = spawn_parts(ctx, def, llm_override).await;
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
pub(super) async fn spawn_detached(
    ctx: &Context,
    prompt: &str,
    def: Option<&crate::agents::AgentDef>,
    llm_override: Option<Arc<dyn ProviderAdapter>>,
) -> String {
    let (sub_id, sub_ctx) = spawn_parts(ctx, def, llm_override).await;
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
    // The parent's turn fence: a mid-turn completion must not append the
    // TaskDone user-message between a ToolCall and its ToolResult — that
    // would break provider pairing (tool_result must immediately follow
    // its tool_use) and hard-400 the next request. Waiting for the turn
    // boundary keeps the pushed fact well-formed.
    let parent_fence = ctx.turn_lock.clone();
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
            let _turn_permit = parent_fence.lock().await;
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
pub(super) async fn spawn_parts(
    ctx: &Context,
    def: Option<&crate::agents::AgentDef>,
    llm_override: Option<Arc<dyn ProviderAdapter>>,
) -> (String, Context) {
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

    // model routing: call-site `model` selector > def `model:` frontmatter
    // > the parent's *active* adapter — a `/model` swap mid-session carries
    // into children that didn't pin one.
    let llm = llm_override
        .or_else(|| {
            def.as_ref()
                .and_then(|d| d.model.as_deref())
                .and_then(|sel| ctx.models.as_ref().and_then(|m| m.adapter_for(sel)))
        })
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
    if ctx.depth + 1 >= super::MAX_DEPTH {
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
        session_id: std::sync::RwLock::new(sub_id.clone()),
        depth: ctx.depth + 1,
        lane,
        lane_counter: ctx.lane_counter.clone(),
        cancelled: std::sync::atomic::AtomicBool::new(false),
        read_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
        // the child's ledger is its own — keyed to its session id, not the
        // parent's: a parallel sibling's `taken` must not starve this one's
        // snapshots (same rule as read_paths; unlike session_grants which
        // IS shared)
        checkpoints: std::sync::Mutex::new(crate::checkpoints::load(&ctx.cwd, &sub_id)),
        session_grants: ctx.session_grants.clone(),
        // the session's stance is shared, not copied — a mid-session /mode
        // switch applies to children already running
        approval_mode: ctx.approval_mode.clone(),
        readonly_verbs: ctx.readonly_verbs.clone(),
        live_sink: std::sync::OnceLock::new(),
        models: ctx.models.clone(),
        agent_name: def.map(|d| d.name.clone()),
        extra_plugin_roots: ctx.extra_plugin_roots.clone(),
        // the child spawns its own extension children against its own
        // session id — parent's processes are never shared
        ext: Arc::new(crate::ext::ExtRegistry::new()),
        // app bridges proxy against the shared server pool — a sub-agent's
        // islands call the same MCP servers its parent's would
        mcp_servers: ctx.mcp_servers.clone(),
        // the parent's driver applies — a preset-named loop is
        // session-level, not per-agent
        loop_driver: ctx.loop_driver,
        live_tasks: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        // the child's list is its own plan, not a copy of the parent's —
        // sub-session logs only carry their own Todos events.
        todos: std::sync::Mutex::new(Vec::new()),
        // steering is a top-level UX surface — sub-agents never take
        // mid-turn user input; their queue stays empty
        steer: std::sync::Mutex::new(std::collections::VecDeque::new()),
        // own fence: children must never queue behind the parent's turn
        turn_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
    };
    // extension children come up in the child's scope; their tools follow
    // the same `tools:` whitelist rule as native ones — an ext tool not on
    // the list is filtered out with everything else.
    sub_ctx.connect_extensions().await;
    if let Some(names) = &allow_names {
        sub_ctx.tools = sub_ctx.tools.filtered(names);
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
