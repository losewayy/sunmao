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

impl HookEngine {
    /// One command hook frozen for async execution — resolves the command
    /// string and serializes the dialect payload up front, so the detached
    /// path never borrows `HookInput` past the call frame.
    fn planned_hooks(
        &self,
        event: HookEvent,
        cwd: &Path,
        input: &HookInput<'_>,
    ) -> Vec<PlannedHook> {
        let base = self.payload(event, cwd, input);
        let mut planned = Vec::new();
        let Some(groups) = self.groups.get(event.as_str()) else {
            return planned;
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
        planned
    }

    /// Run a frozen plan — the async half both `fire` and `fire_detached`
    /// share. `base` feeds extension children (identical dialect payload).
    async fn run_planned(
        &self,
        event: HookEvent,
        cwd: &Path,
        planned: &[PlannedHook],
        base: Value,
        outcome: &mut HookOutcome,
    ) {
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
        let planned = self.planned_hooks(event, cwd, input);
        let base = self.payload(event, cwd, input);
        let mut outcome = HookOutcome::default();
        self.run_planned(event, cwd, &planned, base, &mut outcome)
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
    /// spawning. Off-runtime callers return false too — detached fire
    /// needs a tokio handle to land anywhere.
    pub fn fire_detached(
        engine: &std::sync::Arc<Self>,
        event: HookEvent,
        cwd: &Path,
        input: &HookInput<'_>,
    ) -> bool {
        let planned = engine.planned_hooks(event, cwd, input);
        if planned.is_empty() && engine.ext.is_none() {
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
            me.run_planned(event, &cwd, &planned, base, &mut outcome)
                .await;
        });
        true
    }
}
