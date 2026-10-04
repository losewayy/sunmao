//! Model capability knowledge — the table that fills what gateways don't
//! tell you. A provider's `/models` listing usually returns bare ids;
//! `context_length`, `input_modalities`, thinking levels, tool support all
//! stay blank. The knowledge table supplies those fields by *pattern*: a
//! gateway id is normalized (`cn:deepseek-v4-pro` → `deepseek-v4-pro`,
//! `deepseek/deepseek-v4-pro` → `deepseek-v4-pro`) and matched against
//! curated entries.
//!
//! Three layers merge by `match` key, later wins:
//!   1. the compiled-in seed (`assets/model-knowledge.json` — cold-plug,
//!      generated from OpenRouter's `/api/v1/models`)
//!   2. `~/.sunmao/model-knowledge.json` (user layer — the "刷新知识库"
//!      button writes here)
//!   3. `.sunmao/model-knowledge.json` (project layer — per-pool overrides)
//!
//! Fills are *defaults, not verdicts*: `fill` only writes fields the entry
//! leaves unset, so a gateway-declared or hand-edited value always wins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::models::CatalogEntry;

const SEED: &str = include_str!("../assets/model-knowledge.json");

/// OpenRouter vendor slugs the refresh pulls — mainstream families only;
/// the full catalog (~500 rows, mostly niche) is noise for this table.
const FAMILIES: &[&str] = &[
    "deepseek",
    "qwen",
    "z-ai",
    "openai",
    "anthropic",
    "google",
    "moonshotai",
    "mistralai",
    "meta-llama",
    "x-ai",
    "minimax",
    "tencent",
    "cohere",
    "bytedance-seed",
    "xiaomi",
];

/// One curated capability guess, keyed on the normalized `match` id.
#[derive(Debug, Clone, Default, Deserialize)]
struct KnowledgeEntry {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    max_output: Option<u64>,
    /// non-text input kinds the model accepts: "image" | "audio" | "video" | "file"
    #[serde(default)]
    input: Vec<String>,
    #[serde(default)]
    thinking: Vec<String>,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    tools: bool,
    #[serde(default)]
    structured: bool,
}

/// `~/.sunmao` — the user layer's home. Mirrors the IM workspace's
/// home-dir rule (USERPROFILE first, HOME fallback).
pub fn sunmao_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".sunmao")
}

