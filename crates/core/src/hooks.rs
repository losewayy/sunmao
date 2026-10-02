//! Hook dispatcher — speaks the dominant lifecycle dialect natively.
//!
//! Config: `.sunmao/hooks.json` in the cwd (same shape as Claude settings):
//! ```json
//! {"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "..."}]}]}}
//! ```
//!
//! Contract per hook command: JSON payload on stdin
//! (`{session_id, transcript_path, cwd, hook_event_name, prompt?, tool_name?,
//! tool_use_id?, tool_input?, tool_response?}`), JSON decision on stdout;
//! exit code 2 = block with stderr as the reason.
//!
//! Stdout protocol (Claude dialect): `continue:false`+`stopReason`,
//! `systemMessage`, `hookSpecificOutput.{additionalContext,permissionDecision,
//! permissionDecisionReason,updatedInput}`, legacy `decision:"block"`.
//! `permissionDecision` maps onto the dispatch gate: `deny` blocks,
//! `allow` skips the approval prompt, `ask` forces one.
//! `updatedInput` (PreToolUse) replaces the tool arguments before dispatch.
//!
//! Matchers follow the ecosystem rule: empty/`*` match all, a valid regex is
//! matched by search (`Bash|Read`, `mcp__` prefix both work), an invalid
//! pattern degrades to literal substring match.
//!
//! Commands run through the embedded POSIX shell — identical on Windows.
//! `${CLAUDE_PLUGIN_ROOT}` in plugin-bundled commands expands to the plugin
//! directory the hook was loaded from.
//!
//! Second dialect on board: **Cursor** (`hooks/cursor.rs`) — `.cursor/hooks.json`
//! flat entries, camelCase events, `permission`/`updated_input`/`additional_context`
//! replies normalize into the same `HookOutcome`. Codex's `.codex/hooks.json`
//! rides the Claude path (identical file shape) and needs no normalization.

use crate::context::RwLockRecover;
mod cursor;
mod dialect;
mod exec;
mod fire;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) use dialect::apply_ext_reply;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    /// A tool call ended in a failed result (deny, error, crash) — fired
    /// after PostToolUse so failure listeners see the settled outcome.
    PostToolUseFailure,
    PreCompact,
    PostCompact,
    Stop,
    /// The turn aborted on an error (stream failure, hook veto abort) —
    /// fired instead of a clean Stop when the outcome wasn't normal.
    StopFailure,
    /// The user interrupted a running turn (Esc / cancel button / ACP
    /// cancel). Fired detached from `AgentLoop::cancel` — its outcome is
    /// never observed, so the interrupt itself can't be delayed or vetoed;
    /// the event exists for audit logs and cleanup scripts that must see
    /// interruptions. Fires only while a kernel turn holds `turn_lock` —
    /// cancelling an idle session is a no-op and earns no event.
    Interrupt,
    SessionEnd,
    SubagentStart,
    SubagentStop,
    /// The loop wants user attention — an approval prompt fired. Hooks
    /// can relay it (desktop notify, sound); the outcome is advisory only.
    Notification,
    /// A refusal settled — deny rule, hook veto, or a card answered "no".
    /// Fired after the verdict so listeners see the finished refusal; a
    /// decision-capable prompt event is `PreToolUse` (its
    /// `permissionDecision` IS the PermissionRequest surface — this one
    /// is observability only).
    PermissionDenied,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::Interrupt => "Interrupt",
            Self::SessionEnd => "SessionEnd",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::Notification => "Notification",
            Self::PermissionDenied => "PermissionDenied",
        }
    }
}

/// Permission verdict a hook's `permissionDecision` field hands to the
/// dispatch gate (`hookSpecificOutput.permissionDecision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPermission {
    /// Skip the approval prompt (still subject to deny rules).
    Allow,
    /// Force the approval prompt.
    Ask,
    /// Hard block — reason travels in `HookOutcome::block_reason`.
    Deny,
}

