//! Fusion mode — Lead/Sidekick delegation for `TurnMode::Fusion`.
//!
//! The session's context becomes the Lead: read-only at the gate
//! (`ctx.read_only`), a write-tool-stripped declaration surface, and a
//! `fusion-lead` tail-of-request prompt. Work reaches the Sidekick — a
//! fresh-context sub-agent — through the `FusionExecute` call, which this
//! module implements.
//!
//! Delegation state lives on the Lead's Context (`ctx.fusion`) rather than
//! in a task-local: the Sidekick survives across turn iterations (rework
//! steers the SAME session), so the handle must outlive any one call.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::context::{Context, MutexRecover};
use crate::session::SessionEvent;
use crate::tool::{ToolImpl, ToolResult};
use sunmao_llm::types::Tool;

/// One Sidekick's live handle + the delegation's accounting.
///
/// `whitelist` is the file set the Lead granted write access to —
/// canonicalized absolute paths, stored on the LEAD's context and mirrored
/// onto the Sidekick's `fusion.whitelist` each time it grows. Add-only by
/// construction: no code path shrinks it, and every growth appends a
/// `fusion.whitelist` audit fact.
#[derive(Default)]
pub(crate) struct FusionState {
    /// "This context IS a Sidekick" — set on the child at spawn. The gate's
    /// whitelist arm keys on it, NOT on a non-empty list: the Lead keeps a
    /// `whitelist` ledger of the same grant on its own context, and without
    /// the marker an escalated Lead would refuse its own writes against the
    /// file set it granted away.
    pub(crate) is_sidekick: bool,
    /// Running/finished Sidekick context — reused across `steer` rework so
    /// the child keeps its transcript, whitelist and read ledger.
    pub(crate) sidekick: Option<Arc<Context>>,
    /// The child's `sub-…-lN` session id — doubles as the audit link.
    pub(crate) sidekick_id: Option<String>,
    /// Delegation-spec counter — `FusionSpec.seq`.
    pub(crate) spec_seq: u64,
    /// Files the Sidekick may Write/Edit (canonicalized). Add-only.
    pub(crate) whitelist: Vec<PathBuf>,
    /// Verify commands from the latest spec — rework steers re-run them.
    pub(crate) verify: Vec<String>,
    /// Consecutive failed delegations — the escalation streak.
    pub(crate) verify_fails: u32,
    /// The Lead unlocked for the rest of this turn (read_only disarmed) —
    /// the turn loop restores the flag at turn end.
    pub(crate) escalated: bool,
}

/// Delegations before the Lead escalates and finishes the job itself.
pub(crate) const ESCALATE_AFTER: u32 = 2;

/// Is `name` on the fusion Lead's declared surface? `advertised_tools`
/// delegates the answer here so the surface table sits with the rest of
/// the delegation's policy, not inside Context's field soup. An armed
/// Lead keeps the read tools + Bash + UpdateGoal + FusionExecute (Task's
/// slot — the spec contract replaces free-form sub-agent prompts); an
/// escalated Lead returns to the standard surface minus the delegation
/// tool — it finishes the job itself.
pub(crate) fn lead_decl(name: &str, escalated: bool) -> bool {
    if escalated {
        return name != "SearchTools" && name != "FusionExecute";
    }
    const LEAD_TOOLS: &[&str] = &[
        "Read",
        "Grep",
        "Glob",
        "WebFetch",
        "JobOutput",
        "Bash",
        "UpdateGoal",
        "FusionExecute",
    ];
    LEAD_TOOLS.contains(&name)
}

/// Sidekick model selection and adapter resolution.
pub(crate) mod sidekick;

/// The verdict stage — verify execution, escalation accounting and the
/// Sidekick trace digest. (Split out: delegation plumbing lives here.)
mod verdict;

/// The Lead's delegation tool — fusion's replacement for `Task`'s slot.
pub struct FusionExecuteTool;

