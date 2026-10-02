//! The client-side handler every MCP connection gets — the piece `()`
//! used to be. It owns the parts of the session the protocol can mutate
//! under us: list-changed notifications refresh the catalog + bump the
//! version (the agent loop drains bumps at turn boundaries), elicitations
//! answer with a protocol error + a user-visible notice instead of
//! silently declining.

use crate::context::{MutexRecover, RwLockRecover};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use rmcp::RoleClient;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{ElicitRequestParams, ElicitResult, ErrorData as McpError};
use rmcp::service::{NotificationContext, RequestContext};

use super::{McpPromptInfo, McpResourceInfo, McpToolInfo, tool_info};

/// State the handler and the handle share — a notification mutates the
/// same catalog the registry reads.
pub(crate) struct Shared {
    /// snapshot of the server's `tools/list` — re-read on
    /// `notifications/tools/list_changed`
    pub(crate) tools: RwLock<Vec<McpToolInfo>>,
    /// `prompts/list` — `/srv:prompt` completion + resolution read this
    pub(crate) prompts: RwLock<Vec<McpPromptInfo>>,
    /// `resources/list` — the `/mcp` roster reports the count
    pub(crate) resources: RwLock<Vec<McpResourceInfo>>,
    /// bumped inside the write gate after each refreshed catalog is
    /// applied — per-Context `seen_version` compares against it, and a
    /// nonzero value tells connect_one's bootstrap write a push already
    /// owns the catalog
    pub(crate) version: AtomicU64,
    /// things worth a session note (elicitation declined, list refresh
    /// failures) — drained at the next turn boundary
    pub(crate) notices: Mutex<VecDeque<String>>,
    /// serializes catalog writes — the bootstrap listing in connect_one
    /// races a pushed list_changed refresh; under the gate, whoever runs
    /// second sees the bump and (for bootstrap) stands down, so a stale
    /// first listing can never clobber a post-push catalog
    pub(crate) write_gate: Mutex<()>,
}

impl Shared {
    pub(crate) fn new(
        tools: Vec<McpToolInfo>,
        prompts: Vec<McpPromptInfo>,
        resources: Vec<McpResourceInfo>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tools: RwLock::new(tools),
            prompts: RwLock::new(prompts),
            resources: RwLock::new(resources),
            version: AtomicU64::new(0),
            notices: Mutex::new(VecDeque::new()),
            write_gate: Mutex::new(()),
        })
    }

    fn note(&self, text: String) {
        self.notices.lock_or_recover().push_back(text);
    }
}

/// The per-server client service — replaced the `()` placeholder when the
/// connection started caring about push traffic.
pub struct SessionHandler {
    server: String,
    shared: Arc<Shared>,
}

impl SessionHandler {
    pub(crate) fn new(server: impl Into<String>, shared: Arc<Shared>) -> Self {
        Self {
            server: server.into(),
            shared,
        }
    }

    /// A `list_changed` notification re-reads the advertised catalog —
    /// tools and the two list surfaces it could name. Writes go through
    /// `write_gate`: a bootstrap listing still in flight inside
    /// connect_one must finish first (or lose), never interleave.
    async fn refresh(&self, peer: &rmcp::service::Peer<RoleClient>) {
        let tools = match peer.list_all_tools().await {
            Ok(listed) => Some(listed.iter().map(|t| tool_info(&self.server, t)).collect()),
            Err(e) => {
                self.shared
                    .note(format!("{}: tools/list refresh failed: {e:#}", self.server));
                None
            }
        };
        let prompts = peer
            .list_all_prompts()
            .await
            .ok()
            .map(|listed| listed.iter().map(super::prompt_info).collect());
        let resources = peer
            .list_all_resources()
            .await
            .ok()
            .map(|listed| listed.iter().map(super::resource_info).collect());
        // refresh always wins the gate — it rides a real push, so its
        // listing is by definition the newer state. The version bumps
        // inside the same gate AFTER the write: `version` reads as
        // "catalog applied", so a drain that observes the bump is
        // guaranteed to see this listing, and a bootstrap write in
        // connect_one checking `version == 0` under the gate can never
        // slip between this write and its bump.
        let _g = self.shared.write_gate.lock_or_recover();
        if let Some(t) = tools {
            *self.shared.tools.write_or_recover() = t;
        }
        if let Some(p) = prompts {
            *self.shared.prompts.write_or_recover() = p;
        }
        if let Some(r) = resources {
            *self.shared.resources.write_or_recover() = r;
        }
        self.shared.version.fetch_add(1, Ordering::Relaxed);
    }
}

impl ClientHandler for SessionHandler {
    /// Servers that elicit get a protocol error, not a silent decline —
    /// "unsupported" is a real answer, and the notice makes it visible to
    /// whoever owns this session.
    fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<ElicitResult, McpError>> + Send + '_ {
        let msg = match &request {
            ElicitRequestParams::FormElicitationParams { message, .. } => message.clone(),
            ElicitRequestParams::UrlElicitationParams { message, .. } => message.clone(),
            _ => String::new(),
        };
        self.shared.note(format!(
            "{}: elicitation refused — {} (this client doesn't prompt mid-tool)",
            self.server, msg
        ));
        std::future::ready(Err(McpError::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            "elicitation not supported by this client",
            None,
        )))
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.refresh(&context.peer).await;
    }

    async fn on_prompt_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.refresh(&context.peer).await;
    }

    async fn on_resource_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.refresh(&context.peer).await;
    }
}
