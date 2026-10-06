use super::*;

fn resolver() -> ModelResolver {
    let r = ModelResolver::load(
        Path::new("."), // no .sunmao/models.json here — pure defaults
        ProviderDef {
            base_url: "http://local/v1".into(),
            api_key_env: None,
            api_key: None,
            dialect: "openai".into(),
            catalog: Vec::new(),
            extra: Default::default(),
        },
        "default",
    );
    let mut file = r.file.write_or_recover();
    file.providers.insert(
        "big".into(),
        ProviderDef {
            base_url: "http://big/v1".into(),
            api_key_env: Some("NOPE_NOT_SET".into()),
            api_key: None,
            dialect: "anthropic".into(),
            catalog: Vec::new(),
            extra: Default::default(),
        },
    );
    file.routes.insert(
        "smol".into(),
        RouteValue::Chain(vec!["big/claude-haiku".into(), "tiny-1b".into()]),
    );
    drop(file);
    r
}

#[test]
fn bare_model_uses_default_provider() {
    let r = resolver();
    let t = r.resolve("qwen-flash").unwrap();
    assert_eq!(t.model, "qwen-flash");
    assert_eq!(t.provider.base_url, "http://local/v1");
}

/// The selector list is what a session can actually run: an `@route` or a
/// catalog-backed `provider/model`. A provider with no catalog must not
/// contribute a bare `provider/` entry — resolving that yields an EMPTY model
/// id (the GUI menu's `default/` line), which the provider then rejects.
#[test]
fn selectors_only_offer_real_models() {
    let r = resolver();
    {
        let mut file = r.file.write_or_recover();
        file.providers
            .get_mut("big")
            .unwrap()
            .catalog
            .push(CatalogEntry {
                id: "claude-haiku".into(),
                ..Default::default()
            });
        file.providers.insert(
            "empty".into(),
            ProviderDef {
                base_url: "http://empty/v1".into(),
                ..Default::default()
            },
        );
    }
    let sel = r.selectors();
    assert!(sel.contains(&"@smol".into()), "{sel:?}");
    assert!(sel.contains(&"big/claude-haiku".into()), "{sel:?}");
    assert!(
        !sel.iter().any(|s| s.ends_with('/')),
        "no empty-model entries may be offered: {sel:?}"
    );
    assert!(
        !sel.iter().any(|s| s.starts_with("empty")),
        "a catalog-less provider must not appear: {sel:?}"
    );
    // what the old list offered: it *resolves*, to a model id of "" — usable
    // looking, unusable in practice, which is exactly why it must not be listed
    let t = r.resolve("big/").unwrap();
    assert!(
        t.model.is_empty(),
        "an empty model id is the bug, not a target"
    );
}

#[test]
fn route_alias_expands_chain_in_order() {
    let r = resolver();
    let t = r.resolve("@smol").unwrap();
    assert_eq!(t.model, "claude-haiku");
    assert_eq!(t.provider.dialect, "anthropic");
}

#[test]
fn unknown_selector_resolves_none() {
    let r = resolver();
    assert!(r.resolve("@nosuch").is_none());
    assert!(r.resolve("ghost/model").is_none());
}

/// `.claude/models.json` is a compat LAYER, not a replacement — a file
/// that only adds one route must not wipe the project's providers.
#[test]
fn claude_layer_merges_instead_of_replacing() {
    let dir = crate::fresh_test_dir("models-merge");
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    std::fs::write(
        dir.join(".sunmao/models.json"),
        r#"{"providers":{"proj":{"base_url":"http://proj/v1"}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join(".claude/models.json"),
        r#"{"routes":{"ck":"proj/m1"},"providers":{"ck":{"base_url":"http://ck/v1"}}}"#,
    )
    .unwrap();
    let file = read_models_file(&dir);
    assert!(
        file.providers.contains_key("proj"),
        "the .claude layer must not wipe .sunmao providers"
    );
    assert!(file.providers.contains_key("ck"));
    assert!(file.routes.contains_key("ck"));
    std::fs::remove_dir_all(&dir).ok();
}

/// Two providers may share base_url+model with different dialects or
/// credential sources — the adapter cache used to key on endpoint only,
/// so the second selector silently got the first's dialect.
#[test]
fn adapter_cache_key_carries_provider_identity() {
    let dir = crate::fresh_test_dir("models-key");
    std::fs::create_dir_all(&dir).unwrap();
    let r = ModelResolver::load(
        &dir,
        ProviderDef {
            base_url: "http://local/v1".into(),
            api_key_env: None,
            api_key: None,
            dialect: "openai".into(),
            catalog: Vec::new(),
            extra: Default::default(),
        },
        "default",
    );
    {
        let mut file = r.file.write_or_recover();
        for (name, dialect) in [("oa", "openai"), ("an", "anthropic")] {
            file.providers.insert(
                name.into(),
                ProviderDef {
                    base_url: "http://shared/v1".into(),
                    api_key_env: None,
                    api_key: None,
                    dialect: dialect.into(),
                    catalog: Vec::new(),
                    extra: Default::default(),
                },
            );
        }
    }
    let a = r.adapter_for("oa/m").unwrap();
    let b = r.adapter_for("an/m").unwrap();
    assert!(
        !Arc::ptr_eq(&a, &b),
        "different dialects on one endpoint must not share an adapter"
    );
    // same selector twice still hits the cache
    let a2 = r.adapter_for("oa/m").unwrap();
    assert!(Arc::ptr_eq(&a, &a2));
    std::fs::remove_dir_all(&dir).ok();
}

