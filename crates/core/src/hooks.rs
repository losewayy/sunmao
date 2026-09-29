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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PreCompact,
    PostCompact,
    Stop,
    SessionEnd,
    SubagentStart,
    SubagentStop,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::Stop => "Stop",
            Self::SessionEnd => "SessionEnd",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
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
}

pub struct HookEngine {
    groups: HashMap<String, Vec<MatcherGroup>>,
    session_id: String,
    /// Session log path — payload field `transcript_path` (the dialect
    /// requires it to be a real file; ours is the JSONL event log).
    transcript_path: PathBuf,
}

impl HookEngine {
    /// Load hook groups from (later sources override/append):
    /// `<cwd>/.sunmao/hooks.json`, `<cwd>/.claude/settings.json`,
    /// `<cwd>/.claude/settings.local.json`, `~/.claude/settings.json`.
    /// All share the same `{"hooks": {Event: [{matcher, hooks:[{type,command}]}]}}`
    /// dialect — native contract, ecosystem configs work unmodified.
    pub fn load(cwd: &Path, session_id: &str) -> Self {
        let transcript_path = crate::session::session_log_path(cwd, session_id);
        let mut groups: HashMap<String, Vec<MatcherGroup>> = HashMap::new();
        let mut paths = vec![
            cwd.join(".sunmao").join("hooks.json"),
            cwd.join(".claude").join("settings.json"),
            cwd.join(".claude").join("settings.local.json"),
        ];
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            paths.push(Path::new(&home).join(".claude").join("settings.json"));
        }
        for path in paths {
            merge_hooks_file(&mut groups, &path, None);
        }
        // plugin manifests — a plugin dir bundles hooks/mcp/skills/commands;
        // we merge its hooks section here (mcp/skills handled by their loaders)
        for manifest in [
            cwd.join(".sunmao").join("plugin.json"),
            cwd.join(".claude-plugin").join("plugin.json"),
        ] {
            if let Some(root) = manifest.parent().map(|p| p.to_path_buf()) {
                merge_plugin_manifest(&mut groups, &manifest, &root);
            }
        }
        // plugin bundles installed under .sunmao/plugins/<name>/ and
        // .claude/plugins/<name>/: read their manifest plus the conventional
        // hooks/hooks.json sibling file (the real packaging format).
        for base in [
            cwd.join(".sunmao").join("plugins"),
            cwd.join(".claude").join("plugins"),
        ] {
            if let Ok(entries) = std::fs::read_dir(&base) {
                for entry in entries.flatten() {
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
        }
        if !groups.is_empty() {
            let total: usize = groups.values().map(|g| g.len()).sum();
            tracing::info!("hooks loaded: {total} matcher groups");
        }
        Self {
            groups,
            session_id: session_id.to_string(),
            transcript_path,
        }
    }

    /// Fire one lifecycle event.
    pub async fn fire(&self, event: HookEvent, cwd: &Path, input: &HookInput<'_>) -> HookOutcome {
        let Some(groups) = self.groups.get(event.as_str()) else {
            return HookOutcome::default();
        };
        let mut outcome = HookOutcome::default();
        for group in groups {
            if !matches(&group.matcher, input.tool_name.unwrap_or("")) {
                continue;
            }
            for hook in &group.hooks {
                if hook.kind != "command" {
                    continue;
                }
                let command = expand_plugin_root(&hook.command, hook.plugin_root.as_deref());
                tracing::debug!(event = event.as_str(), %command, "firing hook");
                let payload = json!({
                    "session_id": self.session_id,
                    "transcript_path": self.transcript_path.display().to_string().replace("\\\\?\\", ""),
                    "cwd": cwd.display().to_string().replace("\\\\?\\", ""),
                    "hook_event_name": event.as_str(),
                    "prompt": input.prompt,
                    "source": input.source,
                    "tool_name": input.tool_name,
                    "tool_use_id": input.tool_use_id,
                    "tool_input": input.tool_input,
                    "tool_response": input.tool_response,
                });
                match run_hook_command(&command, &payload, cwd).await {
                    Ok((code, stdout, stderr)) => {
                        tracing::debug!(
                            code,
                            stdout = &stdout[..stdout.len().min(512)],
                            stderr = &stderr[..stderr.len().min(256)],
                            "hook finished"
                        );
                        apply_result(code, &stdout, &stderr, &mut outcome);
                    }
                    Err(e) => {
                        tracing::warn!("hook failed to spawn: {e:#}");
                    }
                }
            }
        }
        outcome
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

/// Hook commands get the payload on stdin and run under the embedded shell,
/// same as the Bash tool — one execution model for all shell surfaces.
async fn run_hook_command(
    command: &str,
    payload: &Value,
    cwd: &Path,
) -> anyhow::Result<(i32, String, String)> {
    let command = command.to_string();
    let payload = serde_json::to_string(payload)?;
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || -> anyhow::Result<(i32, String, String)> {
        let list = deno_task_shell::parser::parse(&command)
            .map_err(|e| anyhow::anyhow!("bad hook command: {e}"))?;
        let env_vars: HashMap<std::ffi::OsString, std::ffi::OsString> =
            std::env::vars_os().collect();
        let state =
            deno_task_shell::ShellState::new(env_vars, cwd, Default::default(), Default::default());
        let (out_r, out_w) = deno_task_shell::pipe();
        let (err_r, err_w) = deno_task_shell::pipe();
        // stdin carries the JSON payload. A writer thread guards against
        // pipe-buffer backpressure on large PostToolUse payloads; join it
        // after exec — when write_all returns, in_w drops → child sees EOF.
        // (Detaching without join is what the old code did; on a slow
        // scheduler the EOF could arrive late, stalling stdin-blocking hooks.)
        let (in_r, mut in_w) = std::io::pipe()?;
        let feed_thread = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = in_w.write_all(payload.as_bytes());
        });
        let exec = deno_task_shell::execute_with_pipes(
            list,
            state,
            deno_task_shell::ShellPipeReader::from_raw(in_r),
            out_w,
            err_w,
        );
        let rt = tokio::runtime::Handle::current();
        let code = rt.block_on(exec);
        let _ = feed_thread.join();
        let mut out = Vec::new();
        let mut err = Vec::new();
        out_r.pipe_to(&mut out).ok();
        err_r.pipe_to(&mut err).ok();
        Ok((
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        ))
    })
    .await?
}

