//! MCP client — spawn stdio servers, surface their tools as native tools.
//!
//! Config: `.sunmao/mcp.json`, Claude's mcpServers shape:
//! ```json
//! {"mcpServers": {"fs": {"command": "npx", "args": ["-y", "srv"], "env": {}}}}
//! ```
//! Remote (`url`) servers additionally accept `headers`, `auth_env`,
//! `token_file` and `timeout_secs` (see `spec.rs`). Tools register as
//! `mcp__{server}__{tool}` — name-spacing keeps the model's view
//! unambiguous and the audit trail attributable. Server-advertised
//! prompts surface as `/srv:prompt` slash commands; `list_changed`
//! notifications refresh the catalogs at the next turn boundary, and
//! elicitation requests get a protocol error (this client never prompts
//! mid-tool) plus a session note.

use crate::context::{MutexRecover, RwLockRecover};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use rmcp::RoleClient;
use rmcp::service::RunningService;
use serde_json::Value;

mod connect;
#[doc(hidden)]
pub mod handler; // visible only because ClientHandle embeds it
mod spec;

use handler::{SessionHandler, Shared};

#[cfg(test)]
pub(crate) use connect::connect_one;
pub(crate) use connect::resolve_servers;
pub use connect::{audit_skips, connect_all};

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

type ClientHandle = Arc<RunningService<RoleClient, SessionHandler>>;

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

/// One prompt a connected server advertised — `/srv:name` resolves through
/// this at submit time; `arg_names` declares positional slots in order, so
/// a bare `/srv:name a b` maps `a`,`b` onto the first two declared args.
#[derive(Debug, Clone)]
pub struct McpPromptInfo {
    pub name: String,
    pub description: Option<String>,
    /// declared positional arg names, in order
    pub arg_names: Vec<String>,
}

/// One resource a connected server advertised — roster-visible (the island
/// bridge already proxies `resources/read` by URI).
#[derive(Debug, Clone)]
pub struct McpResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
}

/// A connected MCP server — the app bridge needs the handle to proxy
/// `resources/read` (ui:// fetch) and `tools/call` out of the island, and
/// turn boundaries read `version`/`notices` off `shared` to catch
/// list-changed updates the server pushed between turns.
pub struct McpServerHandle {
    pub name: String,
    /// `"stdio"` (spawned child) or `"http"` (streamable-HTTP URL) — the
    /// `/mcp` roster reports it; not part of the wire dialect.
    pub transport: &'static str,
    pub client: ClientHandle,
    /// catalogs + notices the client-side handler mutates on server push
    /// (list_changed / elicitation) — shared across every clone
    shared: Arc<Shared>,
    /// this Context's drain watermark — manual Clone resets it to current
    /// so a new session doesn't replay old bumps (it already registers the
    /// fresh catalog at build time)
    seen_version: Arc<AtomicU64>,
}

/// One roster row for `/mcp` — name, transport, how many tools/prompts/
/// resources it advertises, and whether the connection is still live.
#[derive(Debug, Clone)]
pub struct McpServerStatus {
    pub name: String,
    pub transport: &'static str,
    pub tools: usize,
    pub prompts: usize,
    pub resources: usize,
    /// the client service hasn't been closed/cancelled — a dead child or
    /// dropped HTTP transport shows false
    pub connected: bool,
}

impl Clone for McpServerHandle {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            transport: self.transport,
            client: self.client.clone(),
            shared: self.shared.clone(),
            seen_version: Arc::new(AtomicU64::new(
                self.shared
                    .version
                    .load(std::sync::atomic::Ordering::Relaxed),
            )),
        }
    }
}

impl McpServerHandle {
    /// The advertised tool catalog (list_changed-fresh).
    pub fn tools(&self) -> Vec<McpToolInfo> {
        self.shared.tools.read_or_recover().clone()
    }

    /// The advertised prompt catalog.
    pub fn prompts(&self) -> Vec<McpPromptInfo> {
        self.shared.prompts.read_or_recover().clone()
    }

    /// The advertised resource catalog.
    pub fn resources(&self) -> Vec<McpResourceInfo> {
        self.shared.resources.read_or_recover().clone()
    }

    /// Version the shared catalog reached — this Context's
    /// `seen_version` lags it until the next turn boundary drains.
    pub(crate) fn version(&self) -> u64 {
        self.shared
            .version
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The last catalog version this Context applied.
    pub(crate) fn seen_version(&self) -> u64 {
        self.seen_version.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Bump the drain watermark — called once the fresh catalog has been
    /// applied to this Context's registry.
    pub(crate) fn mark_seen(&self) {
        self.seen_version
            .store(self.version(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Things worth a session note — elicitation declines, refresh
    /// failures. Drained at turn boundaries.
    pub(crate) fn drain_notices(&self) -> Vec<String> {
        self.shared.notices.lock_or_recover().drain(..).collect()
    }

    /// Roster row snapshot — reads off the live service state.
    pub fn status(&self) -> McpServerStatus {
        McpServerStatus {
            name: self.name.clone(),
            transport: self.transport,
            tools: self.shared.tools.read_or_recover().len(),
            prompts: self.shared.prompts.read_or_recover().len(),
            resources: self.shared.resources.read_or_recover().len(),
            connected: !self.client.is_closed(),
        }
    }
}

/// What `connect_all` assembled: model-facing tools plus the per-server
/// handles/catalogs the MCP Apps host bridge rides on.
pub struct McpConnected {
    pub tools: Vec<Box<dyn crate::tool::ToolImpl>>,
    pub servers: Vec<McpServerHandle>,
    /// `mcp.untrusted` audit details — stdio specs the trust ledger didn't
    /// pin, skipped before spawn (fail-closed). The caller folds them into
    /// the session log once one exists (`audit_skips`).
    pub skipped: Vec<String>,
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

/// `rmcp::model::Tool` → our catalog row (`mcp__{server}__{tool}` wire
/// name, schema verbatim, ui meta).
fn tool_info(server: &str, t: &rmcp::model::Tool) -> McpToolInfo {
    let ui = ui_meta(t);
    McpToolInfo {
        name: format!("mcp__{server}__{}", t.name),
        server_tool: t.name.to_string(),
        description: t.description.as_deref().unwrap_or_default().to_string(),
        schema: serde_json::to_value(&t.input_schema).unwrap_or_else(|_| json_object()),
        app_visible: ui.as_ref().map(|u| u.app_visible()).unwrap_or(true),
        ui,
    }
}

fn prompt_info(p: &rmcp::model::Prompt) -> McpPromptInfo {
    McpPromptInfo {
        name: p.name.to_string(),
        description: p.description.as_deref().map(str::to_string),
        arg_names: p
            .arguments
            .as_ref()
            .map(|a| a.iter().map(|x| x.name.to_string()).collect())
            .unwrap_or_default(),
    }
}

fn resource_info(r: &rmcp::model::Resource) -> McpResourceInfo {
    McpResourceInfo {
        uri: r.uri.to_string(),
        name: r.name.to_string(),
        description: r.description.as_deref().map(str::to_string),
    }
}

fn json_object() -> Value {
    serde_json::json!({"type": "object"})
}

#[cfg(test)]
mod tests;
