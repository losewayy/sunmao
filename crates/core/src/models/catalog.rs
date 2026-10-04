//! `/models` listing fetch + row→`CatalogEntry` parse — the network half
//! of the model surface, split out of `models.rs` (routing/resolution
//! stays there). Everything here is best-effort: a listing that omits
//! capability fields just yields bare ids, and the knowledge table
//! (`crate::model_knowledge`) fills the gaps downstream.

use super::{CatalogEntry, ProviderDef};

/// Fetch a provider's model catalog — `GET {base_url}/models` with the
/// provider's key. Auth header follows the dialect (Anthropic uses
/// `x-api-key` + version, everyone else gets a Bearer token). Returns
/// best-effort entries: `data[].id` plus whatever capabilities the listing
/// advertises (context length, image input, reasoning levels). A listing
/// that omits them just yields bare ids — no field is mandatory.
pub async fn fetch_catalog(provider: &ProviderDef) -> anyhow::Result<Vec<CatalogEntry>> {
    use anyhow::Context as _;
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let key = provider
        .api_key_env
        .as_deref()
        .and_then(|env| std::env::var(env).ok())
        .or_else(|| provider.api_key.clone())
        .unwrap_or_default();
    let client = reqwest::Client::builder()
        .user_agent("sunmao/0.1")
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut req = client.get(&url);
    if provider.dialect == "anthropic" {
        req = req
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01");
    } else if !key.is_empty() {
        req = req.bearer_auth(&key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("http {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await.context("models listing is not json")?;
    // OpenAI shape: {"data":[{id,…}]}; Anthropic: {"data":[{id,…}]} too.
    // Also accept a bare array — some gateways return it.
    let rows: &[serde_json::Value] = match (v.pointer("/data"), &v) {
        (Some(serde_json::Value::Array(a)), _) => a,
        (_, serde_json::Value::Array(a)) => a,
        _ => anyhow::bail!("models listing has no data[]"),
    };
    let mut out = Vec::new();
    for row in rows {
        let id = row
            .pointer("/id")
            .and_then(|i| i.as_str())
            .filter(|i| !i.is_empty());
        let Some(id) = id else { continue };
        out.push(catalog_entry(row, id));
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// One `data[]` row → CatalogEntry, pulling the capability fields gateways
/// actually emit: `context_length`/`context_window`, `input_modalities`
/// (OpenRouter shape) or `capabilities`, `reasoning`/`thinking` support.
fn catalog_entry(row: &serde_json::Value, id: &str) -> CatalogEntry {
    let get = |paths: &[&str]| -> Option<&serde_json::Value> {
        paths.iter().find_map(|p| row.pointer(p))
    };
    let context_length = get(&[
        "/context_length",
        "/context_window",
        "/max_input_tokens",
        "/max_context_length",
    ])
    .and_then(|v| v.as_u64());
    let vision = get(&["/input_modalities", "/modalities/input", "/capabilities"])
        .map(|v| match v {
            serde_json::Value::Array(a) => a
                .iter()
                .any(|m| matches!(m.as_str(), Some("image") | Some("video"))),
            serde_json::Value::Object(o) => o
                .get("image")
                .and_then(|b| b.as_bool())
                .or_else(|| o.get("vision").and_then(|b| b.as_bool()))
                .unwrap_or(false),
            _ => false,
        })
        .unwrap_or(false);
    // full input kinds — OpenRouter's `architecture.input_modalities` is
    // the canonical shape; plain `input_modalities`/`modalities.input`
    // arrays carry the same vocabulary. `text` is implied, not stored.
    let mut input_modalities: Vec<String> = Vec::new();
    for path in [
        "/architecture/input_modalities",
        "/input_modalities",
        "/modalities/input",
    ] {
        if let Some(serde_json::Value::Array(kinds)) = row.pointer(path) {
            for k in kinds {
                if let Some(s) = k.as_str()
                    && s != "text"
                    && !input_modalities.iter().any(|m| m == s)
                {
                    input_modalities.push(s.to_string());
                }
            }
            break;
        }
    }
    // completion ceiling — OpenRouter nests it under `top_provider`; a
    // few listings flatten it
    let max_output = get(&[
        "/top_provider/max_completion_tokens",
        "/max_completion_tokens",
        "/max_output_tokens",
    ])
    .and_then(|v| v.as_u64());
    // `supported_parameters` enumerates wire features — `tools`/`tool_choice`
    // = function calling, `structured_outputs`/`response_format` = JSON mode
    let has_param = |names: &[&str]| {
        get(&["/supported_parameters"])
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str())
                    .any(|p| names.contains(&p))
            })
            .unwrap_or(false)
    };
    let supports_tools = has_param(&["tools", "tool_choice", "functions"]);
    let structured_outputs = has_param(&["structured_outputs", "response_format"]);
    let mut thinking = Vec::new();
    for path in ["/supported_reasoning", "/reasoning_levels", "/thinking"] {
        if let Some(serde_json::Value::Array(levels)) = row.pointer(path) {
            for l in levels {
                if let Some(s) = l.as_str()
                    && !thinking.iter().any(|t| t == s)
                {
                    thinking.push(s.to_string());
                }
            }
        }
    }
    // reasoning support without a level vocabulary: OpenRouter's
    // `supported_parameters` names it, Anthropic-style listings carry a
    // `reasoning` field — either flag lets callers offer the canonical
    // low/medium/high trio rather than a silently-absent picker.
    let reasoning = get(&[
        "/supported_parameters",
        "/reasoning",
        "/capabilities/reasoning",
    ])
    .map(|v| match v {
        serde_json::Value::Array(a) => a.iter().any(
            |m| matches!(m.as_str(), Some(s) if s.contains("reasoning") || s.contains("thinking")),
        ),
        serde_json::Value::Bool(b) => *b,
        _ => true, // a non-boolean `reasoning` value exists = supported
    })
    .unwrap_or(false);
    let mut e = CatalogEntry {
        id: id.to_string(),
        vision,
        input_modalities,
        context_length,
        max_output,
        thinking,
        reasoning,
        supports_tools,
        structured_outputs,
    };
    e.migrate_vision();
    e
}
