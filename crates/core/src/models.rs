//! Model routing — name → provider adapter, resolved lazily.
//!
//! `.sunmao/models.json` declares named providers and routes; agent defs
//! pin a `model:` selector that resolves through this table. A selector is
//! `provider/model`, a bare `model` (default provider), or `@route`
//! (a named chain — each entry tried in order). Unknown/unresolvable
//! selectors resolve to `None` and the caller falls back to the session
//! model — a typo must not break a spawn.
//!
//! Keys never live in the file: `api_key_env` names the environment
//! variable to read at resolve time.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use sunmao_llm::ProviderAdapter;

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct ProviderDef {
    pub base_url: String,
    /// env var holding the API key — absent means keyless (local servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// literal key — CLI bootstrap uses this for the session provider;
    /// config files should prefer `api_key_env` (no secrets in files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// "openai" (default) | "anthropic"
    #[serde(default = "default_dialect")]
    pub dialect: String,
    /// the provider's model catalog — filled by `POST /models/fetch` or by
    /// hand; entries become concrete `provider/id` selectors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub catalog: Vec<CatalogEntry>,
}

fn default_dialect() -> String {
    "openai".into()
}

#[derive(Debug, Default, Clone, serde::Serialize, Deserialize)]
pub struct ModelsFile {
    #[serde(default)]
    pub providers: HashMap<String, ProviderDef>,
    /// route name → selector or ordered selector chain
    #[serde(default)]
    pub routes: HashMap<String, RouteValue>,
}

/// Which files feed a resolver — `.sunmao/models.json` then `.claude/` for
/// compat, both under `cwd`. `reload()` re-reads them after a GUI edit.
fn read_models_file(cwd: &Path) -> ModelsFile {
    let mut file = ModelsFile::default();
    for dir in [cwd.join(".sunmao"), cwd.join(".claude")] {
        if let Ok(text) = std::fs::read_to_string(dir.join("models.json")) {
            match serde_json::from_str::<ModelsFile>(&text) {
                Ok(f) => file = f,
                Err(e) => {
                    tracing::warn!("{}: ignoring invalid models.json — {e}", dir.display())
                }
            }
        }
    }
    file
}

/// `"a/b"` or `["a/b", "@other", ...]` — JSON string-or-list.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(untagged)]
pub enum RouteValue {
    One(String),
    Chain(Vec<String>),
}

impl RouteValue {
    fn selectors(&self) -> Vec<&str> {
        match self {
            RouteValue::One(s) => vec![s.as_str()],
            RouteValue::Chain(v) => v.iter().map(|s| s.as_str()).collect(),
        }
    }
}

/// One configured route target: which provider shape + which model id.
#[derive(Debug, Clone)]
pub struct ModelTarget {
    pub provider: ProviderDef,
    pub model: String,
}

/// A model the provider advertises — the GUI catalog row. `id` is the wire
/// name; the rest are best-effort capabilities (a `/models` listing that
/// omits them just shows less).
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    /// true when the model takes image input — from `input_modalities`/
    /// `capabilities` in a /models listing, or hand-edited.
    #[serde(default, skip_serializing_if = "is_false")]
    pub vision: bool,
    /// advertised context window, when the listing reports one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// declared thinking levels — free-form ("low"/"high", …) since
    /// providers don't agree on a vocabulary
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking: Vec<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Loads `models.json`, resolves selectors, caches built adapters.
/// The file is behind a `RwLock` so `reload()` can hot-swap it after a
/// GUI edit without rebuilding sessions.
pub struct ModelResolver {
    file: std::sync::RwLock<ModelsFile>,
    /// where the file was loaded from — `reload()` rescans it
    cwd: std::path::PathBuf,
    /// provider used for bare `model` selectors — defaults to "default".
    default_provider: String,
    /// the session's own provider def — re-registered on every reload so
    /// edits can never delete it
    default_def: ProviderDef,
    cache: std::sync::Mutex<HashMap<String, Arc<dyn ProviderAdapter>>>,
    /// pre-built adapters keyed by selector — tests inject fakes here so a
    /// routed spawn is observable without real HTTP.
    overrides: HashMap<String, Arc<dyn ProviderAdapter>>,
}

impl ModelResolver {
    /// `default_*` describes the session's own provider (CLI/env), so a bare
    /// selector like `model: "qwen-flash"` keeps working without config.
    /// A file entry under the same name contributes its `catalog` — the
    /// process's own credentials still win, but the GUI-managed model list
    /// survives the re-registration.
    pub fn load(cwd: &Path, default_provider: ProviderDef, default_name: &str) -> Self {
        let mut file = read_models_file(cwd);
        let catalog = file
            .providers
            .get(default_name)
            .map(|p| p.catalog.clone())
            .unwrap_or_default();
        let mut def = default_provider;
        if !catalog.is_empty() {
            def.catalog = catalog;
        }
        file.providers.insert(default_name.to_string(), def.clone());
        Self {
            file: std::sync::RwLock::new(file),
            cwd: cwd.to_path_buf(),
            default_provider: default_name.to_string(),
            default_def: def,
            cache: std::sync::Mutex::new(HashMap::new()),
            overrides: HashMap::new(),
        }
    }

    /// Re-read the models file(s) — the GUI's provider editor writes the
    /// file then calls this through the host so the session picks up new
    /// providers/catalog without a restart. The default provider is
    /// re-registered (keeping its file-managed catalog); the adapter cache
    /// clears so edited keys take effect.
    pub fn reload(&self) {
        let mut file = read_models_file(&self.cwd);
        let catalog = file
            .providers
            .get(&self.default_provider)
            .map(|p| p.catalog.clone())
            .unwrap_or_default();
        let mut def = self.default_def.clone();
        if !catalog.is_empty() {
            def.catalog = catalog;
        }
        file.providers.insert(self.default_provider.clone(), def);
        *self.file.write().unwrap() = file;
        self.cache.lock().unwrap().clear();
    }

