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

use crate::context::MutexRecover;
use std::sync::Arc;

use anyhow::bail;
use serde::Deserialize;
use serde_json::{Value, json};
use sunmao_llm::types::Tool;

use crate::context::Context;
use crate::tool::{ToolImpl, ToolResult};

mod parts;
mod resume;
mod spawn;
use resume::resume_sub;
use spawn::{spawn_detached, spawn_one};

const MAX_DEPTH: u8 = 2;

pub struct TaskTool;

/// One task in a batch spawn (`tasks[]` items).
#[derive(Deserialize)]
struct TaskItem {
    prompt: String,
    subagent_type: Option<String>,
    /// call-site model selector — routes this spawn to another adapter
    model: Option<String>,
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
             concurrently. `model` routes a spawn to another provider/model via \
             .sunmao/models.json (@route or provider/<model-id>) — multi-model \
             orchestration, e.g. cheap model for probes, strong model for the \
             final pass. `run_in_background: true` detaches each spawn: returns \
             task ids now, and each finished sub-agent pushes its result into this \
             session as a tagged message. `steer` + `message` injects a mid-run \
             user message into a named running sub-agent. `resume` + `prompt` \
             continues a finished sub-agent on its own transcript (optionally \
             re-routed via `model`). The reverse channel exists too: a child \
             calls `SendMessage` to deliver a tagged user message back into \
             this session mid-run. Use for parallelizable or scope-isolated \
             work; sub-agents cannot spawn further sub-agents beyond the depth cap.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Complete instructions for the subtask (flat form); the continuation instruction when combined with `resume`"},
                    "subagent_type": {"type": "string", "description": "Named agent def from .sunmao/agents/*.md or .claude/agents/*.md"},
                    "model": {"type": "string", "description": "Model selector — @route or provider/<model-id> from .sunmao/models.json; child runs on that adapter instead of inheriting the parent's"},
                    "steer": {"type": "string", "description": "Inject a mid-run user message into the named running sub-agent (sub-<id>). The child folds it at its next request boundary. Requires `message`."},
                    "message": {"type": "string", "description": "The steer's text — required with `steer`"},
                    "resume": {"type": "string", "description": "Continue a previous sub-agent: its sub-<id> log becomes the transcript base. Combine with `prompt` for the continuation instruction; `model` may re-route to another provider (rate-limit escape)."},
                    "context": {"type": "string", "description": "Shared background prepended to every task (batch form)"},
                    "tasks": {
                        "type": "array",
                        "description": "Batch form: run all items concurrently",
                        "items": {
                            "type": "object",
                            "properties": {
                                "prompt": {"type": "string"},
                                "subagent_type": {"type": "string"},
                                "model": {"type": "string", "description": "Model selector for this item — @route or provider/<model-id>"}
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
            model: Option<String>,
            steer: Option<String>,
            message: Option<String>,
            resume: Option<String>,
            context: Option<String>,
            tasks: Option<Vec<TaskItem>>,
            run_in_background: Option<bool>,
        }
        let a: Args = serde_json::from_value(args)?;

        // steer/resume are per-sub-id operations — they never combine with
        // the spawn forms (batch stays spawn-only).
        if a.tasks.is_some() && (a.steer.is_some() || a.resume.is_some()) {
            bail!("steer/resume take a single sub-id — tasks[] is spawn-only");
        }
        if let Some(sub_id) = a.steer.as_deref() {
            if a.resume.is_some() {
                bail!("steer and resume are exclusive");
            }
            if a.prompt.is_some()
                || a.subagent_type.is_some()
                || a.context.is_some()
                || a.run_in_background.unwrap_or(false)
            {
                bail!("steer takes only `message` — no spawn args apply");
            }
            let Some(msg) = a.message.as_deref() else {
                bail!("steer needs `message` — the text to inject into {sub_id}");
            };
            return match ctx.steer_sub(sub_id, msg.to_string()) {
                Ok(()) => Ok(ToolResult {
                    output: format!("steered {sub_id}"),
                    ok: true,
                }),
                Err(e) => Ok(ToolResult {
                    output: e.to_string(),
                    ok: false,
                }),
            };
        }
        if a.message.is_some() {
            bail!("`message` is the steer's text — it needs `steer`");
        }
        if let Some(sub_id) = a.resume.as_deref() {
            if a.subagent_type.is_some() {
                bail!("resume re-resolves the roster's def — subagent_type doesn't apply");
            }
            if a.context.is_some() {
                bail!("`context` is the batch preamble — it doesn't apply to resume");
            }
            let Some(prompt) = a.prompt.as_deref() else {
                bail!("resume needs `prompt` — the continuation instruction");
            };
            if ctx.depth >= MAX_DEPTH {
                bail!("sub-agent depth limit reached ({MAX_DEPTH})");
            }
            // def re-resolution: the roster remembered the def name (minus
            // the `-r` lineage tag a fresh resumed entry carries); a child
            // the roster never knew — a process restart left only the log —
            // resumes as a generic sub-agent.
            let agent_name = {
                let tasks = ctx.live_tasks.lock_or_recover();
                tasks
                    .iter()
                    .find(|t| t.id == sub_id)
                    .and_then(|t| t.agent.as_deref())
                    .map(|n| n.strip_suffix("-r").unwrap_or(n).to_string())
            };
            let def = agent_name
                .as_deref()
                .and_then(|n| crate::agents::find(&ctx.cwd, &ctx.extra_plugin_roots, n));
            let llm_override = match &a.model {
                None => None,
                Some(sel) => match ctx.models.as_ref() {
                    Some(m) => m.adapter_for(sel).map(Some).ok_or_else(|| {
                        anyhow::anyhow!(
                            "unknown model selector `{sel}` — available: {}",
                            m.describe().join(", ")
                        )
                    })?,
                    None => bail!("model selector `{sel}` needs .sunmao/models.json routes"),
                },
            };
            return resume_sub(
                ctx,
                sub_id,
                prompt,
                def.as_ref(),
                llm_override,
                a.run_in_background.unwrap_or(false),
            )
            .await;
        }

        if ctx.depth >= MAX_DEPTH {
            bail!("sub-agent depth limit reached ({MAX_DEPTH})");
        }

        // normalize both shapes into (prompt, subagent_type) items
        let items: Vec<TaskItem> = match (a.prompt, a.tasks) {
            (Some(p), None) => vec![TaskItem {
                prompt: p,
                subagent_type: a.subagent_type,
                model: a.model,
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
                        model: t.model,
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

        // call-site model selectors resolve up front: an explicit selector
        // that resolves to nothing fails the call — silently inheriting the
        // parent would hide a typo'd route behind a working-looking spawn.
        let llm_overrides: Vec<Option<Arc<dyn sunmao_llm::ProviderAdapter>>> = items
            .iter()
            .map(|it| match &it.model {
                None => Ok(None),
                Some(sel) => match ctx.models.as_ref() {
                    Some(m) => m.adapter_for(sel).map(Some).ok_or_else(|| {
                        anyhow::anyhow!(
                            "unknown model selector `{sel}` — available: {}",
                            m.describe().join(", ")
                        )
                    }),
                    None => {
                        anyhow::bail!("model selector `{sel}` needs .sunmao/models.json routes")
                    }
                },
            })
            .collect::<Result<_, _>>()?;

        // detached lane: ids now, results pushed into the session log later
        if a.run_in_background.unwrap_or(false) {
            let mut ids = Vec::with_capacity(items.len());
            for ((it, def), llm) in items.iter().zip(&defs).zip(&llm_overrides) {
                ids.push(spawn_detached(ctx, &it.prompt, def.as_ref(), llm.clone()).await);
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
            .zip(&llm_overrides)
            .map(|((it, def), llm)| spawn_one(ctx, &it.prompt, def.as_ref(), llm.clone()))
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
    if let Some(list) = allowed.as_deref()
        && !list.iter().any(|n| n == name)
    {
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

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_cancel;
#[cfg(test)]
mod tests_steer_resume;
