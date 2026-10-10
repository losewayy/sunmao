//! Project-local remote MCP config editor. The GUI writes JSON only;
//! a running host is not reconfigured and no process is spawned.

use super::super::host::Shared;
use super::HostResponse;
use std::path::Path;
use std::sync::Arc;

fn path(s: &Arc<Shared>, sess: Option<String>) -> std::path::PathBuf {
    super::ops::project_for(s, sess).join(".sunmao/mcp.json")
}

fn read(path: &Path) -> Result<serde_json::Value, HostResponse> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::json!({"mcpServers":{}}));
        }
        Err(e) => return Err(HostResponse::err(500, format!("{e}"))),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| HostResponse::err(409, format!("invalid existing mcp.json: {e}")))?;
    if !value.is_object() || value.get("mcpServers").is_some_and(|v| !v.is_object()) {
        return Err(HostResponse::err(
            409,
            "mcp.json requires mcpServers object".into(),
        ));
    }
    Ok(value)
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn valid_url(input: &str) -> bool {
    if input.len() > 2048 {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(input) else {
        return false;
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    match url.scheme() {
        "https" => url.host_str().is_some(),
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    }
}

pub(super) fn view(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let value = match read(&path(s, sess)) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let configured = value["mcpServers"].as_object().map(|m| m.iter().map(|(name,spec)| {
        serde_json::json!({"name":name,"transport":if spec.get("url").is_some() {"http"} else {"stdio"}})
    }).collect::<Vec<_>>()).unwrap_or_default();
    HostResponse::json(serde_json::json!({"configured":configured,"effective_on":"new_session"}))
}

pub(super) fn put(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    if body.len() > 4096 {
        return HostResponse::err(413, "request too large".into());
    }
    let input: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HostResponse::err(400, format!("bad json: {e}")),
    };
    let Some(name) = input["name"].as_str() else {
        return HostResponse::err(400, "missing server name".into());
    };
    if !valid_name(name) {
        return HostResponse::err(400, "invalid server name".into());
    }
    let path = path(s, sess.clone());
    let mut value = match read(&path) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if value.get("mcpServers").is_none() {
        value["mcpServers"] = serde_json::json!({});
    }
    let Some(servers) = value["mcpServers"].as_object_mut() else {
        return HostResponse::err(409, "invalid server map".into());
    };
    if input["remove"].as_bool() == Some(true) {
        if servers.remove(name).is_none() {
            return HostResponse::err(404, "project server not found".into());
        }
    } else {
        let Some(url) = input["url"].as_str() else {
            return HostResponse::err(400, "missing URL".into());
        };
        if !valid_url(url) {
            return HostResponse::err(
                400,
                "use HTTPS or localhost HTTP, without embedded credentials or query tokens".into(),
            );
        }
        if servers.contains_key(name) {
            return HostResponse::err(
                409,
                "MCP server name exists; remove it before replacing".into(),
            );
        }
        if servers.len() >= 32 {
            return HostResponse::err(400, "too many MCP servers".into());
        }
        servers.insert(name.into(), serde_json::json!({"url":url}));
    }
    let Some(dir) = path.parent() else {
        return HostResponse::err(500, "invalid path".into());
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        return HostResponse::err(500, format!("{e}"));
    }
    let pretty = match serde_json::to_string_pretty(&value) {
        Ok(v) => v,
        Err(e) => return HostResponse::err(500, format!("{e}")),
    };
    if let Err(e) = std::fs::write(&path, pretty) {
        return HostResponse::err(500, format!("{e}"));
    }
    view(s, sess)
}

#[cfg(test)]
mod tests {
    use super::{valid_name, valid_url};
    #[test]
    fn project_mcp_editor_rejects_unsafe_inputs() {
        assert!(valid_name("my-mcp"));
        assert!(!valid_name("../other"));
        assert!(valid_url("https://example.com/mcp"));
        assert!(valid_url("http://localhost:3000/mcp"));
        assert!(!valid_url("http://example.com/mcp"));
        assert!(!valid_url("https://u:secret@example.com/mcp"));
        assert!(!valid_url("https://example.com/mcp?token=secret"));
    }
}
