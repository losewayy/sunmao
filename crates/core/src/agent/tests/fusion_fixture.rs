use std::sync::Arc;

use crate::context::{Context, FusionModelSettings};
use crate::session::SessionLog;
use sunmao_llm::ProviderAdapter;

fn ensure_fusion_catalog(dir: &std::path::Path) {
    let path = dir.join(".sunmao/models.json");
    let mut file = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let root = file.as_object_mut().unwrap();
    let providers = root
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap();
    let provider = providers
        .entry("default")
        .or_insert_with(|| serde_json::json!({"base_url":"http://unused/v1","catalog":[]}));
    if provider["base_url"].as_str().is_none_or(str::is_empty) {
        provider["base_url"] = serde_json::json!("http://unused/v1");
    }
    if !provider["catalog"].is_array() {
        provider["catalog"] = serde_json::json!([]);
    }
    let catalog = provider["catalog"].as_array_mut().unwrap();
    for id in ["lead", "sidekick"] {
        if !catalog.iter().any(|entry| entry["id"] == id) {
            catalog.push(serde_json::json!({"id":id}));
        }
    }
    let routes = root
        .entry("routes")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap();
    routes
        .entry("sidekick")
        .or_insert_with(|| serde_json::json!("default/sidekick"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(&file).unwrap()).unwrap();
}

pub(crate) fn fusion_context(
    dir: &std::path::Path,
    log: SessionLog,
    lead: Arc<dyn ProviderAdapter>,
    sidekick: Arc<dyn ProviderAdapter>,
    configured: bool,
) -> Context {
    ensure_fusion_catalog(dir);
    let mut raw = Context::new(
        lead.clone(),
        log,
        crate::tool::builtin_registry(),
        dir.to_path_buf(),
    );
    raw.models = Some(Arc::new(
        crate::models::ModelResolver::load(
            dir,
            crate::models::ProviderDef {
                base_url: "http://unused/v1".into(),
                api_key_env: None,
                api_key: None,
                dialect: "openai".into(),
                catalog: Vec::new(),
                extra: Default::default(),
            },
            "default",
        )
        .with_adapter("default/lead", lead)
        .with_adapter("default/sidekick", sidekick.clone())
        .with_adapter("@sidekick", sidekick),
    ));
    if configured {
        *raw.fusion_models.write().unwrap() = FusionModelSettings {
            lead: Some("default/lead".into()),
            sidekick: Some("default/sidekick".into()),
        };
    }
    raw
}

pub(crate) fn fusion_ctx(
    dir: &std::path::Path,
    log: SessionLog,
    lead: Arc<dyn ProviderAdapter>,
    sidekick: Arc<dyn ProviderAdapter>,
) -> Arc<Context> {
    Arc::new(fusion_context(dir, log, lead, sidekick, true))
}

pub(crate) fn fusion_ctx_unconfigured(
    dir: &std::path::Path,
    log: SessionLog,
    lead: Arc<dyn ProviderAdapter>,
    sidekick: Arc<dyn ProviderAdapter>,
) -> Arc<Context> {
    Arc::new(fusion_context(dir, log, lead, sidekick, false))
}
