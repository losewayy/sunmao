//! `/channels` — the IM gateway's config + live status over the shared
//! route table (axum and the Tauri scheme both reach it). Reads write
//! nothing; `PUT` validates the JSON before persisting so a broken edit
//! can't take the daemon down at next launch.

use std::sync::Arc;

use super::HostResponse;
use crate::im::config;
use crate::im::store::Store;
use crate::serve::host::Shared;

/// `GET /channels` — `{config, status, pairing, allowlist}`:
/// the raw channels.json text (the settings page edits it verbatim), the
/// daemon's status.json heartbeat, and the admission ledger's two tables.
/// Missing pieces report null — a host without `sunmao im` running still
/// answers the page.
pub(super) async fn view() -> HostResponse {
    let config_text = std::fs::read_to_string(config::config_path()).ok();
    let status = std::fs::read_to_string(config::state_dir().join("status.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let store = Store::open(&config::state_dir());
    let (pairing, allowlist) = match &store {
        Ok(s) => (
            s.pairing_all()
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "code": r.code, "channel": r.channel,
                        "sender": r.sender, "expires": r.expires,
                    })
                })
                .collect::<Vec<_>>(),
            s.allow_list()
                .iter()
                .map(
                    |(ch, s2, role)| serde_json::json!({"channel": ch, "sender": s2, "role": role}),
                )
                .collect::<Vec<_>>(),
        ),
        Err(_) => (Vec::new(), Vec::new()),
    };
    HostResponse::json(serde_json::json!({
        "config": config_text,
        "status": status,
        "pairing": pairing,
        "allowlist": allowlist,
    }))
}

/// `PUT /channels` — `{config: <json text>}`. Parsed before the write:
/// the daemon reads this file at startup, so an invalid document saved
/// now is a dead daemon later. The gateway stays cold-plug — running
/// channels don't hot-reload.
pub(super) async fn put(body: &[u8]) -> HostResponse {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HostResponse::err(400, format!("bad json: {e}")),
    };
    let Some(text) = v["config"].as_str() else {
        return HostResponse::err(400, "missing config".into());
    };
    if let Err(e) = serde_json::from_str::<config::ChannelsConfig>(text) {
        return HostResponse::err(400, format!("channels.json invalid: {e}"));
    }
    let path = config::config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(&path, text) {
        Ok(()) => HostResponse::json(serde_json::json!({"ok": true})),
        Err(e) => HostResponse::err(500, format!("write {}: {e}", path.display())),
    }
}

/// `POST /channels/pairing/approve {code}` — the same admission the
/// `sunmao pairing approve` CLI runs, exposed for the settings page.
pub(super) async fn approve(_s: &Arc<Shared>, body: &[u8]) -> HostResponse {
    let code = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["code"].as_str().map(str::to_string))
        .unwrap_or_default();
    if code.is_empty() {
        return HostResponse::err(400, "missing code".into());
    }
    let Ok(store) = Store::open(&config::state_dir()) else {
        return HostResponse::err(500, "store unavailable".into());
    };
    match store.pairing_approve(&code) {
        Ok(Some((ch, sender, role))) => HostResponse::json(serde_json::json!({
            "ok": true, "channel": ch, "sender": sender, "role": role,
        })),
        Ok(None) => HostResponse::err(404, "no live pairing code".into()),
        Err(e) => HostResponse::err(500, format!("{e:#}")),
    }
}
