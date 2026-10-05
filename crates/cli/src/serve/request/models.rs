//! provider/model surface — the GUI settings page edits `.sunmao/models.json`
//! and the composer picker lists selectors, both through GET/PUT `/models`
//! (+ `POST /models/fetch`). Split out of `request.rs` to keep the route
//! table under the god-file budget.

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

/// `PUT /models?sess=` — fold the page's desired table onto
/// `<session's project>/.sunmao/models.json`, then reload every live host's
/// resolver so the picker/settings see it at once.
///
/// The body is NOT the whole file: the settings page rebuilds the provider
/// table from the redacted GET view (no literal keys) and carries only the
/// top-level keys it knows. Replacing the file wholesale therefore deleted a
/// hand-written `api_key` and any key this build doesn't know; the merge in
/// `ModelsFile::merge_save` keeps them.
///
/// `rename_from` (`{new: old}`) is a request-only field the editor adds when
/// a save renames an entry: it names where a key has to move, and it lets a
/// rename onto a name another provider already owns fail instead of silently
/// overwriting it. `adding` (`"<name>"`) is the same kind of field for the
/// create path: an add and an edit look identical in the body (it always
/// carries the whole provider map), so the page declares which save creates a
/// new entry and the server can refuse a name that is already taken. Both are
/// stripped before the file parse, so neither lands in `extra`.
pub(super) async fn put(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    let mut v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HostResponse::err(400, format!("bad models.json: {e}")),
    };
    let rename_from: HashMap<String, String> = match v.get("rename_from") {
        None | Some(serde_json::Value::Null) => HashMap::new(),
        Some(x) => match serde_json::from_value(x.clone()) {
            Ok(m) => m,
            Err(e) => return HostResponse::err(400, format!("bad rename_from: {e}")),
        },
    };
    let adding: Option<String> = match v.get("adding") {
        None | Some(serde_json::Value::Null) => None,
        Some(x) => match serde_json::from_value(x.clone()) {
            Ok(n) => Some(n),
            Err(e) => return HostResponse::err(400, format!("bad adding: {e}")),
        },
    };
    if let Some(o) = v.as_object_mut() {
        o.remove("rename_from");
        o.remove("adding");
    }
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
    // the file this PUT replaces — the merge needs what the redacted view
    // couldn't carry. Missing or unparseable is simply nothing to preserve.
    let existing: sunmao_core::models::ModelsFile = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
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
    // deleting the provider `default_model` points at must not write a dead
    // selector back
    merged.prune_dangling_default(&default_name);
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
            "rename_from": {"local": "plain"},
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
            "the rename hint is a request field, never file content"
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
            "rename_from": {"renamed": "plain"},
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
            "adding": "local",
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
            "the add marker is a request field, never file content"
        );

        // a genuinely new name still lands
        let body = serde_json::json!({
            "providers": {
                "local": {"base_url": "http://local/v1", "dialect": "openai", "catalog": []},
                "fresh": {"base_url": "http://fresh/v1", "dialect": "openai", "catalog": []},
            },
            "routes": {},
            "adding": "fresh",
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
}
