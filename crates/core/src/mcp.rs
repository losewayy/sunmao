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
use rmcp::RoleClient;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::TokioChildProcess;
use serde::Deserialize;
use serde_json::Value;
use sunmao_llm::types::Tool;

use crate::tool::{ToolImpl, ToolResult, archive_prev};

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

/// MCP Apps (SEP-1865) tool metadata — `_meta.ui` on a tools/list entry.
/// `resource_uri` names the `ui://` resource the host renders as the
/// tool's View; `visibility` decides who may call it — the model-facing
/// registry gets only `"model"` tools, the app bridge (GUI island →
/// `tools/call` proxy) gets `"app"`.
#[derive(Debug, Clone, Default)]
pub struct UiToolMeta {
    pub resource_uri: Option<String>,
    /// raw visibility list (`["model","app"]` default when absent)
    pub visibility: Vec<String>,
}

impl UiToolMeta {
    fn app_visible(&self) -> bool {
        self.visibility.is_empty() || self.visibility.iter().any(|v| v == "app")
    }
}

/// One tool a connected server advertised — the catalog the app bridge
/// checks `tools/call` requests against (visibility + ui linkage).
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    /// wire name in our registry: `mcp__{server}__{tool}`
    pub name: String,
    /// the bare tool name the server knows
    pub server_tool: String,
    pub description: String,
    /// declared input schema, verbatim (the registry rebuilds tools from
    /// this — one connection can front several session Contexts)
    pub schema: Value,
    pub ui: Option<UiToolMeta>,
    /// `_meta.ui.visibility` allows app-initiated calls (default true)
    pub app_visible: bool,
}

/// A connected MCP server — the app bridge needs the handle to proxy
/// `resources/read` (ui:// fetch) and `tools/call` out of the island.
#[derive(Clone)]
pub struct McpServerHandle {
    pub name: String,
    pub client: ClientHandle,
    pub tools: Vec<McpToolInfo>,
}

/// What `connect_all` assembled: model-facing tools plus the per-server
/// handles/catalogs the MCP Apps host bridge rides on.
pub struct McpConnected {
    pub tools: Vec<Box<dyn ToolImpl>>,
    pub servers: Vec<McpServerHandle>,
}

/// Parse `_meta.ui` off a listed tool — the nested `ui.resourceUri` shape
/// plus the deprecated flat `ui/resourceUri` (pre-GA servers still ship it).
fn ui_meta(tool: &rmcp::model::Tool) -> Option<UiToolMeta> {
    let meta = tool.meta.as_ref()?;
    let ui = meta.0.get("ui").map(|v| UiToolMeta {
        resource_uri: v
            .get("resourceUri")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string()),
        visibility: v
            .get("visibility")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
    });
    match ui {
        Some(mut m) => {
            if m.resource_uri.is_none() {
                m.resource_uri = meta
                    .0
                    .get("ui/resourceUri")
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string());
            }
            Some(m)
        }
        None => meta
            .0
            .get("ui/resourceUri")
            .and_then(|u| u.as_str())
            .map(|s| UiToolMeta {
                resource_uri: Some(s.to_string()),
                visibility: Vec::new(),
            }),
    }
}

/// One MCP server tool wrapped as a native [`ToolImpl`].
struct McpTool {
    server: String,
    tool_name: String,
    description: String,
    schema: Value,
    client: ClientHandle,
    ui: Option<UiToolMeta>,
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

    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult> {
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
        // MCP Apps (SEP-1865): the tool's `_meta.ui.resourceUri` names a
        // `ui://` resource on the same server — fetch it, persist it as an
        // artifact + a `{name}.ui.json` sidecar carrying the call's input
        // and raw result (the GUI's sandboxed island bridges off it).
        if let Some(ui) = &self.ui
            && let Some(uri) = &ui.resource_uri
            && let Err(e) = self.land_ui_artifact(uri, &args, &res, ctx).await
        {
            tracing::warn!("mcp ui resource {uri}: {e:#}");
        }
        Ok(ToolResult {
            output: out.trim_end().to_string(),
            ok: res.is_error != Some(true),
        })
    }
}

