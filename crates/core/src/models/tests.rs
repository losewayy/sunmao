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
