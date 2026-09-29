//! MCP client — spawn stdio servers, surface their tools as native tools.
//!
//! Config: `.sunmao/mcp.json`, Claude's mcpServers shape:
//! ```json
//! {"mcpServers": {"fs": {"command": "npx", "args": ["-y", "srv"], "env": {}}}}
//! ```
//! Tools register as `mcp__{server}__{tool}` — name-spacing keeps the model's
//! view unambiguous and the audit trail attributable.

use std::collections::HashMap;
use std::path::Path;
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
struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    servers: HashMap<String, ServerSpec>,
}

#[derive(Debug, Deserialize)]
struct ServerSpec {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
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
            params = params.with_arguments(obj.clone().into());
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
pub async fn connect_all(cwd: &Path) -> Vec<Box<dyn ToolImpl>> {
    let path = cwd.join(".sunmao").join("mcp.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let config: McpConfig = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("bad .sunmao/mcp.json: {e}");
            return Vec::new();
        }
    };

    let mut tools: Vec<Box<dyn ToolImpl>> = Vec::new();
    for (name, spec) in config.servers {
        match connect_one(&name, &spec).await {
            Ok(t) => tools.extend(t),
            Err(e) => tracing::warn!("mcp server {name} failed: {e:#}"),
        }
    }
    tools
}

async fn connect_one(name: &str, spec: &ServerSpec) -> anyhow::Result<Vec<Box<dyn ToolImpl>>> {
    let mut cmd = tokio::process::Command::new(&spec.command);
    cmd.args(&spec.args)
        .envs(&spec.env)
        // MCP servers read stdin, write stdout; silence stderr to keep
        // servers that log startup noise from polluting the protocol
        .stderr(std::process::Stdio::null());
    let transport = TokioChildProcess::new(cmd)?;
    let client: ClientHandle = Arc::new(().serve(transport).await?);

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
