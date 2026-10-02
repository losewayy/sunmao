//! `sunmao dataflow` — fold a session log into a "what data went where" report.
//!
//! The audit-native promise: the event log is the only source of truth, so the
//! report is a pure fold — no instrumentation needed anywhere else.

use std::path::Path;

use serde_json::{Value, json};
use sunmao_core::SessionEvent;
use tokio::io::AsyncBufReadExt;

pub async fn report(session_path: &Path) -> anyhow::Result<Value> {
    let file = tokio::fs::File::open(session_path).await?;
    let mut lines = tokio::io::BufReader::new(file).lines();

    let mut files_read = Vec::<String>::new();
    let mut files_written = Vec::<String>::new();
    let mut shell_commands = Vec::<String>::new();
    let mut uplinks = Vec::<String>::new();
    let mut downlinks = Vec::<String>::new();
    let mut tool_calls: Vec<(String, bool)> = Vec::new();
    let mut messages = 0usize;
    let mut compactions = 0usize;
    let mut total_prompt = 0u64;
    let mut total_completion = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;

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
            | SessionEvent::TaskDone { .. }
            | SessionEvent::Todos { .. }
            | SessionEvent::Goal { .. }
            | SessionEvent::ModeChange { .. }
            | SessionEvent::SessionMeta { .. }
            | SessionEvent::Checkpoint { .. }
            | SessionEvent::Hook { .. } => {}
            SessionEvent::LocalShell { command, .. } => {
                shell_commands.push(command);
            }
            SessionEvent::Usage { usage } => {
                total_prompt += usage.prompt_tokens;
                total_completion += usage.completion_tokens;
                cache_read += usage.cache_read_input_tokens;
                cache_write += usage.cache_creation_input_tokens;
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
                "SendMessage" => uplinks.push(s(&args, "message")),
                "Task" => {
                    // a Task call carrying `steer` is a parent→child push —
                    // the spawn fields live on a different arm
                    if args["steer"].is_string() {
                        downlinks.push(format!(
                            "steer→{}: {}",
                            s(&args, "steer"),
                            s(&args, "message")
                        ));
                    } else if args["resume"].is_string() {
                        downlinks.push(format!("resume→{}", s(&args, "resume")));
                    }
                }
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
            // the cost dial: what share of input came from the provider's
            // cache instead of full-price compute. Usage.prompt_tokens is
            // already the normalized total (cached reads folded in — see
            // llm::Usage's Deserialize), so the share divides by it alone.
            "cache_read": cache_read,
            "cache_write": cache_write,
            "cache_hit_pct": cache_read
                .checked_mul(100)
                .and_then(|n| n.checked_div(total_prompt))
                .unwrap_or(0),
        },
        "data_flow": {
            "files_read": files_read,
            "files_written": files_written,
            "shell_commands": shell_commands,
            // agent↔agent channel traffic — SendMessage is the child's
            // uplink text, steer/resume the parent's downlink
            "sub_agent_uplinks": uplinks,
            "sub_agent_downlinks": downlinks,
        }
    }))
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("?").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_lines(dir: &Path, name: &str, lines: &[&str]) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

    #[tokio::test]
    async fn report_attributes_uplink_and_downlink() {
        let dir = std::env::temp_dir().join(format!("sunmao-df-{}", std::process::id()));
        let log = write_lines(
            &dir,
            "s.jsonl",
            &[
                r#"{"type":"tool_call","call":{"id":"c1","function":{"name":"Task","arguments":"{\"steer\":\"sub-1-l1\",\"message\":\"go\"}"}},"depth":0}"#,
                r#"{"type":"tool_call","call":{"id":"c2","function":{"name":"Task","arguments":"{\"resume\":\"sub-2-l2\"}"}},"depth":0}"#,
                r#"{"type":"tool_call","call":{"id":"c3","function":{"name":"Task","arguments":"{\"prompt\":\"spawn\"}"}},"depth":0}"#,
                r#"{"type":"tool_call","call":{"id":"c4","function":{"name":"SendMessage","arguments":"{\"message\":\"hello parent\"}"}},"depth":0}"#,
            ],
        );
        let out = report(&log).await.unwrap();
        let df = &out["data_flow"];
        assert_eq!(
            df["sub_agent_downlinks"],
            json!(["steer→sub-1-l1: go", "resume→sub-2-l2"])
        );
        assert_eq!(df["sub_agent_uplinks"], json!(["hello parent"]));
        // a plain spawn (prompt only) contributes neither an up- nor a
        // downlink — it's not channel traffic
        let _ = std::fs::remove_dir_all(&dir);
    }
}