fn write_models(dir: &Path, layer: &str, body: &str) {
    std::fs::create_dir_all(dir.join(layer)).unwrap();
    std::fs::write(dir.join(layer).join("models.json"), body).unwrap();
}

/// The new-session default is a top-level key like `routes`: it must parse
/// when present, stay absent when not, and a blank value means unpinned.
#[test]
fn default_model_parses_and_blanks_are_unpinned() {
    let dir = crate::fresh_test_dir("models-default");
    write_models(
        &dir,
        ".sunmao",
        r#"{"providers":{"p":{"base_url":"http://p/v1"}},"default_model":"p/m1"}"#,
    );
    let file = read_models_file(&dir);
    assert_eq!(file.default_selector(), Some("p/m1"));
    assert_eq!(default_selector(&dir).as_deref(), Some("p/m1"));

    write_models(
        &dir,
        ".sunmao",
        r#"{"providers":{"p":{"base_url":"http://p/v1"}}}"#,
    );
    let file = read_models_file(&dir);
    assert_eq!(file.default_selector(), None, "no key = nothing pinned");
    assert!(default_selector(&dir).is_none());

    write_models(&dir, ".sunmao", r#"{"default_model":"   "}"#);
    let file = read_models_file(&dir);
    assert_eq!(
        file.default_selector(),
        None,
        "a blank key is not a selector — it must not pin an empty model id"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The GUI's save body is built from the REDACTED GET view: no literal key,
/// no unknown top-level key, no provider-level key. `merge_save` folds it onto
/// the file so none dies on save. (The route-level GET→PUT trip is
/// `crates/cli/src/serve/request/models.rs::tests`, which core can't reach —
/// the old version of this test only round-tripped the struct through serde
/// and stayed green while every real save erased the key.)
#[test]
fn merge_save_keeps_what_the_redacted_view_cannot_carry() {
    let existing: ModelsFile = serde_json::from_str(
        r#"{"providers":{
             "local":{"base_url":"http://local/v1","api_key":"sk-x","headers":{"X-Trace":"on"},"catalog":[{"id":"m1"}]},
             "plain":{"base_url":"http://plain/v1"}},
           "routes":{"fast":"local/m1"},"default_model":"local/m1","something_new":{"a":1}}"#,
    )
    .unwrap();
    // what `connection.js::modelsBody` builds for a default-model pick
    let body: ModelsFile = serde_json::from_str(
        r#"{"providers":{
             "local":{"base_url":"http://local/v1","catalog":[{"id":"m1"}]},
             "plain":{"base_url":"http://plain/v1"}},
           "routes":{"fast":"local/m1"},"default_model":"plain/m2"}"#,
    )
    .unwrap();
    let merged = ModelsFile::merge_save(&existing, &body, &HashMap::new());
    assert_eq!(merged.providers["local"].api_key.as_deref(), Some("sk-x"));
    assert_eq!(merged.providers["local"].extra["headers"]["X-Trace"], "on");
    assert_eq!(merged.extra["something_new"]["a"], 1);
    assert_eq!(
        merged.extra.len(),
        1,
        "only the unknown key lands in `extra`"
    );
    assert_eq!(merged.default_model.as_deref(), Some("plain/m2"));

    // a rename declares the hop, and the key plus the provider-level unknowns
    // move with it
    let renamed: ModelsFile = serde_json::from_str(
        r#"{"providers":{"renamed":{"base_url":"http://local/v1","catalog":[{"id":"m1"}]}},"routes":{}}"#,
    )
    .unwrap();
    let hop = HashMap::from([("renamed".to_string(), "local".to_string())]);
    let merged = ModelsFile::merge_save(&existing, &renamed, &hop);
    assert!(!merged.providers.contains_key("local"), "the source goes");
    assert_eq!(merged.providers["renamed"].api_key.as_deref(), Some("sk-x"));
    assert_eq!(
        merged.providers["renamed"].extra["headers"]["X-Trace"],
        "on"
    );
    assert_eq!(
        merged.extra.len(),
        1,
        "unknown top-level keys still ride along"
    );

    // a body that declares a credential is authoritative: typing a literal
    // key must not resurrect the env var the entry used to have (`adapter`
    // prefers the env, so a carried-over one would silently win)
    let env_only: ModelsFile = serde_json::from_str(
        r#"{"providers":{"p":{"base_url":"http://p/v1","api_key_env":"OLD_ENV"}}}"#,
    )
    .unwrap();
    let typed: ModelsFile = serde_json::from_str(
        r#"{"providers":{"p":{"base_url":"http://p/v1","api_key":"sk-typed"}}}"#,
    )
    .unwrap();
    let merged = ModelsFile::merge_save(&env_only, &typed, &HashMap::new());
    assert_eq!(merged.providers["p"].api_key.as_deref(), Some("sk-typed"));
    assert!(
        merged.providers["p"].api_key_env.is_none(),
        "the stale env must not shadow the typed key"
    );
    // ...while a keyless body entry keeps what the file had
    let untouched: ModelsFile =
        serde_json::from_str(r#"{"providers":{"p":{"base_url":"http://p/v1"}}}"#).unwrap();
    let merged = ModelsFile::merge_save(&env_only, &untouched, &HashMap::new());
    assert_eq!(
        merged.providers["p"].api_key_env.as_deref(),
        Some("OLD_ENV")
    );
}

