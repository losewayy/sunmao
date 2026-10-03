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

/// Verify commands get the same default budget a `Bash` call would.
const VERIFY_TIMEOUT_SECS: u64 = 120;

/// The Lead's delegation tool — fusion's replacement for `Task`'s slot.
pub struct FusionExecuteTool;

#[derive(Deserialize)]
struct Args {
    /// The complete brief — object (`goal`/`context`/`constraints`…), a
    /// plain string, or a list of instructions. Serialized verbatim into
    /// the Sidekick's opening prompt; it sees nothing else.
    spec: Value,
    /// Files the Sidekick may Write/Edit — the whitelist the gate enforces.
    #[serde(default)]
    files: Vec<String>,
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
             toolset while you stay read-only. `spec` is the complete brief: the \
             Sidekick sees NOTHING of this conversation, so include goal, context \
             and constraints. `files` whitelists the paths it may Write/Edit \
             (add-only once granted). `verify_commands` run through the real \
             shell afterwards and their exit codes come back in this result — \
             they are execution facts, not the Sidekick's claim. On a failed \
             verdict, call again with `steer` (feedback text) to resume the SAME \
             Sidekick; new `files` entries widen — never shrink — the whitelist, \
             and new `verify_commands` replace the check list. After repeated \
             failures the delegation escalates: your write tools unlock so you \
             can finish the work yourself.",
            json!({
                "type": "object",
                "properties": {
                    "spec": {
                        "description": "Complete work brief — object (e.g. {goal, context, constraints}), plain text, or a list of instructions"
                    },
                    "files": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Files the Sidekick may write — whitelist enforced at the gate; add-only once granted"
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
                "required": ["spec"]
            }),
        )
    }

    async fn call(&self, args: Value, ctx: &Arc<Context>) -> anyhow::Result<ToolResult> {
        let a: Args = serde_json::from_value(args.clone())?;
        if a.steer.is_some() {
            rework(ctx, &a).await
        } else {
            delegate(ctx, &a).await
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
    let llm_override = match &a.model {
        None => None,
        Some(sel) => match ctx.models.as_ref() {
            Some(m) => m.adapter_for(sel).map(Some).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown model selector `{sel}` — available: {}",
                    m.describe().join(", ")
                )
            })?,
            None => anyhow::bail!("model selector `{sel}` needs .sunmao/models.json routes"),
        },
    };
    let sys_prompt = crate::prompt::PromptAssembler::new(&ctx.cwd)
        .with_extra_roots(&ctx.extra_plugin_roots)
        .assemble_fusion_sidekick();
    let (sub_id, sub_ctx) =
        crate::task::parts::spawn_parts(ctx, None, llm_override, Some(sys_prompt)).await;

    // The spec's file grant — canonicalized the way the gate's
    // whitelist_covers will resolve the child's Write/Edit paths. An
    // unresolvable entry just never matches (fail-closed by symmetry).
    let grant = canonicalize_all(&ctx.cwd, &a.files);
    sub_ctx.fusion.lock_or_recover().whitelist = grant.clone();
    let (seq, hash) = {
        let mut f = ctx.fusion.lock_or_recover();
        f.spec_seq += 1;
        f.whitelist = grant;
        f.verify = a.verify_commands.clone();
        (f.spec_seq, spec_hash(&a.spec))
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
    let cancel = crate::context::SubCancel::new(&sub_ctx);
    crate::task::spawn::register_task(
        ctx,
        &sub_id,
        sub_ctx.lane,
        &spec_digest(&a.spec),
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
    settle(ctx, seq, res).await
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
        (f.spec_seq, spec_hash(&a.spec))
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
    settle(ctx, seq, res).await
}

/// The verdict stage both paths share: the Sidekick's run result + the
/// verify commands executed for real. Clean run + every exit code 0 →
/// `FusionAccepted` and ok; anything else counts toward the escalation
/// streak, and past `ESCALATE_AFTER` the Lead's write tools unlock for the
/// rest of the turn (`FusionEscalated` — the audit's "delegation gave up"
/// fact).
async fn settle(ctx: &Arc<Context>, seq: u64, res: ToolResult) -> anyhow::Result<ToolResult> {
    let sidekick_id = ctx.fusion.lock_or_recover().sidekick_id.clone();
    let verifies = run_verifies(ctx).await;

    let mut out = String::new();
    out.push_str(&res.output);
    if !verifies.is_empty() {
        out.push_str("\n\n[verify — executed by the harness, not the Sidekick]");
        for (command, exit_code, detail) in &verifies {
            out.push_str(&format!("\n$ {command}\nexit {exit_code}"));
            if *exit_code != 0 {
                out.push_str(&format!("\n{}", crate::agent::truncate_output(detail)));
            }
        }
    }

    if res.ok && verifies.iter().all(|(_, code, _)| *code == 0) {
        ctx.fusion.lock_or_recover().verify_fails = 0;
        let mut log = ctx.sessions.lock().await;
        log.append_audit(&SessionEvent::FusionAccepted {
            spec_seq: seq,
            sidekick: sidekick_id.unwrap_or_default(),
        })
        .await;
        return Ok(ToolResult {
            output: format!("[delegation accepted — verify clean]\n{out}"),
            ok: true,
        });
    }

    let (fails, escalate) = {
        let mut f = ctx.fusion.lock_or_recover();
        f.verify_fails += 1;
        (f.verify_fails, f.verify_fails >= ESCALATE_AFTER && !f.escalated)
    };
    if escalate {
        {
            let mut f = ctx.fusion.lock_or_recover();
            f.escalated = true;
        }
        ctx.read_only
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let reason = format!("{fails} consecutive delegation(s) failed verify");
        {
            let mut log = ctx.sessions.lock().await;
            log.append_audit(&SessionEvent::FusionEscalated {
                spec_seq: seq,
                reason: reason.clone(),
            })
            .await;
        }
        audit(ctx, "fusion.escalated", &reason).await;
        out.push_str(&format!(
            "\n\n[delegation escalated: {reason} — your write tools are \
             unlocked for the rest of this turn; finish the work yourself and \
             re-run the verify commands]"
        ));
    } else {
        out.push_str(&format!(
            "\n\n[delegation failed verify ({fails}/{ESCALATE_AFTER} before \
             escalation) — call FusionExecute with `steer` to rework the same \
             Sidekick, or a fresh spec to respawn]"
        ));
    }
    Ok(ToolResult {
        output: out,
        ok: false,
    })
}

/// Run the delegation's verify commands through the real shell path —
/// `run_foreground` returns actual exit codes, so "verify passed" is an
/// execution fact. Each run lands in the Sidekick's own log as a
/// `LocalShell` fact: the rework transcript shows the command, the code and
/// the output exactly the way a `!` command would, and the audit trail
/// shows which checks the spec's verdict rested on.
/// `(command, exit_code, combined output)` — a run that never resolved
/// (parse error, timeout kill) counts as exit -1 with the message.
async fn run_verifies(ctx: &Arc<Context>) -> Vec<(String, i32, String)> {
    let (sidekick, commands) = {
        let f = ctx.fusion.lock_or_recover();
        (f.sidekick.clone(), f.verify.clone())
    };
    let Some(sub_ctx) = sidekick else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(commands.len());
    for command in commands {
        let (code, detail) = match crate::tool::run_foreground(
            &command,
            sub_ctx.cwd.clone(),
            VERIFY_TIMEOUT_SECS,
            sub_ctx.shell,
            Some(ctx.cancel_notify.clone()),
        )
        .await
        {
            Ok(run) => (run.exit_code, crate::tool::render_run(&run)),
            Err(msg) => (-1, msg),
        };
        {
            let mut log = sub_ctx.sessions.lock().await;
            log.append_audit(&SessionEvent::LocalShell {
                command: command.clone(),
                exit_code: code,
                output: detail.clone(),
            })
            .await;
        }
        out.push((command, code, detail));
    }
    out
}

/// Audit fact on the LEAD's log + live sink — `gate::audit_fact` takes the
/// turn's observer, which a tool body doesn't see; the live sink is the
/// same row's mirror.
async fn audit(ctx: &Arc<Context>, event: &str, detail: &str) {
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
    let spec = match &a.spec {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    let mut p = format!("# Spec\n{spec}");
    if !a.files.is_empty() {
        p.push_str(&format!("\n\n# Files you may write\n{}", a.files.join("\n")));
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
