//! Named sub-agent definitions — `.sunmao/agents/*.md`, `.claude/agents/*.md`.
//! Frontmatter `name`/`description`/`model`, body becomes the sub-agent's
//! system prompt. `model` is a `models.json` selector (`provider/model`,
//! bare model id, or `@route`) — absent means inherit the parent's model.

use std::path::Path;

pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    /// Optional model selector resolved through `ModelResolver`.
    pub model: Option<String>,
}

/// Load all agent definitions under the convention dirs.
pub fn load_all(cwd: &Path) -> Vec<AgentDef> {
    let mut dirs = vec![
        cwd.join(".sunmao/agents"),
        cwd.join(".claude/agents"),
        cwd.join(".sunmao/plugin/agents"),
    ];
    for base in [cwd.join(".sunmao/plugins"), cwd.join(".claude/plugins")] {
        if let Ok(ps) = std::fs::read_dir(&base) {
            for p in ps.flatten() {
                dirs.push(p.path().join("agents"));
            }
        }
    }
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "md").unwrap_or(false) {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    if let Some(def) = parse(&text, &p) {
                        out.push(def);
                    }
                }
            }
        }
    }
    out
}

fn parse(text: &str, path: &Path) -> Option<AgentDef> {
    let mut name = path.file_stem()?.to_string_lossy().to_string();
    let mut desc = String::new();
    let mut model = None;
    let mut body = text;

    // YAML-lite frontmatter: --- name: x description: y model: @route ---
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
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
            }
            body = &rest[end + 4..];
        }
    }
    Some(AgentDef {
        name,
        description: desc,
        system_prompt: body.trim().to_string(),
        model,
    })
}