fn artifact_slug(server: &str, tool: &str) -> String {
    let mut s = format!("mcp-{server}-{tool}").to_ascii_lowercase();
    s.retain(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    s
}

impl McpTool {
    async fn land_ui_artifact(
        &self,
        uri: &str,
        args: &Value,
        res: &rmcp::model::CallToolResult,
        ctx: &crate::context::Context,
    ) -> anyhow::Result<()> {
        use rmcp::model::ReadResourceRequestParams;
        let rr = self
            .client
            .peer()
            .read_resource_once(ReadResourceRequestParams::new(uri.to_string()))
            .await?;
        let read = match rr {
            rmcp::model::ReadResourceResponse::Complete(r) => r,
            _ => anyhow::bail!("unexpected resource response shape"),
        };
        let content = read
            .contents
            .into_iter()
            .next()
            .context("ui resource empty")?;
        let (html, res_meta) = match content {
            rmcp::model::ResourceContents::TextResourceContents { text, meta, .. } => (text, meta),
            rmcp::model::ResourceContents::BlobResourceContents { blob, meta, .. } => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&blob)
                    .context("ui resource blob: bad base64")?;
                (
                    String::from_utf8(bytes).context("ui resource blob: not utf-8")?,
                    meta,
                )
            }
            _ => anyhow::bail!("unknown resource contents variant"),
        };
        // CSP the sandbox must enforce — declared domains ride in the
        // sidecar, the GUI host builds the iframe headers from them.
        let csp = res_meta
            .as_ref()
            .and_then(|m| m.0.get("ui"))
            .and_then(|u| u.get("csp"))
            .cloned();
        let dir = ctx.cwd.join(".sunmao").join("artifacts");
        tokio::fs::create_dir_all(&dir).await?;
        let name = artifact_slug(&self.server, &self.tool_name);
        let rev = archive_prev(&dir, &name).await?;
        let path = dir.join(format!("{name}.html"));
        let bytes = html.len();
        tokio::fs::write(&path, &html).await?;
        let sidecar = dir.join(format!("{name}.ui.json"));
        tokio::fs::write(
            &sidecar,
            serde_json::to_string_pretty(&serde_json::json!({
                "server": self.server,
                "tool": self.tool_name,
                "uri": uri,
                "csp": csp,
                "arguments": args,
                "result": res,
            }))?,
        )
        .await?;
        ctx.sessions
            .lock()
            .await
            .append(&crate::session::SessionEvent::Artifact {
                name: name.clone(),
                path: path.display().to_string(),
                bytes,
                rev,
            })
            .await?;
        if let Some(sink) = ctx.live_sink.get() {
            sink.on_event(&crate::agent::LiveEvent::Artifact {
                name,
                path: path.display().to_string(),
                bytes,
                rev,
            });
        }
        Ok(())
    }
}

/// Connect to every configured server, collect tools + server handles.
/// Failures degrade to a warning — one bad server must not brick the session.
/// `extra_roots` are enabled preset dirs, layered after the installed
/// plugins — a same-named preset server overrides an installed one.
/// `McpConnected.servers` feeds `Context.mcp_servers`: the MCP Apps island
/// bridge proxies `tools/call`/`resources/read` through these handles, and
/// app-only tools (visibility `["app"]`) never reach the model registry.
pub async fn connect_all(cwd: &Path, extra_roots: &[PathBuf]) -> McpConnected {
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
    let mut handles: Vec<McpServerHandle> = Vec::new();
    for (name, spec) in servers {
        match connect_one(&name, &spec).await {
            Ok((handle, t)) => {
                handles.push(handle);
                tools.extend(t);
            }
            Err(e) => tracing::warn!("mcp server {name} failed: {e:#}"),
        }
    }
    McpConnected {
        tools,
        servers: handles,
    }
}

/// Model-visible tools ride the registry; every tool (incl. app-only)
/// lands in the handle's catalog so the island bridge can enforce
/// `visibility` itself — the model never sees `["app"]` tools.
async fn connect_one(
    name: &str,
    spec: &ServerSpec,
) -> anyhow::Result<(McpServerHandle, Vec<Box<dyn ToolImpl>>)> {
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
    let catalog: Vec<McpToolInfo> = listed
        .iter()
        .map(|t| {
            let ui = ui_meta(t);
            McpToolInfo {
                name: format!("mcp__{name}__{}", t.name),
                server_tool: t.name.to_string(),
                description: t.description.as_deref().unwrap_or_default().to_string(),
                schema: serde_json::to_value(&t.input_schema).unwrap_or_else(|_| json_object()),
                app_visible: ui.as_ref().map(|u| u.app_visible()).unwrap_or(true),
                ui,
            }
        })
        .collect();
    // visibility: ["app"] hides the tool from the MODEL — it still enters
    // the catalog so the island can call it.
    let tools = handle_tools(name, &client, &catalog);
    Ok((
        McpServerHandle {
            name: name.to_string(),
            client,
            tools: catalog,
        },
        tools,
    ))
}

/// Rebuild a handle's model-facing tool set — one MCP connection can front
/// several session Contexts (the `serve` multi-session host), each needs
/// its own `McpTool` instances because `Box<dyn ToolImpl>` isn't Clone.
fn handle_tools(
    name: &str,
    client: &ClientHandle,
    catalog: &[McpToolInfo],
) -> Vec<Box<dyn ToolImpl>> {
    catalog
        .iter()
        .filter(|info| {
            info.ui
                .as_ref()
                .map(|u| u.visibility.is_empty() || u.visibility.iter().any(|v| v == "model"))
                .unwrap_or(true)
        })
        .map(|info| {
            Box::new(McpTool {
                server: name.to_string(),
                tool_name: info.server_tool.clone(),
                description: if info.description.is_empty() {
                    format!("mcp tool {name}:{}", info.server_tool)
                } else {
                    info.description.clone()
                },
                schema: info.schema.clone(),
                client: client.clone(),
                ui: info.ui.clone(),
            }) as Box<dyn ToolImpl>
        })
        .collect()
}

impl McpServerHandle {
    /// Fresh tool instances for another Context sharing this connection —
    /// the island bridge and the model registry stay per-session.
    pub fn tool_impls(&self) -> Vec<Box<dyn ToolImpl>> {
        handle_tools(&self.name, &self.client, &self.tools)
    }
}

fn json_object() -> Value {
    serde_json::json!({"type": "object"})
}

#[cfg(test)]
mod tests;
