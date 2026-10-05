//! provider/model surface — the GUI settings page edits `.sunmao/models.json`
//! and the composer picker lists selectors, both through GET/PUT `/models`
//! (+ `POST /models/fetch`). Split out of `request.rs` to keep the route
//! table under the god-file budget.

use std::sync::Arc;

use super::super::host::{Host, Shared, effort_frame};
use super::HostResponse;

// ── provider/model surface (GUI settings page + composer picker) ──

/// The host to answer models/config questions for — `?sess=` picks a live
/// session's project; absent it, the newest live host; no live host → the
/// launch dir (settings still work on the bare file).
fn models_host(s: &Arc<Shared>, sess: Option<String>) -> Option<Arc<Host>> {
    if let Some(id) = sess
        && let Some(h) = s.host(&id)
    {
        return Some(h);
    }
    s.newest_live_id().and_then(|id| s.host(&id))
}

/// `GET /models?sess=` — providers (keys redacted) + routes + completable
/// selectors, everything the settings page and the composer picker need
/// in one shot. Anchored to the viewed session's project.
pub(super) async fn view(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let host = models_host(s, sess);
    let resolver = host.as_ref().and_then(|h| h.agent.models_resolver());
    let file = match resolver.as_ref() {
        Some(m) => m.file(),
        None => {
            // no live host — the bare file still answers (the session's
            // own `default` row is honestly absent: nothing is running)
            let text =
                std::fs::read_to_string(s.cwd.join(".sunmao/models.json")).unwrap_or_default();
            let mut f: sunmao_core::models::ModelsFile =
                serde_json::from_str(&text).unwrap_or_default();
            let knowledge = sunmao_core::model_knowledge::Knowledge::load(&s.cwd);
            for p in f.providers.values_mut() {
                for e in &mut p.catalog {
                    e.migrate_vision();
                    knowledge.fill(e);
                }
            }
            f
        }
    };
    HostResponse::json(serde_json::json!({
        "providers": providers_view(&file),
        "routes": file.routes,
        // the new-session pick — the settings page renders it and writes it
        // back through the same PUT (null = nothing pinned)
        "default_model": file.default_selector(),
        "selectors": resolver.as_ref().map(|m| m.selectors()).unwrap_or_default(),
        "default_provider": resolver.as_ref().map(|m| m.default_provider()).unwrap_or_else(|| "default".into()),
    }))
}

/// Serialize the provider table for the GUI — keys are redacted to a
/// `api_key_set` boolean; the settings editor writes keys, it never
/// reads them back.
fn providers_view(file: &sunmao_core::models::ModelsFile) -> serde_json::Value {
    file.providers
        .iter()
        .map(|(name, p)| {
            (
                name.clone(),
                serde_json::json!({
                    "base_url": p.base_url,
                    "dialect": p.dialect,
                    "api_key_env": p.api_key_env,
                    "api_key_set": p.api_key_env.is_some() || p.api_key.is_some(),
                    "catalog": p.catalog,
                }),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into()
}

/// `POST /models/fetch?sess= {provider}` — proxy the provider's own
/// `/models` listing. Body may instead carry an inline
/// `{"base_url","api_key",…}` for a provider the user is still typing
/// (not yet saved).
pub(super) async fn fetch(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HostResponse::err(400, format!("bad json: {e}")),
    };
    let def: sunmao_core::models::ProviderDef = if let Some(name) = v["provider"].as_str() {
        let resolver = models_host(s, sess.clone()).and_then(|h| h.agent.models_resolver());
        let file = resolver.as_ref().map(|m| m.file()).unwrap_or_else(|| {
            let text =
                std::fs::read_to_string(s.cwd.join(".sunmao/models.json")).unwrap_or_default();
            serde_json::from_str(&text).unwrap_or_default()
        });
        match file.providers.get(name) {
            Some(p) => p.clone(),
            None => return HostResponse::err(404, format!("no such provider: {name}")),
        }
    } else {
        match serde_json::from_value(v) {
            Ok(p) => p,
            Err(e) => {
                return HostResponse::err(400, format!("provider object or provider name: {e}"));
            }
        }
    };
    match sunmao_core::models::fetch_catalog(&def).await {
        Ok(mut catalog) => {
            // the listing is best-effort — the knowledge table fills
            // whatever it leaves unset (never overrides declared fields)
            let cwd = models_host(s, sess)
                .map(|h| h.agent.session_cwd())
                .unwrap_or_else(|| s.cwd.clone());
            let knowledge = sunmao_core::model_knowledge::Knowledge::load(&cwd);
            for e in &mut catalog {
                e.migrate_vision();
                knowledge.fill(e);
            }
            HostResponse::json(serde_json::json!({ "catalog": catalog }))
        }
        Err(e) => HostResponse::err(502, format!("{e:#}")),
    }
}

/// `PUT /models?sess=` — replace `<session's project>/.sunmao/models.json`
/// wholesale, then reload every live host's resolver so the
/// picker/settings see it at once. The file shape is `ModelsFile`.
pub(super) async fn put(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    let file: sunmao_core::models::ModelsFile = match serde_json::from_slice(body) {
        Ok(f) => f,
        Err(e) => return HostResponse::err(400, format!("bad models.json: {e}")),
    };
    let cwd = models_host(s, sess.clone())
        .map(|h| h.agent.session_cwd())
        .unwrap_or_else(|| s.cwd.clone());
    let dir = cwd.join(".sunmao");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    let pretty = match serde_json::to_string_pretty(&file) {
        Ok(t) => t,
        Err(e) => return HostResponse::err(400, format!("{e:#}")),
    };
    if let Err(e) = std::fs::write(dir.join("models.json"), pretty) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    for id in s.live_ids() {
        if let Some(h) = s.host(&id) {
            h.agent.reload_models();
            // the level vocabulary (and the default it implies) comes from
            // the catalog this write just replaced: re-announce it now, or
            // the composer keeps yesterday's ladder until a model swap or a
            // restart. Broadcast, not `emit` — every open page needs it.
            let _ = s.live.send(effort_frame(&h).await);
        }
    }
    s.emit(serde_json::json!({"type": "models_changed"}));
    view(s, sess).await
}

// Knowledge refresh was removed: gateway guesses kept overwriting the
// doc-verified level sets. The layers stay file-editable (user/project
// `model-knowledge.json`), maintained by the agent via provider docs —
// see the sunmao-config skill's knowledge-table section.
