//! Hook dispatcher — speaks the dominant lifecycle dialect natively.
//!
//! Config: `.sunmao/hooks.json` in the cwd (same shape as Claude settings):
//! ```json
//! {"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "..."}]}]}}
//! ```
//!
//! Contract per hook command: JSON payload on stdin
//! (`{session_id, cwd, hook_event_name, tool_name?, tool_input?, tool_response?}`),
//! JSON decision on stdout; exit code 2 = block with stderr as the reason.
//! Commands run through the embedded POSIX shell — identical on Windows.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
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
            Self::Stop => "Stop",
            Self::SessionEnd => "SessionEnd",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
        }
    }
}

/// Aggregated effect of all hooks fired for one event.
#[derive(Debug, Default)]
pub struct HookOutcome {
    /// Some(reason) → the action is blocked; reason goes back to the model.
    pub block_reason: Option<String>,
    /// Extra context to inject into the transcript (systemMessage /
    /// additionalContext from hook stdout JSON).
    pub extra_context: Vec<String>,
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
}

pub struct HookEngine {
    groups: HashMap<String, Vec<MatcherGroup>>,
    session_id: String,
}

impl HookEngine {
    /// Load hook groups from (later sources override/append):
    /// `<cwd>/.sunmao/hooks.json`, `<cwd>/.claude/settings.json`,
    /// `<cwd>/.claude/settings.local.json`, `~/.claude/settings.json`.
    /// All share the same `{"hooks": {Event: [{matcher, hooks:[{type,command}]}]}}`
    /// dialect — native contract, ecosystem configs work unmodified.
    pub fn load(cwd: &Path, session_id: &str) -> Self {
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
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(file) = serde_json::from_str::<HooksFile>(&text) else {
                tracing::warn!("bad hooks file {}", path.display());
                continue;
            };
            for (event, mut gs) in file.hooks {
                groups.entry(event).or_default().append(&mut gs);
            }
        }
        // plugin manifests — a plugin dir bundles hooks/mcp/skills/commands;
        // we merge its hooks section here (mcp/skills handled by their loaders)
        for manifest in [
            cwd.join(".sunmao").join("plugin.json"),
            cwd.join(".claude-plugin").join("plugin.json"),
        ] {
            if let Ok(text) = std::fs::read_to_string(&manifest) {
                merge_plugin_groups(&mut groups, &text);
            }
        }
        if !groups.is_empty() {
            let total: usize = groups.values().map(|g| g.len()).sum();
            tracing::info!("hooks loaded: {total} matcher groups");
        }
        Self {
            groups,
            session_id: session_id.to_string(),
        }
    }

    /// Fire one lifecycle event. `tool_name`/`tool_input`/`tool_response` are
    /// populated for PreToolUse/PostToolUse.
    pub async fn fire(
        &self,
        event: HookEvent,
        cwd: &Path,
        tool_name: Option<&str>,
        tool_input: Option<&Value>,
        tool_response: Option<&str>,
    ) -> HookOutcome {
        let Some(groups) = self.groups.get(event.as_str()) else {
            return HookOutcome::default();
        };
        let mut outcome = HookOutcome::default();
        for group in groups {
            if !matches(group.matcher.as_str(), tool_name.unwrap_or("")) {
                continue;
            }
            for hook in &group.hooks {
                if hook.kind != "command" {
                    continue;
                }
                let payload = json!({
                    "session_id": self.session_id,
                    "cwd": cwd.display().to_string(),
                    "hook_event_name": event.as_str(),
                    "tool_name": tool_name,
                    "tool_input": tool_input,
                    "tool_response": tool_response,
                });
                match run_hook_command(&hook.command, &payload, cwd).await {
                    Ok((code, stdout, stderr)) => {
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

fn matches(matcher: &str, tool_name: &str) -> bool {
    matcher.is_empty() || matcher == "*" || matcher == tool_name
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
        // stdin carries the JSON payload
        let (in_r, mut in_w) = std::io::pipe()?;
        let feed = payload.clone();
        let feed_thread = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = in_w.write_all(feed.as_bytes());
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
        drop(feed_thread);
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
/// stdout may carry `decision`/`systemMessage`/`hookSpecificOutput.additionalContext`.
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
        assert!(matches("", "Bash"));
        assert!(matches("*", "Read"));
        assert!(matches("Bash", "Bash"));
        assert!(!matches("Bash", "Read"));
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
                Some("Bash"),
                Some(&json!({"command": "ls"})),
                None,
            )
            .await;
        assert_eq!(out.block_reason.as_deref(), Some("nope"));
        // payload landed on the hook's stdin
        let payload: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("payload.json")).unwrap())
                .unwrap();
        assert_eq!(payload["hook_event_name"], "PreToolUse");
        assert_eq!(payload["tool_name"], "Bash");
    }
}

/// Merge a plugin manifest's `hooks` section into the engine. A plugin is a
/// directory containing `plugin.json` — the bundle format both Claude and
/// sunmao speak; paths inside are relative to the plugin dir.
fn merge_plugin_groups(groups: &mut HashMap<String, Vec<MatcherGroup>>, text: &str) {
    let Ok(file) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    if let Some(hooks) = file
        .get("hooks")
        .and_then(|h| serde_json::from_value::<HashMap<String, Vec<MatcherGroup>>>(h.clone()).ok())
    {
        for (event, mut gs) in hooks {
            groups.entry(event).or_default().append(&mut gs);
        }
    }
}