#[derive(Deserialize)]
struct Args {
    /// The complete brief — object (`goal`/`context`/`constraints`…), a
    /// plain string, or a list of instructions. Required for a fresh
    /// delegation; optional on `steer`, where the Sidekick already holds
    /// its transcript and the steer text IS the delta.
    spec: Option<Value>,
    /// Files the Sidekick may Write/Edit — the whitelist the gate enforces.
    #[serde(default)]
    files: Vec<String>,
    /// Paths the Sidekick should Read before editing — it has the read
    /// tools, so naming the file beats pasting its contents into `spec`
    /// (cheaper, and can't drift from what's on disk).
    #[serde(default)]
    context_files: Vec<String>,
    /// Shell commands that must exit 0 after the run — executed by the
    /// harness (`tool::run_foreground`), never by the model's own claim.
    #[serde(default)]
    verify_commands: Vec<String>,
    /// Rework feedback — resumes the SAME Sidekick instead of spawning.
    steer: Option<String>,
    /// Optional model selector routed through `.sunmao/models.json` —
    /// the same `@route`/`provider/<id>` vocabulary as `Task.model`.
    model: Option<String>,
}

#[async_trait::async_trait]
impl ToolImpl for FusionExecuteTool {
    fn name(&self) -> &'static str {
        "FusionExecute"
    }

    fn decl(&self) -> Tool {
        Tool::function(
            "FusionExecute",
            "Delegate a work spec to the Sidekick — it executes with the full \
             toolset (its Bash included — it can and should run builds/tests \
             itself) while you stay read-only. `spec` is the complete brief, \
             required for a fresh delegation: the Sidekick sees NOTHING of \
             this conversation, so include goal, context and constraints — \
             name files it should read via `context_files` instead of pasting \
             their contents in. `files` whitelists the paths it may Write/Edit \
             (add-only once granted). `verify_commands` run through the real \
             shell afterwards and their exit codes come back in this result — \
             execution facts, not the Sidekick's claim; a command that could \
             not even start (missing toolchain, spawn error) reports \
             [inconclusive — environment] and does NOT count toward \
             escalation. On a failed verdict, call again with `steer` alone \
             (feedback text; `spec` may be omitted) to resume the SAME \
             Sidekick; new `files` entries widen — never shrink — the \
             whitelist, and new `verify_commands` replace the check list. \
             After repeated failures the delegation escalates: your write \
             tools unlock so you can finish the work yourself.",
            json!({
                "type": "object",
                "properties": {
                    "spec": {
                        "description": "Complete work brief — object (e.g. {goal, context, constraints}), plain text, or a list of instructions. Required for a fresh delegation; omit when `steer` alone carries the rework"
                    },
                    "files": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Files the Sidekick may write — whitelist enforced at the gate; add-only once granted"
                    },
                    "context_files": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Paths (project-relative) the Sidekick should Read before editing — cheaper than inlining their contents into spec"
                    },
                    "verify_commands": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Shell commands run after the delegation; every one must exit 0"
                    },
                    "steer": {
                        "type": "string",
                        "description": "Rework feedback — resumes the SAME Sidekick (transcript + whitelist kept) instead of spawning a new one"
                    },
                    "model": {
                        "type": "string",
                        "description": "Model selector — @route or provider/<model-id> from .sunmao/models.json; the Sidekick runs on that adapter instead of inheriting yours"
                    }
                },
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &Arc<Context>) -> anyhow::Result<ToolResult> {
        let a: Args = serde_json::from_value(args.clone())?;
        if a.steer.is_some() {
            rework(ctx, &a).await
        } else if a.spec.is_some() {
            delegate(ctx, &a).await
        } else {
            Ok(ToolResult {
                output: "missing `spec` — a fresh delegation needs the complete \
                         brief (`steer` alone only resumes a live Sidekick)"
                    .into(),
                ok: false,
            })
        }
    }
}

