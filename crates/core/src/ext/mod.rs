//! Extension host — the `sunmao` JSON-RPC extension protocol from
//! `docs/PROTOCOLS.md`: one spawned child per extension per session,
//! line-delimited JSON-RPC 2.0 on stdin/stdout, `ext__{plugin}__{tool}`
//! namespaced tools, `ext/event` replies folded into the same
//! `HookOutcome` command hooks produce.
//!
//! Manifest source is the plugin bundle's `plugin.json` — an
//! `"extensions": [{"command","args","env"}]` array resolved by
//! [`crate::mcp::plugin_manifests`], the same scan MCP uses, so presets
//! carry extensions identically. Cold-plug: spawned at session start,
//! dead with it, never hot-plugged. A dying extension degrades to a
//! warning — same rule as `mcp.rs`, one bad child bricks only itself.

mod proto;
mod registry;

pub use registry::{ExtChild, ExtInit, ExtRegistry};

/// Scan the manifest set and spawn every declared extension.
/// Per-child failures warn and continue; the session must survive a
/// broken plugin. `cwd`/`session_id`/`transcript_path` become the
/// `ext/initialize` params — the extension sees the same session facts
/// hooks do.
pub(crate) async fn connect_all(
    reg: &ExtRegistry,
    cwd: &std::path::Path,
    session_id: &str,
    extra_roots: &[std::path::PathBuf],
) {
    let transcript_path = crate::session::session_log_path(cwd, session_id)
        .display()
        .to_string()
        .replace("\\\\?\\", "");
    let init = ExtInit {
        cwd: cwd.display().to_string().replace("\\\\?\\", ""),
        session_id: session_id.to_string(),
        transcript_path,
    };
    for (spec, plugin_name) in resolve_specs(cwd, extra_roots) {
        if let Err(e) = reg.connect(&spec, &init, &plugin_name).await {
            tracing::warn!("extension {plugin_name} failed: {e:#}");
        }
    }
}

/// The `{"extensions": [...]}` entries across every plugin manifest,
/// paired with the plugin's tool-namespace name.
fn resolve_specs(
    cwd: &std::path::Path,
    extra_roots: &[std::path::PathBuf],
) -> Vec<(ExtSpec, String)> {
    let mut specs = Vec::new();
    for (manifest, root) in crate::mcp::plugin_manifests(cwd, extra_roots) {
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(file) = serde_json::from_str::<serde_json::Value>(&text) else {
            tracing::warn!("bad {}", manifest.display());
            continue;
        };
        let name = plugin_name(&file, &root);
        let Some(entries) = file
            .get("extensions")
            .and_then(|e| serde_json::from_value::<Vec<ExtSpec>>(e.clone()).ok())
        else {
            continue;
        };
        for mut spec in entries {
            spec.expand_plugin_root(&root.display().to_string().replace("\\\\?\\", ""));
            specs.push((spec, name.clone()));
        }
    }
    specs
}

/// The extension's tool namespace — the bundle's `plugin.json` `name`
/// field, falling back to the root dir's basename. Sanitized: model
/// tool-name grammars reject anything outside `[a-zA-Z0-9_-]`, and the
/// `.sunmao`/`.claude-plugin` dirnames would otherwise leak dots.
fn plugin_name(manifest: &serde_json::Value, root: &std::path::Path) -> String {
    let raw = manifest
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "ext".into());
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `{"command","args","env"}` spawn descriptor — same shape MCP server
/// specs use, minus the `url` branch (extensions are stdio only).
#[derive(Debug, serde::Deserialize)]
pub struct ExtSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

impl ExtSpec {
    fn expand_plugin_root(&mut self, root: &str) {
        self.command = crate::mcp::expand_plugin_root(&self.command, root);
        for a in &mut self.args {
            *a = crate::mcp::expand_plugin_root(a, root);
        }
        for v in self.env.values_mut() {
            *v = crate::mcp::expand_plugin_root(v, root);
        }
    }
}

#[cfg(test)]
mod tests;
