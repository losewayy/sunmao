//! The Cursor hook dialect — the second lifecycle contract we consume.
//!
//! Cursor's `hooks.json` looks almost like the Claude shape but diverges on
//! three axes (verified against cursor.com/docs/agent/hooks):
//!
//! - **config shape**: event arrays hold flat command entries
//!   `{"command": ".cursor/hooks/x.sh", "matcher": "Shell"}` — no nested
//!   `hooks: []` group (the Claude layout has `{matcher, hooks:[{type,command}]}`).
//! - **event names**: camelCase (`preToolUse`, `sessionStart`, …). We map the
//!   eight that have a native counterpart; the rest warn-and-skip at load.
//! - **reply keys**: snake_case. `permission` (allow/ask/deny) replaces
//!   `hookSpecificOutput.permissionDecision`; `updated_input` and
//!   `additional_context` are the flat forms; `continue:false` + `user_message`
//!   carries a block; `followup_message` folds into context.
//!
//! Matchers run against CURSOR tool names — `Shell` not `Bash`, `MCP:<tool>`
//! not `mcp__<srv>__<tool>` — so a cursor entry's matching happens against
//! the mapped name, while its stdin payload also spells the mapped name
//! (cursor plugins must see their own dialect, not ours).

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{HookCommand, MatcherGroup};

/// Which dialect a matcher group/command was loaded from — replies and
/// matcher names only make sense inside the dialect that declared them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Dialect {
    /// `{"hooks": {Event: [{matcher, hooks: [{type, command}]}]}}` — Claude /
    /// Codex / our own `.sunmao` shape.
    #[default]
    Claude,
    /// Cursor `hooks.json` — flat entries, camelCase events, snake replies.
    Cursor,
}

/// Cursor event → our HookEvent name (its canonical PascalCase form). The
/// cursor-only surface (beforeShellExecution, afterFileEdit, beforeMCP*,
/// Tab hooks, workspaceOpen, afterAgent*) has no native event — those entries
/// are skipped at load, not silently mis-fired on the wrong point.
pub(crate) fn native_event_name(cursor_event: &str) -> Option<&'static str> {
    Some(match cursor_event {
        "sessionStart" => "SessionStart",
        "sessionEnd" => "SessionEnd",
        "preToolUse" => "PreToolUse",
        "postToolUse" => "PostToolUse",
        "subagentStart" => "SubagentStart",
        "subagentStop" => "SubagentStop",
        "beforeSubmitPrompt" => "UserPromptSubmit",
        "preCompact" => "PreCompact",
        "stop" => "Stop",
        _ => return None,
    })
}

/// Our tool name spelled the cursor way — `preToolUse` matchers in cursor
/// configs filter on `Shell`/`MCP:<tool>` etc. Both the group match and the
/// stdin `tool_name` field use this.
pub(crate) fn cursor_tool_name(native: &str) -> String {
    match native {
        "Bash" => "Shell".into(),
        "Edit" => "Write".into(),
        other if other.starts_with("mcp__") => {
            let short = other.rsplit("__").next().unwrap_or(other);
            format!("MCP:{short}")
        }
        other => other.to_string(),
    }
}

#[derive(Deserialize)]
struct CursorEntry {
    command: String,
    #[serde(default)]
    matcher: String,
    /// Cursor supports a per-entry timeout (seconds); we honor it — a hook
    /// asking for less budget than the global 60s gets it.
    #[serde(default)]
    timeout: Option<u64>,
}

/// Parse a cursor `hooks.json` and merge groups under native event names.
/// `{"version":1, "hooks": {"preToolUse": [{"command": ".cursor/x.sh"}]}}`.
/// Each flat entry becomes its own matcher group (cursor semantics: the
/// matcher lives ON the command, there is no group layer).
pub(crate) fn merge_cursor_file(
    groups: &mut HashMap<String, Vec<MatcherGroup>>,
    path: &Path,
    plugin_root: Option<&Path>,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(file) = serde_json::from_str::<serde_json::Value>(&text) else {
        tracing::warn!("bad cursor hooks file {}", path.display());
        return;
    };
    let Some(hooks) = file.get("hooks").and_then(|h| h.as_object()) else {
        return;
    };
    for (cursor_event, entries) in hooks {
        let Some(native) = native_event_name(cursor_event) else {
            tracing::debug!("cursor hook event {cursor_event} has no native counterpart — skipped");
            continue;
        };
        let Ok(list) = serde_json::from_value::<Vec<CursorEntry>>(entries.clone()) else {
            tracing::warn!("bad {cursor_event} entries in {}", path.display());
            continue;
        };
        for e in list {
            groups
                .entry(native.to_string())
                .or_default()
                .push(MatcherGroup {
                    matcher: e.matcher,
                    dialect: Dialect::Cursor,
                    hooks: vec![HookCommand {
                        kind: "command".into(),
                        command: e.command,
                        plugin_root: plugin_root.map(|p| p.to_path_buf()),
                        dialect: Dialect::Cursor,
                        event_name: cursor_event.clone(),
                        timeout: e.timeout,
                    }],
                });
        }
    }
}

