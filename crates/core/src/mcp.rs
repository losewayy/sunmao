//! MCP client — spawn stdio servers, surface their tools as native tools.
//!
//! Config: `.sunmao/mcp.json`, Claude's mcpServers shape:
//! ```json
//! {"mcpServers": {"fs": {"command": "npx", "args": ["-y", "srv"], "env": {}}}}
//! ```
//! Tools register as `mcp__{server}__{tool}` — name-spacing keeps the model's
//! view unambiguous and the audit trail attributable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::TokioChildProcess;
use rmcp::RoleClient;
use serde::Deserialize;
use serde_json::Value;
use sunmao_llm::types::Tool;

use crate::tool::{ToolImpl, ToolResult};

#[derive(Debug, Deserialize)]
struct ServerSpec {
    /// stdio transport: spawn this command.
    #[serde(default)]
    command: Option<String>,
    /// streamable-HTTP transport: connect to this URL instead.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

impl ServerSpec {
    /// Expand `${CLAUDE_PLUGIN_ROOT}` in command/args/env — plugin manifests
    /// use it to address files inside their own bundle.
    fn expand_plugin_root(&mut self, root: &str) {
        if let Some(c) = &mut self.command {
            *c = expand_plugin_root(c, root);
        }
        for a in &mut self.args {
            *a = expand_plugin_root(a, root);
        }
        for v in self.env.values_mut() {
            *v = expand_plugin_root(v, root);
        }
    }
}

/// `${CLAUDE_PLUGIN_ROOT}` substitution — shared by MCP server specs and
/// extension specs; `root` arrives pre-stripped of the `\\?\` prefix.
pub(crate) fn expand_plugin_root(text: &str, root: &str) -> String {
    text.replace("${CLAUDE_PLUGIN_ROOT}", root)
        .replace("$CLAUDE_PLUGIN_ROOT", root)
}

/// Every plugin manifest the session should read, paired with its bundle
/// root: convention manifests, installed plugin dirs, then `extra_roots`
/// (preset dirs — they can carry servers/extensions like any bundle).
/// Presets without a `plugin.json` fall back to a bare `mcp.json` —
/// reading a nonexistent manifest is the caller's `continue`.
pub(crate) fn plugin_manifests(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let mut manifests: Vec<(PathBuf, PathBuf)> = Vec::new(); // (manifest, plugin_root)
    for p in [
        cwd.join(".sunmao").join("plugin.json"),
        cwd.join(".claude-plugin").join("plugin.json"),
        // "this project is a plugin" — the bundled manifest is a manifest
        cwd.join(".sunmao").join("plugin").join("plugin.json"),
    ] {
        if let Some(root) = p.parent().map(|d| d.to_path_buf()) {
            manifests.push((p, root));
        }
    }
    for base in [
        cwd.join(".sunmao").join("plugins"),
        cwd.join(".claude").join("plugins"),
    ] {
        for entry in crate::sorted_entries(&base) {
            let root = entry.path();
            let manifest = root.join("plugin.json");
            if root.is_dir() && manifest.exists() {
                manifests.push((manifest, root));
            }
        }
    }
    // presets: plugin.json is the bundle manifest; a bare mcp.json covers
    // presets that ship only servers and no manifest.
    for root in extra_roots {
        let manifest = if root.join("plugin.json").exists() {
            root.join("plugin.json")
        } else {
            root.join("mcp.json")
        };
        manifests.push((manifest, root.clone()));
    }
    manifests
}

type ClientHandle = Arc<RunningService<RoleClient, ()>>;

/// One MCP server tool wrapped as a native [`ToolImpl`].
struct McpTool {
    server: String,
    tool_name: String,
    description: String,
    schema: Value,
    client: ClientHandle,
}

#[async_trait::async_trait]
impl ToolImpl for McpTool {
    fn name(&self) -> &'static str {
        // leaked name is fine: tool names are process-lifetime constants
        Box::leak(format!("mcp__{}__{}", self.server, self.tool_name).into_boxed_str())
    }

    fn decl(&self) -> Tool {
        Tool::function(
            &format!("mcp__{}__{}", self.server, self.tool_name),
            &self.description,
            self.schema.clone(),
        )
    }

