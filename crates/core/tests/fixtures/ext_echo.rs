//! Minimal `ext/*` protocol child — the Rust fixture that replaces the
//! old Node echo-ext for live registry tests. Compiled at test time by
//! `crates/core/src/ext/tests.rs`; not part of the library build.
//!
//! Speaks line-delimited JSON-RPC 2.0 over stdio:
//!   ext/initialize → {name, version, capabilities:{tools:true, events:[…]}}
//!   ext/tools/list → {tools:[{name,description,input_schema}]}
//!   ext/tools/call → {content, is_error}  (echoes arguments.msg)
//!   ext/event      → SessionStart: extra_context; PreToolUse: veto on "rm"
//!   ext/shutdown   → exit
//!
//! `--die` mode: answer initialize, exit immediately — the dead-child
//! fixture (post-exit requests must degrade, not hang).

use std::io::{BufRead, BufReader, Write};

fn main() {
    let die = std::env::args().any(|a| a == "--die");
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in BufReader::new(stdin.lock()).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let id = num_field(&line, "\"id\"");
        let method = str_field(&line, "\"method\"");
        let reply = match method.as_deref() {
            Some("ext/initialize") => {
                // --die reports no tools so the handshake ends here —
                // the child exits right after this reply.
                let caps = if die {
                    "{\"tools\":false,\"events\":[\"SessionStart\"]}"
                } else {
                    "{\"tools\":true,\"events\":[\"SessionStart\",\"PreToolUse\"]}"
                };
                Some(format!(
                    "{{\"name\":\"echo\",\"version\":\"0\",\"capabilities\":{caps}}}"
                ))
            }
            Some("ext/tools/list") => Some(
                "{\"tools\":[{\"name\":\"ping\",\"description\":\"echo the msg\",\"input_schema\":{\"type\":\"object\",\"properties\":{\"msg\":{\"type\":\"string\"}}}}]}"
                    .to_string(),
            ),
            Some("ext/tools/call") => {
                let msg = str_field(&line, "\"msg\"").unwrap_or_default();
                Some(format!("{{\"content\":\"echo: {msg}\"}}"))
            }
            Some("ext/event") => {
                if line.contains("\"SessionStart\"") {
                    Some("{\"extra_context\":[\"warm\"]}".to_string())
                } else if line.contains("\"PreToolUse\"") {
                    let cmd = str_field(&line, "\"command\"").unwrap_or_default();
                    if cmd.contains("rm") {
                        Some("{\"block\":\"fake veto\"}".to_string())
                    } else {
                        Some("{}".to_string())
                    }
                } else {
                    Some("{}".to_string())
                }
            }
            Some("ext/shutdown") => std::process::exit(0),
            // unknown request: a correct peer still answers its id
            Some(_) => Some("{}".to_string()),
            // notifications (no id) and non-frames: nothing to answer
            None => None,
        };
        if let (Some(id), Some(result)) = (id, reply) {
            writeln!(
                out,
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}"
            )
            .unwrap();
            out.flush().unwrap();
        }
        if die && method.as_deref() == Some("ext/initialize") {
            std::process::exit(0);
        }
    }
}

/// `"key":"value"` → `value` — whitespace between colon and string is
/// legal JSON, so scan past it.
fn str_field(s: &str, key: &str) -> Option<String> {
    let i = s.find(key)? + key.len();
    let rest = s[i..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let v = rest.strip_prefix('"')?;
    let end = v.find('"')?;
    Some(v[..end].to_string())
}

/// `"key":N` → `N` (request ids are numbers, not strings)
fn num_field(s: &str, key: &str) -> Option<String> {
    let i = s.find(key)? + key.len();
    let rest = s[i..].trim_start().strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_string())
}