/// stdin payload spelled the cursor way — `hook_event_name` keeps the
/// camelCase name, cursor tools are named cursor-style, `conversation_id`
/// and `workspace_roots` join the shared fields a cursor script expects.
pub(crate) fn cursor_payload(
    cursor_event: &str,
    session_id: &str,
    transcript_path: &str,
    cwd: &str,
    input: &super::HookInput<'_>,
    tool_name: &str,
) -> Value {
    json!({
        "hook_event_name": cursor_event,
        "conversation_id": session_id,
        "session_id": session_id,
        "transcript_path": transcript_path,
        "cwd": cwd,
        "workspace_roots": [cwd],
        "prompt": input.prompt,
        "source": input.source,
        "tool_name": tool_name,
        "tool_use_id": input.tool_use_id,
        "tool_input": input.tool_input,
        "tool_response": input.tool_response,
    })
}

/// Fold a cursor reply into the Claude spelling `apply_result` already
/// reads — one code path downstream, two dialects upstream.
pub(crate) fn normalize_reply(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string(); // not JSON — downstream treats it as inert
    };
    if !v.is_object() {
        return raw.to_string();
    }
    let mut out = json!({});
    // permission: allow/ask/deny → the verdict channel; user_message and
    // agent_message are both reasons worth surfacing (user sees the first,
    // the model sees the second — our one channel carries both).
    if let Some(p) = v.get("permission").and_then(|x| x.as_str()) {
        let reason = [
            v.get("user_message").and_then(|x| x.as_str()),
            v.get("agent_message").and_then(|x| x.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" — ");
        out["hookSpecificOutput"]["permissionDecision"] = json!(p);
        if !reason.is_empty() {
            out["hookSpecificOutput"]["permissionDecisionReason"] = json!(reason);
        }
    }
    if let Some(ui) = v.get("updated_input") {
        out["hookSpecificOutput"]["updatedInput"] = ui.clone();
    }
    if let Some(ac) = v.get("additional_context").and_then(|x| x.as_str()) {
        out["hookSpecificOutput"]["additionalContext"] = json!(ac);
    }
    if v.get("continue").and_then(|c| c.as_bool()) == Some(false) {
        out["continue"] = json!(false);
        if let Some(um) = v.get("user_message").and_then(|x| x.as_str()) {
            out["stopReason"] = json!(um);
        }
    }
    // stop/subagentStop loop mechanism — the nearest native semantic is
    // context injection (the model reads it next turn).
    if let Some(f) = v.get("followup_message").and_then(|x| x.as_str()) {
        out["hookSpecificOutput"]["additionalContext"] = json!(f);
    }
    out.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_map_covers_the_shared_surface() {
        assert_eq!(native_event_name("preToolUse"), Some("PreToolUse"));
        assert_eq!(native_event_name("sessionStart"), Some("SessionStart"));
        assert_eq!(
            native_event_name("beforeSubmitPrompt"),
            Some("UserPromptSubmit")
        );
        assert_eq!(native_event_name("preCompact"), Some("PreCompact"));
        assert_eq!(native_event_name("stop"), Some("Stop"));
        // cursor-only events have no counterpart — skipped, not mis-fired
        assert_eq!(native_event_name("beforeShellExecution"), None);
        assert_eq!(native_event_name("afterFileEdit"), None);
        assert_eq!(native_event_name("workspaceOpen"), None);
    }

    #[test]
    fn tool_names_map_to_cursor_vocabulary() {
        assert_eq!(cursor_tool_name("Bash"), "Shell");
        assert_eq!(cursor_tool_name("Edit"), "Write");
        assert_eq!(cursor_tool_name("Read"), "Read");
        assert_eq!(cursor_tool_name("mcp__fs__read_file"), "MCP:read_file");
        assert_eq!(cursor_tool_name("Task"), "Task");
    }

    #[test]
    fn reply_permission_and_messages_fold() {
        let norm: Value = serde_json::from_str(&normalize_reply(
            r#"{"permission":"deny","user_message":"no","agent_message":"policy hit"}"#,
        ))
        .unwrap();
        assert_eq!(norm["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            norm["hookSpecificOutput"]["permissionDecisionReason"],
            "no — policy hit"
        );
    }

    #[test]
    fn reply_rewrite_and_context_fold() {
        let norm: Value = serde_json::from_str(&normalize_reply(
            r#"{"updated_input":{"command":"rtk ls"},"additional_context":"warm","permission":"allow"}"#,
        ))
        .unwrap();
        assert_eq!(
            norm["hookSpecificOutput"]["updatedInput"]["command"],
            "rtk ls"
        );
        assert_eq!(norm["hookSpecificOutput"]["additionalContext"], "warm");
        assert_eq!(norm["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn reply_continue_false_blocks() {
        let norm: Value = serde_json::from_str(&normalize_reply(
            r#"{"continue":false,"user_message":"halt"}"#,
        ))
        .unwrap();
        assert_eq!(norm["continue"], false);
        assert_eq!(norm["stopReason"], "halt");
    }

    #[test]
    fn reply_followup_becomes_context() {
        let norm: Value =
            serde_json::from_str(&normalize_reply(r#"{"followup_message":"keep going"}"#)).unwrap();
        assert_eq!(
            norm["hookSpecificOutput"]["additionalContext"],
            "keep going"
        );
    }

    #[test]
    fn non_json_passes_through() {
        assert_eq!(normalize_reply("plain noise"), "plain noise");
    }
}