    async fn call(
        &self,
        args: Value,
        _ctx: &crate::context::Context,
    ) -> anyhow::Result<ToolResult> {
        let mut params = CallToolRequestParams::new(self.tool_name.clone());
        if let Some(obj) = args.as_object() {
            params = params.with_arguments(obj.clone());
        }
        let res = self
            .client
            .peer()
            .call_tool(params)
            .await
            .context("mcp call_tool failed")?;
        let mut out = String::new();
        for c in &res.content {
            if let Some(t) = c.as_text() {
                out.push_str(&t.text);
                out.push('\n');
            }
        }
        if out.is_empty() {
            out = serde_json::to_string_pretty(&res.structured_content).unwrap_or_default();
        }
        Ok(ToolResult {
            output: out.trim_end().to_string(),
            ok: res.is_error != Some(true),
        })
    }
}

/// Connect to every configured server, collect tools. Failures degrade to a
/// warning — one bad server must not brick the session.
/// `extra_roots` are enabled preset dirs, layered after the installed
/// plugins — a same-named preset server overrides an installed one.
pub async fn connect_all(cwd: &Path, extra_roots: &[PathBuf]) -> Vec<Box<dyn ToolImpl>> {
    // merge mcpServers from .sunmao/mcp.json + plugin manifests — the
    // plugin.json bundle format contributes MCP servers the same way.
    // `${CLAUDE_PLUGIN_ROOT}` inside a manifest's command/args/env expands
    // to the plugin's own directory.
    let mut servers: std::collections::HashMap<String, ServerSpec> = Default::default();
    // project mcp.json first — the manifest layers below override it, as
    // PROTOCOLS documents
    let project_spec = cwd.join(".sunmao").join("mcp.json");
    let layered: Vec<(PathBuf, PathBuf)> = std::iter::once(project_spec)
        .map(|p| (p, cwd.join(".sunmao")))
        .chain(plugin_manifests(cwd, extra_roots))
        .collect();
    for (p, root) in layered {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            tracing::warn!("bad {}", p.display());
            continue;
        };
        if let Some(mut m) = v.get("mcpServers").and_then(|m| {
            serde_json::from_value::<std::collections::HashMap<String, ServerSpec>>(m.clone()).ok()
        }) {
            let root = root.display().to_string().replace("\\\\?\\", "");
            for spec in m.values_mut() {
                spec.expand_plugin_root(&root);
            }
            servers.extend(m);
        }
    }

    let mut tools: Vec<Box<dyn ToolImpl>> = Vec::new();
    for (name, spec) in servers {
        match connect_one(&name, &spec).await {
            Ok(t) => tools.extend(t),
            Err(e) => tracing::warn!("mcp server {name} failed: {e:#}"),
        }
    }
    tools
}

async fn connect_one(name: &str, spec: &ServerSpec) -> anyhow::Result<Vec<Box<dyn ToolImpl>>> {
    let client: ClientHandle = if let Some(url) = &spec.url {
        // remote server over streamable-HTTP (MCP 2025-03-26 transport)
        let transport = rmcp::transport::StreamableHttpClientTransport::from_uri(url.clone());
        Arc::new(().serve(transport).await?)
    } else {
        let Some(command) = &spec.command else {
            anyhow::bail!("server {name}: needs `command` (stdio) or `url` (http)");
        };
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&spec.args)
            .envs(&spec.env)
            // MCP servers read stdin, write stdout; silence stderr to keep
            // servers that log startup noise from polluting the protocol
            .stderr(std::process::Stdio::null());
        let transport = TokioChildProcess::new(cmd)?;
        Arc::new(().serve(transport).await?)
    };

    let listed = client.peer().list_all_tools().await?;
    tracing::info!("mcp server {name}: {} tools", listed.len());
    Ok(listed
        .into_iter()
        .map(|t| {
            Box::new(McpTool {
                server: name.to_string(),
                tool_name: t.name.to_string(),
                description: t
                    .description
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| format!("mcp tool {}:{}", name, t.name)),
                schema: serde_json::to_value(&t.input_schema).unwrap_or(json_object()),
                client: client.clone(),
            }) as Box<dyn ToolImpl>
        })
        .collect())
}

fn json_object() -> Value {
    serde_json::json!({"type": "object"})
}