/// The `.claude` compat layer overrides a `.sunmao` twin per key — the
/// default-model pick follows the same rule instead of always taking the
/// first layer's value.
#[test]
fn claude_layer_can_override_the_default_model() {
    let dir = crate::fresh_test_dir("models-default-layer");
    write_models(&dir, ".sunmao", r#"{"default_model":"proj/m1"}"#);
    write_models(&dir, ".claude", r#"{"routes":{"ck":"proj/m2"}}"#);
    assert_eq!(default_selector(&dir).as_deref(), Some("proj/m1"));
    write_models(&dir, ".claude", r#"{"default_model":"proj/m2"}"#);
    assert_eq!(default_selector(&dir).as_deref(), Some("proj/m2"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reload_keeps_the_file_default_provider() {
    let dir = crate::fresh_test_dir("models-reload-default");
    let r = ModelResolver::load(
        &dir,
        ProviderDef {
            base_url: "http://session/v1".into(),
            api_key_env: Some("SESSION_KEY".into()),
            dialect: "openai".into(),
            ..Default::default()
        },
        "default",
    );
    write_models(
        &dir,
        ".sunmao",
        r#"{"providers":{"default":{"base_url":"http://file/v1","api_key_env":"FILE_KEY","dialect":"anthropic","catalog":[{"id":"saved"}],"request_timeout":7}}}"#,
    );
    r.reload();
    let file = r.file();
    let provider = file.providers.get("default").unwrap();
    assert_eq!(provider.base_url, "http://file/v1");
    assert_eq!(provider.api_key_env.as_deref(), Some("FILE_KEY"));
    assert_eq!(provider.dialect, "anthropic");
    assert_eq!(provider.catalog[0].id, "saved");
    assert_eq!(provider.extra["request_timeout"], serde_json::json!(7));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_keeps_the_file_default_provider() {
    let dir = crate::fresh_test_dir("models-load-default");
    write_models(
        &dir,
        ".sunmao",
        r#"{"providers":{"default":{"base_url":"http://file/v1","api_key_env":"FILE_KEY","dialect":"anthropic","catalog":[{"id":"saved"}],"request_timeout":7}}}"#,
    );
    let r = ModelResolver::load(
        &dir,
        ProviderDef {
            base_url: "http://session/v1".into(),
            api_key_env: Some("SESSION_KEY".into()),
            dialect: "openai".into(),
            ..Default::default()
        },
        "default",
    );
    let file = r.file();
    let provider = file.providers.get("default").unwrap();
    assert_eq!(provider.base_url, "http://file/v1");
    assert_eq!(provider.api_key_env.as_deref(), Some("FILE_KEY"));
    assert_eq!(provider.dialect, "anthropic");
    assert_eq!(provider.catalog[0].id, "saved");
    assert_eq!(provider.extra["request_timeout"], serde_json::json!(7));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn file_default_uses_runtime_credentials_without_persisting_them() {
    let dir = crate::fresh_test_dir("models-runtime-credential");
    write_models(
        &dir,
        ".sunmao",
        r#"{"providers":{"default":{"base_url":"http://file/v1","dialect":"anthropic","catalog":[{"id":"saved"}]}}}"#,
    );
    let resolver = ModelResolver::load(
        &dir,
        ProviderDef {
            base_url: "http://session/v1".into(),
            api_key: Some("session-test-key".into()),
            dialect: "openai".into(),
            ..Default::default()
        },
        "default",
    );
    let target = resolver.resolve("default/saved").unwrap();
    assert_eq!(target.provider.base_url, "http://file/v1");
    assert_eq!(target.provider.dialect, "anthropic");
    assert_eq!(target.provider.api_key.as_deref(), Some("session-test-key"));
    assert!(resolver.provider_api_key_set("default"));
    let effective = resolver.provider_def("default").unwrap();
    assert_eq!(effective.base_url, "http://file/v1");
    assert_eq!(effective.api_key.as_deref(), Some("session-test-key"));
    let file_provider = resolver.file().providers.remove("default").unwrap();
    assert!(file_provider.api_key.is_none());
    assert!(file_provider.api_key_env.is_none());
    std::fs::remove_dir_all(&dir).ok();
}
