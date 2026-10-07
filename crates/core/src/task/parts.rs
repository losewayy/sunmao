//! Child-context assembly — `spawn_parts` mints id+lane+log for a fresh
//! spawn, `build_sub_ctx` fills a Context over a given log (shared by
//! resumes, which keep the on-disk log). The driving side (foreground /
//! detached run + roster bookkeeping) stays in `spawn.rs`; what a child
//! *is* — model routing, tool whitelist, shared vs own seams — lives here.

use std::sync::Arc;

use sunmao_llm::ProviderAdapter;

use crate::context::Context;
use crate::session::{SessionEvent, SessionLog};
use crate::tool::builtin_registry;

/// Build the child's context: lane claimed *first* so it doubles as the
/// session-file dedup suffix — two spawns in the same millisecond used to
/// collide on `sub-<ms>` and share one log file. `def` is pre-resolved by
/// `resolve_spawn_def` (spawn policy already applied). `sys_prompt`
/// overrides the def/default system prompt entirely — fusion's Sidekick
/// carries its own contract (`fusion-sidekick`), not the generic
/// sub-agent's.
pub(crate) async fn spawn_parts(
    ctx: &Context,
    def: Option<&crate::agents::AgentDef>,
    llm_override: Option<Arc<dyn ProviderAdapter>>,
    sys_prompt: Option<String>,
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
    let mut log = match SessionLog::open(&dir, &sub_id).await {
        Ok(l) => l,
        Err(e) => {
            // a dead sessions dir must not kill the spawn — but it must not
            // be silent either: an ephemeral child loses its audit trail
            tracing::warn!("sub-agent log fell back to ephemeral: {e:#}");
            SessionLog::ephemeral()
        }
    };
    // an explicit sys_prompt wins (fusion Sidekick); else agents/*.md
    // named def; else the `subagent-default` prompt section — assembled by
    // the same PromptAssembler as everything else. Either way the
    // environment tail (guidance/dialect/+ptc) is appended: a contract
    // names the child's job, never the session facts it runs under.
    let assembler = crate::prompt::PromptAssembler::new(&ctx.cwd)
        .with_extra_roots(&ctx.extra_plugin_roots)
        .with_driver(ctx.loop_driver);
    let sys_prompt = assembler.subagent_prompt(sys_prompt.unwrap_or_else(|| {
        def.as_ref()
            .map(|d| d.system_prompt.clone())
            .unwrap_or_else(|| assembler.assemble_subagent(def.map(|d| d.name.as_str())))
    }));
    {
        if let Err(e) = log
            .append(&SessionEvent::Message {
                message: sunmao_llm::types::Message::system(sys_prompt),
            })
            .await
        {
            tracing::warn!("sub-agent system message not durable: {e:#}");
        }
    }
    (
        sub_id.clone(),
        build_sub_ctx(ctx, sub_id, lane, def, llm_override, log).await,
    )
}

