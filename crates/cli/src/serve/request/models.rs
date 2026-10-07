//! provider/model surface — the GUI settings page edits `.sunmao/models.json`
//! and the composer picker lists selectors, both through GET/PUT `/models`
//! (+ `POST /models/fetch`). Split out of `request.rs` to keep the route
//! table under the god-file budget.
// arch: allow-god-file models route plus its GET/PUT credential-loss regression suite are one seam

use std::collections::HashMap;
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
    let default_provider = resolver
        .as_ref()
        .map(|m| m.default_provider())
        .unwrap_or_else(|| "default".into());
    let file = match resolver.as_ref() {
        Some(m) => {
            // The page has to show the FILE, not this process's last snapshot:
            // an edit made elsewhere (another frontend, a hand edit) must be
            // visible here, or the next save writes the stale table back over
            // it — that is how a configured catalog gets silently reduced.
            if let Some(h) = host.as_ref() {
                h.agent.reload_models().await;
            } else {
                m.reload();
            }
            m.file()
        }
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
    let (fusion_models, fusion_efforts, fusion_ready, turn_mode) = host
        .as_ref()
        .map(|host| {
            (
                host.agent.fusion_models(),
                host.agent.fusion_efforts(),
                host.agent.fusion_ready(),
                host.agent.turn_mode().as_str(),
            )
        })
        .unwrap_or(((None, None), (None, None), false, "standard"));
    HostResponse::json(serde_json::json!({
        "providers": providers_view(&file, resolver.as_deref()),
        "routes": file.routes,
        // the new-session pick — the settings page renders it and writes it
        // back through the same PUT (null = nothing pinned). A stored pin
        // that no longer resolves reads as unpinned: the pill must never show
        // a selector the picker cannot offer, and the next save drops it.
        "default_model": file
            .default_selector()
            .filter(|sel| default_model_problem(sel, &file, &default_provider).is_none()),
        "fusion_lead": fusion_models.0,
        "fusion_sidekick": fusion_models.1,
        "fusion_lead_effort": fusion_efforts.0,
        "fusion_sidekick_effort": fusion_efforts.1,
        "fusion_ready": fusion_ready,
        "turn_mode": turn_mode,
        "selectors": resolver.as_ref().map(|m| m.selectors()).unwrap_or_default(),
        "default_provider": default_provider,
    }))
}