/// `id` → match key: lowercase, vendor path (`deepseek/`) stripped, pool
/// prefix (`cn:`/`global:`/`openai:`) stripped — the head of a `:` split
/// counts as a pool tag only when it's pure letters, so a variant suffix
/// (`o3:batch` → `o3-batch`, head `o3` carries a digit) survives instead
/// of collapsing to `batch`. A bare id that isn't a real model
/// (`global:auto-chat`) simply matches nothing — honest miss.
fn normalize(id: &str) -> String {
    let mut s = id.trim().to_lowercase();
    if let Some(i) = s.rfind('/') {
        s = s[i + 1..].to_string();
    }
    if let Some((head, tail)) = s.split_once(':')
        && !head.is_empty()
        && head.chars().all(|c| c.is_ascii_alphabetic())
    {
        s = tail.to_string();
    }
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// The merged table. Cheap to rebuild — callers construct per reload.
pub struct Knowledge {
    by_key: HashMap<String, KnowledgeEntry>,
}

impl Knowledge {
    /// Seed + user + project layers. `cwd` scopes the project layer.
    pub fn load(cwd: &Path) -> Self {
        let mut by_key: HashMap<String, KnowledgeEntry> = HashMap::new();
        let mut merge = |text: &str| {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
                && let Some(rows) = v.get("models").and_then(|m| m.as_array())
            {
                for row in rows {
                    let Some(key) = row.get("match").and_then(|k| k.as_str()) else {
                        continue;
                    };
                    if let Ok(e) = serde_json::from_value::<KnowledgeEntry>(row.clone()) {
                        by_key.insert(normalize(key), e);
                    }
                }
            }
        };
        merge(SEED);
        for p in [
            sunmao_home().join("model-knowledge.json"),
            cwd.join(".sunmao").join("model-knowledge.json"),
        ] {
            if let Ok(text) = std::fs::read_to_string(&p) {
                merge(&text);
            }
        }
        Self { by_key }
    }

    /// Best match for a normalized id: exact, then the longest table key
    /// inside the id (`deepseek-v4-pro` ⊆ `deepseek-v4-pro-0813`), then
    /// the longest key containing the id (a dated upstream variant).
    fn lookup(&self, id: &str) -> Option<&KnowledgeEntry> {
        let norm = normalize(id);
        if let Some(e) = self.by_key.get(&norm) {
            return Some(e);
        }
        self.by_key
            .iter()
            .filter(|(k, _)| norm.contains(k.as_str()) || k.contains(norm.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, e)| e)
    }

    /// Fill only unset fields — declared/hand-edited values always win.
    pub fn fill(&self, e: &mut CatalogEntry) {
        let Some(k) = self.lookup(&e.id) else { return };
        if e.context_length.is_none() {
            e.context_length = k.context;
        }
        if e.max_output.is_none() {
            e.max_output = crate::models::sane_max_output(e.context_length, k.max_output);
        }
        if e.input_modalities.is_empty() && !k.input.is_empty() {
            e.input_modalities = k.input.clone();
        }
        if e.thinking.is_empty() && !e.reasoning {
            if !k.thinking.is_empty() {
                e.thinking = k.thinking.clone();
            } else {
                e.reasoning = k.reasoning;
            }
        }
        e.supports_tools |= k.tools;
        e.structured_outputs |= k.structured;
        e.migrate_vision();
    }
}

/// Pull OpenRouter's catalog + litellm's community registry, keep the
/// mainstream families, write the user layer
/// (`~/.sunmao/model-knowledge.json`). OpenRouter is the structural source
/// (modalities, supported_parameters); litellm is the curated source for
/// `max_output` — provider self-reports there are documented limits, not
/// the "context × 0.9" formulas gateways hand back. An optional
/// freshness action, never a runtime dependency.
pub async fn refresh_user_layer() -> anyhow::Result<usize> {
    let client = reqwest::Client::builder()
        .user_agent("sunmao/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let (v, ltm_raw) = tokio::try_join!(
        async {
            client
                .get("https://openrouter.ai/api/v1/models")
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await
        },
        async {
            client
                .get("https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json")
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await
        },
    )?;
    let rows = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow::anyhow!("listing has no data[]"))?;
    // litellm keys carry provider prefixes (`azure_ai/x`, `sail/z-ai/Y`)
    // and duplicates disagree — bucket by normalized basename, and per
    // model take the largest sane value: provider-limited deployments
    // report LOW ceilings (a 32k-window host caps output at 32k), while
    // the documented model cap is the biggest believable one
    let mut ltm: HashMap<String, Vec<u64>> = HashMap::new();
    if let Some(map) = ltm_raw.as_object() {
        for (k, e) in map {
            if let Some(m) = e.get("max_output_tokens").and_then(|x| x.as_u64()) {
                ltm.entry(normalize(k)).or_default().push(m);
            }
        }
    }
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let Some(id) = row.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        let vendor = id.split('/').next().unwrap_or("").trim_start_matches('~');
        if !FAMILIES.contains(&vendor) || id.starts_with('~') || id.ends_with(":batch") {
            continue;
        }
        let ctx = row
            .get("context_length")
            .or_else(|| row.pointer("/top_provider/context_length"))
            .and_then(|c| c.as_u64());
        let Some(ctx) = ctx else { continue };
        let key = normalize(id);
        if !seen.insert(key.clone()) {
            continue;
        }
        let sp: Vec<&str> = row
            .get("supported_parameters")
            .and_then(|s| s.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        let mut e = serde_json::json!({"match": key, "context": ctx});
        // max_output: prefer the curated registry's largest sane value;
        // the gateway's own report is only a fallback (it shrugs in
        // "context × 0.9" formulas that fail the sanity bands)
        let ltm_best = ltm
            .get(&key)
            .into_iter()
            .flatten()
            .filter_map(|&m| crate::models::sane_max_output(Some(ctx), Some(m)))
            .max();
        let or_best = crate::models::sane_max_output(
            Some(ctx),
            row.pointer("/top_provider/max_completion_tokens")
                .and_then(|m| m.as_u64()),
        );
        if let Some(mo) = ltm_best.or(or_best) {
            e["max_output"] = mo.into();
        }
        if let Some(serde_json::Value::Array(inp)) = row.pointer("/architecture/input_modalities") {
            let kinds: Vec<&serde_json::Value> =
                inp.iter().filter(|m| m.as_str() != Some("text")).collect();
            if !kinds.is_empty() {
                e["input"] = serde_json::json!(kinds);
            }
        }
        if sp.contains(&"reasoning_effort") {
            e["thinking"] = serde_json::json!(["low", "medium", "high", "max"]);
        } else if sp.iter().any(|s| s.contains("reasoning")) {
            e["reasoning"] = serde_json::json!(true);
        }
        if sp.contains(&"tools") {
            e["tools"] = true.into();
        }
        if sp.contains(&"structured_outputs") {
            e["structured"] = true.into();
        }
        out.push(e);
    }
    out.sort_by(|a, b| {
        a["match"]
            .as_str()
            .unwrap_or("")
            .cmp(b["match"].as_str().unwrap_or(""))
    });
    let dir = sunmao_home();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("model-knowledge.json"),
        serde_json::to_string_pretty(&serde_json::json!({"models": out}))?,
    )?;
    Ok(out.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_pool_and_vendor() {
        assert_eq!(normalize("cn:deepseek-v4-pro"), "deepseek-v4-pro");
        assert_eq!(normalize("global:glm-5.3-flash"), "glm-5-3-flash");
        assert_eq!(normalize("deepseek/deepseek-v4-pro"), "deepseek-v4-pro");
        assert_eq!(normalize("openai/o4-mini"), "o4-mini");
        // variant suffixes survive — `o3` carries a digit so the head is
        // not a pool tag
        assert_eq!(normalize("openai/o3:batch"), "o3-batch");
        assert_eq!(normalize("gemini-2.0-flash:free"), "gemini-2-0-flash-free");
    }

    #[test]
    fn fill_only_writes_unset_fields() {
        let mut k = Knowledge {
            by_key: HashMap::new(),
        };
        k.by_key.insert(
            "deepseek-v4-pro".into(),
            KnowledgeEntry {
                context: Some(1_000_000),
                max_output: Some(128_000),
                input: vec!["image".into()],
                thinking: vec!["low".into(), "high".into()],
                tools: true,
                ..Default::default()
            },
        );
        let mut e = CatalogEntry {
            id: "cn:deepseek-v4-pro".into(),
            context_length: Some(512_000), // gateway declared — must win
            ..Default::default()
        };
        k.fill(&mut e);
        assert_eq!(e.context_length, Some(512_000));
        assert_eq!(e.max_output, Some(128_000));
        assert_eq!(e.input_modalities, vec!["image"]);
        assert_eq!(e.thinking, vec!["low", "high"]);
        assert!(e.supports_tools);
    }

    #[test]
    fn legacy_vision_migrates_into_modalities() {
        let mut e = CatalogEntry {
            id: "x".into(),
            vision: true,
            ..Default::default()
        };
        e.migrate_vision();
        assert_eq!(e.input_modalities, vec!["image"]);
        e.migrate_vision(); // idempotent
        assert_eq!(e.input_modalities, vec!["image"]);
    }

    #[test]
    fn lookup_prefers_exact_then_longest() {
        let mut k = Knowledge {
            by_key: HashMap::new(),
        };
        for (key, ctx) in [("glm-5", 1u64), ("glm-5-1", 2), ("glm-5v-turbo", 3)] {
            k.by_key.insert(
                key.into(),
                KnowledgeEntry {
                    context: Some(ctx),
                    ..Default::default()
                },
            );
        }
        // exact wins over the `glm-5` prefix
        assert_eq!(k.lookup("cn:glm-5.1").unwrap().context, Some(2));
        // a longer gateway variant still lands on the base key
        assert_eq!(
            k.lookup("cn:glm-5v-turbo-preview").unwrap().context,
            Some(3)
        );
        // unknown ids miss honestly rather than grabbing a false positive
        assert!(k.lookup("global:auto-chat").is_none());
    }
}