/// Per-event input — grouped because the payload surface keeps growing with
/// the dialect (prompt for UserPromptSubmit, tool_use_id for tool events).
/// Borrowed fields are fine: `plan()` freezes them into owned JSON payloads
/// before any async boundary, so detached fires never carry this struct.
#[derive(Debug, Default)]
pub struct HookInput<'a> {
    /// The user's prompt text (UserPromptSubmit payload field `prompt`).
    pub prompt: Option<&'a str>,
    /// Session lifecycle qualifier (`startup`/`resume`/`clear`/`compact`).
    pub source: Option<&'a str>,
    /// Tool name being gated (PreToolUse/PostToolUse).
    pub tool_name: Option<&'a str>,
    /// Provider-assigned call id (payload field `tool_use_id`).
    pub tool_use_id: Option<&'a str>,
    /// The tool's arguments — pre-hook value; hooks see what the model sent.
    pub tool_input: Option<&'a Value>,
    /// Serialized tool output (PostToolUse).
    pub tool_response: Option<&'a str>,
    /// Connected MCP server names (SessionStart payload field
    /// `mcp_servers`) — owned because the caller builds the list off
    /// `ctx.mcp_servers`, not a borrowable field.
    pub mcp_servers: Option<Vec<String>>,
}

/// Aggregated effect of all hooks fired for one event.
#[derive(Debug, Default)]
pub struct HookOutcome {
    /// Some(reason) → the action is blocked; reason goes back to the model.
    pub block_reason: Option<String>,
    /// Extra context to inject into the transcript (systemMessage /
    /// additionalContext from hook stdout JSON).
    pub extra_context: Vec<String>,
    /// Last `permissionDecision` verdict seen (deny > ask > allow ordering
    /// is resolved by the caller — later hooks may override earlier ones).
    pub permission_decision: Option<HookPermission>,
    /// Replacement tool arguments (PreToolUse `updatedInput`). Only the
    /// last hook that sets it wins; callers apply it before dispatch.
    pub updated_input: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: HashMap<String, Vec<MatcherGroup>>,
}

#[derive(Debug, Deserialize)]
struct MatcherGroup {
    /// Regex or literal tool-name matcher; "*" / "" match everything.
    #[serde(default)]
    matcher: String,
    #[serde(default)]
    hooks: Vec<HookCommand>,
    /// Which file dialect built this group — cursor flat entries are
    /// single-command groups tagged at load (claude default).
    #[serde(skip)]
    dialect: cursor::Dialect,
}

#[derive(Debug, Deserialize)]
struct HookCommand {
    #[serde(rename = "type")]
    kind: String,
    command: String,
    /// Plugin dir this command was loaded from — source of truth for
    /// `${CLAUDE_PLUGIN_ROOT}` expansion. None for non-plugin sources.
    #[serde(skip)]
    plugin_root: Option<PathBuf>,
    /// Dialect tag drives payload shaping + reply normalization.
    #[serde(skip)]
    dialect: cursor::Dialect,
    /// The dialect-native event name the config wrote (`preToolUse` for
    /// cursor — payload's hook_event_name must echo it verbatim).
    #[serde(skip)]
    event_name: String,
    /// Per-command timeout in seconds (cursor's per-entry field). Falls
    /// back to the global budget when unset.
    #[serde(skip)]
    timeout: Option<u64>,
}

pub struct HookEngine {
    groups: HashMap<String, Vec<MatcherGroup>>,
    /// Session id feeding every payload — RwLock because `/resume`
    /// swaps the live log under a shared `Arc<Context>`; hooks must name
    /// the *current* session, not the one the engine loaded against.
    session_id: std::sync::RwLock<String>,
    /// Session log path — payload field `transcript_path` (the dialect
    /// requires it to be a real file; ours is the JSONL event log).
    transcript_path: std::sync::RwLock<PathBuf>,
    /// Extension children attached after `load` — they fire *after*
    /// command hooks in the same event and fold into the same outcome.
    ext: Option<std::sync::Arc<crate::ext::ExtRegistry>>,
}

