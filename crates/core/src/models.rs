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

use crate::context::{MutexRecover, RwLockRecover};
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
    /// "openai" (default) | "openai-responses" | "anthropic"
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
/// The layers MERGE per key (a `.claude` provider/route overrides its
/// same-named `.sunmao` twin) — wholesale replacement used to let an empty
/// `.claude/models.json` wipe the project's whole routing table.
fn read_models_file(cwd: &Path) -> ModelsFile {
    let mut file = ModelsFile::default();
    for dir in [cwd.join(".sunmao"), cwd.join(".claude")] {
        if let Ok(text) = std::fs::read_to_string(dir.join("models.json")) {
            match serde_json::from_str::<ModelsFile>(&text) {
                Ok(f) => {
                    file.providers.extend(f.providers);
                    file.routes.extend(f.routes);
                }
                Err(e) => {
                    tracing::warn!("{}: ignoring invalid models.json — {e}", dir.display())
                }
            }
        }
    }
    // capability defaults: legacy `vision` folds into `input_modalities`,
    // then the knowledge table fills whatever the file leaves unset —
    // declared values always win, so a hand edit can never be overwritten
    let knowledge = crate::model_knowledge::Knowledge::load(cwd);
    for p in file.providers.values_mut() {
        for e in &mut p.catalog {
            e.migrate_vision();
            knowledge.fill(e);
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
#[derive(Debug, Default, Clone, serde::Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    /// legacy image flag — kept for reads of older files; `migrate_vision`
    /// folds it into `input_modalities` and writers stop emitting it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub vision: bool,
    /// non-text input kinds: "image" | "audio" | "video" | "file" — from
    /// `architecture.input_modalities` (OpenRouter spelling), a listing's
    /// `input_modalities`, or the knowledge table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    /// advertised context window, when the listing reports one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// advertised completion ceiling — distinct from `context_length`
    /// (a 1M window does not mean 1M out)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
    /// declared thinking levels — free-form ("low"/"high", …) since
    /// providers don't agree on a vocabulary
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking: Vec<String>,
    /// listing signals reasoning support without naming levels —
    /// `supported_parameters`/capabilities containing `reasoning` (the
    /// OpenRouter spelling) or a `reasoning` field. Readers may offer the
    /// canonical low/medium/high trio on this hint.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reasoning: bool,
    /// tool calling is in `supported_parameters` — absent = unknown, not
    /// "unsupported" (a bare listing proves nothing either way)
    #[serde(default, skip_serializing_if = "is_false")]
    pub supports_tools: bool,
    /// `structured_outputs`/`response_format` advertised — same caveat
    #[serde(default, skip_serializing_if = "is_false")]
    pub structured_outputs: bool,
}

impl CatalogEntry {
    /// Legacy `vision:true` → `input_modalities += "image"`. Idempotent;
    /// run after load and after every knowledge/fetch fill.
    pub fn migrate_vision(&mut self) {
        if self.vision && !self.input_modalities.iter().any(|m| m == "image") {
            self.input_modalities.push("image".into());
        }
    }
}

/// Is this a believable completion ceiling? Provider listings are full of
/// formula values — `context × 0.9`, `context × 0.8` — that mean "the API
/// accepts a max_tokens up to the window", not a documented model limit
/// (OpenRouter relays ~944K on a 1M window; real documented caps top out
/// around DeepSeek V4's 384K). Two-band rule: an absolute ceiling, then a
/// ratio catch for formula noise in smaller windows. Anything failing
/// both is shown as an honest blank — never an invented number.
pub fn sane_max_output(context: Option<u64>, max_out: Option<u64>) -> Option<u64> {
    match (context, max_out) {
        (_, Some(m)) if m > 400_000 => None,
        (Some(c), Some(m)) if m * 4 >= c * 3 => None,
        _ => max_out,
    }
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
        *self.file.write_or_recover() = file;
        self.cache.lock_or_recover().clear();
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
        let file = self.file.read_or_recover();
        for sel in expand(selector, &file, 0) {
            if let Some(t) = self.resolve_one(&file, &sel) {
                return Some(t);
            }
        }
        None
    }

    /// Build (or fetch from cache) the adapter for a resolved target.
    /// The key carries the provider's identity, not just the endpoint —
    /// two providers may share a base_url+model with different dialects or
    /// credential sources. The literal key participates as a DefaultHasher
    /// digest so the map never stores the secret itself.
    pub fn adapter(&self, t: &ModelTarget) -> Option<Arc<dyn ProviderAdapter>> {
        let key_hash = t.provider.api_key.as_deref().map(|k| {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            k.hash(&mut h);
            format!("{:x}", h.finish())
        });
        let key = format!(
            "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
            t.provider.base_url,
            t.provider.dialect,
            t.provider.api_key_env.as_deref().unwrap_or_default(),
            key_hash.as_deref().unwrap_or_default(),
            t.model,
        );
        if let Some(a) = self.cache.lock_or_recover().get(&key) {
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
            "openai-responses" => Arc::new(sunmao_llm::ResponsesClient::new(
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
        self.cache.lock_or_recover().insert(key, adapter.clone());
        Some(adapter)
    }

    /// Resolved context window for a selector — the provider catalog's
    /// `context_length` when a `/models` fetch (or a hand-written entry)
    /// advertised one. `None` = unknown; callers fall back to a session
    /// default. Used to size auto-compaction against the model the turn
    /// is *actually* on, not a global constant.
    pub fn context_length_for(&self, selector: &str) -> Option<u64> {
        let t = self.resolve(selector)?;
        let file = self.file.read_or_recover();
        let provider = file
            .providers
            .values()
            .find(|p| p.base_url == t.provider.base_url && p.dialect == t.provider.dialect)?;
        provider
            .catalog
            .iter()
            .find(|e| e.id == t.model)
            .and_then(|e| e.context_length)
    }

    /// Thinking levels a selector's model advertises — the catalog's
    /// declared `thinking` list, or the canonical low/medium/high trio
    /// when the listing signaled reasoning support without naming levels
    /// (`reasoning` flag). Empty = the model doesn't advertise thinking —
    /// frontends should hide the picker rather than invent levels.
    pub fn thinking_levels(&self, selector: &str) -> Vec<String> {
        let Some(t) = self.resolve(selector) else {
            return Vec::new();
        };
        let file = self.file.read_or_recover();
        let Some(provider) = file
            .providers
            .values()
            .find(|p| p.base_url == t.provider.base_url && p.dialect == t.provider.dialect)
        else {
            return Vec::new();
        };
        let Some(entry) = provider.catalog.iter().find(|e| e.id == t.model) else {
            return Vec::new();
        };
        if !entry.thinking.is_empty() {
            return entry.thinking.clone();
        }
        if entry.reasoning {
            return ["low", "medium", "high"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
        Vec::new()
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
        self.file.read_or_recover().clone()
    }

    /// What `/model` offers: `@route` aliases, catalog entries as concrete
    /// `provider/id` selectors, and `provider/` prefixes for anything not
    /// catalogued yet.
    pub fn describe(&self) -> Vec<String> {
        let file = self.file.read_or_recover();
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
        let file = self.file.read_or_recover();
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

/// `/models` listing fetch + row→CatalogEntry parse — the network half
/// of this module, split for the file budget.
mod catalog;
pub use catalog::fetch_catalog;

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
mod tests;
