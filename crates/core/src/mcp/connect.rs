//! Connection machinery — spec → live client → catalog + tool impls.
//! `connect_all` merges every `mcpServers` layer (project file, plugin
//! manifests, preset roots) and connects each one; `connect_one` drives a
//! single spec to a served client plus its model-facing `McpTool`s.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use anyhow::Context as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::ServiceExt;
use rmcp::transport::TokioChildProcess;
use serde_json::Value;
use sunmao_llm::types::Tool;

use crate::tool::{ToolImpl, ToolResult, archive_prev};

use super::handler::{SessionHandler, Shared};
use super::spec::ServerSpec;
use super::{
    ClientHandle, McpConnected, McpServerHandle, McpToolInfo, UiToolMeta, plugin_manifests,
    prompt_info, resource_info, tool_info,
};

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
/// Prompts and resources are discovered here too: `list_changed`
/// notifications keep the shared catalog current, `prompts/get` resolves
/// through `AgentLoop::mcp_prompt_text` (`/srv:prompt` at submit).
pub(crate) async fn connect_one(
    name: &str,
    spec: &ServerSpec,
) -> anyhow::Result<(McpServerHandle, Vec<Box<dyn ToolImpl>>)> {
    let shared = Shared::new(Vec::new(), Vec::new(), Vec::new());
    let handler = SessionHandler::new(name, shared.clone());
    let client: ClientHandle = if let Some(url) = &spec.url {
        // remote server over streamable-HTTP — headers/auth/timeout land
        // in the transport config; a credential that can't resolve fails
        // this server (warn-and-skip), never a silent unauthenticated call
        let transport = spec.http_transport(url)?;
        Arc::new(handler.serve(transport).await?)
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
        Arc::new(handler.serve(transport).await?)
    };

    let transport = if spec.url.is_some() { "http" } else { "stdio" };
    let listed = client.peer().list_all_tools().await?;
    tracing::info!("mcp server {name}: {} tools", listed.len());
    let catalog: Vec<McpToolInfo> = listed.iter().map(|t| tool_info(name, t)).collect();
    // prompts/resources are discovery surfaces — a server that can't list
    // them keeps its tools (warn-degrade, same spirit as connect_all)
    let prompts = match client.peer().list_all_prompts().await {
        Ok(listed) => Some(listed.iter().map(prompt_info).collect()),
        Err(e) => {
            tracing::debug!("mcp server {name}: prompts/list: {e:#}");
            None
        }
    };
    let resources = match client.peer().list_all_resources().await {
        Ok(listed) => Some(listed.iter().map(resource_info).collect()),
        Err(e) => {
            tracing::debug!("mcp server {name}: resources/list: {e:#}");
            None
        }
    };
    // visibility: ["app"] hides the tool from the MODEL — it still enters
    // the catalog so the island can call it. Tool impls build off our
    // listing, not the shared snapshot — a push may own the catalog by
    // now, and the drain at the next turn boundary converges either way.
    let tools = handle_tools(name, &client, &catalog);
    // the write gate: a pushed list_changed bumps version first and owns
    // the catalog — a bootstrap listing that completes after it must not
    // overwrite the post-push state, so we only write when no push has
    // landed (version still 0)
    {
        let _g = shared.write_gate.lock().unwrap();
        if shared.version.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            *shared.tools.write().unwrap() = catalog;
            if let Some(p) = prompts {
                *shared.prompts.write().unwrap() = p;
            }
            if let Some(r) = resources {
                *shared.resources.write().unwrap() = r;
            }
        }
    }
    Ok((
        McpServerHandle {
            name: name.to_string(),
            transport,
            client,
            shared,
            seen_version: Arc::new(AtomicU64::new(0)),
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
        handle_tools(&self.name, &self.client, &self.tools())
    }
}
