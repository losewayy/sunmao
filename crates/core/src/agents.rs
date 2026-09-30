//! Named sub-agent definitions — `.sunmao/agents/*.md`, `.claude/agents/*.md`.
//! Frontmatter `name`/`description`/`model`/`tools`/`spawns`, body becomes
//! the sub-agent's system prompt. `model` is a `models.json` selector
//! (`provider/model`, bare model id, or `@route`) — absent means inherit
//! the parent's model. `tools` (CSV or `[a,b]`) trims the child's tool
//! registry — absent means the full builtin set. `spawns` (CSV or `[a,b]`,
//! `*` = unrestricted) whitelists which agent names this one may itself
//! spawn; absent means unrestricted, `[]` means the child can't Task at all.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    /// Optional model selector resolved through `ModelResolver`.
    pub model: Option<String>,
    /// Tool-name whitelist — `None` = full builtin registry.
    pub tools: Option<Vec<String>>,
    /// Spawnable agent names — `None` = unrestricted, `Some([])` = none.
    pub spawns: Option<Vec<String>>,
}

/// Load all agent definitions under the convention dirs. `extra_roots` are
/// enabled preset dirs — their `agents/` subdirs append after the installed
/// plugins (a same-named preset def fills the gap; first hit still wins).
pub fn load_all(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<AgentDef> {
    let mut dirs = vec![
        cwd.join(".sunmao/agents"),
        cwd.join(".claude/agents"),
        cwd.join(".sunmao/plugin/agents"),
    ];
    for base in [cwd.join(".sunmao/plugins"), cwd.join(".claude/plugins")] {
        for p in crate::sorted_entries(&base) {
            dirs.push(p.path().join("agents"));
        }
    }
    for root in extra_roots {
        dirs.push(root.join("agents"));
    }
    let mut out = Vec::new();
    for dir in dirs {
        for e in crate::sorted_entries(&dir) {
            let p = e.path();
            if p.extension().map(|x| x == "md").unwrap_or(false)
                && let Ok(text) = std::fs::read_to_string(&p)
                && let Some(def) = parse(&text, &p)
            {
                out.push(def);
            }
        }
    }
    out
}

fn parse(text: &str, path: &Path) -> Option<AgentDef> {
    let mut name = path.file_stem()?.to_string_lossy().to_string();
    let mut desc = String::new();
    let mut model = None;
    let mut tools = None;
    let mut spawns = None;
    let mut body = text;

    // YAML-lite frontmatter: --- name: x description: y model: @route ---
    if let Some(rest) = text.strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
        for line in rest[..end].lines() {
            if let Some(v) = line.strip_prefix("name:") {
                name = v.trim().to_string();
            }
            if let Some(v) = line.strip_prefix("description:") {
                desc = v.trim().to_string();
            }
            if let Some(v) = line.strip_prefix("model:") {
                let v = v.trim();
                if !v.is_empty() {
                    model = Some(v.to_string());
                }
            }
            if let Some(v) = line.strip_prefix("tools:") {
                tools = Some(parse_list(v));
            }
            if let Some(v) = line.strip_prefix("spawns:") {
                let v = v.trim();
                // `*` (and the missing field) means unrestricted; an
                // empty value means "can't spawn at all".
                spawns = if v == "*" { None } else { Some(parse_list(v)) };
            }
        }
        body = &rest[end + 4..];
    }
    Some(AgentDef {
        name,
        description: desc,
        system_prompt: body.trim().to_string(),
        model,
        tools,
        spawns,
    })
}

/// `a, b` or `[a, b]` — both shapes appear in the wild.
fn parse_list(v: &str) -> Vec<String> {
    v.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Look up one definition by exact name — the spawn path needs the def
/// more than once (prompt, model, tools, spawns), so callers hold it.
pub fn find(cwd: &Path, extra_roots: &[PathBuf], name: &str) -> Option<AgentDef> {
    load_all(cwd, extra_roots)
        .into_iter()
        .find(|d| d.name == name)
}
