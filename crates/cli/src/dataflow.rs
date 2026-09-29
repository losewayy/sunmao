//! `sunmao dataflow` — fold a session log into a "what data went where" report.
//!
//! The audit-native promise: the event log is the only source of truth, so the
//! report is a pure fold — no instrumentation needed anywhere else.

use std::path::Path;

use serde_json::{json, Value};
use sunmao_core::SessionEvent;
use tokio::io::AsyncBufReadExt;

pub async fn report(session_path: &Path) -> anyhow::Result<Value> {
    let file = tokio::fs::File::open(session_path).await?;
    let mut lines = tokio::io::BufReader::new(file).lines();

    let mut files_read = Vec::<String>::new();
    let mut files_written = Vec::<String>::new();
    let mut shell_commands = Vec::<String>::new();
    let mut tool_calls: Vec<(String, bool)> = Vec::new();
    let mut messages = 0usize;
    let mut compactions = 0usize;
    let mut total_prompt = 0u64;
    let mut total_completion = 0u64;

    while let Some(line) = lines.next_line().await? {
        let Ok(ev) = serde_json::from_str::<SessionEvent>(&line) else {
            continue;
        };
        match ev {
            SessionEvent::Message { .. } => messages += 1,
            SessionEvent::Compacted { .. } => compactions += 1,
            SessionEvent::Started { .. }
            | SessionEvent::ToolCall { .. }
            | SessionEvent::Artifact { .. }
            | SessionEvent::Hook { .. } => {}
            SessionEvent::LocalShell { command, .. } => {
                shell_commands.push(command);
            }
            SessionEvent::Usage { usage } => {
                total_prompt += usage.prompt_tokens;
                total_completion += usage.completion_tokens;
            }
            SessionEvent::ToolResult {
                name, ok, output, ..
            } => {
                tool_calls.push((name.clone(), ok));
                // attribute dataflow sinks/sources from tool inputs where known
                let _ = output;
            }
        }
    }

    // second pass on raw lines for tool-call arguments (ToolCall events carry
    // them but ToolResult doesn't — join by call id would need a map; v0.1
    // report lists them by shape)
    let file = tokio::fs::File::open(session_path).await?;
    let mut lines = tokio::io::BufReader::new(file).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v["type"] == "tool_call" {
            let name = v["call"]["function"]["name"].as_str().unwrap_or("");
            let args: Value =
                serde_json::from_str(v["call"]["function"]["arguments"].as_str().unwrap_or("{}"))
                    .unwrap_or(json!({}));
            match name {
                "Read" => files_read.push(s(&args, "path")),
                "Write" | "Edit" => files_written.push(s(&args, "path")),
                "Bash" => shell_commands.push(s(&args, "command")),
                _ => {}
            }
        }
    }

    Ok(json!({
        "session": session_path.display().to_string(),
        "messages": messages,
        "tool_calls": tool_calls.len(),
        "tool_failures": tool_calls.iter().filter(|(_, ok)| !ok).count(),
        "compactions": compactions,
        "tokens": {
            "prompt": total_prompt,
            "completion": total_completion,
            "total": total_prompt + total_completion,
        },
        "data_flow": {
            "files_read": files_read,
            "files_written": files_written,
            "shell_commands": shell_commands,
        }
    }))
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("?").to_string()
}
