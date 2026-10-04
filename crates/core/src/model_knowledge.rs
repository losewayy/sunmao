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
//!      curated; `thinking`/`reasoning` rows are normalized through the
//!      doc-verified `assets/thinking-levels.txt` table at load)
//!   2. `~/.sunmao/model-knowledge.json` (user layer — hand-editable;
//!      the old OpenRouter refresh that wrote here was removed because
//!      gateway guesses kept clobbering verified fields)
//!   3. `.sunmao/model-knowledge.json` (project layer — per-pool overrides)
//!
//! Fills are *defaults, not verdicts*: `fill` only writes fields the entry
//! leaves unset, so a gateway-declared or hand-edited value always wins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::models::CatalogEntry;

const SEED: &str = include_str!("../assets/model-knowledge.json");

/// `assets/thinking-levels.txt` — the doc-verified effort vocabulary per
/// model family. Refresh consults it BEFORE trusting a gateway's bare
/// `supported_parameters` list: `reasoning_effort` appearing there only
/// says *a* level field exists, never which strings it accepts (DeepSeek
/// takes high|max, GLM-5.3 low|high|max, Qwen3.8 low|medium|xhigh — the
/// uniform guess got all of those wrong).
const LEVELS: &str = include_str!("../assets/thinking-levels.txt");

/// What a family accepts: a level vocabulary, a bare toggle, or
/// (for a known non-thinking member of a thinking family) nothing at all.
enum LevelSet {
    Levels(Vec<String>),
    Toggle,
    None,
}

/// Tiny glob: `*` spans any run; no wildcard = exact match. Patterns run
/// against `normalize()`d ids (`glm-5.3-flash` → `glm-5-3-flash`).
fn glob_match(pat: &str, s: &str) -> bool {
    if !pat.contains('*') {
        return pat == s;
    }
    let mut rest = s;
    let mut first = true;
    let anchored = !pat.starts_with('*');
    for part in pat.split('*') {
        if part.is_empty() {
            continue;
        }
        if first && anchored {
            if !rest.starts_with(part) {
                return false;
            }
            rest = &rest[part.len()..];
        } else if let Some(p) = rest.find(part) {
            rest = &rest[p + part.len()..];
        } else {
            return false;
        }
        first = false;
    }
    pat.ends_with('*') || rest.is_empty()
}