/// Assemble a child's Context over a given log — shared by fresh spawns
/// (`spawn_parts` mints id+lane+log) and resumes (`resume_parts` keeps the
/// id and the on-disk log). Model routing, the `tools:`/`spawns:` surface
/// trim, the `permissions:` deny/ask overlay and per-child seams are
/// policy, identical on both paths.
pub(crate) async fn build_sub_ctx(
    ctx: &Context,
    sub_id: String,
    lane: u16,
    def: Option<&crate::agents::AgentDef>,
    llm_override: Option<Arc<dyn ProviderAdapter>>,
    log: SessionLog,
) -> Context {
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
        // under `ptc` the only model-emittable tools are RunCode+SearchTools
        // — a whitelist without them leaves the child zero usable tools
        if ctx.loop_driver == crate::agent::LoopDriver::Ptc {
            for n in ["RunCode", "SearchTools"] {
                if !names.iter().any(|x| x == n) {
                    names.push(n.into());
                }
            }
        }
        names
    });
    let mut tools = builtin_registry();
    // seed the parent's drained MCP catalog into the child's registry — the
    // shared handles reset seen_version on clone, so without this the child
    // waits for a list_changed bump that only the parent's drain loop sees
    for srv in ctx.mcp_servers.iter() {
        tools.replace_prefixed(&format!("mcp__{}__", srv.name), srv.tool_impls());
    }
    if let Some(names) = &allow_names {
        tools = tools.filtered(names);
    }
    if ctx.depth + 1 >= super::MAX_DEPTH {
        tools.remove("Task");
    }

    // permission overlay: the def's `permissions:` deny/ask rules fold onto
    // the freshly loaded session table — never the parent's struct itself.
    // deny>ask ordering means this can only narrow the child's surface.
    let permissions = {
        let base = crate::permissions::Permissions::load(&ctx.cwd, &ctx.extra_plugin_roots);
        let entries = def.map(|d| d.permissions.as_slice()).unwrap_or(&[]);
        if entries.is_empty() {
            base
        } else {
            let (ask, deny, ignored) = crate::permissions::Permissions::classify_overlay(entries);
            if ignored > 0 {
                tracing::warn!(
                    "agent {:?}: {ignored} permissions: entr{} ignored \
                     (only `deny:`/`ask:` rules apply — allow can't widen)",
                    def.map(|d| d.name.as_str()).unwrap_or("?"),
                    if ignored == 1 { "y" } else { "ies" },
                );
            }
            base.with_deny_ask_overlay(&ask, &deny)
        }
    };

    // fresh context, one depth deeper, on its own lane
    let sessions = Arc::new(tokio::sync::Mutex::new(log));
    let mut hook_engine =
        crate::hooks::HookEngine::load(&ctx.cwd, &sub_id, &ctx.extra_plugin_roots);
    // trust-skip audit rows land on the CHILD's log — same ownership rule
    // as its ToolResult facts
    hook_engine.attach_sessions(sessions.clone());
    let mut sub_ctx = Context {
        // the child's own store — sub-agent sessions are isolated logs
        ptc_store: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        llm,
        llm_override: std::sync::RwLock::new(None),
        active_selector: std::sync::RwLock::new(None),
        sessions,
        tools,
        permissions,
        approval: ctx.approval.clone(),
        // inherit the parent's resolved table (presets already folded in —
        // re-parsing here would append them twice)
        risk_table: ctx.risk_table.clone(),
        // sub-agents inherit the parent's preset layers — a preset is a
        // session-level property, not per-agent
        hooks: Arc::new(hook_engine),
        cwd: ctx.cwd.clone(),
        session_id: std::sync::RwLock::new(sub_id.clone()),
        depth: ctx.depth + 1,
        lane,
        lane_counter: ctx.lane_counter.clone(),
        cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        // the child runs the same shells — the choice is session-level
        // (SUNMAO_SHELL / .sunmao/shell.txt resolved on the parent)
        shell: ctx.shell,
        local_shell: ctx.local_shell,
        tool_timeouts: ctx.tool_timeouts.clone(),
        read_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
        // the child's ledger is its own — keyed to its session id, not the
        // parent's: a parallel sibling's `taken` must not starve this one's
        // snapshots (same rule as read_paths; unlike session_grants which
        // IS shared)
        checkpoints: std::sync::Mutex::new(crate::checkpoints::load(&ctx.cwd, &sub_id)),
        // session grants stay shared with the parent — re-prompting an
        // already-approved command on every spawn would degrade the
        // approval UX. The leak this creates is bounded by the def's
        // `permissions:` overlay above: a def deny rule outranks any
        // inherited grant (deny is checked before grants in gate_call).
        session_grants: ctx.session_grants.clone(),
        // the session's stance is shared, not copied — a mid-session /mode
        // switch applies to children already running
        approval_mode: ctx.approval_mode.clone(),
        // turn mode is per-context, unlike approval_mode: a sub-agent
        // always runs Standard — a Sidekick can't delegate a second level
        turn_mode: std::sync::RwLock::new(crate::agent::TurnMode::Standard),
        fusion_models: std::sync::RwLock::new(crate::context::FusionModelSettings::default()),
        // same per-context rule: the Lead's read_only flag is its own —
        // arming fusion must never lock the child it delegates to
        read_only: std::sync::atomic::AtomicBool::new(false),
        fusion: std::sync::Mutex::new(crate::agent::fusion::FusionState::default()),
        // same sharing rule as approval_mode — /effort applies to the
        // whole session, children included
        reasoning_effort: ctx.reasoning_effort.clone(),
        effort_default: ctx.effort_default.clone(),
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
        persisted_driver: ctx.persisted_driver,
        hook_tail: std::sync::Mutex::new(Vec::new()),
        promoted_tools: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        // a sub-agent gets a fresh surface — the parent's pins encode ITS
        // history (an MCP tool the parent saw eagerly must not be forced
        // onto a child that never advertised it)
        advertised_pins: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        persisted_pins: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        live_tasks: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        // the child's list is its own plan, not a copy of the parent's —
        // sub-session logs only carry their own Todos events.
        todos: std::sync::Mutex::new(Vec::new()),
        // a Task spawn IS the parent's "work until done" surface — the
        // child gets its own goal slot (its own log seeds it) but never
        // inherits the parent's standing objective.
        goal: std::sync::Mutex::new(None),
        input_pending: std::sync::atomic::AtomicUsize::new(0),
        // the child's steer queue is shared with the parent's roster
        // (TaskEntry.steer) — `steer_sub`/`Task{steer}` push mid-run
        // messages that this child's turn drains like any other steer
        steer: crate::context::SteerQueue::default(),
        // the parent's steer queue is the child's uplink — `SendMessage`
        // pushes here; the parent's next request boundary folds it in as
        // a tagged user message. Cloning the Arc keeps the address stable
        // across the parent's `/resume` (the QUEUE survives a log swap).
        parent_steer: Some(ctx.steer.clone()),
        // the child's job registry is its own, same rule as live_tasks — a
        // sub-agent's background jobs are listed and stopped from its own
        // surface, and their completion lands in its own log.
        jobs: crate::tool::jobs::new_table(),
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
    // audit trail for the overlay — the fact lives in the child's own log
    // and mirrors to the parent's live spine so --dataflow/TUI see it
    if let Some(d) = def
        && !d.permissions.is_empty()
    {
        let detail = format!(
            "{}: {} deny/ask rules overlaid",
            d.name,
            d.permissions.len()
        );
        {
            let mut l = sub_ctx.sessions.lock().await;
            l.append_audit(&crate::session::SessionEvent::Hook {
                event: "agent.perms".into(),
                detail: detail.clone(),
            })
            .await;
        }
        if let Some(s) = ctx.live_sink.get() {
            s.on_event(&crate::agent::LiveEvent::Hook {
                event: "agent.perms".into(),
                detail: format!("[l{}] {detail}", sub_ctx.lane),
            });
        }
    }
    sub_ctx
}