    /// Bind a selector to a ready-made adapter — overrides file resolution.
    /// Exists for tests and for embedders that own adapter construction.
    pub fn with_adapter(mut self, selector: &str, adapter: Arc<dyn ProviderAdapter>) -> Self {
        self.overrides.insert(selector.to_string(), adapter);
        self
    }

    /// Selector → concrete target. Chains (`@route` or inline lists in the
    /// file) try each member in order; the first resolvable one wins.
    /// `None` = nothing matched — callers fall back, never hard-fail.
    pub fn resolve(&self, selector: &str) -> Option<ModelTarget> {
        let file = self.file.read().unwrap();
        for sel in expand(selector, &file, 0) {
            if let Some(t) = self.resolve_one(&file, &sel) {
                return Some(t);
            }
        }
        None
    }

    /// Build (or fetch from cache) the adapter for a resolved target.
    pub fn adapter(&self, t: &ModelTarget) -> Option<Arc<dyn ProviderAdapter>> {
        let key = format!("{}\u{0}{}", t.provider.base_url, t.model);
        if let Some(a) = self.cache.lock().unwrap().get(&key) {
            return Some(a.clone());
        }
        let key_str = t
            .provider
            .api_key_env
            .as_deref()
            .and_then(|env| std::env::var(env).ok())
            .or_else(|| t.provider.api_key.clone())
            .unwrap_or_default();
        let adapter: Arc<dyn ProviderAdapter> = match t.provider.dialect.as_str() {
            "anthropic" => Arc::new(sunmao_llm::AnthropicClient::new(
                &t.provider.base_url,
                key_str,
                &t.model,
            )),
            _ => Arc::new(sunmao_llm::OaiClient::new(
                &t.provider.base_url,
                key_str,
                &t.model,
            )),
        };
        self.cache.lock().unwrap().insert(key, adapter.clone());
        Some(adapter)
    }

    /// Selector → ready adapter, or `None` (caller inherits parent's).
    /// Selector-level overrides win over file resolution.
    pub fn adapter_for(&self, selector: &str) -> Option<Arc<dyn ProviderAdapter>> {
        if let Some(a) = self.overrides.get(selector) {
            return Some(a.clone());
        }
        self.resolve(selector).and_then(|t| self.adapter(&t))
    }

    /// `provider/model` or bare `model` (default provider). A `@route` that
    /// didn't expand (unknown route name) is a dead end, not a model id.
    fn resolve_one(&self, file: &ModelsFile, selector: &str) -> Option<ModelTarget> {
        if selector.starts_with('@') {
            return None;
        }
        let (pname, model) = match selector.split_once('/') {
            Some((p, m)) => (p.to_string(), m.to_string()),
            None => (self.default_provider.clone(), selector.to_string()),
        };
        let provider = file.providers.get(&pname)?.clone();
        Some(ModelTarget { provider, model })
    }

    /// Read-access to the merged config — the GUI's settings page renders
    /// this; mutation goes through the file + `reload()`, never in place.
    pub fn file(&self) -> ModelsFile {
        self.file.read().unwrap().clone()
    }

    /// What `/model` offers: `@route` aliases, catalog entries as concrete
    /// `provider/id` selectors, and `provider/` prefixes for anything not
    /// catalogued yet.
    pub fn describe(&self) -> Vec<String> {
        let file = self.file.read().unwrap();
        let mut out: Vec<String> = file
            .routes
            .iter()
            .map(|(name, r)| format!("@{name} → {}", r.selectors().join(", ")))
            .collect();
        for (name, p) in &file.providers {
            for m in &p.catalog {
                out.push(format!("{name}/{}", m.id));
            }
            if p.catalog.is_empty() {
                out.push(format!("{name}/<model-id>"));
            }
        }
        out.sort();
        out
    }

    /// Completable selectors for `/model` argument completion: `@route`
    /// names, concrete catalog ids, and `provider/` prefixes (a model id
    /// that isn't catalogued still resolves — the provider may know it).
    pub fn selectors(&self) -> Vec<String> {
        let file = self.file.read().unwrap();
        let mut out: Vec<String> = file.routes.keys().map(|r| format!("@{r}")).collect();
        for (name, p) in &file.providers {
            out.push(format!("{name}/"));
            for m in &p.catalog {
                out.push(format!("{name}/{}", m.id));
            }
        }
        out.sort();
        out
    }

    /// The provider name bare selectors resolve through — the GUI marks it
    /// so the settings page can flag the process's own credentials row.
    pub fn default_provider(&self) -> String {
        self.default_provider.clone()
    }
}

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
    CatalogEntry {
        id: id.to_string(),
        vision,
        context_length,
        thinking,
    }
}

/// Expand `@route` aliases into their selector chains (one level of
/// indirection is enough — deeper nesting is a config smell).
fn expand(selector: &str, file: &ModelsFile, depth: u8) -> Vec<String> {
    if depth > 2 {
        return vec![selector.to_string()];
    }
    if let Some(name) = selector.strip_prefix('@')
        && let Some(route) = file.routes.get(name)
    {
        return route
            .selectors()
            .into_iter()
            .flat_map(|s| expand(s, file, depth + 1))
            .collect();
    }
    vec![selector.to_string()]
}

#[cfg(test)]
mod tests {
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
        let mut file = r.file.write().unwrap();
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
}