/// Dialect semantics: exit 2 = block (stderr is the reason); exit 0 + JSON
/// stdout may carry `decision`/`systemMessage`/`hookSpecificOutput.{additionalContext,
/// permissionDecision,updatedInput}`.
fn apply_result(code: i32, stdout: &str, stderr: &str, outcome: &mut HookOutcome) {
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
        if let Some(msg) = v.get("systemMessage").and_then(|m| m.as_str()) {
            outcome.extra_context.push(msg.to_string());
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
        // PreToolUse/PostToolUse "decision": "block" (older dialect spelling)
        if v.get("decision").and_then(|d| d.as_str()) == Some("block") {
            let reason = v
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("blocked by hook");
            outcome.block_reason = Some(reason.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher_semantics() {
        // literals + wildcards
        assert!(matches("", "Bash"));
        assert!(matches("*", "Read"));
        assert!(matches("Bash", "Bash"));
        assert!(!matches("Bash", "Read"));
        // regex alternation — the ecosystem's multi-tool matcher
        assert!(matches("Bash|Read", "Bash"));
        assert!(matches("Bash|Read", "Read"));
        assert!(!matches("Bash|Read", "Write"));
        // prefix match — the mcp__ wildcard convention
        assert!(matches("mcp__", "mcp__fs__read"));
        assert!(matches(
            "mcp__plugin_context-mode.*",
            "mcp__plugin_context-mode_x__y"
        ));
        assert!(!matches("mcp__", "Bash"));
    }

    #[test]
    fn exit2_blocks_with_stderr() {
        let mut o = HookOutcome::default();
        apply_result(2, "", "no deletes allowed", &mut o);
        assert_eq!(o.block_reason.as_deref(), Some("no deletes allowed"));
    }

    #[test]
    fn json_continue_false_blocks() {
        let mut o = HookOutcome::default();
        apply_result(0, r#"{"continue":false,"stopReason":"halt"}"#, "", &mut o);
        assert_eq!(o.block_reason.as_deref(), Some("halt"));
    }

    #[test]
    fn json_injects_context() {
        let mut o = HookOutcome::default();
        apply_result(
            0,
            r#"{"systemMessage":"hi","hookSpecificOutput":{"additionalContext":"ctx"}}"#,
            "",
            &mut o,
        );
        assert_eq!(o.extra_context, vec!["hi", "ctx"]);
        assert!(o.block_reason.is_none());
    }

    #[test]
    fn permission_decision_maps() {
        let mut o = HookOutcome::default();
        apply_result(
            0,
            r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#,
            "",
            &mut o,
        );
        assert_eq!(o.permission_decision, Some(HookPermission::Allow));
        assert!(o.block_reason.is_none());

        let mut o = HookOutcome::default();
        apply_result(
            0,
            r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#,
            "",
            &mut o,
        );
        assert_eq!(o.permission_decision, Some(HookPermission::Ask));

        let mut o = HookOutcome::default();
        apply_result(
            0,
            r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"nope"}}"#,
            "",
            &mut o,
        );
        assert_eq!(o.permission_decision, Some(HookPermission::Deny));
        assert_eq!(o.block_reason.as_deref(), Some("nope"));
    }

    #[test]
    fn updated_input_replaces_args() {
        let mut o = HookOutcome::default();
        apply_result(
            0,
            r#"{"hookSpecificOutput":{"updatedInput":{"command":"rtk git status"}}}"#,
            "",
            &mut o,
        );
        assert_eq!(
            o.updated_input
                .and_then(|v| v["command"].as_str().map(String::from)),
            Some("rtk git status".into())
        );
    }

    #[test]
    fn plugin_root_expands() {
        let root = Path::new("C:/plugins/cm");
        assert_eq!(
            expand_plugin_root(r#"node "${CLAUDE_PLUGIN_ROOT}/hooks/x.mjs""#, Some(root)),
            r#"node "C:/plugins/cm/hooks/x.mjs""#
        );
        assert_eq!(
            expand_plugin_root("echo hi", Some(root)),
            "echo hi",
            "no placeholder → untouched"
        );
        assert_eq!(
            expand_plugin_root("echo $CLAUDE_PLUGIN_ROOT", None),
            "echo $CLAUDE_PLUGIN_ROOT",
            "non-plugin command untouched"
        );
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    #[tokio::test]
    async fn pre_tool_use_hook_blocks_via_exit2() {
        let dir = std::env::temp_dir().join(format!("sunmao-hook-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
        std::fs::write(
            dir.join(".sunmao/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"cat > payload.json; echo nope >&2; exit 2"}]}]}}"#,
        )
        .unwrap();
        let engine = HookEngine::load(&dir, "test");
        let out = engine
            .fire(
                HookEvent::PreToolUse,
                &dir,
                &HookInput {
                    tool_name: Some("Bash"),
                    tool_input: Some(&json!({"command": "ls"})),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(out.block_reason.as_deref(), Some("nope"));
        // payload landed on the hook's stdin — including the dialect fields
        let payload: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("payload.json")).unwrap())
                .unwrap();
        assert_eq!(payload["hook_event_name"], "PreToolUse");
        assert_eq!(payload["tool_name"], "Bash");
        assert!(payload["transcript_path"]
            .as_str()
            .unwrap()
            .ends_with(".jsonl"));
    }
}