/// First table row whose glob hits the normalized id wins.
fn level_set(norm: &str) -> Option<LevelSet> {
    for line in LEVELS.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (globs, rhs) = line.split_once('|')?;
        if globs.split(',').any(|g| glob_match(g.trim(), norm)) {
            return if rhs.trim() == "-" {
                Some(LevelSet::Toggle)
            } else if rhs.trim() == "!" {
                Some(LevelSet::None)
            } else {
                Some(LevelSet::Levels(
                    rhs.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                ))
            };
        }
    }
    None
}

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
                    if let Ok(mut e) = serde_json::from_value::<KnowledgeEntry>(row.clone()) {
                        // the curated table heals stale guesses in older
                        // layers — a refresh-era `low,medium,high,max`
                        // on deepseek/glm gets overwritten by the real set
                        match level_set(&normalize(key)) {
                            Some(LevelSet::Levels(ls)) => {
                                e.thinking = ls;
                                e.reasoning = false;
                            }
                            Some(LevelSet::Toggle) => {
                                e.thinking.clear();
                                e.reasoning = true;
                            }
                            Some(LevelSet::None) => {
                                e.thinking.clear();
                                e.reasoning = false;
                            }
                            None => {}
                        }
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

/// Heal refresh-era `thinking` values saved into `models.json` catalogs.
/// The old pipeline stamped exactly two generated vocabularies —
/// `low,medium,high,max` (its universal guess) and `low,medium,high` (the
/// supported_parameters fallback). A declared set equal to either is
/// generated data, not a user choice; re-derive it from the family table.
/// Any other combination is hand-edited and stays untouched.
pub fn heal_refresh_levels(e: &mut CatalogEntry) {
    if e.thinking.is_empty() {
        return;
    }
    let mut s: Vec<&str> = e.thinking.iter().map(String::as_str).collect();
    s.sort_unstable();
    // sorted: low,medium,high,max → high,low,max,medium ; low,medium,high → high,low,medium
    let generated = s == ["high", "low", "max", "medium"] || s == ["high", "low", "medium"];
    if !generated {
        return;
    }
    match level_set(&normalize(&e.id)) {
        Some(LevelSet::Levels(ls)) => {
            e.thinking = ls;
            e.reasoning = false;
        }
        Some(LevelSet::Toggle) => {
            e.thinking.clear();
            e.reasoning = true;
        }
        Some(LevelSet::None) => {
            e.thinking.clear();
            e.reasoning = false;
        }
        None => {}
    }
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
    fn level_table_matches_doc_verified_families() {
        let lv = |id| match level_set(&normalize(id)) {
            Some(LevelSet::Levels(v)) => v.join(","),
            Some(LevelSet::Toggle) => "-".into(),
            Some(LevelSet::None) => "!".into(),
            None => "?".into(),
        };
        assert_eq!(lv("cn:deepseek-v4-pro"), "high,max");
        assert_eq!(lv("deepseek-v3-2"), "high,max");
        assert_eq!(lv("glm-5-3"), "low,high,max");
        assert_eq!(lv("glm-5-2"), "none,minimal,low,medium,high,xhigh,max");
        assert_eq!(lv("glm-4-6"), "-");
        assert_eq!(lv("kimi-k3"), "low,high,max");
        assert_eq!(lv("kimi-k2-6"), "-");
        assert_eq!(lv("claude-opus-4-6"), "low,medium,high,max");
        assert_eq!(lv("claude-opus-5"), "low,medium,high,xhigh,max");
        assert_eq!(lv("claude-sonnet-4-5"), "-");
        assert_eq!(lv("openai/gpt-5-1"), "none,low,medium,high");
        assert_eq!(lv("openai/o3"), "low,medium,high");
        assert_eq!(lv("qwen3-8-max"), "low,medium,xhigh");
        assert_eq!(lv("gemini-3-5-flash"), "minimal,low,medium,high");
        assert_eq!(lv("grok-4-6"), "low,medium,high,xhigh");
        assert_eq!(lv("minimax-m2-7"), "-");
        assert_eq!(lv("seed-2-1-pro"), "minimal,low,medium,high");
        // the new-generation rows the table gained in 2026-10
        assert_eq!(lv("gpt-6-astra"), "low,medium,high,xhigh,max");
        assert_eq!(lv("gpt-6-1-sol"), "low,medium,high");
        assert_eq!(lv("minimax-m3"), "none,high");
        assert_eq!(lv("gemini-3-1-flash-lite"), "minimal,low,high");
        assert_eq!(lv("deepseek-v4-1-flash"), "high,max");
        // `!` actively suppresses non-thinking members of thinking families
        assert_eq!(lv("deepseek-chat"), "!");
        assert_eq!(lv("kimi-k2"), "!");
        assert_eq!(lv("kimi-k2-6"), "-");
        // unlisted families fall through to the caller's fallback
        assert_eq!(lv("llama-4-scout"), "?");
    }

    #[test]
    fn heal_only_repairs_the_two_generated_signatures() {
        let entry = |id: &str, t: &[&str]| CatalogEntry {
            id: id.into(),
            thinking: t.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        // the universal stamp gets re-derived from the table
        let mut e = entry("cn:deepseek-v4-flash", &["low", "medium", "high", "max"]);
        heal_refresh_levels(&mut e);
        assert_eq!(e.thinking, ["high", "max"]);
        // the sp-fallback stamp too — and toggle-only shapes land as
        // reasoning, not levels
        let mut e = entry("cn:kimi-k2.6", &["low", "medium", "high"]);
        heal_refresh_levels(&mut e);
        assert!(e.thinking.is_empty() && e.reasoning);
        // `!` rows clear both
        let mut e = entry("global:deepseek-chat", &["low", "medium", "high", "max"]);
        heal_refresh_levels(&mut e);
        assert!(e.thinking.is_empty() && !e.reasoning);
        // a hand-picked set (or the same letters out of the known orders)
        // is a user choice — untouched
        let mut e = entry("cn:deepseek-v4-flash", &["high", "max"]);
        heal_refresh_levels(&mut e);
        assert_eq!(e.thinking, ["high", "max"]);
        let mut e = entry("cn:glm-5.3", &["low", "max", "xhigh"]);
        heal_refresh_levels(&mut e);
        assert_eq!(e.thinking, ["low", "max", "xhigh"]);
    }

    #[test]
    fn glob_handles_anchors_and_spans() {
        assert!(glob_match("deepseek-v4*", "deepseek-v4-pro-0813"));
        assert!(!glob_match("deepseek-v4*", "deepseek-v3-2"));
        assert!(glob_match("gemini-*pro*", "gemini-3-1-pro-preview"));
        assert!(!glob_match("gemini-*pro*", "gemini-3-5-flash"));
        assert!(glob_match("o4-*", "o4-mini"));
        assert!(!glob_match("o4-*", "o3"));
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