impl HookEngine {
    /// Load hook groups from (later sources override/append):
    /// `<cwd>/.sunmao/hooks.json`, `<cwd>/.claude/settings.json`,
    /// `<cwd>/.claude/settings.local.json`, `~/.claude/settings.json`.
    /// All share the same `{"hooks": {Event: [{matcher, hooks:[{type,command}]}]}}`
    /// dialect — native contract, ecosystem configs work unmodified.
    /// `extra_roots` are preset plugin dirs — they merge last so an enabled
    /// preset's hooks run after everything the project itself declared.
    ///
    /// Two more dialects ride alongside: **Codex** (`.codex/hooks.json`, same
    /// file shape — a rtk `init --codex` bundle works verbatim) and **Cursor**
    /// (`.cursor/hooks.json` + `~/.cursor/hooks.json`, flat entries parsed by
    /// `cursor::merge_cursor_file`; replies normalize into `HookOutcome`).
    pub fn load(cwd: &Path, session_id: &str, extra_roots: &[PathBuf]) -> Self {
        let transcript_path = crate::session::session_log_path(cwd, session_id);
        let mut groups: HashMap<String, Vec<MatcherGroup>> = HashMap::new();
        let mut paths = vec![
            cwd.join(".sunmao").join("hooks.json"),
            cwd.join(".codex").join("hooks.json"),
            cwd.join(".claude").join("settings.json"),
            cwd.join(".claude").join("settings.local.json"),
        ];
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            paths.push(Path::new(&home).join(".claude").join("settings.json"));
            paths.push(Path::new(&home).join(".codex").join("hooks.json"));
        }
        for path in paths {
            merge_hooks_file(&mut groups, &path, None);
        }
        // cursor files keep their own parser — flat {command, matcher}
        // entries tagged Dialect::Cursor so payloads/replies normalize.
        cursor::merge_cursor_file(&mut groups, &cwd.join(".cursor").join("hooks.json"), None);
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            cursor::merge_cursor_file(
                &mut groups,
                &Path::new(&home).join(".cursor").join("hooks.json"),
                None,
            );
        }
        // plugin manifests — a plugin dir bundles hooks/mcp/skills/commands;
        // we merge its hooks section here (mcp/skills handled by their loaders)
        for manifest in [
            cwd.join(".sunmao").join("plugin.json"),
            cwd.join(".claude-plugin").join("plugin.json"),
            // "this project is a plugin" — same treatment as a top manifest
            cwd.join(".sunmao").join("plugin").join("plugin.json"),
        ] {
            if let Some(root) = manifest.parent().map(|p| p.to_path_buf()) {
                merge_plugin_manifest(&mut groups, &manifest, &root);
            }
        }
        // the project-plugin's conventional hooks file rides alongside its
        // manifest, exactly like an installed bundle's
        {
            let root = cwd.join(".sunmao").join("plugin");
            merge_hooks_file(
                &mut groups,
                &root.join("hooks").join("hooks.json"),
                Some(&root),
            );
        }
        // plugin bundles installed under .sunmao/plugins/<name>/ and
        // .claude/plugins/<name>/: read their manifest plus the conventional
        // hooks/hooks.json sibling file (the real packaging format).
        for base in [
            cwd.join(".sunmao").join("plugins"),
            cwd.join(".claude").join("plugins"),
        ] {
            for entry in crate::sorted_entries(&base) {
                let root = entry.path();
                if !root.is_dir() {
                    continue;
                }
                merge_plugin_manifest(&mut groups, &root.join("plugin.json"), &root);
                merge_hooks_file(
                    &mut groups,
                    &root.join("hooks").join("hooks.json"),
                    Some(&root),
                );
            }
        }
        // preset dirs are plugin bundles too — appended last in CLI layering
        // order so `--preset a --preset b` runs b's hooks after a's.
        for root in extra_roots {
            merge_plugin_manifest(&mut groups, &root.join("plugin.json"), root);
            merge_hooks_file(
                &mut groups,
                &root.join("hooks").join("hooks.json"),
                Some(root),
            );
        }
        if !groups.is_empty() {
            let total: usize = groups.values().map(|g| g.len()).sum();
            tracing::info!("hooks loaded: {total} matcher groups");
        }
        Self {
            groups,
            session_id: std::sync::RwLock::new(session_id.to_string()),
            transcript_path: std::sync::RwLock::new(transcript_path),
            ext: None,
        }
    }

    /// Point the payload's `session_id`/`transcript_path` at a new log —
    /// `/resume` swaps the session under a shared context, and hooks
    /// must name the live session from the next fire on.
    pub fn retarget(&self, session_id: &str, transcript_path: PathBuf) {
        *self.session_id.write_or_recover() = session_id.to_string();
        *self.transcript_path.write_or_recover() = transcript_path;
    }

    /// Attach the session's extension registry — `ext/event` requests then
    /// deliver inside `fire` after command hooks, folding into the same
    /// `HookOutcome` (extensions see the identical dialect payload).
    pub fn attach_ext(&mut self, exts: std::sync::Arc<crate::ext::ExtRegistry>) {
        self.ext = Some(exts);
    }

    /// The dialect payload both channels share — command hooks read it
    /// from stdin, extensions get it as `ext/event`'s `payload` param.
    fn payload(&self, event: HookEvent, cwd: &Path, input: &HookInput<'_>) -> Value {
        json!({
            "session_id": self.session_id.read_or_recover().clone(),
            "transcript_path": self.transcript_path.read_or_recover().display().to_string().replace("\\\\?\\", ""),
            "cwd": cwd.display().to_string().replace("\\\\?\\", ""),
            "hook_event_name": event.as_str(),
            "prompt": input.prompt,
            "source": input.source,
            "tool_name": input.tool_name,
            "tool_use_id": input.tool_use_id,
            "tool_input": input.tool_input,
            "tool_response": input.tool_response,
            "mcp_servers": input.mcp_servers,
        })
    }
}

