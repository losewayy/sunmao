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

/// `PUT /models` replaces the whole file: a top-level key this build doesn't
/// know has to come back out of the parse, or a GUI save silently deletes it.
#[test]
fn unknown_top_level_keys_survive_a_round_trip() {
    let dir = crate::fresh_test_dir("models-extra");
    write_models(
        &dir,
        ".sunmao",
        r#"{"providers":{},"default_model":"@fast","something_new":{"a":1}}"#,
    );
    let file = read_models_file(&dir);
    let text = serde_json::to_string(&file).unwrap();
    let back: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(back["something_new"]["a"], 1);
    assert_eq!(back["default_model"], "@fast");
    assert_eq!(file.extra.len(), 1, "only the unknown key lands in `extra`");
    std::fs::remove_dir_all(&dir).ok();
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
