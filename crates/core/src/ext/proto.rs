//! The wire's small vocabulary: one JSON object per line, `id` correlates
//! a reply to its request, `{"result": ...}` or `{"error": ...}` carries
//! the payload. No Content-Length headers — that is deliberately not the
//! contract (see PROTOCOLS.md "Frames").

use serde_json::{Value, json};

/// A request frame — `"id"` present means the peer must answer.
pub(crate) fn request_frame(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// A notification frame — no `id`, no reply expected.
pub(crate) fn notification_frame(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// One line → one frame. Blank lines and non-objects (a stray number the
/// child printed, BOM noise) are skipped, not errors.
pub(crate) fn parse_frame(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(line) {
        Ok(v) if v.is_object() => Some(v),
        _ => None,
    }
}

/// The `id` a reply correlates to — u64 only, other shapes can't match a
/// request we sent.
pub(crate) fn reply_id(frame: &Value) -> Option<u64> {
    frame.get("id").and_then(|i| i.as_u64())
}

/// Unwrap `result`, or fold `error` into the anyhow chain.
pub(crate) fn reply_result(frame: Value) -> anyhow::Result<Value> {
    if let Some(err) = frame.get("error") {
        anyhow::bail!("{}", err);
    }
    Ok(frame.get("result").cloned().unwrap_or(Value::Null))
}
