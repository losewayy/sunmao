use std::sync::Arc;

use crate::context::{Context, FusionModelSettings};
use crate::session::{SessionEvent, SessionLog};
use sunmao_llm::types::Usage;
use sunmao_llm::{ProviderAdapter, StreamDelta, ToolCallFragment};

/// Scripted response helpers shared by every fusion test file.
pub(crate) fn tool_call(id: &str, name: &str, args: &str) -> Vec<StreamDelta> {
    vec![
        StreamDelta::ToolCalls(vec![
            ToolCallFragment {
                index: 0,
                id: Some(id.into()),
                name: Some(name.into()),
                arguments: None,
            },
            ToolCallFragment {
                index: 0,
                arguments: Some(args.into()),
                ..Default::default()
            },
        ]),
        StreamDelta::Finish {
            reason: Some("tool_calls".into()),
            usage: None,
        },
    ]
}

pub(crate) fn text(s: &str) -> Vec<StreamDelta> {
    vec![
        StreamDelta::Content(s.into()),
        StreamDelta::Finish {
            reason: Some("stop".into()),
            usage: Some(Usage::default()),
        },
    ]
}

pub(crate) fn queued(
    v: Vec<Vec<StreamDelta>>,
) -> std::sync::Mutex<std::collections::VecDeque<Vec<StreamDelta>>> {
    std::sync::Mutex::new(std::collections::VecDeque::from(v))
}

/// Fold a context's session log — the assertion surface for durable facts.
pub(crate) async fn events(ctx: &Arc<Context>) -> Vec<SessionEvent> {
    ctx.sessions.lock().await.events().await.unwrap_or_default()
}

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
            ..Default::default()
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
