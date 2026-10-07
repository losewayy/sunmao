//! The delegation verdict stage — shared by `delegate` and `rework`: the
//! Sidekick's run result plus the harness-executed verify commands settle
//! into accepted / failed / escalated (or inconclusive, an environment
//! verdict that burns no streak).

use std::sync::Arc;

use crate::context::{Context, MutexRecover};
use crate::session::SessionEvent;
use crate::tool::ToolResult;

use super::{ESCALATE_AFTER, audit};

/// Verify commands get the same default budget a `Bash` call would.
const VERIFY_TIMEOUT_SECS: u64 = 120;

/// The verdict stage both paths share: the Sidekick's run result + the
/// verify commands executed for real. Clean run + every exit code 0 →
/// `FusionAccepted` and ok; anything else counts toward the escalation
/// streak, and past `ESCALATE_AFTER` the Lead's write tools unlock for the
/// rest of the turn (`FusionEscalated` — the audit's "delegation gave up"
/// fact). A verify that couldn't run at all (environment fault) is
/// inconclusive: not accepted, but not counted either.
pub(super) async fn settle(
    ctx: &Arc<Context>,
    seq: u64,
    res: ToolResult,
) -> anyhow::Result<ToolResult> {
    let sidekick_id = ctx.fusion.lock_or_recover().sidekick_id.clone();
    let verifies = run_verifies(ctx).await;

    let mut out = String::new();
    out.push_str(&res.output);
    if !verifies.is_empty() {
        out.push_str("\n\n[verify — executed by the harness, not the Sidekick]");
        for (command, exit_code, detail) in &verifies {
            out.push_str(&format!("\n$ {command}\nexit {exit_code}"));
            if *exit_code != 0 {
                if inconclusive(*exit_code, detail) {
                    out.push_str("  [inconclusive — environment]");
                }
                out.push_str(&format!("\n{}", crate::agent::truncate_output(detail)));
            }
        }
    }
    // the verdict alone hid HOW the child spent its turn — a "the gate
    // blocked everything" claim had to be taken on faith
    out.push_str(&sidekick_trace(ctx).await);

    if res.ok && verifies.iter().all(|(_, code, _)| *code == 0) {
        ctx.fusion.lock_or_recover().verify_fails = 0;
        let mut log = ctx.sessions.lock().await;
        log.append_audit(&SessionEvent::FusionAccepted {
            spec_seq: seq,
            sidekick: sidekick_id.unwrap_or_default(),
        })
        .await;
        return Ok(ToolResult {
            exit_code: None,
            output: format!("[delegation accepted — verify clean]\n{out}"),
            ok: true,
        });
    }

    // a run that never resolved is an environment verdict, not a work
    // verdict — it neither accepts the delegation nor burns the streak
    // (an env fault must not point the Lead at code that was never the
    // problem); a failed RUN is a real failure regardless
    let real_fail = !res.ok
        || verifies
            .iter()
            .any(|(_, c, d)| *c != 0 && !inconclusive(*c, d));
    if !real_fail {
        out.push_str(
            "\n\n[verify inconclusive — the commands could not run (environment \
             fault, marked above). This does NOT count toward escalation. Fix \
             the environment or pass different verify_commands, then \
             FusionExecute with `steer` to retry the same Sidekick]",
        );
        return Ok(ToolResult {
            exit_code: None,
            output: out,
            ok: false,
        });
    }

    let (fails, escalate) = {
        let mut f = ctx.fusion.lock_or_recover();
        f.verify_fails += 1;
        (
            f.verify_fails,
            f.verify_fails >= ESCALATE_AFTER && !f.escalated,
        )
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
        exit_code: None,
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
            Some(ctx.cancel_signal()),
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

/// exit -1 (the spawn never resolved — parse error, lost worker) and the
/// classic "command could not start" shapes mean the ENVIRONMENT failed,
/// not the work. Classifying them as work failures burns the escalation
/// streak and nearly drives the Lead to rewrite code that was never the
/// problem. Two guards keep the label honest: a run that hit its timeout
/// is a WORK verdict (the delegated command hung — saying "environment"
/// would let the Lead steer a hang forever), and the free-text match is
/// restricted to the codes a missing-command produces (1 on pwsh, -1 on
/// spawn) so a real exit-2 failure whose output merely mentions the
/// phrases still counts.
fn inconclusive(code: i32, detail: &str) -> bool {
    if detail.contains("timed out") {
        return false;
    }
    code == -1
        || code == 127
        || code == 9009
        || (code == 1
            && [
                "could not execute process",
                "is not recognized",
                "command not found",
                "没有应用程序",
            ]
            .iter()
            .any(|t| detail.contains(t)))
}

/// A compact digest of the Sidekick's own tool calls appended to the
/// delegation result — `Read×3 · Bash×5 (2 failed)` plus the transcript
/// id so the Lead can audit the claim instead of trusting the report.
async fn sidekick_trace(ctx: &Arc<Context>) -> String {
    let (sub, sub_id) = {
        let f = ctx.fusion.lock_or_recover();
        match (&f.sidekick, &f.sidekick_id) {
            (Some(s), Some(id)) => (s.clone(), id.clone()),
            _ => return String::new(),
        }
    };
    let events = sub.sessions.lock().await.events().await.unwrap_or_default();
    let mut order: Vec<String> = Vec::new();
    let mut calls: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut fails: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for e in &events {
        match e {
            SessionEvent::ToolCall { call, .. } => {
                let n = &call.function.name;
                if !calls.contains_key(n) {
                    order.push(n.clone());
                }
                *calls.entry(n.clone()).or_default() += 1;
            }
            SessionEvent::ToolResult {
                name, ok: false, ..
            } => *fails.entry(name.clone()).or_default() += 1,
            _ => {}
        }
    }
    if order.is_empty() {
        return String::new();
    }
    let parts = order
        .iter()
        .map(|n| {
            let f = fails.get(n).copied().unwrap_or(0);
            if f > 0 {
                format!("{n}×{} ({f} failed)", calls[n])
            } else {
                format!("{n}×{}", calls[n])
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("\n\n[sidekick ran: {parts} — transcript: {sub_id}]")
}

#[cfg(test)]
mod tests {
    use super::inconclusive;

    #[test]
    fn inconclusive_covers_environment_faults_only() {
        // a spawn that never resolved, and the classic not-found codes
        assert!(inconclusive(-1, "could not execute process"));
        assert!(inconclusive(127, "bash: foo: command not found"));
        assert!(inconclusive(9009, "'foo' is not recognized"));
        assert!(inconclusive(
            1,
            "foo : The term 'foo' is not recognized as a cmdlet"
        ));
        // a hung command is a WORK verdict — the run resolved, the work
        // didn't finish; calling it "environment" would let the Lead
        // steer a hang forever without escalating
        assert!(!inconclusive(-1, "[timed out after 120s — killed]"));
        assert!(!inconclusive(
            -1,
            "partial output\n[timed out after 120s — killed]"
        ));
        // a real failure whose output merely mentions the phrases still
        // counts — the free-text match only applies to the codes a
        // missing command produces
        assert!(!inconclusive(
            2,
            "grep found 'command not found' in the log"
        ));
        assert!(!inconclusive(1, "test failed: 3 assertions"));
    }
}
