//! The fire pipeline — split into a sync *plan* half and an async *run*
//! half so advisory events can execute detached (`fire_detached`) without
//! pinning borrows past the caller's stack frame.
//!
//! Which events detach is a caller contract, not an engine decision:
//! events whose `HookOutcome` is never read (Notification,
//! PermissionDenied, PostToolUseFailure, Interrupt) must not let a slow
//! hook process hold the approval card or the tool_result hostage.
//! Everything whose outcome flows back into the loop — PreToolUse veto,
//! UserPromptSubmit injection, PostToolUse context, the permissionDecision
//! verdict — still awaits every hook before returning.
//!
//! Trust pinning is enforced at plan time (`hooks/trust.rs`): an
//! unpinned project/plugin command never becomes a PlannedHook — there is
//! no path from config file to exec that skips the gate. The skip itself
//! is a `hook.untrusted` audit fact (durable row + live ⚙ line + tracing),
//! so "the hook that didn't run" is reconstructible from the log alone.

use std::path::Path;

use serde_json::Value;

use super::dialect::apply_result;
use super::exec::run_hook_command;
use super::{HookEngine, HookEvent, HookInput, HookOutcome, cursor, expand_plugin_root, matches};
use crate::context::RwLockRecover;

/// One frozen hook execution — the payload is already dialect-shaped JSON,
/// `command` already plugin-root-expanded. `run_planned` consumes these.
pub(super) struct PlannedHook {
    command: String,
    timeout: Option<u64>,
    payload: Value,
    /// reply normalization dialect — cursor's snake_case stdout folds into
    /// the Claude shape `apply_result` reads.
    normalize: cursor::Dialect,
}

/// A command the trust gate refused — the audit trail's `detail` half is
/// assembled once here so the durable row and the live line say the same
/// thing.
struct SkippedHook {
    detail: String,
}

impl HookEngine {
    /// One command hook frozen for async execution — resolves the command
    /// string and serializes the dialect payload up front, so the detached
    /// path never borrows `HookInput` past the call frame. The trust gate
    /// lives here: `command_trusted` is the ONLY check, and an unpinned
    /// command returns a skip record instead of a plan — no second code
    /// path can reach exec without passing it.
    fn planned_hooks(
        &self,
        event: HookEvent,
        cwd: &Path,
        input: &HookInput<'_>,
    ) -> (Vec<PlannedHook>, Vec<SkippedHook>) {
        let base = self.payload(event, cwd, input);
        let mut planned = Vec::new();
        let mut skipped = Vec::new();
        let Some(groups) = self.groups.get(event.as_str()) else {
            return (planned, skipped);
        };
        for group in groups {
            // cursor matchers filter on THEIR tool names (Shell, MCP:<t>)
            // — everything else matches the native name.
            let match_name = match group.dialect {
                cursor::Dialect::Cursor => cursor::cursor_tool_name(input.tool_name.unwrap_or("")),
                cursor::Dialect::Claude => input.tool_name.unwrap_or("").to_string(),
            };
            if !matches(&group.matcher, &match_name) {
                continue;
            }
            for hook in &group.hooks {
                if hook.kind != "command" {
                    continue;
                }
                if !self.command_trusted(hook) {
                    skipped.push(SkippedHook {
                        detail: format!(
                            "{}: {} (from {})",
                            event.as_str(),
                            hook.command,
                            hook.origin.display().to_string().replace("\\\\?\\", "")
                        ),
                    });
                    continue;
                }
                // cursor commands read a cursor-shaped payload — their
                // event name, their tool names, their extra fields.
                let hook_payload = match hook.dialect {
                    cursor::Dialect::Cursor => cursor::cursor_payload(
                        &hook.event_name,
                        &self.session_id.read_or_recover().clone(),
                        &self
                            .transcript_path
                            .read_or_recover()
                            .display()
                            .to_string()
                            .replace("\\\\?\\", ""),
                        &cwd.display().to_string().replace("\\\\?\\", ""),
                        input,
                        &match_name,
                    ),
                    cursor::Dialect::Claude => base.clone(),
                };
                planned.push(PlannedHook {
                    command: expand_plugin_root(&hook.command, hook.plugin_root.as_deref()),
                    timeout: hook.timeout,
                    payload: hook_payload,
                    normalize: hook.dialect,
                });
            }
        }
        (planned, skipped)
    }