/// Serialize the provider table for the GUI — keys are redacted to a
/// `api_key_set` boolean; the settings editor writes keys, it never
/// reads them back.
fn providers_view(
    file: &sunmao_core::models::ModelsFile,
    resolver: Option<&sunmao_core::models::ModelResolver>,
) -> serde_json::Value {
    file.providers
        .iter()
        .map(|(name, p)| {
            (
                name.clone(),
                serde_json::json!({
                    "base_url": p.base_url,
                    "dialect": p.dialect,
                    "api_key_env": p.api_key_env,
                    "api_key_set": p.api_key_env.is_some() || p.api_key.is_some() || resolver.is_some_and(|resolver| resolver.provider_api_key_set(name)),
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
        let provider = if let Some(resolver) = resolver {
            resolver.provider_def(name)
        } else {
            let text =
                std::fs::read_to_string(s.cwd.join(".sunmao/models.json")).unwrap_or_default();
            let file: sunmao_core::models::ModelsFile =
                serde_json::from_str(&text).unwrap_or_default();
            file.providers.get(name).cloned()
        };
        match provider {
            Some(provider) => provider,
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

/// The save's request-level metadata, under the reserved `$request` key.
///
/// A save has to say things the document shape cannot: "this entry is a
/// create, not an edit", "this entry was renamed from X", "this save decides
/// that provider's model list", "the user actually picked the default model".
/// Bare top-level names would collide with hand-written file keys
/// (`models.json` may legitimately carry `adding`), so they ride in one
/// reserved namespace that the server strips before the file parse. Everything
/// outside `$request` is document content, unknown keys included.
#[derive(serde::Deserialize, Default)]
struct SaveRequest {
    /// `{new: old}` — where a renamed entry's credential has to move, and
    /// what lets a rename onto a taken name fail instead of overwriting it.
    #[serde(default)]
    rename_from: HashMap<String, String>,
    /// the name this save CREATES — the body always carries the whole provider
    /// map, so only the page knows which entry is a new one.
    #[serde(default)]
    adding: Option<String>,
    /// the provider whose catalog THIS save decides (the editor's candidate
    /// pool was changed). Every other provider's catalog is taken from the
    /// file, so a page whose snapshot is stale or empty can never shrink one.
    #[serde(default)]
    catalog_owner: Option<String>,
    /// the user picked or cleared `default_model`, rather than the page
    /// carrying the current value through: a value this save names must be
    /// usable, so a dead one is refused instead of quietly stored.
    #[serde(default)]
    set_default: bool,
}

/// Why `default_model` cannot be used — `None` means it is fine or nothing is
/// pinned. A bare id resolves through the session's own provider (never in the
/// file), `provider/` names no model at all, and a `@route` has to exist.
fn default_model_problem(
    sel: &str,
    file: &sunmao_core::models::ModelsFile,
    session_provider: &str,
) -> Option<String> {
    let sel = sel.trim();
    if sel.is_empty() {
        return None;
    }
    if let Some(route) = sel.strip_prefix('@') {
        return (!file.routes.contains_key(route)).then(|| format!("no route named @{route}"));
    }
    let (prov, model) = sel.split_once('/')?;
    if model.trim().is_empty() {
        return Some(format!("`{sel}` names no model"));
    }
    if !file.providers.contains_key(prov) && prov != session_provider {
        return Some(format!("no provider named {prov}"));
    }
    None
}

/// `PUT /models?sess=` — fold the page's desired table onto
/// `<session's project>/.sunmao/models.json`, then reload every live host's
/// resolver so the picker/settings see it at once.
///
/// The body is NOT the whole file: the settings page rebuilds the provider
/// table from the redacted GET view (no literal keys) and carries only the
/// top-level keys it knows. Replacing the file wholesale therefore deleted a
/// hand-written `api_key` and any key this build doesn't know; the merge in
/// `ModelsFile::merge_save` keeps them, provider-level keys included.
pub(super) async fn put(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    let mut v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HostResponse::err(400, format!("bad models.json: {e}")),
    };
    let req: SaveRequest = match v.get("$request") {
        None | Some(serde_json::Value::Null) => SaveRequest::default(),
        Some(x) => match serde_json::from_value(x.clone()) {
            Ok(r) => r,
            Err(e) => return HostResponse::err(400, format!("bad $request: {e}")),
        },
    };
    if let Some(o) = v.as_object_mut() {
        o.remove("$request");
    }
    let SaveRequest {
        rename_from,
        adding,
        catalog_owner,
        set_default,
    } = req;
    let incoming: sunmao_core::models::ModelsFile = match serde_json::from_value(v) {
        Ok(f) => f,
        Err(e) => return HostResponse::err(400, format!("bad models.json: {e}")),
    };
    let host = models_host(s, sess.clone());
    let cwd = host
        .as_ref()
        .map(|h| h.agent.session_cwd())
        .unwrap_or_else(|| s.cwd.clone());
    let default_name = host
        .as_ref()
        .and_then(|h| h.agent.models_resolver())
        .map(|m| m.default_provider())
        .unwrap_or_else(|| "default".to_string());
    let dir = cwd.join(".sunmao");
    let path = dir.join("models.json");
    // the merged `.sunmao` → `.claude` table this save merges against, not
    // `.sunmao` alone: a provider that lives only in the compat layer would
    // otherwise be materialized here as a keyless copy, because the redacted
    // GET view cannot carry its credential. A missing/corrupt file is simply
    // nothing to preserve.
    let existing = sunmao_core::models::read_models(&cwd);
    // a save that CREATES `adding` cannot land on a name the file already
    // owns: the provider map has no room for both, and the old save silently
    // overwrote the existing entry.
    if let Some(new) = &adding
        && existing.providers.contains_key(new)
    {
        return HostResponse::err(409, format!("a provider named {new} already exists"));
    }
    // a rename onto a name another provider already owns is a collision: the
    // page's provider map has no room for both, and the old save overwrote
    // the twin and deleted the source. Refuse it.
    for (new, old) in &rename_from {
        if new != old && existing.providers.contains_key(new) {
            return HostResponse::err(409, format!("provider already exists: {new}"));
        }
    }
    let mut merged =
        sunmao_core::models::ModelsFile::merge_save(&existing, &incoming, &rename_from);
    // A catalog can only be decided by an editor that showed one. For every
    // other provider the file keeps its model list: the page posts whatever
    // snapshot it had, and a stale or empty one must not shrink (or clear) a
    // configured catalog — `$request.catalog_owner` names the one provider
    // whose pool the user actually changed.
    for (name, old) in &existing.providers {
        if old.catalog.is_empty() || catalog_owner.as_deref() == Some(name.as_str()) {
            continue;
        }
        if let Some(p) = merged.providers.get_mut(name) {
            p.catalog = old.catalog.clone();
        }
    }
    // a pin this save named explicitly must be usable; one that merely rode
    // along (its provider was just deleted, or a hand edit left it stale) is
    // dropped instead of blocking an unrelated save
    if let Some(sel) = merged.default_model.clone()
        && let Some(why) = default_model_problem(&sel, &merged, &default_name)
    {
        if set_default {
            return HostResponse::err(400, format!("default_model: {why}"));
        }
        merged.default_model = None;
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    let pretty = match serde_json::to_string_pretty(&merged) {
        Ok(t) => t,
        Err(e) => return HostResponse::err(400, format!("{e:#}")),
    };
    if let Err(e) = std::fs::write(&path, pretty) {
        return HostResponse::err(500, format!("{e:#}"));
    }
    for id in s.live_ids() {
        if let Some(h) = s.host(&id) {
            h.agent.reload_models().await;
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

/// Route-level tests live here, not in `request/tests.rs`: that file sits at
/// 570/600 lines and the arch gate counts test files against the god-file
/// budget. These are the end-to-end GET/PUT cases the core models test can
/// only approximate (core can't reach the serve layer).
#[cfg(test)]
mod tests {
    use super::super::super::host::{HostHandle, Shared};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    /// A `Shared` whose session factory never runs — with no live host the
    /// models routes answer off the project file, which is the dormant
    /// settings-page path the audit exercised.
    fn shared_at(cwd: std::path::PathBuf) -> Shared {
        use std::sync::Mutex;
        use tokio::sync::{broadcast, mpsc};
        let (live, _) = broadcast::channel::<serde_json::Value>(8);
        let (mgmt, _rx) = mpsc::unbounded_channel();
        Shared {
            cwd,
            roots: Vec::new(),
            live,
            sessions: Mutex::new(HashMap::new()),
            factory: crate::serve::SessionFactory {
                make: Box::new(|_, _, _| Box::pin(async { anyhow::bail!("test factory") })),
            },
            model_label: String::new(),
            model_override: None,
            sandbox_port: 0,
            prompt_override: None,
            driver_override: None,
            pending_drivers: Mutex::new(Default::default()),
            approval_ids: Arc::new(AtomicU64::new(0)),
            adopt_lock: tokio::sync::Mutex::new(()),
            adopt_seq: AtomicU64::new(0),
            mgmt,
        }
    }

    fn models_dir(tag: &str, file: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("sunmao-models-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".sunmao/sessions")).unwrap();
        if !file.is_empty() {
            std::fs::write(root.join(".sunmao/models.json"), file).unwrap();
        }
        root
    }

    fn handle(root: &std::path::Path) -> HostHandle {
        HostHandle {
            s: Arc::new(shared_at(root.to_path_buf())),
        }
    }

    fn disk(root: &std::path::Path) -> serde_json::Value {
        let text = std::fs::read_to_string(root.join(".sunmao/models.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// The PUT body `connection.js::modelsBody` builds: every provider of the
    /// redacted GET view, field by field, plus routes and the default pick.
    /// A literal key is not in the view, so it cannot be in this body — which
    /// is exactly why the server has to merge instead of replace.
    fn gui_body(view: &serde_json::Value) -> serde_json::Value {
        let mut providers = serde_json::Map::new();
        for (name, p) in view["providers"].as_object().unwrap() {
            let mut e = serde_json::Map::new();
            for k in ["base_url", "dialect", "catalog"] {
                e.insert(k.into(), p[k].clone());
            }
            if !p["api_key_env"].is_null() {
                e.insert("api_key_env".into(), p["api_key_env"].clone());
            }
            providers.insert(name.clone(), e.into());
        }
        let mut body = serde_json::Map::new();
        body.insert("providers".into(), providers.into());
        body.insert("routes".into(), view["routes"].clone());
        if !view["default_model"].is_null() {
            body.insert("default_model".into(), view["default_model"].clone());
        }
        body.into()
    }

    #[tokio::test]
    async fn models_save_keeps_literal_keys_and_unknown_top_level_keys() {
        let root = models_dir(
            "keep",
            r#"{"providers":{
                 "local":{"base_url":"http://127.0.0.1:9/v1","dialect":"openai","api_key":"sk-literal-not-a-real-secret","catalog":[{"id":"m1"}]},
                 "plain":{"base_url":"http://plain/v1","dialect":"openai"}},
               "routes":{"fast":"local/m1"},"default_model":"local/m1","something_new":{"a":1}}"#,
        );
        let h = handle(&root);
        let got = h.request("GET", "/models", b"").await;
        // the raw response must not carry the literal key either: the view is
        // redacted on purpose (a key on the wire ends up in the browser, dev
        // tools and any log that records the response). A future "helpful"
        // `api_key` field in `providers_view` has to fail here.
        assert!(
            !String::from_utf8_lossy(&got.body).contains("sk-literal-not-a-real-secret"),
            "GET /models must never put a literal key on the wire"
        );
        let view: serde_json::Value = serde_json::from_slice(&got.body).unwrap();
        assert!(
            view["providers"]["local"]["api_key"].is_null(),
            "the view keeps redacting keys"
        );

        // the page's default-model pick, providers rebuilt from that view; the
        // base_url edit proves the PUT actually lands (a no-op can't pass)
        let mut body = gui_body(&view);
        body["default_model"] = "plain/m2".into();
        body["providers"]["plain"]["base_url"] = "http://edited/v1".into();
        let put = h
            .request("PUT", "/models", body.to_string().as_bytes())
            .await;
        assert_eq!(put.status, 200, "{}", String::from_utf8_lossy(&put.body));

        let d = disk(&root);
        assert_eq!(
            d["providers"]["local"]["api_key"], "sk-literal-not-a-real-secret",
            "a save must not erase a hand-written literal key"
        );
        assert_eq!(
            d["something_new"]["a"], 1,
            "a top-level key this build doesn't know must survive the save"
        );
        assert_eq!(d["providers"]["plain"]["base_url"], "http://edited/v1");
        assert_eq!(d["default_model"], "plain/m2");
        assert_eq!(d["routes"]["fast"], "local/m1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn models_save_refuses_a_rename_onto_an_existing_provider() {
        let root = models_dir(
            "collide",
            r#"{"providers":{
                 "local":{"base_url":"http://local/v1","dialect":"openai","api_key":"sk-keep"},
                 "plain":{"base_url":"http://plain/v1","dialect":"openai"}}}"#,
        );
        let h = handle(&root);
        // the form opened on `plain` and saved as `local` — a collision, not
        // a rename: nothing may be overwritten
        let body = serde_json::json!({
            "providers": {"local": {"base_url": "http://plain/v1", "dialect": "openai", "catalog": [{"id": "pm1"}]}},
            "routes": {},
            "$request": {"rename_from": {"local": "plain"}},
        });
        let put = h
            .request("PUT", "/models", body.to_string().as_bytes())
            .await;
        assert_eq!(put.status, 409, "{}", String::from_utf8_lossy(&put.body));
        let d = disk(&root);
        assert_eq!(d["providers"]["local"]["api_key"], "sk-keep");
        assert_eq!(d["providers"]["local"]["base_url"], "http://local/v1");
        assert_eq!(d["providers"]["plain"]["base_url"], "http://plain/v1");
        assert!(
            d["rename_from"].is_null(),
            "the request namespace is never file content"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn models_save_moves_a_literal_key_with_a_renamed_provider() {
        let root = models_dir(
            "rename",
            r#"{"providers":{"plain":{"base_url":"http://plain/v1","dialect":"openai","api_key":"sk-move"}}}"#,
        );
        let h = handle(&root);
        let body = serde_json::json!({
            "providers": {"renamed": {"base_url": "http://plain/v1", "dialect": "openai", "catalog": [{"id": "m1"}]}},
            "routes": {},
            "$request": {"rename_from": {"renamed": "plain"}},
        });
        let put = h
            .request("PUT", "/models", body.to_string().as_bytes())
            .await;
        assert_eq!(put.status, 200, "{}", String::from_utf8_lossy(&put.body));
        let d = disk(&root);
        assert!(d["providers"]["plain"].is_null(), "the old name is gone");
        assert_eq!(d["providers"]["renamed"]["api_key"], "sk-move");
        assert!(d["rename_from"].is_null());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn models_save_clears_a_default_model_whose_provider_is_gone() {
        let root = models_dir(
            "dangle",
            r#"{"providers":{
                 "local":{"base_url":"http://local/v1","dialect":"openai","catalog":[{"id":"m1"}]},
                 "other":{"base_url":"http://other/v1","dialect":"openai"}},
               "default_model":"local/m1"}"#,
        );
        let h = handle(&root);
        // the page deletes `local`: the provider entry goes, the carried-over
        // default_model does not — that is the bug
        let body = serde_json::json!({
            "providers": {"other": {"base_url": "http://other/v1", "dialect": "openai", "catalog": []}},
            "routes": {},
            "default_model": "local/m1",
        });
        let put = h
            .request("PUT", "/models", body.to_string().as_bytes())
            .await;
        assert_eq!(put.status, 200, "{}", String::from_utf8_lossy(&put.body));
        let d = disk(&root);
        assert!(d["providers"]["local"].is_null());
        assert!(
            d["default_model"].is_null(),
            "a deleted provider must not leave a dangling pin"
        );

        // a pin that still resolves stays put
        let body = serde_json::json!({
            "providers": {"other": {"base_url": "http://other/v1", "dialect": "openai", "catalog": []}},
            "routes": {},
            "default_model": "other/m9",
        });
        assert_eq!(
            h.request("PUT", "/models", body.to_string().as_bytes())
                .await
                .status,
            200
        );
        assert_eq!(disk(&root)["default_model"], "other/m9");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An add that reuses an existing provider's name is the same data loss as
    /// a colliding rename, and the server cannot tell add from edit — so the
    /// page declares the add (`adding: "<name>"`, stripped like `rename_from`)
    /// and the server refuses the name instead of overwriting the twin.
    #[tokio::test]
    async fn models_save_refuses_an_add_onto_an_existing_provider() {
        let root = models_dir(
            "addcollide",
            r#"{"providers":{"local":{"base_url":"http://local/v1","dialect":"openai","api_key":"sk-keep"}}}"#,
        );
        let h = handle(&root);
        let body = serde_json::json!({
            "providers": {"local": {"base_url": "http://brand-new/v1", "dialect": "openai", "catalog": []}},
            "routes": {},
            "$request": {"adding": "local"},
        });
        let put = h
            .request("PUT", "/models", body.to_string().as_bytes())
            .await;
        assert_eq!(put.status, 409, "{}", String::from_utf8_lossy(&put.body));
        let d = disk(&root);
        assert_eq!(d["providers"]["local"]["base_url"], "http://local/v1");
        assert_eq!(d["providers"]["local"]["api_key"], "sk-keep");
        assert!(
            d["adding"].is_null(),
            "the request namespace is never file content"
        );

        // a genuinely new name still lands
        let body = serde_json::json!({
            "providers": {
                "local": {"base_url": "http://local/v1", "dialect": "openai", "catalog": []},
                "fresh": {"base_url": "http://fresh/v1", "dialect": "openai", "catalog": []},
            },
            "routes": {},
            "$request": {"adding": "fresh"},
        });
        assert_eq!(
            h.request("PUT", "/models", body.to_string().as_bytes())
                .await
                .status,
            200
        );
        let d = disk(&root);
        assert_eq!(d["providers"]["fresh"]["base_url"], "http://fresh/v1");
        assert_eq!(
            d["providers"]["local"]["api_key"], "sk-keep",
            "the carried key still survives"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The legacy project Sidekick key is preserved as an unknown field, but
    /// it neither configures a session role nor appears as an active pick.
    #[tokio::test]
    async fn models_save_preserves_but_does_not_use_legacy_project_sidekick() {
        let root = models_dir(
            "sidekick",
            r#"{"providers":{"local":{"base_url":"http://local/v1"}},"fusion_sidekick":"local/cheap"}"#,
        );
        let h = handle(&root);
        let view: serde_json::Value =
            serde_json::from_slice(&h.request("GET", "/models", b"").await.body).unwrap();
        assert!(view["fusion_sidekick"].is_null());
        assert_eq!(view["fusion_ready"], false);
        let body = gui_body(&view);
        assert!(!body.as_object().unwrap().contains_key("fusion_sidekick"));
        assert_eq!(
            h.request("PUT", "/models", body.to_string().as_bytes())
                .await
                .status,
            200
        );
        assert_eq!(disk(&root)["fusion_sidekick"], "local/cheap");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A hand-written provider-level key the view never carries must survive a
    /// save the same way a top-level one does.
    #[tokio::test]
    async fn models_save_keeps_hand_written_provider_keys() {
        let root = models_dir(
            "provkey",
            r#"{"providers":{"local":{"base_url":"http://local/v1","dialect":"openai","headers":{"X-Trace":"on"},"rpm":10}}}"#,
        );
        let h = handle(&root);
        let view: serde_json::Value =
            serde_json::from_slice(&h.request("GET", "/models", b"").await.body).unwrap();
        assert!(
            view["providers"]["local"]["headers"].is_null(),
            "the view does not carry provider-level unknowns"
        );
        let body = gui_body(&view);
        assert_eq!(
            h.request("PUT", "/models", body.to_string().as_bytes())
                .await
                .status,
            200
        );
        let d = disk(&root);
        assert_eq!(d["providers"]["local"]["headers"]["X-Trace"], "on");
        assert_eq!(d["providers"]["local"]["rpm"], 10);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A provider that lives only in the `.claude` compat layer must not be
    /// materialized into `.sunmao` as a keyless copy: the save merges against
    /// the layered table, so the copy keeps the credential.
    #[tokio::test]
    async fn models_save_does_not_copy_a_compat_layer_provider_keyless() {
        let root = models_dir(
            "claudelayer",
            r#"{"providers":{"local":{"base_url":"http://local/v1"}}}"#,
        );
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            root.join(".claude/models.json"),
            r#"{"providers":{"compat":{"base_url":"http://compat/v1","dialect":"openai","api_key":"sk-compat-literal"}}}"#,
        )
        .unwrap();
        let h = handle(&root);
        // what a live-host GET view shows and the page posts back
        let body = serde_json::json!({
            "providers": {
                "local": {"base_url": "http://local/v1", "dialect": "openai", "catalog": []},
                "compat": {"base_url": "http://compat/v1", "dialect": "openai", "catalog": []},
            },
            "routes": {},
        });
        assert_eq!(
            h.request("PUT", "/models", body.to_string().as_bytes())
                .await
                .status,
            200
        );
        assert_eq!(
            disk(&root)["providers"]["compat"]["api_key"],
            "sk-compat-literal",
            "a compat-layer provider must not be copied in keyless"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `adding` / `rename_from` are ordinary file keys: a save must store them
    /// as content instead of spending them as markers, whether they arrive in
    /// the body or already sit in the file. Only the reserved request
    /// namespace may be consumed by a save.
    #[tokio::test]
    async fn models_save_leaves_hand_written_adding_and_rename_from_alone() {
        let root = models_dir(
            "reserved",
            r#"{"providers":{"local":{"base_url":"http://local/v1"},"plain":{"base_url":"http://plain/v1"}},
                "routes":{},"adding":"local","rename_from":{"local":"plain"}}"#,
        );
        let h = handle(&root);
        let view: serde_json::Value =
            serde_json::from_slice(&h.request("GET", "/models", b"").await.body).unwrap();
        // a plain GUI save of the file above
        let put = h
            .request("PUT", "/models", gui_body(&view).to_string().as_bytes())
            .await;
        assert_eq!(put.status, 200, "{}", String::from_utf8_lossy(&put.body));
        let d = disk(&root);
        assert_eq!(
            d["adding"], "local",
            "a hand-written `adding` is file content"
        );
        assert_eq!(d["rename_from"]["local"], "plain");
        assert_eq!(
            d["providers"]["local"]["base_url"], "http://local/v1",
            "no rename may happen"
        );
        assert_eq!(d["providers"]["plain"]["base_url"], "http://plain/v1");

        // the same names sent in the BODY are still content, not markers: a
        // body carrying them must not trip a collision or lose them
        let body = serde_json::json!({
            "providers": {
                "local": {"base_url": "http://local/v1", "dialect": "openai", "catalog": []},
                "plain": {"base_url": "http://plain/v1", "dialect": "openai", "catalog": []},
            },
            "routes": {},
            "adding": "local",
            "rename_from": {"local": "plain"},
        });
        let put = h
            .request("PUT", "/models", body.to_string().as_bytes())
            .await;
        assert_eq!(
            put.status,
            200,
            "those keys must not read as a create/rename: {}",
            String::from_utf8_lossy(&put.body)
        );
        let d = disk(&root);
        assert_eq!(d["adding"], "local");
        assert_eq!(d["rename_from"]["local"], "plain");
        assert_eq!(d["providers"]["plain"]["base_url"], "http://plain/v1");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A save must not shrink a catalog it never showed. The page posts its
    /// snapshot, but the file is the truth: an empty candidate pool means
    /// "nothing to render", not "delete them all". Only an editor that
    /// actually touched the list owns it (`$request.catalog_owner`).
    #[tokio::test]
    async fn models_save_keeps_a_catalog_the_form_did_not_own() {
        let root = models_dir(
            "catalogkeep",
            r#"{"providers":{"local":{"base_url":"http://local/v1",
                 "catalog":[{"id":"m1"},{"id":"m2"},{"id":"m3"}]}}}"#,
        );
        let h = handle(&root);
        // what "open the editor, change nothing catalog-related, hit 保存" posts
        let body = serde_json::json!({
            "providers": {"local": {"base_url": "http://local/v1", "dialect": "openai", "catalog": []}},
            "routes": {},
        });
        let put = h
            .request("PUT", "/models", body.to_string().as_bytes())
            .await;
        assert_eq!(put.status, 200, "{}", String::from_utf8_lossy(&put.body));
        let d = disk(&root);
        assert_eq!(
            d["providers"]["local"]["catalog"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0),
            3,
            "a form that showed no candidates must not clear the file's catalog"
        );
    }

    /// ...but an editor that DID touch the pool owns it, so unchecking the
    /// last model really does clear the catalog.
    #[tokio::test]
    async fn models_save_lets_the_owning_editor_clear_a_catalog() {
        let root = models_dir(
            "catalogown",
            r#"{"providers":{"local":{"base_url":"http://local/v1",
                 "catalog":[{"id":"m1"},{"id":"m2"}]}}}"#,
        );
        let h = handle(&root);
        let body = serde_json::json!({
            "providers": {"local": {"base_url": "http://local/v1", "dialect": "openai", "catalog": []}},
            "routes": {},
            "$request": {"catalog_owner": "local"},
        });
        let put = h
            .request("PUT", "/models", body.to_string().as_bytes())
            .await;
        assert_eq!(put.status, 200, "{}", String::from_utf8_lossy(&put.body));
        assert_eq!(
            disk(&root)["providers"]["local"]["catalog"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0),
            0,
            "an explicit clear must land"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A default model the save names explicitly has to be usable; one that
    /// merely rode along (its provider was just deleted) is dropped instead of
    /// blocking an unrelated save.
    #[tokio::test]
    async fn models_save_rejects_an_empty_or_dead_default_model() {
        let file =
            r#"{"providers":{"local":{"base_url":"http://local/v1","catalog":[{"id":"m1"}]}}}"#;
        for sel in ["local/", "ghost/m1", "@nosuch"] {
            let root = models_dir("defbad", file);
            let h = handle(&root);
            let body = serde_json::json!({
                "providers": {"local": {"base_url": "http://local/v1", "dialect": "openai", "catalog": [{"id": "m1"}]}},
                "routes": {},
                "default_model": sel,
                "$request": {"set_default": true},
            });
            let put = h
                .request("PUT", "/models", body.to_string().as_bytes())
                .await;
            assert_eq!(
                put.status,
                400,
                "{sel} must be refused: {}",
                String::from_utf8_lossy(&put.body)
            );
            assert!(
                disk(&root)["default_model"].is_null(),
                "{sel}: a refused save writes nothing"
            );
            let _ = std::fs::remove_dir_all(&root);
        }

        // carried, not set: pruned so the stale pin can't strand a new chat
        let root = models_dir("defstale", file);
        let h = handle(&root);
        let body = serde_json::json!({
            "providers": {"local": {"base_url": "http://local/v1", "dialect": "openai", "catalog": [{"id": "m1"}]}},
            "routes": {},
            "default_model": "ghost/m1",
        });
        assert_eq!(
            h.request("PUT", "/models", body.to_string().as_bytes())
                .await
                .status,
            200
        );
        assert!(disk(&root)["default_model"].is_null(), "dead pin dropped");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The happy path: a real catalogued model round-trips through PUT/GET and
    /// stays pinned, with its catalog intact.
    #[tokio::test]
    async fn models_save_round_trips_a_real_default_model() {
        let root = models_dir(
            "defgood",
            r#"{"providers":{"local":{"base_url":"http://local/v1","catalog":[{"id":"m1"}]}}}"#,
        );
        let h = handle(&root);
        let body = serde_json::json!({
            "providers": {"local": {"base_url": "http://local/v1", "dialect": "openai", "catalog": [{"id": "m1"}]}},
            "routes": {},
            "default_model": "local/m1",
            "$request": {"set_default": true, "catalog_owner": "local"},
        });
        let put = h
            .request("PUT", "/models", body.to_string().as_bytes())
            .await;
        assert_eq!(put.status, 200, "{}", String::from_utf8_lossy(&put.body));
        let d = disk(&root);
        assert_eq!(d["default_model"], "local/m1");
        assert_eq!(d["providers"]["local"]["catalog"][0]["id"], "m1");
        let view: serde_json::Value = serde_json::from_slice(&put.body).unwrap();
        assert_eq!(view["default_model"], "local/m1");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The pin rule the save path enforces: a bare id resolves through the
    /// session's own provider, a `provider/model` is fine even when the id is
    /// not catalogued (the provider may know it), and only an empty model
    /// part, a missing provider or a missing route is dead.
    #[test]
    fn default_model_problem_flags_only_dead_pins() {
        let f: sunmao_core::models::ModelsFile = serde_json::from_str(
            r#"{"providers":{"other":{"base_url":"http://o/v1"}},"routes":{"fast":"other/m1"}}"#,
        )
        .unwrap();
        assert_eq!(
            super::default_model_problem("other/m9", &f, "default"),
            None
        );
        assert_eq!(super::default_model_problem("bare-id", &f, "default"), None);
        assert_eq!(super::default_model_problem("", &f, "default"), None);
        assert_eq!(super::default_model_problem("@fast", &f, "default"), None);
        assert_eq!(
            super::default_model_problem("default/m1", &f, "default"),
            None,
            "the session's own provider is injected, never dangling"
        );
        assert!(super::default_model_problem("other/", &f, "default").is_some());
        assert!(super::default_model_problem("ghost/m1", &f, "default").is_some());
        assert!(super::default_model_problem("@nope", &f, "default").is_some());
    }
}
