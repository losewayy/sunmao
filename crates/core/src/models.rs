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

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderDef {
    pub base_url: String,
    /// env var holding the API key — absent means keyless (local servers).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// literal key — CLI bootstrap uses this for the session provider;
    /// config files should prefer `api_key_env` (no secrets in files).
    #[serde(default)]
    pub api_key: Option<String>,
    /// "openai" (default) | "anthropic"
    #[serde(default = "default_dialect")]
    pub dialect: String,
}

fn default_dialect() -> String {
    "openai".into()
}

#[derive(Debug, Default, Deserialize)]
pub struct ModelsFile {
    #[serde(default)]
    pub providers: HashMap<String, ProviderDef>,
    /// route name → selector or ordered selector chain
    #[serde(default)]
    pub routes: HashMap<String, RouteValue>,
}

/// `"a/b"` or `["a/b", "@other", ...]` — JSON string-or-list.
#[derive(Debug, Clone, Deserialize)]
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

/// Loads `models.json`, resolves selectors, caches built adapters.
pub struct ModelResolver {
    file: ModelsFile,
    /// provider used for bare `model` selectors — defaults to "default".
    default_provider: String,
    cache: std::sync::Mutex<HashMap<String, Arc<dyn ProviderAdapter>>>,
    /// pre-built adapters keyed by selector — tests inject fakes here so a
    /// routed spawn is observable without real HTTP.
    overrides: HashMap<String, Arc<dyn ProviderAdapter>>,
}

impl ModelResolver {
    /// `default_*` describes the session's own provider (CLI/env), so a bare
    /// selector like `model: "qwen-flash"` keeps working without config.
    pub fn load(cwd: &Path, default_provider: ProviderDef, default_name: &str) -> Self {
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
        file.providers
            .insert(default_name.to_string(), default_provider);
        Self {
            file,
            default_provider: default_name.to_string(),
            cache: std::sync::Mutex::new(HashMap::new()),
            overrides: HashMap::new(),
        }
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
        for sel in self.expand(selector, 0) {
            if let Some(t) = self.resolve_one(&sel) {
                return Some(t);
            }
        }
        None
    }

    /// Expand `@route` aliases into their selector chains (one level of
    /// indirection is enough — deeper nesting is a config smell).
    fn expand(&self, selector: &str, depth: u8) -> Vec<String> {
        if depth > 2 {
            return vec![selector.to_string()];
        }
        if let Some(name) = selector.strip_prefix('@')
            && let Some(route) = self.file.routes.get(name) {
                return route
                    .selectors()
                    .into_iter()
                    .flat_map(|s| self.expand(s, depth + 1))
                    .collect();
            }
        vec![selector.to_string()]
    }

    /// `provider/model` or bare `model` (default provider). A `@route` that
    /// didn't expand (unknown route name) is a dead end, not a model id.
    fn resolve_one(&self, selector: &str) -> Option<ModelTarget> {
        if selector.starts_with('@') {
            return None;
        }
        let (pname, model) = match selector.split_once('/') {
            Some((p, m)) => (p.to_string(), m.to_string()),
            None => (self.default_provider.clone(), selector.to_string()),
        };
        let provider = self.file.providers.get(&pname)?.clone();
        Some(ModelTarget { provider, model })
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

    /// What `/model` offers: `@route` aliases plus each provider as a
    /// `provider/` prefix to pair with a model id.
    pub fn describe(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .file
            .routes
            .iter()
            .map(|(name, r)| format!("@{name} → {}", r.selectors().join(", ")))
            .collect();
        out.extend(
            self.file
                .providers
                .keys()
                .map(|p| format!("{p}/<model-id>")),
        );
        out.sort();
        out
    }

    /// Completable selectors for `/model` argument completion: `@route`
    /// names and `provider/` prefixes — model ids live provider-side and
    /// can't be enumerated, so the prefix is the furthest we complete.
    pub fn selectors(&self) -> Vec<String> {
        let mut out: Vec<String> = self.file.routes.keys().map(|r| format!("@{r}")).collect();
        out.extend(self.file.providers.keys().map(|p| format!("{p}/")));
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> ModelResolver {
        let mut r = ModelResolver::load(
            Path::new("."), // no .sunmao/models.json here — pure defaults
            ProviderDef {
                base_url: "http://local/v1".into(),
                api_key_env: None,
                api_key: None,
                dialect: "openai".into(),
            },
            "default",
        );
        r.file.providers.insert(
            "big".into(),
            ProviderDef {
                base_url: "http://big/v1".into(),
                api_key_env: Some("NOPE_NOT_SET".into()),
                api_key: None,
                dialect: "anthropic".into(),
            },
        );
        r.file.routes.insert(
            "smol".into(),
            RouteValue::Chain(vec!["big/claude-haiku".into(), "tiny-1b".into()]),
        );
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