/// Fresh delegation: mint a Sidekick context on the fusion system prompt,
/// grant its whitelist, register it on the parent's roster, drive one
/// turn, then let the verify verdict settle the call.
async fn delegate(ctx: &Arc<Context>, a: &Args) -> anyhow::Result<ToolResult> {
    if ctx.depth + 1 >= crate::task::MAX_DEPTH {
        return Ok(ToolResult {
            output: format!("sub-agent depth limit reached ({})", crate::task::MAX_DEPTH),
            ok: false,
        });
    }
    // the pair is the point: the Lead's own model plans and verifies, and the
    // Sidekick runs on the pinned or configured selector (see `sidekick`)
    let llm_override = sidekick::adapter(ctx, a.model.as_deref())?;
    let sys_prompt = crate::prompt::PromptAssembler::new(&ctx.cwd)
        .with_extra_roots(&ctx.extra_plugin_roots)
        .assemble_fusion_sidekick();
    let (sub_id, sub_ctx) =
        crate::task::parts::spawn_parts(ctx, None, llm_override, Some(sys_prompt)).await;

    // The spec's file grant — canonicalized the way the gate's
    // whitelist_covers will resolve the child's Write/Edit paths. An
    // unresolvable entry just never matches (fail-closed by symmetry).
    let grant = canonicalize_all(&ctx.cwd, &a.files);
    {
        let mut f = sub_ctx.fusion.lock_or_recover();
        f.is_sidekick = true;
        f.whitelist = grant.clone();
    }
    let (seq, hash) = {
        let mut f = ctx.fusion.lock_or_recover();
        f.spec_seq += 1;
        f.whitelist = grant;
        f.verify = a.verify_commands.clone();
        // `call` only reaches here with a present spec
        (
            f.spec_seq,
            spec_hash(a.spec.as_ref().unwrap_or(&Value::Null)),
        )
    };
    // The delegation contract is the fusion audit spine — the full spec is
    // durable, `seq`/`spec_hash` tie the accepted/escalated verdicts back.
    {
        let mut log = ctx.sessions.lock().await;
        log.append_audit(&SessionEvent::FusionSpec {
            seq,
            spec_hash: hash,
            spec: json!({
                "spec": a.spec,
                "files": a.files,
                "verify_commands": a.verify_commands,
            }),
            sidekick: sub_id.clone(),
        })
        .await;
    }

    let steer = sub_ctx.steer.clone();
    let cancel = sub_ctx.cancel_signal();
    crate::task::spawn::register_task(
        ctx,
        &sub_id,
        sub_ctx.lane,
        &spec_digest(a.spec.as_ref().unwrap_or(&Value::Null)),
        Some("fusion-sidekick"),
        &steer,
        &cancel,
    );
    let sidekick = Arc::new(sub_ctx);
    {
        let mut f = ctx.fusion.lock_or_recover();
        f.sidekick = Some(sidekick.clone());
        f.sidekick_id = Some(sub_id.clone());
    }
    let _roster = crate::task::spawn::RosterGuard::new(&ctx.live_tasks, &sub_id);
    let res = crate::task::spawn::run_spawn(
        sidekick,
        sidekick_prompt(a),
        ctx.live_sink.get().cloned(),
        "fusion",
    )
    .await;
    crate::task::spawn::finish_task(&ctx.live_tasks, &sub_id, res.ok);
    crate::task::spawn::roster_changed(&ctx.live_sink.get().cloned(), &sub_id);
    verdict::settle(ctx, seq, res).await
}