    /// One audit fact per skip: durable row (the log is the source of
    /// truth — `--dataflow` and replay see it), live ⚙ line for the
    /// frontend that installed a sink, tracing for stderr-only runs.
    async fn audit_skips(&self, skipped: &[SkippedHook]) {
        for s in skipped {
            tracing::warn!("untrusted hook skipped: {}", s.detail);
            if let Some(log) = &self.sessions {
                log.lock()
                    .await
                    .append_audit(&crate::session::SessionEvent::Hook {
                        event: "hook.untrusted".into(),
                        detail: s.detail.clone(),
                    })
                    .await;
            }
            if let Some(sink) = self.live.read_or_recover().as_ref() {
                sink.on_event(&crate::agent::LiveEvent::Hook {
                    event: "hook.untrusted".into(),
                    detail: s.detail.clone(),
                });
            }
        }
    }

    /// Run a frozen plan — the async half both `fire` and `fire_detached`
    /// share. `base` feeds extension children (identical dialect payload).
    /// `skipped` is emitted first so the audit row precedes whatever the
    /// trusted hooks do.
    async fn run_planned(
        &self,
        event: HookEvent,
        cwd: &Path,
        planned: &[PlannedHook],
        skipped: &[SkippedHook],
        base: Value,
        outcome: &mut HookOutcome,
    ) {
        self.audit_skips(skipped).await;
        for hook in planned {
            tracing::debug!(event = event.as_str(), command = %hook.command, "firing hook");
            match run_hook_command(&hook.command, &hook.payload, cwd, hook.timeout).await {
                Ok((code, stdout, stderr)) => {
                    tracing::debug!(
                        code,
                        stdout = &stdout[..stdout.len().min(512)],
                        stderr = &stderr[..stderr.len().min(256)],
                        "hook finished"
                    );
                    // cursor replies normalize into the dialect
                    // apply_result already parses.
                    let stdout = match hook.normalize {
                        cursor::Dialect::Cursor => cursor::normalize_reply(&stdout),
                        cursor::Dialect::Claude => stdout,
                    };
                    apply_result(code, &stdout, &stderr, outcome);
                }
                Err(e) => {
                    tracing::warn!("hook failed to spawn: {e:#}");
                }
            }
        }
        // extension children run after every command hook for the event —
        // same payload, same outcome, replies carry the same effect shape.
        if let Some(ext) = &self.ext {
            ext.fire_event(event.as_str(), &base, outcome).await;
        }
    }

    /// Fire one lifecycle event and await every hook — outcomes fold into
    /// the returned `HookOutcome` (gates, injections, vetoes).
    pub async fn fire(&self, event: HookEvent, cwd: &Path, input: &HookInput<'_>) -> HookOutcome {
        let (planned, skipped) = self.planned_hooks(event, cwd, input);
        let base = self.payload(event, cwd, input);
        let mut outcome = HookOutcome::default();
        self.run_planned(event, cwd, &planned, &skipped, base, &mut outcome)
            .await;
        outcome
    }

    /// Advisory-event sibling of `fire`: plan on the caller's stack, run
    /// detached. Ordering note: the task runs concurrently with whatever
    /// follows, so advisory deliveries may interleave with the next
    /// event's hooks — hooks must not assume a strict happens-before
    /// against the kernel.
    ///
    /// No-ops (nothing planned AND no extensions) return false instead of
    /// spawning — untrusted skips still earn the spawn since their audit
    /// row lands inside `run_planned`. Off-runtime callers return false
    /// too — detached fire needs a tokio handle to land anywhere.
    pub fn fire_detached(
        engine: &std::sync::Arc<Self>,
        event: HookEvent,
        cwd: &Path,
        input: &HookInput<'_>,
    ) -> bool {
        let (planned, skipped) = engine.planned_hooks(event, cwd, input);
        if planned.is_empty() && skipped.is_empty() && engine.ext.is_none() {
            return false;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        let base = engine.payload(event, cwd, input);
        let cwd = cwd.to_path_buf();
        let me = engine.clone();
        handle.spawn(async move {
            let mut outcome = HookOutcome::default();
            me.run_planned(event, &cwd, &planned, &skipped, base, &mut outcome)
                .await;
        });
        true
    }
}