/// Merge a `{"hooks": {...}}`-shaped file into the group map. `plugin_root`
/// stamps commands loaded from a plugin bundle so `${CLAUDE_PLUGIN_ROOT}`
/// can expand at fire time.
fn merge_hooks_file(
    groups: &mut HashMap<String, Vec<MatcherGroup>>,
    path: &Path,
    plugin_root: Option<&Path>,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    // the flat {"matcher","command"} shape is a silent footgun — serde
    // defaults swallow it into a group with zero commands. Catch it
    // before the structured parse so a mis-shaped file warns instead of
    // loading nothing.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(events) = v.get("hooks").and_then(|h| h.as_object())
    {
        for (event, groups) in events {
            if let Some(gs) = groups.as_array() {
                for g in gs {
                    if g.get("command").is_some() && g.get("hooks").is_none() {
                        tracing::warn!(
                            "{}: '{event}' uses flat {{matcher,command}} — wrap it: \
                                 {{matcher, hooks:[{{type:\"command\", command:...}}]}}",
                            path.display()
                        );
                    }
                }
            }
        }
    }
    let Ok(file) = serde_json::from_str::<HooksFile>(&text) else {
        tracing::warn!("bad hooks file {}", path.display());
        return;
    };
    for (event, mut gs) in file.hooks {
        if let Some(root) = plugin_root {
            for g in &mut gs {
                for h in &mut g.hooks {
                    h.plugin_root = Some(root.to_path_buf());
                }
            }
        }
        groups.entry(event).or_default().append(&mut gs);
    }
}

/// Merge a plugin manifest's inline `hooks` section. A plugin is a directory
/// containing `plugin.json` — the bundle format both Claude and sunmao speak;
/// `${CLAUDE_PLUGIN_ROOT}` resolves to that directory.
fn merge_plugin_manifest(
    groups: &mut HashMap<String, Vec<MatcherGroup>>,
    manifest: &Path,
    root: &Path,
) {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return;
    };
    let Ok(file) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    if let Some(hooks) = file
        .get("hooks")
        .and_then(|h| serde_json::from_value::<HashMap<String, Vec<MatcherGroup>>>(h.clone()).ok())
    {
        for (event, mut gs) in hooks {
            for g in &mut gs {
                for h in &mut g.hooks {
                    h.plugin_root = Some(root.to_path_buf());
                }
            }
            groups.entry(event).or_default().append(&mut gs);
        }
    }
}

/// Expand `${CLAUDE_PLUGIN_ROOT}` (and the `$CLAUDE_PLUGIN_ROOT` shorthand)
/// in a hook command — the variable plugin-bundled hooks use to locate
/// their own scripts. Non-plugin commands pass through untouched.
fn expand_plugin_root(command: &str, plugin_root: Option<&Path>) -> String {
    let Some(root) = plugin_root else {
        return command.to_string();
    };
    // canonicalize() on Windows yields `\\?\F:\…` verbatim paths — they
    // poison every downstream consumer (node realpath, CreateProcess),
    // so strip the prefix before substituting.
    let root = root.display().to_string().replace("\\\\?\\", "");
    command
        .replace("${CLAUDE_PLUGIN_ROOT}", &root)
        .replace("$CLAUDE_PLUGIN_ROOT", &root)
}

/// Ecosystem matcher semantics: a valid regex is searched against the tool
/// name (`Bash|Read`, `mcp__` prefix, `Write|Edit` all hit); anything else
/// falls back to literal substring match — a plain tool name still matches
/// itself exactly since it's a substring of itself.
fn matches(matcher: &str, tool_name: &str) -> bool {
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    match regex::Regex::new(matcher) {
        Ok(re) => re.is_match(tool_name),
        Err(_) => tool_name.contains(matcher),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod live_tests;