/// Rework: `steer` resumes the SAME Sidekick — its transcript, read ledger
/// and whitelist survive; the spec may only widen the file set (audited)
/// or swap the verify list.
async fn rework(ctx: &Arc<Context>, a: &Args) -> anyhow::Result<ToolResult> {
    let (sidekick, sub_id) = {
        let f = ctx.fusion.lock_or_recover();
        match (&f.sidekick, &f.sidekick_id) {
            (Some(s), Some(id)) => (s.clone(), id.clone()),
            _ => {
                return Ok(ToolResult {
                    output: "steer needs a live delegation — call FusionExecute \
                             without `steer` to spawn the Sidekick first"
                        .into(),
                    ok: false,
                });
            }
        }
    };
    // Whitelist growth — add-only on both the Lead's ledger and the child's
    // gate set. The audit fact names every new grant: a widened file set is
    // exactly the evidence a reviewer needs after the fact.
    let additions: Vec<PathBuf> = {
        let mut f = ctx.fusion.lock_or_recover();
        let mut new_paths = Vec::new();
        for p in canonicalize_all(&ctx.cwd, &a.files) {
            if !f.whitelist.contains(&p) {
                f.whitelist.push(p.clone());
                new_paths.push(p);
            }
        }
        if !a.verify_commands.is_empty() {
            f.verify = a.verify_commands.clone();
        }
        new_paths
    };
    if !additions.is_empty() {
        sidekick
            .fusion
            .lock_or_recover()
            .whitelist
            .extend(additions.iter().cloned());
        audit(
            ctx,
            "fusion.whitelist",
            &format!(
                "+{}: {}",
                additions.len(),
                additions
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .await;
    }
    let (seq, hash) = {
        let mut f = ctx.fusion.lock_or_recover();
        f.spec_seq += 1;
        // a steer-only call has no new spec — the steer text fingerprints it
        (
            f.spec_seq,
            spec_hash(
                &a.spec
                    .clone()
                    .unwrap_or_else(|| json!({ "steer": a.steer })),
            ),
        )
    };
    {
        let mut log = ctx.sessions.lock().await;
        log.append_audit(&SessionEvent::FusionSpec {
            seq,
            spec_hash: hash,
            spec: json!({
                "spec": a.spec,
                "steer": a.steer,
                "files": a.files,
                "verify_commands": a.verify_commands,
            }),
            sidekick: sub_id.clone(),
        })
        .await;
    }
    // the roster row reopens — a steer makes the child live again
    {
        let mut tasks = ctx.live_tasks.lock_or_recover();
        if let Some(e) = tasks.iter_mut().find(|t| t.id == sub_id) {
            e.done = None;
        }
    }
    crate::task::spawn::roster_changed(&ctx.live_sink.get().cloned(), &sub_id);
    let _roster = crate::task::spawn::RosterGuard::new(&ctx.live_tasks, &sub_id);
    let res = crate::task::spawn::run_spawn(
        sidekick,
        a.steer.clone().unwrap_or_default(),
        ctx.live_sink.get().cloned(),
        "fusion-rework",
    )
    .await;
    crate::task::spawn::finish_task(&ctx.live_tasks, &sub_id, res.ok);
    crate::task::spawn::roster_changed(&ctx.live_sink.get().cloned(), &sub_id);
    verdict::settle(ctx, seq, res).await
}

/// Audit fact on the LEAD's log + live sink — `gate::audit_fact` takes the
/// turn's observer, which a tool body doesn't see; the live sink is the
/// same row's mirror.
pub(super) async fn audit(ctx: &Arc<Context>, event: &str, detail: &str) {
    {
        let mut log = ctx.sessions.lock().await;
        log.append_audit(&SessionEvent::Hook {
            event: event.to_string(),
            detail: detail.to_string(),
        })
        .await;
    }
    if let Some(s) = ctx.live_sink.get() {
        s.on_event(&crate::agent::LiveEvent::Hook {
            event: event.to_string(),
            detail: detail.to_string(),
        });
    }
}

/// The Sidekick's opening user message — spec plus the two grant lists it
/// needs up front (its own system prompt already carries the contract).
fn sidekick_prompt(a: &Args) -> String {
    let spec = match a.spec.as_ref() {
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        None => String::new(),
    };
    let mut p = format!("# Spec\n{spec}");
    if !a.context_files.is_empty() {
        p.push_str(&format!(
            "\n\n# Read these files first (they carry the context the spec references)\n{}",
            a.context_files.join("\n")
        ));
    }
    if !a.files.is_empty() {
        p.push_str(&format!(
            "\n\n# Files you may write\n{}",
            a.files.join("\n")
        ));
    }
    if !a.verify_commands.is_empty() {
        p.push_str(&format!(
            "\n\n# Verify (run these yourself before finishing)\n{}",
            a.verify_commands
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    p
}

/// One-line digest for the roster row — spec objects digest their `goal`
/// (or whole JSON), strings their first line.
fn spec_digest(spec: &Value) -> String {
    match spec {
        Value::String(s) => s.lines().next().unwrap_or_default().to_string(),
        Value::Object(o) => o
            .get("goal")
            .and_then(|g| g.as_str())
            .map(String::from)
            .unwrap_or_else(|| spec.to_string()),
        other => other.to_string(),
    }
}

/// Every `files` entry resolved against the project root the same way the
/// gate's `whitelist_covers` resolves the child's Write/Edit paths —
/// non-existent targets resolve against their nearest canonical ancestor.
fn canonicalize_all(cwd: &Path, files: &[String]) -> Vec<PathBuf> {
    files
        .iter()
        .filter_map(|f| {
            let joined = cwd.join(f);
            joined.canonicalize().ok().or_else(|| {
                joined
                    .parent()
                    .and_then(|p| p.canonicalize().ok())
                    .map(|p| p.join(joined.file_name().unwrap_or_default()))
            })
        })
        .collect()
}

/// Stable spec fingerprint for the `FusionSpec` fact — FNV via the std
/// hasher; hex'd so accepted/escalated tie back without carrying the spec.
fn spec_hash(spec: &Value) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    spec.to_string().hash(&mut h);
    format!("{:016x}", h.finish())
}
