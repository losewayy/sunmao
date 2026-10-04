//! Reply-side dialect handling — every channel that can produce a hook
//! effect funnels through here. `apply_result` parses the *Claude* spelling
//! (what `.sunmao`/`.claude`/`.codex` hook commands write); cursor replies
//! are rewritten into that spelling by `cursor::normalize_reply` upstream.
//! `apply_ext_reply` folds `ext/event` replies, which carry the same effects
//! under contract spellings.

use serde_json::Value;

use super::{HookOutcome, HookPermission};

/// Extension reply folding — `ext/event` replies carry the same effects a
/// hook can produce, with contract spellings: `block` (a reason string →
/// block_reason), `extra_context` (string or list of them), `updatedInput`
/// (PreToolUse rewrite), `permissionDecision` (same verdicts as the
/// hook-specific channel). Non-object replies and junk fields drop quietly.
pub(crate) fn apply_ext_reply(reply: &Value, outcome: &mut HookOutcome) {
    if let Some(reason) = reply.get("block").and_then(|b| b.as_str()) {
        outcome.block_reason = Some(reason.to_string());
    }
    match reply.get("extra_context") {
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    outcome.extra_context.push(s.to_string());
                }
            }
        }
        Some(Value::String(s)) => outcome.extra_context.push(s.clone()),
        _ => {}
    }
    if let Some(updated) = reply.get("updatedInput") {
        outcome.updated_input = Some(updated.clone());
    }
    match reply.get("permissionDecision").and_then(|d| d.as_str()) {
        Some("deny") => {
            outcome.permission_decision = Some(HookPermission::Deny);
            // double-latch like the command dialect: permission_decision
            // is last-writer-wins across sequential replies, block_reason
            // is not — a later child's `allow` must not erase a deny
            let reason = reply
                .get("permissionDecisionReason")
                .and_then(|r| r.as_str())
                .unwrap_or("denied by extension");
            outcome.block_reason = Some(reason.to_string());
        }
        Some("ask") => outcome.permission_decision = Some(HookPermission::Ask),
        Some("allow") => outcome.permission_decision = Some(HookPermission::Allow),
        _ => {}
    }
}

/// Dialect semantics: exit 2 = block (stderr is the reason); exit 0 + JSON
/// stdout may carry `decision`/`systemMessage`/`hookSpecificOutput.{additionalContext,
/// permissionDecision,updatedInput}`.
pub(super) fn apply_result(code: i32, stdout: &str, stderr: &str, outcome: &mut HookOutcome) {
    if code == 2 {
        let reason = stderr.trim();
        outcome.block_reason = Some(if reason.is_empty() {
            "blocked by hook".into()
        } else {
            reason.to_string()
        });
        return;
    }
    if code != 0 {
        return; // non-zero non-2: hook error, not a block
    }
    let text = stdout.trim();
    if text.is_empty() {
        return;
    }
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if v.get("continue").and_then(|c| c.as_bool()) == Some(false) {
            outcome.block_reason = Some(
                v.get("stopReason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("stopped by hook")
                    .to_string(),
            );
        }
        // `systemMessage` is a USER-facing warning in the Claude dialect,
        // not model context — it lands on the notices channel, which
        // frontends render as a warning line instead of folding it into
        // the transcript as `[hook context]`.
        if let Some(msg) = v.get("systemMessage").and_then(|m| m.as_str()) {
            outcome.notices.push(msg.to_string());
        }
        if let Some(ctx) = v
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(|c| c.as_str())
        {
            outcome.extra_context.push(ctx.to_string());
        }
        // hookSpecificOutput.permissionDecision — the dialect's verdict channel.
        // deny overrides everything else a hook can say.
        match v
            .pointer("/hookSpecificOutput/permissionDecision")
            .and_then(|d| d.as_str())
        {
            Some("deny") => {
                outcome.permission_decision = Some(HookPermission::Deny);
                let reason = v
                    .pointer("/hookSpecificOutput/permissionDecisionReason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("denied by hook");
                outcome.block_reason = Some(reason.to_string());
            }
            Some("ask") => outcome.permission_decision = Some(HookPermission::Ask),
            Some("allow") => outcome.permission_decision = Some(HookPermission::Allow),
            _ => {}
        }
        // PreToolUse input rewrite — the hook replaces the tool arguments
        // wholesale (rtk's command-rewrite mechanism depends on this).
        if let Some(updated) = v.pointer("/hookSpecificOutput/updatedInput") {
            outcome.updated_input = Some(updated.clone());
        }
        // PreToolUse/PostToolUse "decision" — block refuses (older
        // spelling); "approve" is a permission allowance, same verdict
        // channel as permissionDecision's `allow`
        match v.get("decision").and_then(|d| d.as_str()) {
            Some("block") => {
                let reason = v
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("blocked by hook");
                outcome.block_reason = Some(reason.to_string());
            }
            Some("approve") => outcome.permission_decision = Some(HookPermission::Allow),
            _ => {}
        }
    }
}
